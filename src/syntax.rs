//! M7 structural / AST-aware editing, via tree-sitter.
//!
//! Three grammars: [`tree_sitter_md`] (Markdown — prose is mime-rs's home
//! turf), [`tree_sitter_rust`], and [`tree_sitter_python`]. The language is
//! detected from the buffer name's extension ([`Lang::from_buffer_name`]) and
//! can be overridden per buffer (`treesit-set-language`), so a buffer opened
//! from `lib.rs` parses as Rust while a piped stdin buffer defaults to
//! Markdown. For Markdown the *block* tree (`MarkdownTree::block_tree`) is
//! used: `document` → `section` → `atx_heading` / `paragraph` / `list` …, so a
//! `section` is the natural top-level "defun" analog for prose. Rust and Python
//! parse with plain [`tree_sitter::Parser`]; their "defun" kinds are the
//! function/type definition nodes ([`Lang::defun_kinds`]).
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
//!   - Incremental re-parse: feed `InputEdit`s from buffer mutations instead of
//!     a full re-parse per edit (needs edit logging in the stores, lazily
//!     enabled so non-treesit workloads pay nothing).
//!   - More languages (JS/TS, Go, …) — adding one is a `Lang` variant, an
//!     extension mapping, and a `defun_kinds` row.
//!   - AST-edit ops over the current node: `replace-node`, `wrap-node`,
//!     `raise-node`, `kill-node` — thin wrappers now that nodes are first-class
//!     values.
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
    Html,
    Javascript,
    Css,
    Toml,
    Yaml,
    Elisp,
    Typescript,
    Tsx,
    Go,
}

impl Lang {
    /// Detect from a buffer name (for file-backed buffers, the path):
    /// `.md`/`.markdown` → Markdown, `.rs` → Rust, `.py`/`.pyi` → Python.
    pub fn from_buffer_name(name: &str) -> Option<Lang> {
        let ext = std::path::Path::new(name).extension()?.to_str()?;
        Lang::from_token(&ext.to_ascii_lowercase())
    }

    /// Parse a language token the way `treesit-set-language` accepts it: a
    /// language name or its conventional extension.
    pub fn from_token(token: &str) -> Option<Lang> {
        match token {
            "markdown" | "md" => Some(Lang::Markdown),
            "rust" | "rs" => Some(Lang::Rust),
            "python" | "py" | "pyi" => Some(Lang::Python),
            "html" | "htm" => Some(Lang::Html),
            "javascript" | "js" | "mjs" | "cjs" | "jsx" => Some(Lang::Javascript),
            "css" => Some(Lang::Css),
            "toml" => Some(Lang::Toml),
            "yaml" | "yml" => Some(Lang::Yaml),
            // .tl is tulisp — mime-rs's own script dialect parses as elisp.
            "elisp" | "el" | "tl" => Some(Lang::Elisp),
            // .tsx parses with the TSX-capable grammar variant below.
            "typescript" | "ts" | "mts" | "cts" => Some(Lang::Typescript),
            "tsx" => Some(Lang::Tsx),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }

    /// The canonical name, as `treesit-language` reports it.
    pub fn name(&self) -> &'static str {
        match self {
            Lang::Markdown => "markdown",
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Html => "html",
            Lang::Javascript => "javascript",
            Lang::Css => "css",
            Lang::Toml => "toml",
            Lang::Yaml => "yaml",
            Lang::Elisp => "elisp",
            Lang::Typescript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
        }
    }

    /// The tree-sitter grammar (for Markdown, the *block* grammar — the one
    /// `MarkdownTree::block_tree` nodes come from, so queries match it).
    fn grammar(&self) -> tree_sitter::Language {
        match self {
            Lang::Markdown => tree_sitter_md::LANGUAGE.into(),
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Html => tree_sitter_html::LANGUAGE.into(),
            Lang::Javascript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Css => tree_sitter_css::LANGUAGE.into(),
            Lang::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Lang::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Lang::Elisp => tree_sitter_elisp::LANGUAGE.into(),
            Lang::Typescript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            // TSX is NOT a superset of TS: an angle-bracket type assertion
            // (`<Foo>bar`) parses as JSX there. Each extension gets its own
            // grammar variant.
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
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
            Lang::Html => &["element", "script_element", "style_element"],
            Lang::Javascript => &[
                "function_declaration",
                "generator_function_declaration",
                "class_declaration",
                "method_definition",
            ],
            Lang::Css => &["rule_set", "media_statement", "keyframes_statement"],
            Lang::Toml => &["table", "table_array_element"],
            // Every key-value pair: innermost-wins narrowing addresses any
            // nesting level, and the outline doubles as the document's key
            // tree.
            Lang::Yaml => &["block_mapping_pair"],
            Lang::Elisp => &["function_definition", "macro_definition"],
            Lang::Typescript | Lang::Tsx => &[
                "function_declaration",
                "generator_function_declaration",
                "class_declaration",
                "abstract_class_declaration",
                "method_definition",
                "interface_declaration",
                "enum_declaration",
                "type_alias_declaration",
                "module_declaration",
            ],
            Lang::Go => &[
                "function_declaration",
                "method_declaration",
                "type_declaration",
            ],
        }
    }

    /// Whether `c` is a symbol constituent: an alphanumeric or `_` everywhere,
    /// plus the per-mode syntax-table extras `forward-symbol` cares about — in
    /// Emacs Lisp `string-trim-left` is one symbol and `:foo` is a keyword; in
    /// CSS `font-size` is one identifier. A quote or backquote is never part of
    /// a symbol, matching Emacs.
    pub fn is_symbol_char(&self, c: char) -> bool {
        let extra = match self {
            Lang::Elisp => "-+*/<>=!?:%&$~^",
            Lang::Css => "-",
            _ => "",
        };
        c.is_alphanumeric() || c == '_' || extra.contains(c)
    }

    /// The syntax the sexp scanner (`crate::sexp`) needs for this language.
    /// Brackets are `()` `[]` `{}` everywhere; this names the string quotes,
    /// the comment openers and the expression-prefix characters. Not an Emacs
    /// syntax table: Rust char literals and lifetimes, Python f-strings and JS
    /// regex literals are read as plain strings or symbols, and the backslashes
    /// in Rust and Python raw strings still escape.
    pub fn sexp_rule(&self) -> &'static SexpRule {
        match self {
            Lang::Rust => &C_LIKE_RULE,
            Lang::Go => &GO_RULE,
            Lang::Javascript | Lang::Typescript | Lang::Tsx => &JS_RULE,
            Lang::Python => &PYTHON_RULE,
            Lang::Css => &CSS_RULE,
            Lang::Toml | Lang::Yaml => &HASH_RULE,
            Lang::Elisp => &ELISP_RULE,
            Lang::Html => &HTML_RULE,
            Lang::Markdown => &MARKDOWN_RULE,
        }
    }
}

/// What the sexp scanner knows about one language; see [`Lang::sexp_rule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SexpRule {
    /// Characters that both open and close a string. A backslash escapes the
    /// next character inside every string except the `raw_quotes` ones.
    pub quotes: &'static [char],
    /// The `quotes` that take no escapes at all (Go's backtick): a backslash
    /// inside is text.
    pub raw_quotes: &'static [char],
    /// Three of the same quote in a row open a string that three close
    /// (Python).
    pub triple_quotes: bool,
    /// Line comment openers; the comment runs to the end of the line.
    pub line_comments: &'static [&'static str],
    /// Block comment opener and closer pairs.
    pub block_comments: &'static [(&'static str, &'static str)],
    /// Expression prefixes: characters that belong to the sexp after them, as
    /// `'` in `'(a b)` (Emacs's prefix syntax flag).
    pub prefixes: &'static [char],
}

const C_LIKE_RULE: SexpRule = SexpRule {
    quotes: &['"'],
    raw_quotes: &[],
    triple_quotes: false,
    line_comments: &["//"],
    block_comments: &[("/*", "*/")],
    prefixes: &[],
};
const GO_RULE: SexpRule = SexpRule {
    quotes: &['"', '`'],
    raw_quotes: &['`'],
    ..C_LIKE_RULE
};
const JS_RULE: SexpRule = SexpRule {
    quotes: &['"', '\'', '`'],
    ..C_LIKE_RULE
};
const PYTHON_RULE: SexpRule = SexpRule {
    quotes: &['"', '\''],
    raw_quotes: &[],
    triple_quotes: true,
    line_comments: &["#"],
    block_comments: &[],
    prefixes: &[],
};
const CSS_RULE: SexpRule = SexpRule {
    quotes: &['"', '\''],
    line_comments: &[],
    ..C_LIKE_RULE
};
const HASH_RULE: SexpRule = SexpRule {
    triple_quotes: false,
    ..PYTHON_RULE
};
const ELISP_RULE: SexpRule = SexpRule {
    quotes: &['"'],
    raw_quotes: &[],
    triple_quotes: false,
    line_comments: &[";"],
    block_comments: &[],
    prefixes: &['\'', '`', ',', '@', '#'],
};
const HTML_RULE: SexpRule = SexpRule {
    quotes: &['"', '\''],
    raw_quotes: &[],
    triple_quotes: false,
    line_comments: &[],
    block_comments: &[("<!--", "-->")],
    prefixes: &[],
};
const MARKDOWN_RULE: SexpRule = SexpRule {
    quotes: &['"'],
    ..HTML_RULE
};

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
    /// Byte↔char checkpoints, one per ~[`CHECKPOINT_BYTES`] of text (always
    /// starting with `(0, 0)`), each `(byte_offset, chars_before_it)` on a char
    /// boundary. Position conversions binary-search here and scan only the
    /// residue, so they are O(log n + K) instead of the O(text) prefix scan
    /// that made mapping a big query's captures O(captures × file).
    checkpoints: Vec<(usize, usize)>,
}

/// Spacing of the byte↔char conversion checkpoints. 4 KiB keeps the residue
/// scan cache-friendly while the table stays ~16 B per 4 KiB of text.
const CHECKPOINT_BYTES: usize = 4096;

fn build_checkpoints(text: &str) -> Vec<(usize, usize)> {
    let mut cps = vec![(0usize, 0usize)];
    let mut next = CHECKPOINT_BYTES;
    for (chars, (b, _)) in text.char_indices().enumerate() {
        if b >= next {
            cps.push((b, chars));
            next = b + CHECKPOINT_BYTES;
        }
    }
    cps
}

/// A node, projected into mime-rs terms: its kind and a 1-based char span
/// `[start, end)` (end is the position just past the last char, Emacs-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSpan {
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

/// A defun (top-level construct) found by [`Syntax::defuns`]: its span plus the
/// name tree-sitter gives it (`""` if anonymous — e.g. a Markdown section whose
/// heading is empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defun {
    pub kind: String,
    pub name: String,
    /// Where the defun begins, its decoration (doc comment, attributes,
    /// decorators) included.
    pub start: usize,
    pub end: usize,
    /// Where the definition itself begins, after its decoration: the line of
    /// `fn name(` rather than of its doc comment.
    pub node_start: usize,
}

/// What the paragraph filler may reflow; see [`Syntax::prose_unit_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProseKind {
    /// Consecutive line comments, each starting its line.
    LineComments,
    /// One `/* … */`-style comment.
    BlockComment,
    /// A Python docstring: a triple-quoted string that is the first statement
    /// of a module, class or function.
    TripleString,
    /// A Markdown paragraph (a list item's or block quote's included).
    Paragraph,
}

impl ProseKind {
    pub fn name(self) -> &'static str {
        match self {
            ProseKind::LineComments => "comment",
            ProseKind::BlockComment => "block comment",
            ProseKind::TripleString => "docstring",
            ProseKind::Paragraph => "paragraph",
        }
    }
}

/// A prose unit as whole lines: the 1-based char span `[start, end)` from the
/// start of its first line through its last line's newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProseUnit {
    pub kind: ProseKind,
    pub start: usize,
    pub end: usize,
}

/// A durable reference to one node of THIS parse — the data a first-class lisp
/// node value carries. tree-sitter nodes borrow their tree, so they cannot be
/// stored; a `NodeRef` re-finds the node instead: the byte range narrows the
/// search ([`Node::descendant_for_byte_range`] lands on the smallest node in
/// it) and the id — stable for the tree's lifetime — picks the right ancestor
/// when several nodes share the range. Only meaningful against the `Syntax` it
/// came from.
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
            _ => {
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
            checkpoints: build_checkpoints(text),
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
    /// buffer is not syntactically well-formed for its language. The cheap "did
    /// my edit break the file?" check.
    pub fn has_error(&self) -> bool {
        self.root().has_error()
    }

    /// Byte offset of 1-based char position `pos` (clamped into the text). The
    /// byte *before which* the char at `pos` starts; `char_len + 1` maps to the
    /// end of the text.
    fn byte_of(&self, pos: usize) -> usize {
        let target = pos.max(1) - 1; // chars before the position
        // Last checkpoint at or before `target` chars, then walk the residue.
        let i = self.checkpoints.partition_point(|&(_, c)| c <= target) - 1;
        let (mut byte, cp_chars) = self.checkpoints[i];
        let mut it = self.text[byte..].char_indices();
        match it.nth(target - cp_chars) {
            Some((off, _)) => byte += off,
            None => byte = self.text.len(),
        }
        byte
    }

    /// 1-based char position of byte offset `byte` (clamped, and snapped down
    /// to a char boundary so a mid-char byte still maps to that char's
    /// position).
    fn char_of(&self, byte: usize) -> usize {
        let mut byte = byte.min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        // Last checkpoint at or before `byte`, then count only the residue.
        let i = self.checkpoints.partition_point(|&(b, _)| b <= byte) - 1;
        let (cp_byte, cp_chars) = self.checkpoints[i];
        cp_chars + self.text[cp_byte..byte].chars().count() + 1
    }

    /// Project a tree-sitter node into a [`NodeSpan`] (kind + 1-based char
    /// span).
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
    /// `#[attributes]` and `///` / `/** */` doc comments are preceding siblings
    /// of the item node, Go and JavaScript/TypeScript doc comments are the
    /// comment block adjacent above it (and an `export` wrapper is part of the
    /// item), Python decorators live on a wrapping `decorated_definition` — all
    /// belong to the defun an agent means by "delete / replace / narrow to /
    /// anchor on this function". Returns byte offsets. Raw node accessors
    /// (`treesit-node-start` etc.) stay faithful to the tree-sitter node; only
    /// the defun-level views (outline, goto, narrow, begin/end) use this.
    fn defun_extent(&self, node: Node<'_>) -> (usize, usize) {
        let end = node.end_byte();
        // The wrapper, when there is one, is the node whose siblings the
        // decoration sits among.
        let outer = self.wrapper_of(node).unwrap_or(node);
        let mut start = outer.start_byte();
        let mut cur = outer;
        while let Some(p) = cur.prev_named_sibling() {
            if !self.decorates_next(p, cur) {
                break;
            }
            start = p.start_byte();
            cur = p;
        }
        (start, end)
    }

    /// The node that wraps a defun and belongs to it: Python's
    /// `decorated_definition`, JavaScript/TypeScript's `export_statement`.
    fn wrapper_of<'t>(&self, node: Node<'t>) -> Option<Node<'t>> {
        let parent = node.parent()?;
        let wraps = match self.lang {
            Lang::Python => parent.kind() == "decorated_definition",
            Lang::Javascript | Lang::Typescript | Lang::Tsx => parent.kind() == "export_statement",
            _ => false,
        };
        wraps.then_some(parent)
    }

    /// `node` itself when it is a defun, or the defun it wraps (see
    /// [`Self::wrapper_of`]).
    fn defun_of<'t>(&self, node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
        if kinds.contains(&node.kind()) {
            return Some(node);
        }
        if !matches!(node.kind(), "decorated_definition" | "export_statement") {
            return None;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|c| kinds.contains(&c.kind()) && self.wrapper_of(*c) == Some(node))
    }

    /// Whether `node` is decoration belonging to `next`, its following named
    /// sibling: a Rust outer attribute or outer doc comment (`///` or `/** */`;
    /// inner `//!` docs and plain `//` comments are not — and no adjacency
    /// test, since a detached `///` does not compile), or, in Go and
    /// JavaScript/TypeScript, a comment on its own line with no blank line
    /// between it and `next` — those languages' doc-comment convention.  A
    /// comment trailing the previous item's code is that item's.
    fn decorates_next(&self, node: Node<'_>, next: Node<'_>) -> bool {
        match self.lang {
            Lang::Rust => match node.kind() {
                "attribute_item" => true,
                "line_comment" => {
                    let text = &self.text[node.byte_range()];
                    text.starts_with("///") && !text.starts_with("////")
                }
                "block_comment" => {
                    let text = &self.text[node.byte_range()];
                    text.starts_with("/**") && !text.starts_with("/***") && text != "/**/"
                }
                _ => false,
            },
            Lang::Go | Lang::Javascript | Lang::Typescript | Lang::Tsx => {
                node.kind() == "comment"
                    && node.end_position().row + 1 == next.start_position().row
                    && self.starts_its_line(node)
            }
            _ => false,
        }
    }

    /// Whether only whitespace precedes `node` on its line.
    fn starts_its_line(&self, node: Node<'_>) -> bool {
        let start = node.start_byte();
        let line_start = self.text[..start].rfind('\n').map_or(0, |i| i + 1);
        self.text[line_start..start].trim().is_empty()
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
            // Decoration belongs to the defun it decorates: a position on a
            // Rust outer attribute or doc comment, or on a Go / JS / TS doc
            // comment, resolves to the item the decoration chain ends at; one
            // on a Python decorator or a JS `export` to the wrapped definition.
            let mut cur = node;
            while let Some(n) = cur.next_named_sibling() {
                if !self.decorates_next(cur, n) {
                    break;
                }
                if let Some(d) = self.defun_of(n, kinds) {
                    return Some(d);
                }
                cur = n;
            }
            if let Some(d) = self.defun_of(node, kinds) {
                return Some(d);
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
                    node_start: self.char_of(node.start_byte()),
                });
            }
            // Push named children in reverse so the stack pops them in document
            // order.
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
                // section → atx_heading/setext_heading → inline (the heading
                // text).
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
            Lang::Rust
            | Lang::Python
            | Lang::Javascript
            | Lang::Elisp
            | Lang::Typescript
            | Lang::Tsx => node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("type"))
                .map(|n| self.text_of(n).to_string())
                .unwrap_or_default(),
            Lang::Go => node
                .child_by_field_name("name")
                .or_else(|| {
                    // `type Foo struct {…}` is a type_declaration wrapping
                    // type_spec(s); the first spec's name stands in.
                    let mut c = node.walk();
                    node.named_children(&mut c)
                        .find_map(|n| n.child_by_field_name("name"))
                })
                .map(|n| self.text_of(n).to_string())
                .unwrap_or_default(),
            Lang::Html => {
                // element → start_tag/self_closing_tag → tag_name.
                let mut c = node.walk();
                node.named_children(&mut c)
                    .find(|n| matches!(n.kind(), "start_tag" | "self_closing_tag"))
                    .and_then(|st| {
                        let mut sc = st.walk();
                        st.named_children(&mut sc)
                            .find(|n| n.kind() == "tag_name")
                            .map(|n| self.text_of(n).to_string())
                    })
                    .unwrap_or_default()
            }
            Lang::Css => {
                // rule_set → selectors text; @media → its query; @keyframes →
                // its name — i.e. everything before the block, joined.
                let mut c = node.walk();
                let head: Vec<String> = node
                    .named_children(&mut c)
                    .take_while(|n| !n.kind().contains("block"))
                    .map(|n| self.text_of(n).trim().to_string())
                    .collect();
                head.join(" ")
            }
            Lang::Toml => {
                // [table] / [[table_array_element]] → the bracketed key.
                let mut c = node.walk();
                node.named_children(&mut c)
                    .find(|n| n.kind().ends_with("_key") || n.kind() == "key")
                    .map(|n| self.text_of(n).to_string())
                    .unwrap_or_default()
            }
            Lang::Yaml => node
                .child_by_field_name("key")
                .map(|n| self.text_of(n).trim().to_string())
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
    /// chain never reaches the target, so the descent recurses into every child
    /// whose range contains the handle's instead.) `None` only if the handle is
    /// not from this parse — a caller bug surfaced gently.
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

    /// What a node is to the paragraph filler, if it is prose at all. A comment
    /// is a block comment when its text opens with one of the language's block
    /// openers and a line comment otherwise (Go, JS and Python have one
    /// `comment` kind for both); a Python `string` is prose when it is a
    /// triple-quoted docstring; a Markdown `paragraph` always.
    fn prose_kind(&self, n: Node<'_>) -> Option<ProseKind> {
        match n.kind() {
            "line_comment" | "block_comment" | "comment" => {
                let t = self.text_of(n);
                // A shebang is the loader's, not prose.
                if n.start_byte() == 0 && t.starts_with("#!") {
                    return None;
                }
                let rule = self.lang.sexp_rule();
                if rule.block_comments.iter().any(|(o, _)| t.starts_with(o)) {
                    Some(ProseKind::BlockComment)
                } else {
                    Some(ProseKind::LineComments)
                }
            }
            "string" if self.lang == Lang::Python => {
                let open = n.named_child(0)?;
                let quotes = self.text_of(open);
                // A docstring may be raw or unicode-prefixed; an f-string or
                // bytes literal is a value with code inside.
                let plain = quotes
                    .trim_end_matches(['"', '\''])
                    .chars()
                    .all(|c| matches!(c, 'r' | 'R' | 'u' | 'U'));
                (open.kind() == "string_start"
                    && plain
                    && (quotes.ends_with("\"\"\"") || quotes.ends_with("'''"))
                    && self.is_docstring(n))
                .then_some(ProseKind::TripleString)
            }
            // A setext heading's title is a paragraph node; wrapping it would
            // leave a paragraph and a shorter heading.
            "paragraph" if self.lang == Lang::Markdown => n
                .parent()
                .is_none_or(|p| p.kind() != "setext_heading")
                .then_some(ProseKind::Paragraph),
            _ => None,
        }
    }

    /// Whether line comments `a` then `b` sit on consecutive lines: exactly one
    /// newline and otherwise whitespace between the end of `a`'s text and the
    /// start of `b` (a Rust `line_comment` owns its newline; a Python `comment`
    /// does not).
    fn consecutive(&self, a: Node<'_>, b: Node<'_>) -> bool {
        let gap = &self.text[self.text_end(a)..b.start_byte()];
        gap.trim().is_empty() && gap.matches('\n').count() == 1
    }

    /// Byte offset of the start of the line holding byte `b`.
    fn bol_byte(&self, b: usize) -> usize {
        self.text[..b].rfind('\n').map_or(0, |i| i + 1)
    }

    /// Byte offset just past the newline ending the line holding byte `b`, or
    /// the end of the text.
    fn eol_byte(&self, b: usize) -> usize {
        self.text[b..]
            .find('\n')
            .map_or(self.text.len(), |i| b + i + 1)
    }

    /// The end of `n`'s text: its end byte, less the newline some grammars put
    /// inside a comment node (a Rust doc comment) and others leave out (a
    /// Python `comment`).
    fn text_end(&self, n: Node<'_>) -> usize {
        let end = n.end_byte();
        if self.text[..end].ends_with('\n') {
            end - 1
        } else {
            end
        }
    }

    /// Whether a Python string sits in docstring position: the whole first
    /// statement of a module, class or function body (comments before it do not
    /// count). Stricter than PEP 257 in two ways, since neither is reflowed: a
    /// parenthesised docstring, and one made of concatenated literals. Looser
    /// in one: a prefixed literal (an f-string, bytes) there counts, so the
    /// refusal can name the prefix as the reason.
    fn is_docstring(&self, n: Node<'_>) -> bool {
        let Some(stmt) = n.parent() else {
            return false;
        };
        if stmt.kind() != "expression_statement" || stmt.named_child_count() != 1 {
            return false;
        }
        let Some(body) = stmt.parent() else {
            return false;
        };
        let mut cursor = body.walk();
        let first = body
            .named_children(&mut cursor)
            .find(|c| c.kind() != "comment")
            .map(|c| c.id());
        first == Some(stmt.id())
            && (body.kind() == "module"
                || (body.kind() == "block"
                    && body.parent().is_some_and(|p| {
                        matches!(p.kind(), "function_definition" | "class_definition")
                    })))
    }

    /// The Python triple-quoted string `n` is, or is inside — or the
    /// concatenation or parentheses holding one, when point is between members,
    /// on a single-quoted member, or on a bracket. A position inside an
    /// f-string's `{…}` is code, however many strings nest there, so it is not
    /// in any string.
    fn python_string_around<'t>(&self, n: Node<'t>) -> Option<Node<'t>> {
        if self.lang != Lang::Python {
            return None;
        }
        let triple = |x: Node<'_>| {
            let quotes = x.named_child(0).map(|c| self.text_of(c)).unwrap_or("");
            quotes.ends_with("\"\"\"") || quotes.ends_with("'''")
        };
        let mut found = None;
        let mut up = Some(n);
        while let Some(x) = up {
            if x.kind() == "interpolation" {
                return None;
            }
            if found.is_none() {
                if (x.kind() == "string" && triple(x)) || x.kind() == "concatenated_string" {
                    found = Some(x);
                } else if x.kind() == "parenthesized_expression" {
                    // Only parentheses around a string are a string's: the
                    // string may sit behind comments and further parentheses.
                    let mut inner = x;
                    while inner.kind() == "parenthesized_expression" {
                        let mut cursor = inner.walk();
                        inner = inner
                            .named_children(&mut cursor)
                            .find(|c| c.kind() != "comment")?;
                    }
                    let holds = inner.kind() == "concatenated_string"
                        || (inner.kind() == "string" && triple(inner));
                    if !holds {
                        return None;
                    }
                    found = Some(x);
                }
            }
            up = x.parent();
        }
        found
    }

    /// The expression a Python string is part of once the concatenation and
    /// parentheses around it are climbed, if any.
    fn string_holder<'t>(&self, string: Node<'t>) -> Node<'t> {
        let mut holder = string;
        while let Some(p) = holder.parent()
            && matches!(p.kind(), "concatenated_string" | "parenthesized_expression")
        {
            holder = p;
        }
        holder
    }

    /// Why a Python string expression that is not prose is not: out of
    /// docstring position it is data; in position it is a prefixed literal (an
    /// f-string or bytes), a concatenation, or parenthesised.
    fn string_refusal(&self, holder: Node<'_>) -> &'static str {
        if self.is_docstring(holder) {
            "is in docstring position but not a plain docstring (an f-string, bytes, \
             concatenated literals, or parentheses around it), so it is not reflowed"
        } else {
            "is not in docstring position, so it is data, not prose"
        }
    }

    /// Whether only whitespace follows `n` on its line.
    fn ends_its_line(&self, n: Node<'_>) -> bool {
        let end = self.text_end(n);
        self.text[end..self.eol_byte(end)].trim().is_empty()
    }

    /// Whether a prose node owns its lines: nothing but whitespace beside it on
    /// its first and last line, so the whole-line unit holds no code. A
    /// Markdown paragraph always does; its list marker or `>` is part of the
    /// frame.
    fn owns_its_lines(&self, n: Node<'_>, kind: ProseKind) -> bool {
        kind == ProseKind::Paragraph || (self.starts_its_line(n) && self.ends_its_line(n))
    }

    /// The indent and marker run (`//`, `///`, `//!`, `;;`) leading `n`'s line:
    /// what two line comments must share to be one run.
    fn line_lead(&self, n: Node<'_>) -> (&str, &str) {
        let indent = &self.text[self.bol_byte(n.start_byte())..n.start_byte()];
        let openers = self.lang.sexp_rule().line_comments;
        let text = self.text_of(n);
        let marker = text
            .chars()
            .take_while(|c| *c == '!' || openers.iter().any(|o| o.contains(*c)))
            .map(char::len_utf8)
            .sum();
        (indent, &text[..marker])
    }

    /// The unit `n` (a prose node) belongs to, as whole lines: a line comment's
    /// run of consecutive comment siblings with the same indent and marker, any
    /// other prose node on its own.
    fn prose_unit_of(&self, n: Node<'_>, kind: ProseKind) -> ProseUnit {
        let (bol, eol) = self.prose_unit_bytes(n, kind);
        ProseUnit {
            kind,
            start: self.char_of(bol),
            end: self.char_of(eol),
        }
    }

    /// [`Self::prose_unit_of`] as a byte span.
    fn prose_unit_bytes(&self, n: Node<'_>, kind: ProseKind) -> (usize, usize) {
        let (mut first, mut last) = (n, n);
        if kind == ProseKind::LineComments {
            // `n` owns its line, so a sibling with the same lead does too.
            let lead = self.line_lead(n);
            let is_line = |m: Node<'_>| {
                self.prose_kind(m) == Some(ProseKind::LineComments) && self.line_lead(m) == lead
            };
            while let Some(p) = first.prev_named_sibling() {
                if !is_line(p) || !self.consecutive(p, first) {
                    break;
                }
                first = p;
            }
            while let Some(q) = last.next_named_sibling() {
                if !is_line(q) || !self.consecutive(last, q) {
                    break;
                }
                last = q;
            }
        }
        let bol = self.bol_byte(first.start_byte());
        // The line holding the unit's last text char, through its newline.
        let eol = self.eol_byte(self.text_end(last).max(first.start_byte()));
        (bol, eol)
    }

    /// The nearest prose node at or above byte `b`, with its kind.
    fn prose_node_at_byte(&self, b: usize) -> Option<(Node<'_>, ProseKind)> {
        let mut node = self.root().named_descendant_for_byte_range(b, b)?;
        loop {
            if let Some(k) = self.prose_kind(node) {
                return Some((node, k));
            }
            node = node.parent()?;
        }
    }

    /// The prose unit — a run of line comments, a block comment, a Python
    /// docstring, a Markdown paragraph — holding char position `pos`, as a
    /// whole-line span. A position at the end of a line counts as on its last
    /// char, and the end of the text as on the last char before it. `Err` names
    /// what the position is in instead, or the comment or string that shares
    /// its line with code.
    pub fn prose_unit_at(&self, pos: usize) -> Result<ProseUnit, String> {
        let b = self.byte_of(pos);
        // The end of a line counts as its last char; the end of the text as the
        // last char before it. That is the byte the refusal names too.
        let at_eol = self.text[b..].starts_with('\n') || b == self.text.len();
        let probe = if at_eol {
            let from = if b == self.text.len() {
                0
            } else {
                self.bol_byte(b)
            };
            self.text[from..b]
                .trim_end()
                .char_indices()
                .last()
                .map_or(b, |(i, _)| from + i)
        } else {
            b
        };
        let found = self.prose_node_at_byte(b).or_else(|| {
            (probe != b)
                .then(|| self.prose_node_at_byte(probe))
                .flatten()
        });
        let Some((node, kind)) = found else {
            let what = match self.root().named_descendant_for_byte_range(probe, probe) {
                Some(n) if let Some(string) = self.python_string_around(n) => {
                    let holder = self.string_holder(string);
                    return Err(format!(
                        "the string at @{} {}",
                        self.char_of(holder.start_byte()),
                        self.string_refusal(holder)
                    ));
                }
                Some(n) => {
                    // The nearest node that says something: past the inline and
                    // paragraph nodes of a Markdown heading.
                    let mut up = n;
                    while matches!(up.kind(), "inline" | "paragraph")
                        && let Some(p) = up.parent()
                    {
                        up = p;
                    }
                    if up.id() != n.id() {
                        up.kind().to_string()
                    } else {
                        match n.parent().filter(|p| p.parent().is_some()) {
                            Some(p) => format!("{} (inside {})", n.kind(), p.kind()),
                            None => n.kind().to_string(),
                        }
                    }
                }
                None => "an empty buffer".to_string(),
            };
            return Err(format!(
                "@{pos} is in {what}, not in a comment, a docstring or a paragraph"
            ));
        };
        if !self.owns_its_lines(node, kind) {
            return Err(format!(
                "the {} at @{} shares its line with code",
                kind.name(),
                self.char_of(node.start_byte())
            ));
        }
        Ok(self.prose_unit_of(node, kind))
    }

    /// Every prose unit whose lines overlap the char range `[a, b)`, in buffer
    /// order, each once (a run of line comments is one unit however many of its
    /// lines the range touches). A comment or string that shares a line with
    /// code is skipped.
    pub fn prose_units_in(&self, a: usize, b: usize) -> Vec<ProseUnit> {
        let (a, b) = (self.byte_of(a), self.byte_of(b));
        // Units are whole lines, so the range is too: the indentation before a
        // comment selects it.
        let a = self.bol_byte(a);
        let b = if b > a && !self.text[..b].ends_with('\n') {
            self.eol_byte(b)
        } else {
            b
        };
        let mut units = Vec::new();
        // Byte end of the last unit found: the walk runs in document order, so
        // a node before it is a later line of that unit.
        let mut covered = 0;
        let mut stack = vec![self.root()];
        while let Some(n) = stack.pop() {
            if n.end_byte() <= a || n.start_byte() >= b {
                continue;
            }
            match self.prose_kind(n) {
                Some(kind) => {
                    if n.start_byte() < covered || !self.owns_its_lines(n, kind) {
                        continue;
                    }
                    let (bol, eol) = self.prose_unit_bytes(n, kind);
                    units.push(ProseUnit {
                        kind,
                        start: self.char_of(bol),
                        end: self.char_of(eol),
                    });
                    covered = eol;
                }
                None => {
                    // Push named children in reverse so the stack pops them in
                    // document order.
                    for i in (0..n.named_child_count() as u32).rev() {
                        if let Some(child) = n.named_child(i) {
                            stack.push(child);
                        }
                    }
                }
            }
        }
        units
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
    /// `None` where the tree ends. `named` skips anonymous tokens (punctuation,
    /// keywords), which is almost always what an agent wants.
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
    /// structural search: "every `function_item`", "calls to `foo`", … .  `Err`
    /// is the query compile error (pattern syntax / unknown node kind).
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
        // Char position inside "Title" — the smallest named node is the
        // heading's inline content.
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
        // Point in the H1 paragraph → the outer H1 section, which spans the
        // whole document (the H2 section nests inside it).
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

        // Unnamed children are visible when asked for: fn main's body block has
        // `{` as child 0 in the unnamed view.
        let main = syn.find_defun("main").unwrap();
        let main_h = syn.defun_at(main.start).expect("main handle");
        let body = syn.child_by_field_of(main_h, "body").expect("body field");
        let brace = syn.child_of(body, 0, false).expect("the { token");
        assert_eq!(syn.describe(brace).unwrap().kind, "{");
    }

    #[test]
    fn zero_width_nodes_locate_and_navigate() {
        // Incomplete code produces real ZERO-WIDTH nodes (a missing `block` in
        // `def f():`); descendant_for_byte_range skips them, so locate's
        // containment descent must find them — a panic here took down the whole
        // process when a query captured one.
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
        // Em dash (3 bytes) before the heading word shifts byte vs. char
        // offsets.
        let doc = "# Tëa — pot\n\nbody\n";
        let syn = Syntax::parse(doc, Lang::Markdown);
        let p = doc.chars().position(|c| c == 'b').unwrap() + 1; // char index of "body"
        let span = syn
            .node_at(p)
            .and_then(|h| syn.describe(h))
            .expect("a node at point");
        assert_eq!(span.kind, "inline");
        // The span must be addressable as chars: substring by char span
        // recovers the original word.
        let chars: Vec<char> = doc.chars().collect();
        let got: String = chars[span.start - 1..span.end - 1].iter().collect();
        assert_eq!(got, "body");
    }

    #[test]
    fn rust_defun_extent_includes_preceding_attributes() {
        let src = "#[cfg(test)]\n#[test]\nfn check() {\n    assert!(true);\n}\n";
        let syn = Syntax::parse(src, Lang::Rust);
        // The outline span starts at the first attribute, so "delete this test"
        // is the defun span with no manual hop to the #[…] lines.
        let d = syn.find_defun("check").expect("check");
        assert_eq!(d.start, 1, "span starts at #[cfg(test)]");
        // A position ON an attribute resolves to the decorated defun: narrowing
        // / defun-at from the attribute line works.
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
    fn rust_defun_extent_includes_doc_comments_but_not_plain_ones() {
        // The doc comment above the attribute chain belongs to the item; the
        // previous item's span is untouched.
        let src = "fn first() {}\n\n/// doc\n#[test]\nfn second() {}\n";
        let syn = Syntax::parse(src, Lang::Rust);
        let at = |s: &str| src.find(s).unwrap() + 1;
        let first = syn.find_defun("first").expect("first");
        assert_eq!((first.start, first.end), (1, 14));
        let second = syn.find_defun("second").expect("second");
        assert_eq!(second.start, at("/// doc"), "starts at the doc comment");
        // From the doc comment, the enclosing defun is the documented item.
        let span = syn
            .enclosing_defun(at("doc"))
            .expect("from the doc comment");
        assert_eq!(
            (span.kind.as_str(), span.start),
            ("function_item", at("/// doc"))
        );
        assert_eq!(
            syn.enclosing_defun_name(at("doc")).as_deref(),
            Some("second")
        );

        // A block doc comment counts; a plain `//` comment, a `////` rule and
        // an inner `//!` doc do not.
        let src = "/** doc */\nfn a() {}\n\n// note\nfn b() {}\n\n//// rule\nfn c() {}\n\n//! inner\nfn d() {}\n";
        let syn = Syntax::parse(src, Lang::Rust);
        let at = |s: &str| src.find(s).unwrap() + 1;
        assert_eq!(syn.find_defun("a").unwrap().start, 1);
        assert_eq!(syn.find_defun("b").unwrap().start, at("fn b"));
        assert_eq!(syn.find_defun("c").unwrap().start, at("fn c"));
        assert_eq!(syn.find_defun("d").unwrap().start, at("fn d"));
        // Methods carry their doc comments the same way.
        let src = "impl T {\n    /// m doc\n    fn m(&self) {}\n}\n";
        let syn = Syntax::parse(src, Lang::Rust);
        let at = |s: &str| src.find(s).unwrap() + 1;
        assert_eq!(syn.find_defun("m").unwrap().start, at("/// m doc"));
    }

    #[test]
    fn go_and_ts_defun_extent_includes_the_adjacent_comment_block() {
        let src =
            "package p\n\n// A does a.\n// Second line.\nfunc A() {}\n\n// stray\n\nfunc B() {}\n";
        let syn = Syntax::parse(src, Lang::Go);
        let at = |s: &str| src.find(s).unwrap() + 1;
        assert_eq!(syn.find_defun("A").unwrap().start, at("// A does"));
        assert_eq!(
            syn.find_defun("B").unwrap().start,
            at("func B"),
            "a blank line breaks the attachment"
        );
        assert_eq!(syn.enclosing_defun_name(at("Second")).as_deref(), Some("A"));
        // A comment trailing the previous line's code belongs to that line, not
        // to the function below it.
        let src = "package p\n\nconst Max = 3 // tuned\nfunc Retry() {}\n";
        let syn = Syntax::parse(src, Lang::Go);
        let at = |s: &str| src.find(s).unwrap() + 1;
        assert_eq!(syn.find_defun("Retry").unwrap().start, at("func Retry"));
        assert_eq!(syn.enclosing_defun_name(at("tuned")), None);

        // JSDoc above an exported function: the `export` is part of the item
        // and the comment attaches through it.
        let src =
            "/**\n * JSDoc for f.\n */\nexport function f() {}\n\n// stray\n\nfunction g() {}\n";
        let syn = Syntax::parse(src, Lang::Typescript);
        let at = |s: &str| src.find(s).unwrap() + 1;
        let f = syn.find_defun("f").unwrap();
        assert_eq!(f.start, 1);
        assert_eq!(f.end, at("\n\n// stray"));
        assert_eq!(syn.find_defun("g").unwrap().start, at("function g"));
        assert_eq!(syn.enclosing_defun_name(at("JSDoc")).as_deref(), Some("f"));
        assert_eq!(syn.enclosing_defun_name(at("export")).as_deref(), Some("f"));
        let src = "const x = 1; // tuned\nexport function f() {}\n";
        let syn = Syntax::parse(src, Lang::Typescript);
        let at = |s: &str| src.find(s).unwrap() + 1;
        assert_eq!(syn.find_defun("f").unwrap().start, at("export"));
    }

    #[test]
    fn html_outline_names_elements_by_tag() {
        let src = "<html><body><div id=\"m\"><p>hi</p></div><script>1</script></body></html>";
        let syn = Syntax::parse(src, Lang::Html);
        let names: Vec<String> = syn
            .defuns()
            .iter()
            .map(|d| format!("{} {}", d.kind, d.name))
            .collect();
        assert!(names.contains(&"element html".to_string()), "{names:?}");
        assert!(names.contains(&"element div".to_string()), "{names:?}");
        assert!(
            names.contains(&"script_element script".to_string()),
            "{names:?}"
        );
        // Mid-<p>, the innermost element wins.
        let p = src.find("hi").unwrap() + 1;
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("p"));
    }

    #[test]
    fn javascript_outline_and_enclosing() {
        let src = "function foo(a) { return a; }\nclass C { bar() {} }\n";
        let syn = Syntax::parse(src, Lang::Javascript);
        assert!(syn.find_defun("foo").is_some());
        assert!(syn.find_defun("C").is_some());
        assert!(syn.find_defun("bar").is_some());
        let inner = src.find("{}").unwrap() + 1;
        assert_eq!(syn.enclosing_defun_name(inner + 1).as_deref(), Some("bar"));
    }

    #[test]
    fn css_outline_names_rules_by_selector() {
        let src = ".a, p { color: red; }\n@media print { body { margin: 0; } }\n@keyframes spin { from { left: 0; } }\n";
        let syn = Syntax::parse(src, Lang::Css);
        let names: Vec<String> = syn.defuns().into_iter().map(|d| d.name).collect();
        assert!(names.iter().any(|n| n == ".a, p"), "{names:?}");
        assert!(names.iter().any(|n| n == "print"), "{names:?}");
        assert!(names.iter().any(|n| n == "spin"), "{names:?}");
        assert!(names.iter().any(|n| n == "body"), "nested rule: {names:?}");
    }

    #[test]
    fn toml_outline_names_tables() {
        let src = "top = 1\n[server]\nport = 80\n[[bin]]\nname = \"x\"\n[a.b]\nc = 2\n";
        let syn = Syntax::parse(src, Lang::Toml);
        let names: Vec<String> = syn.defuns().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["server", "bin", "a.b"]);
        let p = src.find("port").unwrap() + 1;
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("server"));
    }

    #[test]
    fn yaml_outline_is_the_key_tree() {
        let src = "top: 1\nserver:\n  port: 80\n  hosts:\n    - a\n";
        let syn = Syntax::parse(src, Lang::Yaml);
        let names: Vec<String> = syn.defuns().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["top", "server", "port", "hosts"]);
        let p = src.find("80").unwrap() + 1;
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("port"));
    }

    #[test]
    fn typescript_outline_names_declarations() {
        let src = "interface Point { x: number }\n\
                   type Alias = Point;\n\
                   class Box {\n  get(): Point { return this.p; }\n}\n\
                   function make(): Box { return new Box(); }\n\
                   const arrow = () => 1;\n";
        let syn = Syntax::parse(src, Lang::Typescript);
        let names: Vec<(String, String)> =
            syn.defuns().into_iter().map(|d| (d.kind, d.name)).collect();
        assert!(
            names.contains(&("interface_declaration".into(), "Point".into())),
            "{names:?}"
        );
        assert!(
            names.contains(&("type_alias_declaration".into(), "Alias".into())),
            "{names:?}"
        );
        assert!(
            names.contains(&("class_declaration".into(), "Box".into())),
            "{names:?}"
        );
        assert!(
            names.contains(&("method_definition".into(), "get".into())),
            "{names:?}"
        );
        assert!(
            names.contains(&("function_declaration".into(), "make".into())),
            "{names:?}"
        );
        let p = src.find("return this").unwrap() + 1;
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("get"));
        // .ts and .tsx resolve to their own grammar variants.
        assert_eq!(Lang::from_buffer_name("a.ts"), Some(Lang::Typescript));
        assert_eq!(Lang::from_buffer_name("a.tsx"), Some(Lang::Tsx));
    }

    #[test]
    fn typescript_and_tsx_grammars_accept_their_own_dialects() {
        // An angle-bracket type assertion is valid TS but invalid TSX (it
        // parses as JSX there) — .ts must not use the TSX grammar.
        let ts = "const x = <Foo>bar;\n";
        assert!(!Syntax::parse(ts, Lang::Typescript).has_error());
        assert!(Syntax::parse(ts, Lang::Tsx).has_error());
        // JSX is valid TSX but invalid plain TS.
        let tsx = "const el = <div>hi</div>;\n";
        assert!(!Syntax::parse(tsx, Lang::Tsx).has_error());
        assert!(Syntax::parse(tsx, Lang::Typescript).has_error());
    }

    #[test]
    fn go_outline_names_funcs_methods_and_types() {
        let src = "package main\n\n\
                   type Point struct { X int }\n\n\
                   func (p Point) Norm() int { return p.X }\n\n\
                   func main() { _ = Point{1} }\n";
        let syn = Syntax::parse(src, Lang::Go);
        let names: Vec<(String, String)> =
            syn.defuns().into_iter().map(|d| (d.kind, d.name)).collect();
        assert!(
            names.contains(&("type_declaration".into(), "Point".into())),
            "{names:?}"
        );
        assert!(
            names.contains(&("method_declaration".into(), "Norm".into())),
            "{names:?}"
        );
        assert!(
            names.contains(&("function_declaration".into(), "main".into())),
            "{names:?}"
        );
        let p = src.find("return p.X").unwrap() + 1;
        assert_eq!(syn.enclosing_defun_name(p).as_deref(), Some("Norm"));
        assert_eq!(Lang::from_buffer_name("m.go"), Some(Lang::Go));
    }

    #[test]
    fn elisp_outline_names_defuns_and_macros() {
        let src = "(defun foo (x) (+ x 1))\n(defmacro baz () nil)\n";
        let syn = Syntax::parse(src, Lang::Elisp);
        let names: Vec<(String, String)> =
            syn.defuns().into_iter().map(|d| (d.kind, d.name)).collect();
        assert_eq!(
            names,
            vec![
                ("function_definition".to_string(), "foo".to_string()),
                ("macro_definition".to_string(), "baz".to_string()),
            ]
        );
    }

    #[test]
    fn new_language_extension_detection() {
        for (file, lang) in [
            ("a.html", Lang::Html),
            ("a.htm", Lang::Html),
            ("a.js", Lang::Javascript),
            ("a.mjs", Lang::Javascript),
            ("a.css", Lang::Css),
            ("a.toml", Lang::Toml),
            ("a.yaml", Lang::Yaml),
            ("a.yml", Lang::Yaml),
            ("a.el", Lang::Elisp),
            ("a.tl", Lang::Elisp),
        ] {
            assert_eq!(Lang::from_buffer_name(file), Some(lang), "{file}");
        }
    }

    #[test]
    fn checkpointed_conversions_match_the_naive_scan() {
        // Multibyte text long enough to span several 4 KiB checkpoints, so both
        // the residue walks and the checkpoint hops are exercised.
        let mut text = String::new();
        for i in 0..600 {
            text.push_str(&format!("line {i:04} — naïve café ‸körner\n"));
        }
        let syn = Syntax::parse(&text, Lang::Markdown);
        assert!(
            syn.checkpoints.len() > 3,
            "spans checkpoints: {}",
            syn.checkpoints.len()
        );
        let n_chars = text.chars().count();
        let naive_byte_of = |pos: usize| -> usize {
            let pos = pos.max(1);
            text.char_indices()
                .nth(pos - 1)
                .map_or(text.len(), |(b, _)| b)
        };
        let naive_char_of = |byte: usize| -> usize {
            let mut byte = byte.min(text.len());
            while byte > 0 && !text.is_char_boundary(byte) {
                byte -= 1;
            }
            text[..byte].chars().count() + 1
        };
        // Probe boundaries, checkpoint edges, mid-char bytes, and a spread.
        let mut bytes: Vec<usize> = (0..=text.len()).step_by(997).collect();
        bytes.extend([0, 1, text.len() - 1, text.len(), text.len() + 50]);
        bytes.extend(
            syn.checkpoints
                .iter()
                .flat_map(|&(b, _)| [b, b + 1, b.saturating_sub(1)]),
        );
        for b in bytes {
            assert_eq!(syn.char_of(b), naive_char_of(b), "char_of({b})");
        }
        let mut poss: Vec<usize> = (1..=n_chars + 1).step_by(811).collect();
        poss.extend([1, 2, n_chars, n_chars + 1, n_chars + 9]);
        for p in poss {
            assert_eq!(syn.byte_of(p), naive_byte_of(p), "byte_of({p})");
        }
    }

    #[test]
    fn symbol_constituents_are_per_language() {
        assert!(Lang::Rust.is_symbol_char('_'));
        assert!(!Lang::Rust.is_symbol_char('-'));
        assert!(Lang::Elisp.is_symbol_char('-'));
        assert!(Lang::Elisp.is_symbol_char(':'));
        assert!(!Lang::Elisp.is_symbol_char('\''));
        assert!(Lang::Css.is_symbol_char('-'));
        assert!(!Lang::Markdown.is_symbol_char('-'));
    }

    #[test]
    fn sexp_rules_name_each_languages_quotes_comments_and_prefixes() {
        let rs = Lang::Rust.sexp_rule();
        assert_eq!(rs.quotes, &['"']);
        assert!(!rs.triple_quotes);
        assert_eq!(rs.line_comments, &["//"]);
        assert_eq!(rs.block_comments, &[("/*", "*/")]);
        assert!(rs.prefixes.is_empty());
        assert!(rs.raw_quotes.is_empty());
        let py = Lang::Python.sexp_rule();
        assert_eq!(py.quotes, &['"', '\'']);
        assert!(py.triple_quotes);
        assert_eq!(py.line_comments, &["#"]);
        assert!(py.block_comments.is_empty());
        let js = Lang::Javascript.sexp_rule();
        assert_eq!(js.quotes, &['"', '\'', '`']);
        assert_eq!(Lang::Typescript.sexp_rule(), js);
        assert_eq!(Lang::Tsx.sexp_rule(), js);
        assert_eq!(Lang::Go.sexp_rule().quotes, &['"', '`']);
        assert_eq!(Lang::Go.sexp_rule().raw_quotes, &['`']);
        let el = Lang::Elisp.sexp_rule();
        assert_eq!(el.line_comments, &[";"]);
        assert_eq!(el.prefixes, &['\'', '`', ',', '@', '#']);
        assert_eq!(Lang::Toml.sexp_rule(), Lang::Yaml.sexp_rule());
        assert_eq!(Lang::Html.sexp_rule().block_comments, &[("<!--", "-->")]);
        assert_eq!(Lang::Markdown.sexp_rule().quotes, &['"']);
        assert!(Lang::Css.sexp_rule().line_comments.is_empty());
    }
    // ---- prose units: what fill-paragraph / fill-region may reflow ----

    fn unit_at(text: &str, lang: Lang, pos: usize) -> Result<ProseUnit, String> {
        Syntax::parse(text, lang).prose_unit_at(pos)
    }

    fn span(text: &str, lang: Lang, pos: usize) -> (ProseKind, usize, usize) {
        let u = unit_at(text, lang, pos).unwrap();
        (u.kind, u.start, u.end)
    }

    #[test]
    fn prose_unit_is_the_run_of_adjacent_line_comments_as_whole_lines() {
        let text = "/// a\n/// b\nfn f() {}\n";
        assert_eq!(span(text, Lang::Rust, 3), (ProseKind::LineComments, 1, 13));
        assert_eq!(span(text, Lang::Rust, 9), (ProseKind::LineComments, 1, 13));
    }

    #[test]
    fn a_blank_line_splits_comment_runs() {
        let text = "// a\n\n// b\n";
        assert_eq!(span(text, Lang::Rust, 1), (ProseKind::LineComments, 1, 6));
        assert_eq!(span(text, Lang::Rust, 8), (ProseKind::LineComments, 7, 12));
    }

    #[test]
    fn comment_kind_is_classified_by_text_where_the_grammar_has_one_kind() {
        let text = "package p\n// a\n// b\nfunc f() {}\n";
        assert_eq!(span(text, Lang::Go, 12), (ProseKind::LineComments, 11, 21));
        let text = "/* a\n * b\n */\nfunc f() {}\n";
        assert_eq!(span(text, Lang::Go, 2), (ProseKind::BlockComment, 1, 15));
    }

    #[test]
    fn a_rust_block_comment_is_one_unit() {
        let text = "fn f() {\n    /* a\n     * b */\n}\n";
        assert_eq!(
            span(text, Lang::Rust, 16),
            (ProseKind::BlockComment, 10, 31)
        );
    }

    #[test]
    fn a_python_function_docstring_is_prose() {
        let text = "def f():\n    \"\"\"Doc\n    more\"\"\"\n    x = 'no'\n";
        assert_eq!(
            span(text, Lang::Python, 18),
            (ProseKind::TripleString, 10, 33)
        );
        let err = unit_at(text, Lang::Python, 39).unwrap_err();
        assert!(err.contains("string"), "{err}");
    }

    #[test]
    fn only_a_string_in_docstring_position_is_prose() {
        // Module and class docstrings, comments before them allowed.
        let text =
            "# c\n\"\"\"Doc\nmore\"\"\"\n\nclass C:\n    \"\"\"Cls\n    doc\"\"\"\n    x = 1\n";
        assert_eq!(
            span(text, Lang::Python, 6),
            (ProseKind::TripleString, 5, 20)
        );
        assert_eq!(
            span(text, Lang::Python, 35),
            (ProseKind::TripleString, 30, 52)
        );
        // A string after an assignment is not a docstring (PEP 257), so a block
        // commented out with quotes is never reflowed.
        let text = "X = 1\n\"\"\"Docs\nfor X\"\"\"\n";
        let err = unit_at(text, Lang::Python, 8).unwrap_err();
        assert!(err.contains("@7 is not in docstring position"), "{err}");
        // A triple-quoted string anywhere else is data, not prose.
        let text = "rows = run(\n    \"\"\"\n    select a\n    \"\"\"\n)\n";
        let err = unit_at(text, Lang::Python, 22).unwrap_err();
        assert!(err.contains("not in docstring position"), "{err}");
        let text = "def f():\n    x = 1\n    \"\"\"not\n    doc\"\"\"\n";
        assert!(unit_at(text, Lang::Python, 26).is_err());
        let text = "def f():\n    print(1)\n    \"\"\"not\n    doc\"\"\"\n";
        assert!(unit_at(text, Lang::Python, 30).is_err());
        // A tuple continued onto the string's line is not a lone string.
        let text = "1, \\\n\"\"\"Doc\nmore\"\"\"\n";
        assert!(unit_at(text, Lang::Python, 8).is_err());
        // An f-string or bytes literal is a value; a raw or unicode docstring
        // is prose.
        let text = "def f(x):\n    f\"\"\"Totals {x}\n    done\"\"\"\n";
        let err = unit_at(text, Lang::Python, 20).unwrap_err();
        assert!(
            err.contains("@15 is in docstring position but not a plain docstring"),
            "{err}"
        );
        // Inside the f-string's braces the position is in code, even in a
        // string nested there.
        let err = unit_at(text, Lang::Python, 27).unwrap_err();
        assert!(err.contains("identifier"), "{err}");
        let text = "def f():\n    f\"\"\"{ '''x''' }\"\"\"\n";
        let err = unit_at(text, Lang::Python, 22).unwrap_err();
        assert!(!err.contains("docstring position"), "{err}");
        let text = "def f():\n    b\"\"\"raw\n    bytes\"\"\"\n";
        assert!(unit_at(text, Lang::Python, 20).is_err());
        // Concatenated or parenthesised literals are a docstring to Python, but
        // not reflowed; the message names the whole expression from any member,
        // the space between members, or a plain member.
        let text = "def f():\n    \"\"\"one\"\"\" \"\"\"two\n    lines\"\"\"\n";
        for pos in [26, 23] {
            let err = unit_at(text, Lang::Python, pos).unwrap_err();
            assert!(
                err.contains("@14 is in docstring position but not a plain"),
                "{err}"
            );
        }
        let text = "def f():\n    \"\"\"one\"\"\" 'two'\n";
        let err = unit_at(text, Lang::Python, 25).unwrap_err();
        assert!(
            err.contains("@14 is in docstring position but not a plain"),
            "{err}"
        );
        let text = "def f():\n    (\"\"\"one\n    two\"\"\")\n";
        // On the text, on either bracket, at the end of the line and of the
        // text; with a comment or more parentheses inside.
        for pos in [20, 14, 32, 33, 34] {
            let err = unit_at(text, Lang::Python, pos).unwrap_err();
            assert!(
                err.contains("@14 is in docstring position but not a plain"),
                "{err}"
            );
        }
        let text = "def f():\n    (  # c\n    (\"\"\"one\n    two\"\"\"))\n";
        // On the outer brackets and past the end of the closing line.
        for pos in [14, 41, 44, 45] {
            let err = unit_at(text, Lang::Python, pos).unwrap_err();
            assert!(
                err.contains("@14 is in docstring position but not a plain"),
                "{err}"
            );
        }
        let text = "def f():\n    (  # c\n    \"\"\"one\n    two\"\"\")\n";
        let err = unit_at(text, Lang::Python, 14).unwrap_err();
        assert!(
            err.contains("@14 is in docstring position but not a plain"),
            "{err}"
        );
        // Parentheses around code, or around a plain string, are code.
        for text in ["def f():\n    (1 + 2)\n", "def f():\n    ('a')\n"] {
            let err = unit_at(text, Lang::Python, 14).unwrap_err();
            assert!(!err.contains("docstring position"), "{err}");
        }
        // A blank line does not reach back to the comment above it.
        let err = unit_at("# c\n\nx = 1\n", Lang::Python, 5).unwrap_err();
        assert!(err.contains("is in module"), "{err}");
        // A concatenation out of position is data.
        let text = "x = \"\"\"one\"\"\" \"\"\"two\n\"\"\"\n";
        let err = unit_at(text, Lang::Python, 17).unwrap_err();
        assert!(err.contains("@5 is not in docstring position"), "{err}");
        // A body other than a module, class or function has no docstring.
        assert!(unit_at("if True:\n    \"\"\"not\n    doc\"\"\"\n", Lang::Python, 15).is_err());
        let text = "def f():\n    u\"\"\"Doc\n    more\"\"\"\n";
        assert_eq!(
            span(text, Lang::Python, 20),
            (ProseKind::TripleString, 10, 34)
        );
        let text = "def f():\n    r\"\"\"Doc \\d\n    more\"\"\"\n";
        assert_eq!(
            span(text, Lang::Python, 20),
            (ProseKind::TripleString, 10, 37)
        );
    }

    #[test]
    fn point_in_code_is_refused_naming_the_node() {
        let err = unit_at("fn f() {}\n", Lang::Rust, 4).unwrap_err();
        assert!(err.contains("function_item"), "{err}");
    }

    #[test]
    fn a_comment_after_code_on_its_line_is_refused() {
        let err = unit_at("x = 1  # c\n", Lang::Python, 9).unwrap_err();
        assert!(err.contains("shares its line"), "{err}");
    }

    #[test]
    fn point_on_the_newline_after_a_comment_still_finds_it() {
        assert_eq!(
            span("# c\nx = 1\n", Lang::Python, 4),
            (ProseKind::LineComments, 1, 5)
        );
    }

    #[test]
    fn a_markdown_paragraph_is_a_unit_of_whole_lines() {
        let text = "Para\nmore\n\n- item\n  two\n\n```\ncode\n```\n";
        assert_eq!(span(text, Lang::Markdown, 1), (ProseKind::Paragraph, 1, 11));
        assert_eq!(
            span(text, Lang::Markdown, 15),
            (ProseKind::Paragraph, 12, 25)
        );
        let err = unit_at(text, Lang::Markdown, 30).unwrap_err();
        assert!(err.contains("fenced_code_block"), "{err}");
    }

    #[test]
    fn a_run_needs_one_marker_and_indent() {
        // `///` docs and a `//` remark are two units; so are two indents.
        let text = "/// a\n// b\n";
        assert_eq!(span(text, Lang::Rust, 1), (ProseKind::LineComments, 1, 7));
        assert_eq!(span(text, Lang::Rust, 8), (ProseKind::LineComments, 7, 12));
        let text = "  // a\n   // b\n";
        assert_eq!(span(text, Lang::Rust, 3), (ProseKind::LineComments, 1, 8));
    }

    #[test]
    fn a_shebang_is_not_prose() {
        let text = "#!/usr/bin/env python\n# a\n";
        assert!(unit_at(text, Lang::Python, 1).is_err());
        assert_eq!(
            span(text, Lang::Python, 24),
            (ProseKind::LineComments, 23, 27)
        );
    }

    #[test]
    fn a_block_comment_or_string_sharing_its_line_with_code_is_refused() {
        let err = unit_at("fn f() { /* a */ let x = 1; }\n", Lang::Rust, 12).unwrap_err();
        assert!(err.contains("shares its line"), "{err}");
        let err = unit_at("/* a */ fn f() {}\n", Lang::Rust, 3).unwrap_err();
        assert!(err.contains("shares its line"), "{err}");
        let text = "\"\"\"Doc\nmore\"\"\"; x = 1\n";
        let err = unit_at(text, Lang::Python, 3).unwrap_err();
        assert!(err.contains("shares its line"), "{err}");
        assert!(
            Syntax::parse(text, Lang::Python)
                .prose_units_in(1, 20)
                .is_empty()
        );
    }

    #[test]
    fn a_position_at_the_end_of_a_line_or_the_text_finds_that_line() {
        let text = "// aa\n// bb\n";
        assert_eq!(span(text, Lang::Rust, 6), (ProseKind::LineComments, 1, 13));
        assert_eq!(span(text, Lang::Rust, 13), (ProseKind::LineComments, 1, 13));
    }

    #[test]
    fn a_setext_heading_title_is_not_a_paragraph() {
        let text = "A Fairly Long Title\n===================\n\nbody\n";
        let err = unit_at(text, Lang::Markdown, 3).unwrap_err();
        assert!(err.contains("in setext_heading"), "{err}");
        let err = unit_at("# Title\n\nbody\n", Lang::Markdown, 3).unwrap_err();
        assert!(err.contains("in atx_heading"), "{err}");
        assert_eq!(
            span(text, Lang::Markdown, 43),
            (ProseKind::Paragraph, 42, 47)
        );
    }

    #[test]
    fn a_range_ending_after_a_multibyte_char_does_not_panic() {
        let text = "// café\n// more\nfn f() {}\n";
        let syn = Syntax::parse(text, Lang::Rust);
        assert_eq!(syn.prose_units_in(1, 8).len(), 1);
        assert_eq!(syn.prose_units_in(1, 9).len(), 1);
        let syn = Syntax::parse("Hello world—", Lang::Markdown);
        assert_eq!(syn.prose_units_in(1, 13).len(), 1);
    }

    #[test]
    fn prose_units_in_a_range_come_in_order_and_skip_code() {
        let text = "// a\n// b\nfn f() {\n    // c\n}\n/* d */\n";
        let syn = Syntax::parse(text, Lang::Rust);
        let units: Vec<(ProseKind, usize, usize)> = syn
            .prose_units_in(1, text.chars().count() + 1)
            .into_iter()
            .map(|u| (u.kind, u.start, u.end))
            .collect();
        assert_eq!(
            units,
            vec![
                (ProseKind::LineComments, 1, 11),
                (ProseKind::LineComments, 20, 29),
                (ProseKind::BlockComment, 31, 39),
            ]
        );
        // Only units whose lines the range touches: a range on the indentation
        // before a comment still selects it.
        assert_eq!(syn.prose_units_in(20, 22).len(), 1);
        assert_eq!(syn.prose_units_in(12, 15).len(), 0);
        // A triple-quoted string outside docstring position is not a unit.
        let data = "rows = run(\n    \"\"\"\n    select a\n    \"\"\"\n)\n";
        assert!(
            Syntax::parse(data, Lang::Python)
                .prose_units_in(1, 40)
                .is_empty()
        );
    }
}
