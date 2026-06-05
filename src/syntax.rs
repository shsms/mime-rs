//! M7 structural / AST-aware editing, via tree-sitter.
//!
//! Three grammars: [`tree_sitter_md`] (Markdown — prose is mime-rs's home
//! turf), [`tree_sitter_rust`], and [`tree_sitter_python`]. The language is
//! detected from the buffer name's extension ([`Lang::from_buffer_name`]) and
//! can be overridden per buffer (`treesit-set-language`), so a buffer opened
//! from `lib.rs` parses as Rust while a piped stdin buffer defaults to
//! Markdown. For Markdown the *block* tree (`MarkdownTree::block_tree`) is
//! used: `document` → `section` → `atx_heading` / `paragraph` / `list` …, so a
//! `section` is the natural top-level "defun" analog for prose. Rust and
//! Python parse with plain [`tree_sitter::Parser`]; their "defun" kinds are
//! the function/type definition nodes ([`Lang::defun_kinds`]).
//!
//! The buffer is re-parsed fresh on every call (`Syntax::parse`). That is the
//! simple, always-correct baseline; incremental re-parse is a TODO below.
//!
//! Positions: tree-sitter speaks UTF-8 **byte** offsets; mime-rs speaks 1-based
//! **char** positions (Emacs-style, where a position sits *before* the char of
//! that index). [`Syntax`] converts between the two through the source text, so
//! multibyte content (em dashes, accents) maps correctly.
//!
//! TODO (future M7 work):
//!   - Persistent per-`Session` tree + incremental re-parse: keep the parse on
//!     the `Session` and feed `InputEdit`s on every buffer mutation instead of
//!     re-parsing from scratch; nodes as first-class values (like markers)
//!     unlock relational navigation (`node-parent` / `child-by-field-name`).
//!   - More languages (JS/TS, Go, …) — adding one is a `Lang` variant, an
//!     extension mapping, and a `defun_kinds` row.
//!   - AST-edit ops over the current node: `replace-node`, `wrap-node`,
//!     `raise-node`, `kill-node`.
//!   - Surface a few of these as MCP tools once the builtin surface settles.

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};
use tree_sitter_md::{MarkdownParser, MarkdownTree};

/// A language the syntax layer can parse. Detected from the buffer name
/// (extension) or set explicitly; Markdown is the fallback for nameless /
/// extension-less buffers (stdin pipes, `open_text` scratch buffers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Markdown,
    Rust,
    Python,
}

impl Lang {
    /// Detect from a buffer name (for file-backed buffers, the path):
    /// `.md`/`.markdown` → Markdown, `.rs` → Rust, `.py`/`.pyi` → Python.
    pub fn from_buffer_name(name: &str) -> Option<Lang> {
        let ext = std::path::Path::new(name).extension()?.to_str()?;
        Lang::from_token(&ext.to_ascii_lowercase())
    }

    /// Parse a language token the way `treesit-set-language` accepts it:
    /// a language name or its conventional extension.
    pub fn from_token(token: &str) -> Option<Lang> {
        match token {
            "markdown" | "md" => Some(Lang::Markdown),
            "rust" | "rs" => Some(Lang::Rust),
            "python" | "py" | "pyi" => Some(Lang::Python),
            _ => None,
        }
    }

    /// The canonical name, as `treesit-language` reports it.
    pub fn name(&self) -> &'static str {
        match self {
            Lang::Markdown => "markdown",
            Lang::Rust => "rust",
            Lang::Python => "python",
        }
    }

    /// The tree-sitter grammar (for Markdown, the *block* grammar — the one
    /// `MarkdownTree::block_tree` nodes come from, so queries match it).
    fn grammar(&self) -> tree_sitter::Language {
        match self {
            Lang::Markdown => tree_sitter_md::LANGUAGE.into(),
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    /// Node kinds that count as a "defun" — the enclosing construct
    /// `treesit-beginning-of-defun` / `treesit-narrow-to-defun` target and
    /// `treesit-list-defuns` outlines. Innermost wins when nested (a method
    /// inside an `impl`, a closure-free nested `def`), matching Emacs.
    fn defun_kinds(&self) -> &'static [&'static str] {
        match self {
            Lang::Markdown => &["section"],
            Lang::Rust => &[
                "function_item",
                "impl_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "mod_item",
            ],
            Lang::Python => &["function_definition", "class_definition"],
        }
    }
}

/// The parse result: Markdown keeps the dedicated `MarkdownTree` (block +
/// inline trees), code languages a plain `tree_sitter::Tree`.
enum ParseTree {
    Md(MarkdownTree),
    Code(tree_sitter::Tree),
}

/// A freshly parsed view of a buffer: the tree plus the source text it was
/// parsed from (needed to map node byte ranges back to chars).
pub struct Syntax {
    text: String,
    lang: Lang,
    tree: ParseTree,
}

/// A node, projected into mime-rs terms: its kind and a 1-based char span
/// `[start, end)` (end is the position just past the last char, Emacs-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSpan {
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

/// A defun (top-level construct) found by [`Syntax::defuns`]: its span plus
/// the name tree-sitter gives it (`""` if anonymous — e.g. a Markdown section
/// whose heading is empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defun {
    pub kind: String,
    pub name: String,
    pub start: usize,
    pub end: usize,
}

impl Syntax {
    /// Parse `text` as `lang`. Owns a copy of the text so the returned value is
    /// self-contained (node byte ranges index into it).
    pub fn parse(text: &str, lang: Lang) -> Syntax {
        let tree = match lang {
            Lang::Markdown => {
                let mut parser = MarkdownParser::default();
                // tree-sitter only fails to parse on timeout/cancellation,
                // neither of which the scaffold sets, so an empty document is a
                // safe fallback.
                let tree = parser
                    .parse(text.as_bytes(), None)
                    .unwrap_or_else(|| parser.parse(b"", None).expect("empty parse"));
                ParseTree::Md(tree)
            }
            Lang::Rust | Lang::Python => {
                let mut parser = tree_sitter::Parser::new();
                parser
                    .set_language(&lang.grammar())
                    .expect("bundled grammar matches the tree-sitter ABI");
                let tree = parser
                    .parse(text.as_bytes(), None)
                    .unwrap_or_else(|| parser.parse(b"", None).expect("empty parse"));
                ParseTree::Code(tree)
            }
        };
        Syntax {
            text: text.to_string(),
            lang,
            tree,
        }
    }

    /// The language this view was parsed as.
    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// The root of the tree (`document` for Markdown, `source_file` for Rust,
    /// `module` for Python).
    fn root(&self) -> Node<'_> {
        match &self.tree {
            ParseTree::Md(t) => t.block_tree().root_node(),
            ParseTree::Code(t) => t.root_node(),
        }
    }

    /// Kind of the root node. Proves the parse ran.
    pub fn root_kind(&self) -> String {
        self.root().kind().to_string()
    }

    /// `true` if the parse tree contains any `ERROR` / missing node — i.e. the
    /// buffer is not syntactically well-formed for its language. The cheap
    /// "did my edit break the file?" check.
    pub fn has_error(&self) -> bool {
        self.root().has_error()
    }

    /// Byte offset of 1-based char position `pos` (clamped into the text). The
    /// byte *before which* the char at `pos` starts; `char_len + 1` maps to the
    /// end of the text.
    fn byte_of(&self, pos: usize) -> usize {
        let pos = pos.max(1);
        self.text
            .char_indices()
            .nth(pos - 1)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// 1-based char position of byte offset `byte` (clamped, and snapped down to
    /// a char boundary so a mid-char byte still maps to that char's position).
    fn char_of(&self, byte: usize) -> usize {
        let mut byte = byte.min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        self.text[..byte].chars().count() + 1
    }

    /// Project a tree-sitter node into a [`NodeSpan`] (kind + 1-based char span).
    fn span_of(&self, node: Node<'_>) -> NodeSpan {
        NodeSpan {
            kind: node.kind().to_string(),
            start: self.char_of(node.start_byte()),
            end: self.char_of(node.end_byte()),
        }
    }

    /// The node's source text.
    fn text_of(&self, node: Node<'_>) -> &str {
        &self.text[node.start_byte()..node.end_byte()]
    }

    /// The smallest *named* node covering char position `pos`, as a [`NodeSpan`].
    /// `None` only for a degenerate (empty) tree. A zero-width query at `pos`
    /// finds the node containing the gap before the char at `pos`.
    pub fn named_node_at(&self, pos: usize) -> Option<NodeSpan> {
        let b = self.byte_of(pos);
        self.root()
            .named_descendant_for_byte_range(b, b)
            .map(|n| self.span_of(n))
    }

    /// The nearest enclosing defun-kind node (see [`Lang::defun_kinds`]) at
    /// `pos`, as a char span. Walks up from the smallest node at `pos`; the
    /// innermost qualifying construct wins (a method, not its `impl`). `None`
    /// if `pos` is inside no defun (module top level, leading blank lines, an
    /// empty buffer).
    pub fn enclosing_defun(&self, pos: usize) -> Option<NodeSpan> {
        self.enclosing_defun_node(pos).map(|n| self.span_of(n))
    }

    /// Name of the nearest enclosing defun at `pos` — `None` if there is no
    /// enclosing defun *or* it is anonymous.
    pub fn enclosing_defun_name(&self, pos: usize) -> Option<String> {
        let name = self.name_of(self.enclosing_defun_node(pos)?);
        (!name.is_empty()).then_some(name)
    }

    fn enclosing_defun_node(&self, pos: usize) -> Option<Node<'_>> {
        let b = self.byte_of(pos);
        let kinds = self.lang.defun_kinds();
        let mut node = self.root().descendant_for_byte_range(b, b)?;
        loop {
            if kinds.contains(&node.kind()) {
                return Some(node);
            }
            node = node.parent()?;
        }
    }

    /// Every defun-kind node in the buffer, in document order (nested ones —
    /// methods in an `impl`, subsections — included, after their parent). The
    /// buffer outline.
    pub fn defuns(&self) -> Vec<Defun> {
        let kinds = self.lang.defun_kinds();
        let mut out = Vec::new();
        let mut stack = vec![self.root()];
        while let Some(node) = stack.pop() {
            if kinds.contains(&node.kind()) {
                let span = self.span_of(node);
                out.push(Defun {
                    name: self.name_of(node),
                    kind: span.kind,
                    start: span.start,
                    end: span.end,
                });
            }
            // Push named children in reverse so the stack pops them in
            // document order.
            for i in (0..node.named_child_count() as u32).rev() {
                if let Some(child) = node.named_child(i) {
                    stack.push(child);
                }
            }
        }
        out
    }

    /// The first defun (document order) named `name` — how an agent addresses
    /// "the function `parse_args`" without knowing where it is.
    pub fn find_defun(&self, name: &str) -> Option<Defun> {
        self.defuns().into_iter().find(|d| d.name == name)
    }

    /// A defun's name. Code grammars expose it as the `name` field (Rust
    /// `impl_item` has no name, so its `type` — `impl Foo` → `Foo` — stands
    /// in); a Markdown section is named by its heading text. `""` if the
    /// grammar offers nothing.
    fn name_of(&self, node: Node<'_>) -> String {
        match self.lang {
            Lang::Markdown => {
                // section → atx_heading/setext_heading → inline (the heading text).
                let mut cursor = node.walk();
                let heading = node
                    .named_children(&mut cursor)
                    .find(|c| c.kind().ends_with("_heading"));
                let Some(heading) = heading else {
                    return String::new();
                };
                let mut hc = heading.walk();
                heading
                    .named_children(&mut hc)
                    .find(|c| c.kind() == "inline")
                    .map(|c| self.text_of(c).trim().to_string())
                    .unwrap_or_default()
            }
            Lang::Rust | Lang::Python => node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("type"))
                .map(|n| self.text_of(n).to_string())
                .unwrap_or_default(),
        }
    }

    /// Run a tree-sitter query (`.scm` pattern syntax) over the whole buffer
    /// and return every capture as `(capture_name, span)`, in match order —
    /// structural search: "every `function_item`", "calls to `foo`", … .
    /// `Err` is the query compile error (pattern syntax / unknown node kind).
    pub fn query(&self, pattern: &str) -> Result<Vec<(String, NodeSpan)>, String> {
        let query = Query::new(&self.lang.grammar(), pattern).map_err(|e| e.to_string())?;
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, self.root(), self.text.as_bytes());
        let mut out = Vec::new();
        while let Some(m) = matches.next() {
            for cap in m.captures {
                out.push((
                    names[cap.index as usize].to_string(),
                    self.span_of(cap.node),
                ));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Title\n\nHello para.\n\n## Sub\n\nMore text here.\n";

    // Note: the `p.norm()` call sits OUTSIDE any macro — inside `println!` it
    // would parse as an opaque token_tree, invisible to expression queries.
    const RS: &str = "struct Point {\n    x: i64,\n}\n\nimpl Point {\n    fn norm(&self) -> i64 {\n        self.x.abs()\n    }\n}\n\nfn main() {\n    let p = Point { x: -3 };\n    let _n = p.norm();\n}\n";

    const PY: &str = "class Greeter:\n    def greet(self, name):\n        return f\"hi {name}\"\n\ndef main():\n    print(Greeter().greet(\"you\"))\n";

    #[test]
    fn detects_language_from_buffer_name() {
        assert_eq!(Lang::from_buffer_name("notes.md"), Some(Lang::Markdown));
        assert_eq!(Lang::from_buffer_name("/a/b/lib.rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_buffer_name("tool.py"), Some(Lang::Python));
        assert_eq!(Lang::from_buffer_name("types.PYI"), Some(Lang::Python));
        assert_eq!(Lang::from_buffer_name("stdin"), None);
        assert_eq!(Lang::from_buffer_name("a.tar.gz"), None);
    }

    #[test]
    fn root_is_document() {
        let syn = Syntax::parse(DOC, Lang::Markdown);
        assert_eq!(syn.root_kind(), "document");
    }

    #[test]
    fn rust_and_python_roots() {
        assert_eq!(Syntax::parse(RS, Lang::Rust).root_kind(), "source_file");
        assert_eq!(Syntax::parse(PY, Lang::Python).root_kind(), "module");
    }

    #[test]
    fn empty_buffer_still_parses() {
        for lang in [Lang::Markdown, Lang::Rust, Lang::Python] {
            let syn = Syntax::parse("", lang);
            // No defun to land in, but querying must not panic.
            assert!(syn.enclosing_defun(1).is_none(), "{lang:?}");
            assert!(!syn.has_error(), "{lang:?}");
        }
    }

    #[test]
    fn named_node_at_a_heading_word() {
        let syn = Syntax::parse(DOC, Lang::Markdown);
        // Char position inside "Title" — the smallest named node is the heading's
        // inline content.
        let span = syn.named_node_at(4).expect("a node at point");
        assert_eq!(span.kind, "inline");
        // "# Title\n" — inline "Title" is chars 3..=7, i.e. span [3, 8).
        assert_eq!((span.start, span.end), (3, 8));
    }

    #[test]
    fn named_node_inside_paragraph() {
        let syn = Syntax::parse(DOC, Lang::Markdown);
        // "Hello para." begins at char 10 (after "# Title\n\n").
        let p = DOC.find("Hello").unwrap() + 1; // 1-based char == byte here (ASCII)
        let span = syn.named_node_at(p).expect("a node at point");
        assert_eq!(span.kind, "inline");
    }

    #[test]
    fn enclosing_defun_is_innermost_section() {
        let syn = Syntax::parse(DOC, Lang::Markdown);
        // Point in the H2 body → the H2 section (nested inside the H1 section),
        // i.e. the most local heading scope.
        let p = DOC.find("More").unwrap() + 1;
        let sec = syn.enclosing_defun(p).expect("a section");
        assert_eq!(sec.kind, "section");
        // "## Sub\n\nMore text here.\n" starts at char 23 and runs to end (47).
        assert_eq!(sec.start, 23);
        assert_eq!(sec.end, 47);
    }

    #[test]
    fn enclosing_defun_under_h1() {
        let syn = Syntax::parse(DOC, Lang::Markdown);
        // Point in the H1 paragraph → the outer H1 section, which spans the whole
        // document (the H2 section nests inside it).
        let p = DOC.find("Hello").unwrap() + 1;
        let sec = syn.enclosing_defun(p).expect("a section");
        assert_eq!(sec.start, 1);
    }

    #[test]
    fn rust_enclosing_defun_is_the_method_not_the_impl() {
        let syn = Syntax::parse(RS, Lang::Rust);
        let p = RS.find("abs").unwrap() + 1; // inside Point::norm's body
        let f = syn.enclosing_defun(p).expect("a defun");
        assert_eq!(f.kind, "function_item");
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("norm"));
    }

    #[test]
    fn python_enclosing_defun_and_name() {
        let syn = Syntax::parse(PY, Lang::Python);
        let p = PY.find("return").unwrap() + 1; // inside Greeter.greet
        let f = syn.enclosing_defun(p).expect("a defun");
        assert_eq!(f.kind, "function_definition");
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("greet"));
    }

    #[test]
    fn defuns_outline_rust_in_document_order() {
        let syn = Syntax::parse(RS, Lang::Rust);
        let got: Vec<(String, String)> =
            syn.defuns().into_iter().map(|d| (d.kind, d.name)).collect();
        assert_eq!(
            got,
            vec![
                ("struct_item".into(), "Point".into()),
                ("impl_item".into(), "Point".into()),
                ("function_item".into(), "norm".into()),
                ("function_item".into(), "main".into()),
            ]
        );
    }

    #[test]
    fn defuns_outline_python_and_markdown() {
        let py: Vec<String> = Syntax::parse(PY, Lang::Python)
            .defuns()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(py, vec!["Greeter", "greet", "main"]);

        let md: Vec<String> = Syntax::parse(DOC, Lang::Markdown)
            .defuns()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(md, vec!["Title", "Sub"]);
    }

    #[test]
    fn find_defun_addresses_a_function_by_name() {
        let syn = Syntax::parse(RS, Lang::Rust);
        let d = syn.find_defun("main").expect("main exists");
        assert_eq!(d.kind, "function_item");
        // The span recovers the function's source text.
        let chars: Vec<char> = RS.chars().collect();
        let got: String = chars[d.start - 1..d.end - 1].iter().collect();
        assert!(got.starts_with("fn main()") && got.ends_with('}'));
        assert!(syn.find_defun("nonexistent").is_none());
    }

    #[test]
    fn has_error_flags_broken_code() {
        assert!(!Syntax::parse(RS, Lang::Rust).has_error());
        assert!(Syntax::parse("fn broken( {", Lang::Rust).has_error());
        assert!(Syntax::parse("def broken(:\n", Lang::Python).has_error());
    }

    #[test]
    fn query_finds_calls_by_structure() {
        let syn = Syntax::parse(RS, Lang::Rust);
        // Every method call's name — structural search, not regex.
        let caps = syn
            .query(
                "(call_expression function: (field_expression field: (field_identifier) @callee))",
            )
            .expect("valid query");
        let names: Vec<&str> = caps.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["callee", "callee"]);
        // Spans address the buffer: the first capture is `abs`.
        let (_, span) = &caps[0];
        let chars: Vec<char> = RS.chars().collect();
        let got: String = chars[span.start - 1..span.end - 1].iter().collect();
        assert_eq!(got, "abs");
    }

    #[test]
    fn query_compile_error_is_err_not_panic() {
        let syn = Syntax::parse(RS, Lang::Rust);
        assert!(syn.query("(nonexistent_node_kind) @x").is_err());
        assert!(syn.query("(unbalanced").is_err());
    }

    #[test]
    fn char_positions_handle_multibyte() {
        // Em dash (3 bytes) before the heading word shifts byte vs. char offsets.
        let doc = "# Tëa — pot\n\nbody\n";
        let syn = Syntax::parse(doc, Lang::Markdown);
        let p = doc.chars().position(|c| c == 'b').unwrap() + 1; // char index of "body"
        let span = syn.named_node_at(p).expect("a node at point");
        assert_eq!(span.kind, "inline");
        // The span must be addressable as chars: substring by char span recovers
        // the original word.
        let chars: Vec<char> = doc.chars().collect();
        let got: String = chars[span.start - 1..span.end - 1].iter().collect();
        assert_eq!(got, "body");
    }
}
