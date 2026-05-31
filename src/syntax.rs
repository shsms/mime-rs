//! M7 structural / AST-aware editing, via tree-sitter.
//!
//! mime-rs edits prose — Markdown / org documents — so the scaffold grammar is
//! [`tree_sitter_md`] (Markdown). Its *block* tree (`MarkdownTree::block_tree`)
//! is a standard [`tree_sitter::Tree`] whose nodes are `document` → `section` →
//! `atx_heading` / `paragraph` / `list` …, so a `section` is the natural
//! top-level "defun" analog for prose navigation. (tree-sitter-md also produces
//! per-block *inline* trees; the scaffold uses only the block tree.)
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
//!   - Incremental re-parse: keep the `MarkdownTree` on the `Session` and feed
//!     `InputEdit`s on every buffer mutation instead of re-parsing from scratch.
//!   - More languages: detect by buffer name / extension (`.md`/`.markdown` →
//!     Markdown, `.rs` → Rust, …) and dispatch to the right grammar; generalize
//!     the "top-level construct" notion per language (section vs. function_item).
//!   - AST-edit ops over the current node: `replace-node`, `wrap-node`,
//!     `raise-node`, `kill-node`, plus a tree-query / capture builtin.
//!   - Surface a few of these as MCP tools once the builtin surface settles.

use tree_sitter::Node;
use tree_sitter_md::{MarkdownParser, MarkdownTree};

/// A freshly parsed view of a buffer: the Markdown block tree plus the source
/// text it was parsed from (needed to map node byte ranges back to chars).
pub struct Syntax {
    text: String,
    tree: MarkdownTree,
}

/// A node, projected into mime-rs terms: its kind and a 1-based char span
/// `[start, end)` (end is the position just past the last char, Emacs-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSpan {
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

impl Syntax {
    /// Parse `text` into a Markdown block tree. Owns a copy of the text so the
    /// returned value is self-contained (node byte ranges index into it).
    pub fn parse(text: &str) -> Syntax {
        let mut parser = MarkdownParser::default();
        // tree-sitter-md only fails to parse on timeout/cancellation, neither of
        // which the scaffold sets, so an empty document is a safe fallback.
        let tree = parser
            .parse(text.as_bytes(), None)
            .unwrap_or_else(|| parser.parse(b"", None).expect("empty parse"));
        Syntax {
            text: text.to_string(),
            tree,
        }
    }

    /// The root of the block tree (kind `document`).
    fn root(&self) -> Node<'_> {
        self.tree.block_tree().root_node()
    }

    /// Kind of the root node — `"document"` for any Markdown buffer. Proves the
    /// parse ran.
    pub fn root_kind(&self) -> String {
        self.root().kind().to_string()
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

    /// The smallest *named* node covering char position `pos`, as a [`NodeSpan`].
    /// `None` only for a degenerate (empty) tree. A zero-width query at `pos`
    /// finds the node containing the gap before the char at `pos`.
    pub fn named_node_at(&self, pos: usize) -> Option<NodeSpan> {
        let b = self.byte_of(pos);
        self.root()
            .named_descendant_for_byte_range(b, b)
            .map(|n| self.span_of(n))
    }

    /// The nearest enclosing `section` (Markdown's top-level construct: a heading
    /// and the text under it), as a char span. Walks up from the smallest node at
    /// `pos`. `None` if `pos` is outside any section (e.g. leading blank lines, or
    /// an empty buffer).
    pub fn enclosing_section(&self, pos: usize) -> Option<NodeSpan> {
        let b = self.byte_of(pos);
        let mut node = self.root().descendant_for_byte_range(b, b)?;
        loop {
            if node.kind() == "section" {
                return Some(self.span_of(node));
            }
            node = node.parent()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Title\n\nHello para.\n\n## Sub\n\nMore text here.\n";

    #[test]
    fn root_is_document() {
        let syn = Syntax::parse(DOC);
        assert_eq!(syn.root_kind(), "document");
    }

    #[test]
    fn empty_buffer_still_parses() {
        let syn = Syntax::parse("");
        assert_eq!(syn.root_kind(), "document");
        // No section to land in, but querying must not panic.
        assert!(syn.enclosing_section(1).is_none());
    }

    #[test]
    fn named_node_at_a_heading_word() {
        let syn = Syntax::parse(DOC);
        // Char position inside "Title" — the smallest named node is the heading's
        // inline content.
        let span = syn.named_node_at(4).expect("a node at point");
        assert_eq!(span.kind, "inline");
        // "# Title\n" — inline "Title" is chars 3..=7, i.e. span [3, 8).
        assert_eq!((span.start, span.end), (3, 8));
    }

    #[test]
    fn named_node_inside_paragraph() {
        let syn = Syntax::parse(DOC);
        // "Hello para." begins at char 10 (after "# Title\n\n").
        let p = DOC.find("Hello").unwrap() + 1; // 1-based char == byte here (ASCII)
        let span = syn.named_node_at(p).expect("a node at point");
        assert_eq!(span.kind, "inline");
    }

    #[test]
    fn enclosing_section_is_innermost() {
        let syn = Syntax::parse(DOC);
        // Point in the H2 body → the H2 section (nested inside the H1 section),
        // i.e. the most local heading scope.
        let p = DOC.find("More").unwrap() + 1;
        let sec = syn.enclosing_section(p).expect("a section");
        assert_eq!(sec.kind, "section");
        // "## Sub\n\nMore text here.\n" starts at char 23 and runs to end (47).
        assert_eq!(sec.start, 23);
        assert_eq!(sec.end, 47);
    }

    #[test]
    fn enclosing_section_under_h1() {
        let syn = Syntax::parse(DOC);
        // Point in the H1 paragraph → the outer H1 section, which spans the whole
        // document (the H2 section nests inside it).
        let p = DOC.find("Hello").unwrap() + 1;
        let sec = syn.enclosing_section(p).expect("a section");
        assert_eq!(sec.start, 1);
    }

    #[test]
    fn char_positions_handle_multibyte() {
        // Em dash (3 bytes) before the heading word shifts byte vs. char offsets.
        let doc = "# Tëa — pot\n\nbody\n";
        let syn = Syntax::parse(doc);
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
