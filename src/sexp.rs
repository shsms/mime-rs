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

use std::cell::RefCell;

use crate::motion::{
    FIRST_WINDOW, WINDOW, bol, is_word_char, move_paragraphs, skip_backward, skip_forward,
};
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

/// The kinds of thing `bounds_of` resolves. `defun` is not here: it needs a
/// tree-sitter parse and is resolved by the caller (`thing_bounds` in
/// `builtins.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sexp,
    List,
    Str,
    Word,
    Symbol,
    Line,
    Paragraph,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Kind> {
        Some(match name {
            "sexp" => Kind::Sexp,
            "list" => Kind::List,
            "string" => Kind::Str,
            "word" => Kind::Word,
            "symbol" => Kind::Symbol,
            "line" => Kind::Line,
            "paragraph" => Kind::Paragraph,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Kind::Sexp => "sexp",
            Kind::List => "list",
            Kind::Str => "string",
            Kind::Word => "word",
            Kind::Symbol => "symbol",
            Kind::Line => "line",
            Kind::Paragraph => "paragraph",
        }
    }
}

/// The run of `pred` characters containing `probe`, or an empty span.
fn run_at(
    store: &dyn TextStore,
    probe: usize,
    min: usize,
    max: usize,
    pred: &dyn Fn(char) -> bool,
) -> (usize, usize) {
    if !store.char_after(probe).is_some_and(pred) {
        return (probe, probe);
    }
    (
        skip_backward(store, probe, min, pred),
        skip_forward(store, probe, max, pred),
    )
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

/// The backward lexer's memo: the tokens of `[bound, upto)` in order,
/// comments included, plus at most one token that starts before `upto` and
/// may run past it, as the last element: a string or block comment cut by
/// the position is lexed to its real end; a symbol or line comment cut by it
/// is cut off at `upto`.
struct Lexed {
    bound: usize,
    upto: usize,
    tokens: Vec<Token>,
}

/// The scanner for one store and language. Cheap to build: build one per
/// builtin call or tool call.
pub struct Scanner<'a> {
    store: &'a dyn TextStore,
    lang: Lang,
    rule: &'static SexpRule,
    memo: RefCell<Option<Lexed>>,
}

impl<'a> Scanner<'a> {
    pub fn new(store: &'a dyn TextStore, lang: Lang) -> Scanner<'a> {
        Scanner {
            store,
            lang,
            rule: lang.sexp_rule(),
            memo: RefCell::new(None),
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

    /// Lex `[bound, upto)` into the memo unless it already covers that
    /// range. A string or comment cut by `upto` is lexed to its real end (a
    /// string that never closes is `UnterminatedString`) and kept as the
    /// last token, so a caller can tell "inside a string" from "at a token
    /// boundary". A symbol cut by `upto` is simply cut.
    fn lex_upto(&self, upto: usize, bound: usize) -> Result<(), ScanError> {
        if self
            .memo
            .borrow()
            .as_ref()
            .is_some_and(|m| m.bound == bound && m.upto >= upto)
        {
            return Ok(());
        }
        let mut tokens = Vec::new();
        let mut r = Reader::new(self.store, bound, upto);
        loop {
            match self.raw_token(&mut r) {
                Ok(Some(t)) => tokens.push(t),
                Ok(None) => break,
                Err(ScanError::UnterminatedString { at })
                | Err(ScanError::UnterminatedComment { at }) => {
                    let mut whole = Reader::new(self.store, at, self.store.point_max());
                    tokens.push(self.raw_token(&mut whole)?.expect("a token starts at `at`"));
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        *self.memo.borrow_mut() = Some(Lexed {
            bound,
            upto,
            tokens,
        });
        Ok(())
    }

    /// Run `f` over the tokens (comments included) that end at or before
    /// `from`, and the token cut by `from` if one starts before it and ends
    /// after it.
    fn with_tokens<R>(
        &self,
        from: usize,
        bound: usize,
        f: impl FnOnce(&[Token], Option<Token>) -> R,
    ) -> Result<R, ScanError> {
        self.lex_upto(from, bound)?;
        let memo = self.memo.borrow();
        let all = &memo.as_ref().expect("lexed above").tokens;
        let n = all.partition_point(|t| t.end <= from);
        let cut = all.get(n).filter(|t| t.start < from).copied();
        Ok(f(&all[..n], cut))
    }

    /// The sexp that ends at or before `from`, matching a closer back to its
    /// opener and absorbing the prefixes before it. `Ok(None)` at the
    /// bound. An opener met first is `Unbalanced` (backward over `(` would
    /// leave the group), as is a closer with no opener.
    pub fn sexp_backward(&self, from: usize, bound: usize) -> Result<Option<Sexp>, ScanError> {
        self.with_tokens(from, bound, |before, cut| {
            match cut {
                Some(t) if t.kind == TokenKind::Str => {
                    return Ok(Some(self.with_prefixes(
                        before,
                        Sexp {
                            start: t.start,
                            end: t.end,
                            kind: SexpKind::Str,
                        },
                    )));
                }
                Some(t) if t.kind == TokenKind::Symbol => {
                    return Ok(Some(self.with_prefixes(
                        before,
                        Sexp {
                            start: t.start,
                            end: from,
                            kind: SexpKind::Symbol,
                        },
                    )));
                }
                _ => {} // inside a comment, or at a boundary: the sexp before
            }
            let Some((i, last)) = before
                .iter()
                .enumerate()
                .rev()
                .find(|(_, t)| t.kind != TokenKind::Comment)
            else {
                return Ok(None);
            };
            let sexp = match last.kind {
                TokenKind::Close(_) => Self::group_back(before, i)?,
                TokenKind::Open(_) => return Err(ScanError::Unbalanced { at: last.start }),
                TokenKind::Str => Sexp {
                    start: last.start,
                    end: last.end,
                    kind: SexpKind::Str,
                },
                TokenKind::Symbol => Sexp {
                    start: last.start,
                    end: last.end,
                    kind: SexpKind::Symbol,
                },
                _ => Sexp {
                    start: last.start,
                    end: last.end,
                    kind: SexpKind::Punct,
                },
            };
            Ok(Some(self.with_prefixes(before, sexp)))
        })?
    }

    /// `sexp` extended back over the expression-prefix characters directly
    /// before it (no whitespace between), as `backward-prefix-chars` does.
    fn with_prefixes(&self, before: &[Token], mut sexp: Sexp) -> Sexp {
        // Token ends are increasing, so the tokens before the sexp (its own
        // interior skipped) are a prefix of `before`.
        let outside = before.partition_point(|t| t.end <= sexp.start);
        for t in before[..outside].iter().rev() {
            if t.end == sexp.start && t.kind == TokenKind::Punct && self.is_prefix(t.start) {
                sexp.start = t.start;
            } else {
                break;
            }
        }
        sexp
    }

    /// The group whose closer is `tokens[close]`, walking back to its opener.
    fn group_back(tokens: &[Token], close: usize) -> Result<Sexp, ScanError> {
        let TokenKind::Close(c) = tokens[close].kind else {
            unreachable!("group_back is called on a closer");
        };
        let mut depth = 0usize;
        for t in tokens[..=close].iter().rev() {
            match t.kind {
                TokenKind::Close(_) => depth += 1,
                TokenKind::Open(o) => {
                    depth -= 1;
                    if depth == 0 {
                        if closer_of(o) != c {
                            return Err(ScanError::Unbalanced {
                                at: tokens[close].start,
                            });
                        }
                        return Ok(Sexp {
                            start: t.start,
                            end: tokens[close].end,
                            kind: SexpKind::Group,
                        });
                    }
                }
                _ => {}
            }
        }
        Err(ScanError::Unbalanced {
            at: tokens[close].start,
        })
    }

    /// The bracket group that ends at or before `from`, skipping atoms;
    /// `Unbalanced` at an opener met first, `Ok(None)` at the bound.
    pub fn list_backward(&self, from: usize, bound: usize) -> Result<Option<Sexp>, ScanError> {
        self.with_tokens(from, bound, |before, _cut| {
            for (i, t) in before.iter().enumerate().rev() {
                match t.kind {
                    TokenKind::Close(_) => return Self::group_back(before, i).map(Some),
                    TokenKind::Open(_) => return Err(ScanError::Unbalanced { at: t.start }),
                    _ => {}
                }
            }
            Ok(None)
        })?
    }

    /// The opener of the innermost group containing `from`, or `Ok(None)`
    /// at depth zero.
    pub fn up_backward(&self, from: usize, bound: usize) -> Result<Option<usize>, ScanError> {
        self.with_tokens(from, bound, |before, _cut| {
            let mut depth = 0usize;
            for t in before.iter().rev() {
                match t.kind {
                    TokenKind::Close(_) => depth += 1,
                    TokenKind::Open(_) if depth == 0 => return Some(t.start),
                    TokenKind::Open(_) => depth -= 1,
                    _ => {}
                }
            }
            None
        })
    }

    /// The span of the `kind` thing at `pos`, widened by `up` enclosing
    /// groups (sexp and list only), or `Ok(None)` when there is none. A
    /// `pos` at point-max probes the character before it, so the thing at
    /// the end of the region is the last one, as in Emacs.
    pub fn bounds_of(
        &self,
        kind: Kind,
        pos: usize,
        up: usize,
    ) -> Result<Option<(usize, usize)>, ScanError> {
        let (min, max) = (self.store.point_min(), self.store.point_max());
        let pos = pos.clamp(min, max);
        let probe = if pos == max && pos > min {
            pos - 1
        } else {
            pos
        };
        let span = match kind {
            Kind::Sexp => self.sexp_at(probe, min, max)?,
            Kind::List => match self.sexp_at(probe, min, max)? {
                Some(g) if self.is_group(g) => Some(g),
                _ => match self.up_backward(probe, min)? {
                    Some(open) => self.sexp_forward(open, max)?.map(|g| (g.start, g.end)),
                    None => None,
                },
            },
            Kind::Str => match self.token_at(probe, min)? {
                Some(t) if t.kind == TokenKind::Str => Some((t.start, t.end)),
                _ => None,
            },
            Kind::Word => Some(run_at(self.store, probe, min, max, &is_word_char)),
            Kind::Symbol => {
                let lang = self.lang;
                Some(run_at(self.store, probe, min, max, &move |c| {
                    lang.is_symbol_char(c)
                }))
            }
            Kind::Line => {
                let start = bol(self.store, probe, min);
                let eol = skip_forward(self.store, probe, max, &|c| c != '\n');
                Some((start, (eol + 1).min(max)))
            }
            Kind::Paragraph => {
                let end = move_paragraphs(self.store, pos, 1);
                Some((move_paragraphs(self.store, end, -1), end))
            }
        };
        // `run_at` answers "none" with an empty span.
        let span = span.filter(|(a, b)| a < b);
        match (span, kind) {
            (Some(s), Kind::Sexp | Kind::List) => self.widen(s, up).map(Some),
            (s, _) => Ok(s),
        }
    }

    /// `span` widened to its `up`-th enclosing group; `Unbalanced` at the
    /// span start when the groups run out.
    pub fn widen(&self, span: (usize, usize), up: usize) -> Result<(usize, usize), ScanError> {
        let mut span = span;
        for _ in 0..up {
            let Some(open) = self.up_backward(span.0, self.store.point_min())? else {
                return Err(ScanError::Unbalanced { at: span.0 });
            };
            let g = self
                .sexp_forward(open, self.store.point_max())?
                .expect("an opener starts a sexp");
            span = (g.start, g.end);
        }
        Ok(span)
    }

    /// The token containing `probe` (a string or a symbol also when the
    /// probe is inside it), or `None` on whitespace.
    fn token_at(&self, probe: usize, min: usize) -> Result<Option<Token>, ScanError> {
        let t = self.with_tokens(probe + 1, min, |before, cut| {
            cut.or_else(|| {
                before
                    .last()
                    .copied()
                    .filter(|t| t.start <= probe && t.end == probe + 1)
            })
        })?;
        // A symbol may have been cut by the lex bound at `probe + 1`, same
        // as `before`'s last token was; extend it to its real end (a no-op
        // when it already reached its real end there).
        Ok(match t {
            Some(mut t) if t.kind == TokenKind::Symbol && t.end == probe + 1 => {
                t.end = skip_forward(self.store, t.end, self.store.point_max(), &|c| {
                    self.lang.is_symbol_char(c)
                });
                Some(t)
            }
            other => other,
        })
    }

    /// The sexp at `probe`: the whole group when the probe is on a bracket,
    /// the prefixed sexp when it is on a prefix.
    fn sexp_at(
        &self,
        probe: usize,
        min: usize,
        max: usize,
    ) -> Result<Option<(usize, usize)>, ScanError> {
        let Some(t) = self.token_at(probe, min)? else {
            return Ok(None);
        };
        Ok(match t.kind {
            TokenKind::Comment => None,
            TokenKind::Open(_) => self.sexp_forward(t.start, max)?.map(|g| (g.start, g.end)),
            TokenKind::Punct if self.is_prefix(t.start) => {
                self.sexp_forward(t.start, max)?.map(|g| (g.start, g.end))
            }
            TokenKind::Close(_) => {
                let g = self.with_tokens(t.end, min, |before, _| {
                    Self::group_back(before, before.len() - 1)
                })??;
                Some((g.start, g.end))
            }
            _ => Some((t.start, t.end)),
        })
    }

    /// Whether `span` is a bracket group (after any prefixes).
    fn is_group(&self, span: (usize, usize)) -> bool {
        let mut p = span.0;
        while p < span.1 && self.is_prefix(p) {
            p += 1;
        }
        matches!(self.store.char_after(p), Some('(' | '[' | '{'))
    }

    /// `pos`, or the end of the string or comment `pos` sits inside: a forward
    /// scan started inside one would read its text as code. A string or
    /// comment left unterminated before `pos` means no context is known, and
    /// `pos` stands.
    pub fn out_of_string_or_comment(&self, pos: usize) -> usize {
        self.with_tokens(pos + 1, self.store.point_min(), |before, cut| {
            cut.or_else(|| before.last().copied())
                .filter(|t| {
                    t.start < pos
                        && t.end > pos
                        && matches!(t.kind, TokenKind::Str | TokenKind::Comment)
                })
                .map_or(pos, |t| {
                    // The memo cuts a line comment at its bound instead of
                    // reading it whole: read the token again from its start.
                    let mut whole = Reader::new(self.store, t.start, self.store.point_max());
                    self.raw_token(&mut whole)
                        .ok()
                        .flatten()
                        .map_or(t.end, |w| w.end)
                })
        })
        .unwrap_or(pos)
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

    #[test]
    fn sexp_backward_spans_the_previous_sexp_and_matches_groups_backward() {
        //             123456789012345678901
        let (b, l) = sc("foo(a, \"x)\") [b] ; 'q");
        let s = Scanner::new(&b, l);
        let x = s.sexp_backward(b.point_max(), 1).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (21, 22, SexpKind::Symbol));
        let x = s.sexp_backward(21, 1).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (20, 21, SexpKind::Punct));
        let x = s.sexp_backward(20, 1).unwrap().unwrap();
        assert_eq!(
            (x.start, x.end, x.kind),
            (18, 19, SexpKind::Punct),
            "`;` is punctuation in Rust"
        );
        let x = s.sexp_backward(18, 1).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (14, 17, SexpKind::Group));
        let x = s.sexp_backward(14, 1).unwrap().unwrap();
        assert_eq!(
            (x.start, x.end, x.kind),
            (4, 13, SexpKind::Group),
            "the `)` inside the string is text"
        );
        let x = s.sexp_backward(4, 1).unwrap().unwrap();
        assert_eq!((x.start, x.end), (1, 4));
        assert_eq!(s.sexp_backward(1, 1).unwrap(), None);
    }

    #[test]
    fn backward_from_inside_a_string_comment_or_symbol() {
        //             1234567890123456789
        let (b, l) = sc("a \"b c\" // x (\nfoo");
        let s = Scanner::new(&b, l);
        let x = s.sexp_backward(5, 1).unwrap().unwrap();
        assert_eq!(
            (x.start, x.end, x.kind),
            (3, 8, SexpKind::Str),
            "inside the string: the string"
        );
        let x = s.sexp_backward(12, 1).unwrap().unwrap();
        assert_eq!(
            (x.start, x.end),
            (3, 8),
            "inside the comment: the sexp before it"
        );
        let x = s.sexp_backward(18, 1).unwrap().unwrap();
        assert_eq!((x.start, x.end), (16, 18), "mid-symbol: the symbol so far");
        // A multi-line string: the closing quote on line 2 is a closer.
        //             123456789012 3
        let (b, l) = sc("x \"one\ntwo\" y");
        let s = Scanner::new(&b, l);
        let x = s.sexp_backward(12, 1).unwrap().unwrap();
        assert_eq!((x.start, x.end, x.kind), (3, 12, SexpKind::Str));
    }

    #[test]
    fn backward_list_and_up_scans() {
        //             12345678901234567
        let (b, l) = sc("a (b c) d [e] ) f");
        let s = Scanner::new(&b, l);
        let g = s.list_backward(15, 1).unwrap().unwrap();
        assert_eq!((g.start, g.end), (11, 14));
        let g = s.list_backward(11, 1).unwrap().unwrap();
        assert_eq!((g.start, g.end), (3, 8));
        assert_eq!(s.list_backward(3, 1).unwrap(), None);
        assert_eq!(
            s.up_backward(5, 1).unwrap(),
            Some(3),
            "inside (b c): its opener"
        );
        assert_eq!(s.up_backward(9, 1).unwrap(), None, "at depth zero");
        assert_eq!(
            s.sexp_backward(4, 1),
            Err(ScanError::Unbalanced { at: 3 }),
            "backward over an opener"
        );
        let (b, l) = sc("(a (b) c");
        let s = Scanner::new(&b, l);
        assert_eq!(s.up_backward(8, 1).unwrap(), Some(1));
        let (b, l) = sc("a ) b");
        let s = Scanner::new(&b, l);
        assert_eq!(
            s.sexp_backward(4, 1),
            Err(ScanError::Unbalanced { at: 3 }),
            "a closer with no opener"
        );
    }

    #[test]
    fn backward_sexp_absorbs_adjacent_prefix_characters() {
        //                                    123456789
        let b = Buffer::from_string("t.el", "a '(b) `c");
        let s = Scanner::new(&b, Lang::Elisp);
        let x = s.sexp_backward(b.point_max(), 1).unwrap().unwrap();
        assert_eq!((x.start, x.end), (8, 10));
        let x = s.sexp_backward(8, 1).unwrap().unwrap();
        assert_eq!((x.start, x.end), (3, 7));
    }

    #[test]
    fn the_backward_lexer_is_memoised_per_scanner() {
        // Twelve thousand tokens, a hundred backward hops: linear, not quadratic.
        let text = "(a b) ".repeat(2000);
        let b = Buffer::from_string("t.rs", &text);
        let s = Scanner::new(&b, Lang::Rust);
        let started = std::time::Instant::now();
        let mut p = b.point_max();
        for _ in 0..100 {
            p = s.sexp_backward(p, 1).unwrap().unwrap().start;
        }
        assert_eq!(p, text.chars().count() - 100 * 6 + 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    fn bounds(
        text: &str,
        kind: Kind,
        pos: usize,
        up: usize,
    ) -> Result<Option<(usize, usize)>, ScanError> {
        let b = Buffer::from_string("t.rs", text);
        Scanner::new(&b, Lang::Rust).bounds_of(kind, pos, up)
    }

    #[test]
    fn bounds_of_sexp_and_list_at_a_position() {
        //       123456789012345678901
        let t = "foo(a, [b, \"c)\"]) x";
        assert_eq!(
            bounds(t, Kind::Sexp, 2, 0).unwrap(),
            Some((1, 4)),
            "mid-symbol"
        );
        assert_eq!(
            bounds(t, Kind::Sexp, 1, 0).unwrap(),
            Some((1, 4)),
            "on the symbol's first character"
        );
        assert_eq!(
            bounds(t, Kind::Sexp, 4, 0).unwrap(),
            Some((4, 18)),
            "on the opener"
        );
        assert_eq!(
            bounds(t, Kind::Sexp, 17, 0).unwrap(),
            Some((4, 18)),
            "on the closer"
        );
        assert_eq!(
            bounds(t, Kind::Sexp, 13, 0).unwrap(),
            Some((12, 16)),
            "inside the string"
        );
        assert_eq!(bounds(t, Kind::Sexp, 7, 0).unwrap(), None, "whitespace");
        assert_eq!(
            bounds(t, Kind::Sexp, 6, 0).unwrap(),
            Some((6, 7)),
            "punctuation"
        );
        assert_eq!(
            bounds(t, Kind::List, 7, 0).unwrap(),
            Some((4, 18)),
            "innermost containing group"
        );
        assert_eq!(bounds(t, Kind::List, 9, 0).unwrap(), Some((8, 17)));
        assert_eq!(
            bounds(t, Kind::List, 9, 1).unwrap(),
            Some((4, 18)),
            "up one"
        );
        assert_eq!(
            bounds(t, Kind::List, 9, 2),
            Err(ScanError::Unbalanced { at: 4 }),
            "up runs out"
        );
        assert_eq!(
            bounds(t, Kind::List, 20, 0).unwrap(),
            None,
            "outside every group"
        );
        assert_eq!(
            bounds(t, Kind::Sexp, 8, 1).unwrap(),
            Some((4, 18)),
            "sexp widened"
        );
        // Fixture is 19 chars (point-max 20): pos 21 clamps to 20, probing
        // the last char at 19 -- the symbol `x` at (19, 20).
        assert_eq!(
            bounds(t, Kind::Sexp, 21, 0).unwrap(),
            Some((19, 20)),
            "point-max probes the last char"
        );
        let b = Buffer::from_string("t.el", "x '(a b)");
        assert_eq!(
            Scanner::new(&b, Lang::Elisp)
                .bounds_of(Kind::Sexp, 3, 0)
                .unwrap(),
            Some((3, 9)),
            "on a prefix"
        );
        assert_eq!(
            Scanner::new(&b, Lang::Elisp)
                .bounds_of(Kind::List, 5, 0)
                .unwrap(),
            Some((4, 9))
        );
    }

    #[test]
    fn out_of_string_or_comment_reads_the_cut_construct_whole() {
        //             1234567 8
        let (b, l) = sc("// ab\nx");
        let s = Scanner::new(&b, l);
        assert_eq!(
            s.out_of_string_or_comment(3),
            6,
            "a line comment, past its end"
        );
        assert_eq!(s.out_of_string_or_comment(7), 7, "not inside anything");
        //             12345 67890 12
        let (b, l) = sc("/* a\nb */ x");
        let s = Scanner::new(&b, l);
        assert_eq!(s.out_of_string_or_comment(6), 10, "a block comment");
        let (b, l) = sc("\"a\nb\" x");
        let s = Scanner::new(&b, l);
        assert_eq!(s.out_of_string_or_comment(4), 6, "a string");
        assert_eq!(s.out_of_string_or_comment(1), 1, "on its opening quote");
        let (b, l) = sc("\"oops\nx");
        let s = Scanner::new(&b, l);
        assert_eq!(
            s.out_of_string_or_comment(7),
            7,
            "unterminated: no context known"
        );
    }

    #[test]
    fn bounds_of_string_word_symbol_line_and_paragraph() {
        //       12345678901234 5678901234567 8 90123
        let t = "let s = \"a b\";\nfoo_bar baz\n\nnext";
        assert_eq!(bounds(t, Kind::Str, 10, 0).unwrap(), Some((9, 14)));
        assert_eq!(
            bounds(t, Kind::Str, 9, 0).unwrap(),
            Some((9, 14)),
            "on the quote"
        );
        assert_eq!(bounds(t, Kind::Str, 3, 0).unwrap(), None);
        assert_eq!(
            bounds(t, Kind::Word, 17, 0).unwrap(),
            Some((16, 19)),
            "`foo` only"
        );
        assert_eq!(
            bounds(t, Kind::Symbol, 17, 0).unwrap(),
            Some((16, 23)),
            "`foo_bar`"
        );
        assert_eq!(
            bounds(t, Kind::Word, 15, 0).unwrap(),
            None,
            "on the newline"
        );
        assert_eq!(
            bounds(t, Kind::Line, 17, 0).unwrap(),
            Some((16, 28)),
            "newline included"
        );
        assert_eq!(
            bounds(t, Kind::Line, 30, 0).unwrap(),
            Some((29, 33)),
            "last line, no newline"
        );
        assert_eq!(bounds(t, Kind::Paragraph, 3, 0).unwrap(), Some((1, 28)));
        assert_eq!(Kind::parse("string"), Some(Kind::Str));
        assert_eq!(
            Kind::parse("defun"),
            None,
            "defun is resolved by the caller"
        );
        assert_eq!(Kind::Str.name(), "string");
    }
}
