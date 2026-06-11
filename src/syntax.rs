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
//! The parse persists on the `Session` keyed by content version (see
//! `syntax_of` in builtins.rs); a fresh `Syntax::parse` runs only after an
//! edit. Incremental re-parse is a TODO below.
//!
//! Positions: tree-sitter speaks UTF-8 **byte** offsets; mime-rs speaks 1-based
//! **char** positions (Emacs-style, where a position sits *before* the char of
//! that index). [`Syntax`] converts between the two through the source text, so
//! multibyte content (em dashes, accents) maps correctly.
//!
//! TODO (future M7 work):
//!   - Incremental re-parse: feed `InputEdit`s from buffer mutations instead
//!     of a full re-parse per edit (needs edit logging in the stores, lazily
//!     enabled so non-treesit workloads pay nothing).
//!   - More languages (JS/TS, Go, …) — adding one is a `Lang` variant, an
//!     extension mapping, and a `defun_kinds` row.
//!   - AST-edit ops over the current node: `replace-node`, `wrap-node`,
//!     `raise-node`, `kill-node` — thin wrappers now that nodes are
//!     first-class values.
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

/// A durable reference to one node of THIS parse — the data a first-class
/// lisp node value carries. tree-sitter nodes borrow their tree, so they
/// cannot be stored; a `NodeRef` re-finds the node instead: the byte range
/// narrows the search ([`Node::descendant_for_byte_range`] lands on the
/// smallest node in it) and the id — stable for the tree's lifetime — picks
/// the right ancestor when several nodes share the range. Only meaningful
/// against the `Syntax` it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeRef {
    id: usize,
    start_byte: usize,
    end_byte: usize,
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

    /// The nearest enclosing defun-kind node (see [`Lang::defun_kinds`]) at
    /// `pos`, as a char span. Walks up from the smallest node at `pos`; the
    /// innermost qualifying construct wins (a method, not its `impl`). `None`
    /// if `pos` is inside no defun (module top level, leading blank lines, an
    /// empty buffer).
    pub fn enclosing_defun(&self, pos: usize) -> Option<NodeSpan> {
        self.enclosing_defun_node(pos).map(|n| {
            let (start_b, end_b) = self.defun_extent(n);
            NodeSpan {
                kind: n.kind().to_string(),
                start: self.char_of(start_b),
                end: self.char_of(end_b),
            }
        })
    }

    /// The full extent of a defun INCLUDING its decoration: Rust outer
    /// `#[attributes]` are preceding siblings of the item node, Python
    /// decorators live on a wrapping `decorated_definition` — both belong to
    /// the defun an agent means by "delete / replace / narrow to / anchor on
    /// this function". Returns byte offsets. Raw node accessors
    /// (`treesit-node-start` etc.) stay faithful to the tree-sitter node;
    /// only the defun-level views (outline, goto, narrow, begin/end) use
    /// this.
    fn defun_extent(&self, node: Node<'_>) -> (usize, usize) {
        let mut start = node.start_byte();
        let end = node.end_byte();
        match self.lang {
            Lang::Rust => {
                let mut prev = node.prev_named_sibling();
                while let Some(p) = prev {
                    if p.kind() != "attribute_item" {
                        break;
                    }
                    start = p.start_byte();
                    prev = p.prev_named_sibling();
                }
            }
            Lang::Python => {
                if let Some(parent) = node.parent()
                    && parent.kind() == "decorated_definition"
                {
                    start = parent.start_byte();
                }
            }
            Lang::Markdown => {}
        }
        (start, end)
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
            // Decoration belongs to the defun it decorates: a position on a
            // Rust outer attribute resolves to the item the attribute chain
            // ends at, one on a Python decorator to the wrapped definition.
            if node.kind() == "attribute_item" {
                let mut next = node.next_named_sibling();
                while let Some(n) = next {
                    if kinds.contains(&n.kind()) {
                        return Some(n);
                    }
                    if n.kind() != "attribute_item" {
                        break;
                    }
                    next = n.next_named_sibling();
                }
            }
            if node.kind() == "decorated_definition" {
                let mut cursor = node.walk();
                let inner = node
                    .named_children(&mut cursor)
                    .find(|c| kinds.contains(&c.kind()));
                if let Some(c) = inner {
                    return Some(c);
                }
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
                let (start_b, end_b) = self.defun_extent(node);
                out.push(Defun {
                    name: self.name_of(node),
                    kind: node.kind().to_string(),
                    start: self.char_of(start_b),
                    end: self.char_of(end_b),
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

    // ---- first-class nodes (NodeRef handles) -------------------------------

    fn handle(node: Node<'_>) -> NodeRef {
        NodeRef {
            id: node.id(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        }
    }

    /// Re-find the node a [`NodeRef`] points at: a containment-guided descent
    /// from the root, comparing ids. (`descendant_for_byte_range` is NOT
    /// enough: a ZERO-WIDTH node — a missing `block` in `def f():`, a missing
    /// closer — is skipped by it in favor of an adjacent token whose ancestor
    /// chain never reaches the target, so the descent recurses into every
    /// child whose range contains the handle's instead.) `None` only if the
    /// handle is not from this parse — a caller bug surfaced gently.
    fn locate(&self, h: NodeRef) -> Option<Node<'_>> {
        fn descend<'t>(node: Node<'t>, h: NodeRef) -> Option<Node<'t>> {
            if node.id() == h.id {
                return Some(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                // Containment is non-strict: a zero-width handle on a child's
                // boundary is "inside" both neighbours — try each.
                if child.start_byte() <= h.start_byte
                    && h.end_byte <= child.end_byte()
                    && let Some(found) = descend(child, h)
                {
                    return Some(found);
                }
            }
            None
        }
        descend(self.root(), h)
    }

    /// The smallest *named* node covering char position `pos`, as a handle.
    pub fn node_at(&self, pos: usize) -> Option<NodeRef> {
        let b = self.byte_of(pos);
        self.root()
            .named_descendant_for_byte_range(b, b)
            .map(Self::handle)
    }

    /// The nearest enclosing defun at `pos`, as a handle.
    pub fn defun_at(&self, pos: usize) -> Option<NodeRef> {
        self.enclosing_defun_node(pos).map(Self::handle)
    }

    /// The handle's kind + 1-based char span — what a node value displays.
    pub fn describe(&self, h: NodeRef) -> Option<NodeSpan> {
        self.locate(h).map(|n| self.span_of(n))
    }

    /// The handle's source text.
    pub fn text_of_handle(&self, h: NodeRef) -> Option<String> {
        self.locate(h).map(|n| self.text_of(n).to_string())
    }

    /// Relational navigation. Each returns a handle in this same parse, or
    /// `None` where the tree ends. `named` skips anonymous tokens
    /// (punctuation, keywords), which is almost always what an agent wants.
    pub fn parent_of(&self, h: NodeRef) -> Option<NodeRef> {
        self.locate(h)?.parent().map(Self::handle)
    }

    pub fn child_of(&self, h: NodeRef, i: usize, named: bool) -> Option<NodeRef> {
        let n = self.locate(h)?;
        let i = u32::try_from(i).ok()?; // a 2^32+ index is out of range, not child 0
        if named {
            n.named_child(i).map(Self::handle)
        } else {
            n.child(i).map(Self::handle)
        }
    }

    pub fn child_count_of(&self, h: NodeRef, named: bool) -> Option<usize> {
        let n = self.locate(h)?;
        Some(if named {
            n.named_child_count()
        } else {
            n.child_count()
        })
    }

    pub fn next_sibling_of(&self, h: NodeRef, named: bool) -> Option<NodeRef> {
        let n = self.locate(h)?;
        if named {
            n.next_named_sibling().map(Self::handle)
        } else {
            n.next_sibling().map(Self::handle)
        }
    }

    pub fn prev_sibling_of(&self, h: NodeRef, named: bool) -> Option<NodeRef> {
        let n = self.locate(h)?;
        if named {
            n.prev_named_sibling().map(Self::handle)
        } else {
            n.prev_sibling().map(Self::handle)
        }
    }

    pub fn child_by_field_of(&self, h: NodeRef, field: &str) -> Option<NodeRef> {
        self.locate(h)?.child_by_field_name(field).map(Self::handle)
    }

    /// Run a tree-sitter query (`.scm` pattern syntax) over the whole buffer
    /// and return every capture as `(capture_name, handle)`, in match order —
    /// structural search: "every `function_item`", "calls to `foo`", … .
    /// `Err` is the query compile error (pattern syntax / unknown node kind).
    pub fn query(&self, pattern: &str) -> Result<Vec<(String, NodeRef)>, String> {
        let query = Query::new(&self.lang.grammar(), pattern).map_err(|e| e.to_string())?;
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, self.root(), self.text.as_bytes());
        let mut out = Vec::new();
        while let Some(m) = matches.next() {
            for cap in m.captures {
                out.push((
                    names[cap.index as usize].to_string(),
                    Self::handle(cap.node),
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
        let span = syn
            .node_at(4)
            .and_then(|h| syn.describe(h))
            .expect("a node at point");
        assert_eq!(span.kind, "inline");
        // "# Title\n" — inline "Title" is chars 3..=7, i.e. span [3, 8).
        assert_eq!((span.start, span.end), (3, 8));
    }

    #[test]
    fn named_node_inside_paragraph() {
        let syn = Syntax::parse(DOC, Lang::Markdown);
        // "Hello para." begins at char 10 (after "# Title\n\n").
        let p = DOC.find("Hello").unwrap() + 1; // 1-based char == byte here (ASCII)
        let span = syn
            .node_at(p)
            .and_then(|h| syn.describe(h))
            .expect("a node at point");
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
        // Handles address the buffer: the first capture is `abs`.
        let (_, h) = caps[0];
        assert_eq!(syn.text_of_handle(h).as_deref(), Some("abs"));
        let span = syn.describe(h).unwrap();
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
    fn node_handles_relocate_and_navigate() {
        let syn = Syntax::parse(RS, Lang::Rust);
        // Start from the smallest node inside `self.x.abs()`.
        let p = RS.find("abs").unwrap() + 1;
        let leaf = syn.node_at(p).expect("a node at point");
        assert_eq!(syn.describe(leaf).unwrap().kind, "field_identifier");
        assert_eq!(syn.text_of_handle(leaf).as_deref(), Some("abs"));

        // Ascend: field_expression → call_expression … up to the root.
        let parent = syn.parent_of(leaf).expect("a parent");
        assert_eq!(syn.describe(parent).unwrap().kind, "field_expression");
        let mut up = parent;
        while let Some(next) = syn.parent_of(up) {
            up = next;
        }
        assert_eq!(syn.describe(up).unwrap().kind, "source_file");

        // Fields and children: norm's function_item has a name field.
        let norm = syn.defun_at(p).expect("enclosing defun");
        let name = syn.child_by_field_of(norm, "name").expect("name field");
        assert_eq!(syn.text_of_handle(name).as_deref(), Some("norm"));
        assert!(syn.child_count_of(norm, true).unwrap() >= 2);
        let first = syn.child_of(norm, 0, true).expect("first named child");
        assert_eq!(syn.describe(first).unwrap().kind, "identifier");

        // Siblings walk the impl's surroundings: struct → impl → fn main.
        let strct = syn.defun_at(2).expect("struct at top");
        let next = syn.next_sibling_of(strct, true).expect("impl follows");
        assert_eq!(syn.describe(next).unwrap().kind, "impl_item");
        assert_eq!(
            syn.prev_sibling_of(next, true),
            Some(strct),
            "prev inverts next"
        );

        // Unnamed children are visible when asked for: fn main's body block
        // has `{` as child 0 in the unnamed view.
        let main = syn.find_defun("main").unwrap();
        let main_h = syn.defun_at(main.start).expect("main handle");
        let body = syn.child_by_field_of(main_h, "body").expect("body field");
        let brace = syn.child_of(body, 0, false).expect("the { token");
        assert_eq!(syn.describe(brace).unwrap().kind, "{");
    }

    #[test]
    fn zero_width_nodes_locate_and_navigate() {
        // Incomplete code produces real ZERO-WIDTH nodes (a missing `block`
        // in `def f():`); descendant_for_byte_range skips them, so locate's
        // containment descent must find them — a panic here took down the
        // whole process when a query captured one.
        let syn = Syntax::parse("def f():", Lang::Python);
        let caps = syn.query("(block) @b").expect("valid query");
        assert_eq!(caps.len(), 1, "the zero-width block is captured");
        let (_, h) = caps[0];
        let span = syn.describe(h).expect("zero-width handle locates");
        assert_eq!(span.kind, "block");
        assert_eq!(span.start, span.end, "zero width");
        assert_eq!(syn.text_of_handle(h).as_deref(), Some(""));
        // Navigation works from it too: it has a real parent.
        let parent = syn.parent_of(h).expect("the function_definition");
        assert_eq!(syn.describe(parent).unwrap().kind, "function_definition");
        // And child-by-field reaches it from above.
        let body = syn.child_by_field_of(parent, "body").expect("body field");
        assert_eq!(body, h);
    }

    #[test]
    fn char_positions_handle_multibyte() {
        // Em dash (3 bytes) before the heading word shifts byte vs. char offsets.
        let doc = "# Tëa — pot\n\nbody\n";
        let syn = Syntax::parse(doc, Lang::Markdown);
        let p = doc.chars().position(|c| c == 'b').unwrap() + 1; // char index of "body"
        let span = syn
            .node_at(p)
            .and_then(|h| syn.describe(h))
            .expect("a node at point");
        assert_eq!(span.kind, "inline");
        // The span must be addressable as chars: substring by char span recovers
        // the original word.
        let chars: Vec<char> = doc.chars().collect();
        let got: String = chars[span.start - 1..span.end - 1].iter().collect();
        assert_eq!(got, "body");
    }

    #[test]
    fn rust_defun_extent_includes_preceding_attributes() {
        let src = "#[cfg(test)]\n#[test]\nfn check() {\n    assert!(true);\n}\n";
        let syn = Syntax::parse(src, Lang::Rust);
        // The outline span starts at the first attribute, so "delete this
        // test" is the defun span with no manual hop to the #[…] lines.
        let d = syn.find_defun("check").expect("check");
        assert_eq!(d.start, 1, "span starts at #[cfg(test)]");
        // A position ON an attribute resolves to the decorated defun:
        // narrowing / defun-at from the attribute line works.
        let span = syn.enclosing_defun(3).expect("from the attribute line");
        assert_eq!(span.kind, "function_item");
        assert_eq!(span.start, 1);
        assert_eq!(syn.enclosing_defun_name(3).as_deref(), Some("check"));
    }

    #[test]
    fn python_defun_extent_includes_decorators() {
        let src = "@wraps(f)\n@cached\ndef g():\n    pass\n";
        let syn = Syntax::parse(src, Lang::Python);
        let d = syn.find_defun("g").expect("g");
        assert_eq!(d.start, 1, "span starts at @wraps");
        // From a decorator line, the enclosing defun is the decorated def.
        let span = syn.enclosing_defun(2).expect("from the decorator");
        assert_eq!(span.kind, "function_definition");
        assert_eq!(span.start, 1);
    }

    #[test]
    fn attribute_extent_stops_at_non_attribute_siblings() {
        // The doc comment above the attribute chain is NOT pulled in, and
        // the previous item's span is untouched.
        let src = "fn first() {}\n\n/// doc\n#[test]\nfn second() {}\n";
        let syn = Syntax::parse(src, Lang::Rust);
        let first = syn.find_defun("first").expect("first");
        assert_eq!((first.start, first.end), (1, 14));
        let second = syn.find_defun("second").expect("second");
        // "fn first() {}\n\n/// doc\n" = 13 + 1 + 1 + 8 chars → #[test] at 24.
        assert_eq!(second.start, 24, "starts at #[test], not the doc comment");
    }
}
