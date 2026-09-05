//! The sexp scanner: Emacs's syntax-table notion of a balanced expression,
//! over a [`TextStore`], for the sexp and list motions and the MCP `thing`
//! selector. A sexp is a bracket group (`()` `[]` `{}`), a string, a run of
//! symbol characters (see `Lang::is_symbol_char`), or one punctuation
//! character, with any expression-prefix characters before it (`'` in
//! `'(a b)`). Comments are skipped. The per-language quotes, comment
//! openers and prefixes come from `Lang::sexp_rule`.
//!
//! Forward scans stream tokens from the start position through a windowed
//! reader, so a walk holds one bounded `substring` window at a time (the
//! same discipline as `motion.rs`). Backward scans lex the accessible region
//! from its start up to the position once per [`Scanner`] and walk the token
//! list in reverse: that is the only way to know whether a quote seen
//! backward opens or closes a string, or a `(` sits inside a comment.
//!
//! Known misreads, by design (not an Emacs syntax table): a Rust `'('` char
//! literal reads as punctuation, an open bracket and punctuation, so the
//! bracket counts, and a `'"'` one opens a string that runs to the next
//! quote; Rust and Python raw strings (their backslashes still escape),
//! Python f-strings and JS regex literals read as plain strings or symbols;
//! a TOML `"""` string reads as short strings back to back; a nested block
//! comment (`/* /* */ */`) closes at the first closer, leaving the rest as
//! stray text. Files that hit these have a tree-sitter grammar; the node
//! tools are the fallback.

use crate::motion::{FIRST_WINDOW, WINDOW};
use crate::store::TextStore;
use crate::syntax::{Lang, SexpRule};

/// What one token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An opening bracket, holding the bracket character.
    Open(char),
    /// A closing bracket, holding the bracket character.
    Close(char),
    /// A whole string literal, quotes included.
    Str,
    /// A run of symbol characters.
    Symbol,
    /// One character with no other role.
    Punct,
    /// A line or block comment; only the backward lexer keeps these.
    Comment,
}

/// One lexed token: a 1-based char span, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Why a scan could not finish. The position is where the offending
/// construct starts: the stray closer, the unclosed opener, the quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanError {
    Unbalanced { at: usize },
    UnterminatedString { at: usize },
    UnterminatedComment { at: usize },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Unbalanced { at } => write!(f, "Unbalanced parentheses at {at}"),
            ScanError::UnterminatedString { at } => write!(f, "Unterminated string at {at}"),
            ScanError::UnterminatedComment { at } => write!(f, "Unterminated comment at {at}"),
        }
    }
}

/// The shape of one sexp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SexpKind {
    Group,
    Str,
    Symbol,
    Punct,
}

/// One sexp: a 1-based char span, end exclusive. The span includes any
/// expression-prefix characters before the sexp proper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sexp {
    pub start: usize,
    pub end: usize,
    pub kind: SexpKind,
}

/// The closer that matches an opener.
fn closer_of(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        _ => '}',
    }
}

/// A forward character reader over `[from, bound)` that fetches one bounded
/// `substring` window at a time, starting at [`FIRST_WINDOW`] chars and
/// doubling toward [`WINDOW`].
struct Reader<'a> {
    store: &'a dyn TextStore,
    bound: usize,
    /// The chars of the current window and the position of its first char.
    window: Vec<char>,
    start: usize,
    /// The size of the next window to fetch.
    span: usize,
    /// The position of the next char to read.
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(store: &'a dyn TextStore, from: usize, bound: usize) -> Self {
        Reader {
            store,
            bound: bound.min(store.point_max()),
            window: Vec::new(),
            start: from,
            span: FIRST_WINDOW,
            pos: from,
        }
    }

    /// The char `k` positions ahead of the cursor, or `None` at the bound.
    fn peek(&mut self, k: usize) -> Option<char> {
        let p = self.pos + k;
        if p >= self.bound {
            return None;
        }
        if p < self.start || p >= self.start + self.window.len() {
            let end = self.bound.min(self.pos + self.span.max(k + 1));
            self.window = self.store.substring(self.pos, end).chars().collect();
            self.start = self.pos;
            self.span = (self.span * 2).min(WINDOW);
        }
        self.window.get(p - self.start).copied()
    }

    fn starts_with(&mut self, s: &str) -> bool {
        s.chars().enumerate().all(|(i, c)| self.peek(i) == Some(c))
    }

    fn bump(&mut self, n: usize) {
        self.pos += n;
    }
}

/// The scanner for one store and language. Cheap to build: build one per
/// builtin call or tool call.
pub struct Scanner<'a> {
    store: &'a dyn TextStore,
    lang: Lang,
    rule: &'static SexpRule,
}

impl<'a> Scanner<'a> {
    pub fn new(store: &'a dyn TextStore, lang: Lang) -> Scanner<'a> {
        Scanner {
            store,
            lang,
            rule: lang.sexp_rule(),
        }
    }

    /// The next token at or after `from`, comments skipped; `Ok(None)` when
    /// only whitespace and comments remain before `bound`.
    pub fn next_token(&self, from: usize, bound: usize) -> Result<Option<Token>, ScanError> {
        let mut r = Reader::new(self.store, from, bound);
        self.token(&mut r)
    }

    fn token(&self, r: &mut Reader) -> Result<Option<Token>, ScanError> {
        loop {
            match self.raw_token(r)? {
                Some(t) if t.kind == TokenKind::Comment => continue,
                other => return Ok(other),
            }
        }
    }

    /// The next token, comments included.
    fn raw_token(&self, r: &mut Reader) -> Result<Option<Token>, ScanError> {
        while r.peek(0).is_some_and(char::is_whitespace) {
            r.bump(1);
        }
        let Some(c) = r.peek(0) else { return Ok(None) };
        let start = r.pos;
        if let Some(opener) = self.rule.line_comments.iter().find(|s| r.starts_with(s)) {
            r.bump(opener.chars().count());
            while r.peek(0).is_some_and(|c| c != '\n') {
                r.bump(1);
            }
            return Ok(Some(Token {
                start,
                end: r.pos,
                kind: TokenKind::Comment,
            }));
        }
        if let Some((opener, closer)) = self
            .rule
            .block_comments
            .iter()
            .find(|(o, _)| r.starts_with(o))
        {
            r.bump(opener.chars().count());
            loop {
                if r.peek(0).is_none() {
                    return Err(ScanError::UnterminatedComment { at: start });
                }
                if r.starts_with(closer) {
                    r.bump(closer.chars().count());
                    break;
                }
                r.bump(1);
            }
            return Ok(Some(Token {
                start,
                end: r.pos,
                kind: TokenKind::Comment,
            }));
        }
        let kind = match c {
            '(' | '[' | '{' => {
                r.bump(1);
                TokenKind::Open(c)
            }
            ')' | ']' | '}' => {
                r.bump(1);
                TokenKind::Close(c)
            }
            q if self.rule.quotes.contains(&q) => {
                let triple =
                    self.rule.triple_quotes && r.peek(1) == Some(q) && r.peek(2) == Some(q);
                let width = if triple { 3 } else { 1 };
                let raw = self.rule.raw_quotes.contains(&q);
                r.bump(width);
                loop {
                    match r.peek(0) {
                        None => return Err(ScanError::UnterminatedString { at: start }),
                        Some('\\') if !raw => r.bump(2),
                        Some(ch)
                            if ch == q
                                && (!triple || (r.peek(1) == Some(q) && r.peek(2) == Some(q))) =>
                        {
                            r.bump(width);
                            break;
                        }
                        Some(_) => r.bump(1),
                    }
                }
                TokenKind::Str
            }
            c if self.lang.is_symbol_char(c) => {
                while r.peek(0).is_some_and(|c| self.lang.is_symbol_char(c)) {
                    r.bump(1);
                }
                TokenKind::Symbol
            }
            _ => {
                r.bump(1);
                TokenKind::Punct
            }
        };
        Ok(Some(Token {
            start,
            end: r.pos.min(r.bound),
            kind,
        }))
    }

    /// Whether the character at `at` is an expression prefix.
    fn is_prefix(&self, at: usize) -> bool {
        self.store
            .char_after(at)
            .is_some_and(|c| self.rule.prefixes.contains(&c))
    }

    /// The next sexp at or after `from`: a balanced group to its matching
    /// closer, a string, a symbol run, or one punctuation character, with
    /// the expression prefixes before it. `Ok(None)` when only whitespace
    /// and comments remain before `bound`. A closer at depth zero is
    /// `Unbalanced` at the closer, as in Emacs.
    pub fn sexp_forward(&self, from: usize, bound: usize) -> Result<Option<Sexp>, ScanError> {
        let mut r = Reader::new(self.store, from, bound);
        let Some(mut first) = self.token(&mut r)? else {
            return Ok(None);
        };
        let start = first.start;
        // Prefixes are skipped like whitespace on the way to the sexp; a
        // prefix with nothing after it is a sexp of its own.
        while first.kind == TokenKind::Punct && self.is_prefix(first.start) {
            match self.token(&mut r)? {
                Some(t) => first = t,
                None => {
                    return Ok(Some(Sexp {
                        start,
                        end: first.end,
                        kind: SexpKind::Punct,
                    }));
                }
            }
        }
        let kind = match first.kind {
            TokenKind::Open(o) => return self.group_from(&mut r, start, o),
            TokenKind::Close(_) => return Err(ScanError::Unbalanced { at: first.start }),
            TokenKind::Str => SexpKind::Str,
            TokenKind::Symbol => SexpKind::Symbol,
            TokenKind::Punct | TokenKind::Comment => SexpKind::Punct,
        };
        Ok(Some(Sexp {
            start,
            end: first.end,
            kind,
        }))
    }

    /// Finish a group whose opener `open` the reader has consumed; `start`
    /// is where the sexp began (an expression prefix may sit before the
    /// opener).
    fn group_from(
        &self,
        r: &mut Reader,
        start: usize,
        open: char,
    ) -> Result<Option<Sexp>, ScanError> {
        let mut stack = vec![open];
        loop {
            let Some(t) = self.token(r)? else {
                return Err(ScanError::Unbalanced { at: start });
            };
            match t.kind {
                TokenKind::Open(o) => stack.push(o),
                TokenKind::Close(c) => {
                    let o = stack
                        .pop()
                        .expect("the stack holds at least the first opener");
                    if closer_of(o) != c {
                        return Err(ScanError::Unbalanced { at: t.start });
                    }
                    if stack.is_empty() {
                        return Ok(Some(Sexp {
                            start,
                            end: t.end,
                            kind: SexpKind::Group,
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    /// The next bracket group at or after `from`, skipping atoms. A closer
    /// at depth zero is `Unbalanced` unless `cross_closers`, when it is
    /// stepped over (the `thing: {after}` resolution wants the first group
    /// after a line, whatever depth the line sits at).
    pub fn list_forward(
        &self,
        from: usize,
        bound: usize,
        cross_closers: bool,
    ) -> Result<Option<Sexp>, ScanError> {
        let mut r = Reader::new(self.store, from, bound);
        loop {
            let Some(t) = self.token(&mut r)? else {
                return Ok(None);
            };
            match t.kind {
                TokenKind::Open(o) => return self.group_from(&mut r, t.start, o),
                TokenKind::Close(_) if !cross_closers => {
                    return Err(ScanError::Unbalanced { at: t.start });
                }
                _ => {}
            }
        }
    }

    /// The position after the closer of the group containing `from`, or
    /// `Ok(None)` when the bound comes first (no enclosing group).
    pub fn up_forward(&self, from: usize, bound: usize) -> Result<Option<usize>, ScanError> {
        let mut r = Reader::new(self.store, from, bound);
        loop {
            let Some(t) = self.token(&mut r)? else {
                return Ok(None);
            };
            match t.kind {
                TokenKind::Open(o) => {
                    self.group_from(&mut r, t.start, o)?;
                }
                TokenKind::Close(_) => return Ok(Some(t.end)),
                _ => {}
            }
        }
    }

    /// The position after the next opener at or after `from`, skipping
    /// atoms; `Unbalanced` at a closer met first, `Ok(None)` at the bound.
    pub fn down_forward(&self, from: usize, bound: usize) -> Result<Option<usize>, ScanError> {
        let mut r = Reader::new(self.store, from, bound);
        loop {
            let Some(t) = self.token(&mut r)? else {
                return Ok(None);
            };
            match t.kind {
                TokenKind::Open(_) => return Ok(Some(t.end)),
                TokenKind::Close(_) => return Err(ScanError::Unbalanced { at: t.start }),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn toks(lang: Lang, text: &str) -> Vec<(usize, usize, TokenKind)> {
        let store = Buffer::from_string("t", text);
        let sc = Scanner::new(&store, lang);
        let mut out = Vec::new();
        let mut p = 1;
        while let Some(t) = sc.next_token(p, store.point_max()).unwrap() {
            out.push((t.start, t.end, t.kind));
            p = t.end;
        }
        out
    }

    #[test]
    fn tokens_classify_brackets_strings_symbols_and_punctuation() {
        use TokenKind::*;
        //                       1234567890123456789012
        assert_eq!(
            toks(Lang::Rust, "foo(a, \"b)\") // x\n[1]"),
            vec![
                (1, 4, Symbol),
                (4, 5, Open('(')),
                (5, 6, Symbol),
                (6, 7, Punct),
                (8, 12, Str),
                (12, 13, Close(')')),
                (19, 20, Open('[')),
                (20, 21, Symbol),
                (21, 22, Close(']')),
            ]
        );
    }

    #[test]
    fn strings_honour_escapes_triple_quotes_and_per_language_quotes() {
        use TokenKind::*;
        assert_eq!(toks(Lang::Rust, r#""a\"b""#), vec![(1, 7, Str)]);
        assert_eq!(toks(Lang::Python, "'''a'b'''"), vec![(1, 10, Str)]);
        assert_eq!(
            toks(Lang::Python, "'a' \"b\""),
            vec![(1, 4, Str), (5, 8, Str)]
        );
        // `'` is not a string quote in Rust: a lifetime is punctuation + symbol.
        assert_eq!(toks(Lang::Rust, "'a"), vec![(1, 2, Punct), (2, 3, Symbol)]);
        assert_eq!(toks(Lang::Go, "`x`"), vec![(1, 4, Str)]);
        assert_eq!(
            toks(Lang::Go, "`C:\\`"),
            vec![(1, 6, Str)],
            "a Go raw string has no escapes"
        );
    }

    #[test]
    fn comments_are_skipped_and_do_not_nest_with_strings() {
        use TokenKind::*;
        assert_eq!(toks(Lang::Rust, "/* \" */ x"), vec![(9, 10, Symbol)]);
        assert_eq!(
            toks(Lang::Rust, "\"//\" x"),
            vec![(1, 5, Str), (6, 7, Symbol)]
        );
        assert_eq!(toks(Lang::Python, "# (\nx"), vec![(5, 6, Symbol)]);
        assert_eq!(
            toks(Lang::Elisp, "; (\n(a)"),
            vec![(5, 6, Open('(')), (6, 7, Symbol), (7, 8, Close(')'))]
        );
        assert_eq!(toks(Lang::Html, "<!-- ( --> x"), vec![(12, 13, Symbol)]);
    }

    #[test]
    fn unterminated_strings_and_comments_are_errors_at_their_start() {
        let store = Buffer::from_string("t", "a \"bc");
        let sc = Scanner::new(&store, Lang::Rust);
        assert_eq!(
            sc.next_token(3, 6),
            Err(ScanError::UnterminatedString { at: 3 })
        );
        let store = Buffer::from_string("t", "a /* b");
        let sc = Scanner::new(&store, Lang::Rust);
        assert_eq!(
            sc.next_token(2, 7),
            Err(ScanError::UnterminatedComment { at: 3 })
        );
        assert_eq!(
            ScanError::Unbalanced { at: 7 }.to_string(),
            "Unbalanced parentheses at 7"
        );
    }

    #[test]
    fn elisp_symbols_span_operator_characters_and_a_quote_is_punctuation() {
        use TokenKind::*;
        //                        1234567890123456789012
        assert_eq!(
            toks(Lang::Elisp, "'(string-trim-left :k)"),
            vec![
                (1, 2, Punct),
                (2, 3, Open('(')),
                (3, 19, Symbol),
                (20, 22, Symbol),
                (22, 23, Close(')'))
            ]
        );
    }

    #[test]
    fn a_token_scan_stops_at_the_bound_and_reads_in_windows() {
        // A symbol longer than the first window must still come back whole.
        let text = "x".repeat(5000) + " y";
        let store = Buffer::from_string("t", &text);
        let sc = Scanner::new(&store, Lang::Rust);
        let t = sc.next_token(1, store.point_max()).unwrap().unwrap();
        assert_eq!((t.start, t.end), (1, 5001));
        assert_eq!(
            sc.next_token(5001, 5002).unwrap(),
            None,
            "only a space before the bound"
        );
    }

    fn sc(text: &str) -> (Buffer, Lang) {
        (Buffer::from_string("t.rs", text), Lang::Rust)
    }

    #[test]
    fn sexp_forward_spans_groups_strings_symbols_and_punct() {
        //             123456789012345678901234567
        let (b, l) = sc("foo(a, [1, \"x)\"]) ; \"s\" 'q");
        let s = Scanner::new(&b, l);
        let max = b.point_max();
        let x = s.sexp_forward(1, max).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (1, 4, SexpKind::Symbol));
        let x = s.sexp_forward(4, max).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (4, 18, SexpKind::Group));
        let x = s.sexp_forward(18, max).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (19, 20, SexpKind::Punct));
        let x = s.sexp_forward(20, max).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (21, 24, SexpKind::Str));
        let x = s.sexp_forward(24, max).unwrap().unwrap();
        assert_eq!(
            (x.start, x.end, x.kind),
            (25, 26, SexpKind::Punct),
            "a Rust `'` is plain punctuation"
        );
        assert_eq!(s.sexp_forward(27, max).unwrap(), None);
    }

    #[test]
    fn sexp_forward_reports_unbalanced_groups() {
        let (b, l) = sc("(a [b) c");
        let s = Scanner::new(&b, l);
        assert_eq!(
            s.sexp_forward(1, b.point_max()),
            Err(ScanError::Unbalanced { at: 6 }),
            "mismatched closer"
        );
        let (b, l) = sc(") a");
        let s = Scanner::new(&b, l);
        assert_eq!(
            s.sexp_forward(1, b.point_max()),
            Err(ScanError::Unbalanced { at: 1 }),
            "closer at depth zero"
        );
        let (b, l) = sc("(a (b)");
        let s = Scanner::new(&b, l);
        assert_eq!(
            s.sexp_forward(1, b.point_max()),
            Err(ScanError::Unbalanced { at: 1 }),
            "bound inside the group"
        );
    }

    #[test]
    fn list_up_and_down_scans() {
        //             12345678901234567
        let (b, l) = sc("a (b c) d [e] ) f");
        let s = Scanner::new(&b, l);
        let max = b.point_max();
        let g = s.list_forward(1, max, false).unwrap().unwrap();
        assert_eq!((g.start, g.end), (3, 8));
        let g = s.list_forward(8, max, false).unwrap().unwrap();
        assert_eq!((g.start, g.end), (11, 14));
        assert_eq!(
            s.list_forward(14, max, false),
            Err(ScanError::Unbalanced { at: 15 })
        );
        assert_eq!(
            s.list_forward(14, max, true).unwrap(),
            None,
            "crossing the stray closer finds no list"
        );
        assert_eq!(
            s.up_forward(1, max).unwrap(),
            Some(16),
            "up-list lands after the closer"
        );
        assert_eq!(s.up_forward(16, max).unwrap(), None);
        assert_eq!(
            s.down_forward(1, max).unwrap(),
            Some(4),
            "down-list lands after the opener"
        );
        assert_eq!(s.down_forward(8, max).unwrap(), Some(12));
        assert_eq!(
            s.down_forward(14, max),
            Err(ScanError::Unbalanced { at: 15 })
        );
    }

    #[test]
    fn elisp_prefix_characters_belong_to_the_following_sexp() {
        //                                    123456789012345
        let b = Buffer::from_string("t.el", "'(a b) ,@x #'f");
        let s = Scanner::new(&b, Lang::Elisp);
        let x = s.sexp_forward(1, b.point_max()).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (1, 7, SexpKind::Group));
        let x = s.sexp_forward(7, b.point_max()).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (8, 11, SexpKind::Symbol));
        let x = s.sexp_forward(11, b.point_max()).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (12, 15, SexpKind::Symbol));
        let b = Buffer::from_string("t.el", "a '");
        let s = Scanner::new(&b, Lang::Elisp);
        let x = s.sexp_forward(2, b.point_max()).unwrap().unwrap();
        assert_eq!(
            (x.start, x.end, x.kind),
            (3, 4, SexpKind::Punct),
            "a trailing prefix is just punctuation"
        );
    }
}
