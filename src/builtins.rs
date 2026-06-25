//! Editor primitives, registered on a `TulispContext` as Rust closures.
//! Emacs-Lisp names over an implicit current buffer (held in the shared
//! `Session`). M0 subset: navigation, edit, regex search/replace, reporting.
//! Subagents extend this with region/mark, kill-ring, markers, and narrowing.
use crate::engine::{Checkpoint, SharedSession};
use crate::syntax::{Lang, NodeRef, Syntax};
use tulisp::{Error, Shared, TulispContext, TulispConvertible, TulispObject, TulispValue};

fn bad_regex(e: regex::Error) -> Error {
    Error::lisp_error(format!("Invalid regexp: {e}"))
}

/// Compile a regex, caching by pattern string. The search builtins are commonly
/// called many times with a small set of repeated patterns, where `Regex::new`
/// would otherwise dominate those calls. `Regex` is `Arc`-backed, so the cached
/// clone is cheap. The session is single-threaded.
///
/// Compiled multi-line: `^` / `$` match at line boundaries, like Emacs regexes
/// (where they always mean beginning/end of line), not just at the ends of the
/// searched text. Absolute ends remain addressable as `\A` / `\z`.
pub(crate) fn cached_regex(re: &str) -> Result<regex::Regex, Error> {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, regex::Regex>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    CACHE.with(|c| {
        if let Some(rx) = c.borrow().get(re) {
            return Ok(rx.clone());
        }
        let rx = regex::RegexBuilder::new(re)
            .multi_line(true)
            .build()
            .map_err(bad_regex)?;
        let mut m = c.borrow_mut();
        if m.len() >= 16384 {
            m.clear(); // bound memory for long-lived daemons; rare in practice
        }
        m.insert(re.to_string(), rx.clone());
        Ok(rx)
    })
}

fn err(msg: &str) -> Error {
    Error::lisp_error(msg.to_string())
}

/// A buffer marker: a durable position handle. The `id` indexes the store's
/// marker registry (`TextStore::marker_*`), where the live position lives and
/// auto-adjusts across edits. A first-class tulisp value (via `TulispConvertible`)
/// so `markerp` can tell it apart from a plain integer position, and `goto-char`
/// accepts either.
#[derive(Clone, Copy)]
struct Marker {
    id: usize,
}

impl std::fmt::Display for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#<marker {}>", self.id)
    }
}

impl TulispConvertible for Marker {
    fn from_tulisp(value: &TulispObject) -> Result<Self, Error> {
        value
            .as_any()
            .ok()
            .and_then(|v| v.downcast_ref::<Marker>().copied())
            .ok_or_else(|| err("expected a marker"))
    }
    fn into_tulisp(self) -> TulispObject {
        Shared::new(self).into()
    }
}

/// A first-class parse-tree node: a [`NodeRef`] paired with the `Rc`'d
/// [`Syntax`] it came from and the buffer content version that parse
/// reflects. Accessors refuse an OUTDATED node (the buffer was edited since
/// the parse) instead of serving positions from a stale tree — Emacs's
/// `treesit-node-outdated` discipline. Cheap to clone; the `Rc` keeps the
/// parse alive even after the session cache moves on.
#[derive(Clone)]
struct TsNode {
    syn: std::rc::Rc<Syntax>,
    h: NodeRef,
    version: u64,
}

impl TsNode {
    fn new(syn: &std::rc::Rc<Syntax>, version: u64, h: NodeRef) -> TsNode {
        TsNode {
            syn: std::rc::Rc::clone(syn),
            h,
            version,
        }
    }

    /// A sibling value in the same parse (relational results).
    fn derive(&self, h: NodeRef) -> TsNode {
        TsNode::new(&self.syn, self.version, h)
    }

    /// The node's kind + char span, or an error for a handle the parse can't
    /// relocate (impossible for handles minted by this module — surfaced as
    /// an error rather than a panic or a sentinel, so a bug can't take the
    /// process down or feed position 0 onward).
    fn described(&self) -> Result<crate::syntax::NodeSpan, Error> {
        self.syn
            .describe(self.h)
            .ok_or_else(|| err("internal: node could not be relocated in its parse"))
    }
}

impl std::fmt::Display for TsNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rendered lazily — queries mint hundreds of nodes and most are
        // never printed.
        match self.syn.describe(self.h) {
            Some(s) => write!(f, "#<node {} @{}..{}>", s.kind, s.start, s.end),
            None => write!(f, "#<node ?>"),
        }
    }
}

impl TulispConvertible for TsNode {
    fn from_tulisp(value: &TulispObject) -> Result<Self, Error> {
        value
            .as_any()
            .ok()
            .and_then(|v| v.downcast_ref::<TsNode>().cloned())
            .ok_or_else(|| err("expected a tree-sitter node"))
    }
    fn into_tulisp(self) -> TulispObject {
        Shared::new(self).into()
    }
}

/// `Some(node)` → the node value, `None` → nil (relational dead ends).
trait IntoTulispOpt {
    fn into_tulisp_opt(self) -> TulispObject;
}
impl IntoTulispOpt for Option<TsNode> {
    fn into_tulisp_opt(self) -> TulispObject {
        self.map_or_else(TulispObject::nil, TulispConvertible::into_tulisp)
    }
}

/// An optional lisp flag argument: true unless missing or nil.
fn truthy(v: &Option<TulispObject>) -> bool {
    v.as_ref().is_some_and(|v| !v.null())
}

/// Guard every node accessor: the node's parse must reflect the CURRENT
/// content of one of the session's buffers (versions are globally unique per
/// text state, so a version match IS a content match). The node's own buffer
/// need not be the current one — accessors read the node's own `Rc<Syntax>`,
/// and its positions address the buffer it came from. Note:
/// `treesit-set-language` re-parses but does not retire old nodes (the text
/// is unchanged, so their positions stay right); nodes simply keep the
/// grammar they were minted under.
fn live_node(sess: &crate::engine::Session, n: &TsNode) -> Result<(), Error> {
    if n.version == sess.buffer.version() || sess.inactive.iter().any(|b| b.version() == n.version)
    {
        return Ok(());
    }
    Err(err(
        "outdated node: its buffer changed since the node's parse — \
         re-fetch it (treesit-node-at / treesit-defun-at / treesit-query)",
    ))
}

/// Clamp one occur output line to ~240 chars, keeping a window around the
/// match column (`col`, 0-based chars from line start); `…` marks elision.
/// Keeps a single minified/log line from flooding an occur result.
fn clamp_occur_line(text: &str, col: usize) -> String {
    const MAX: usize = 240;
    let len = text.chars().count();
    if len <= MAX {
        return text.to_string();
    }
    let lo = col.saturating_sub(MAX / 3).min(len - MAX);
    let windowed: String = text.chars().skip(lo).take(MAX).collect();
    format!(
        "{}{windowed}{}",
        if lo > 0 { "…" } else { "" },
        if lo + MAX < len { "…" } else { "" }
    )
}

/// Shared body of `conflict-keep` / `conflict-replace`: pick the addressed
/// hunk, compute its replacement via `produce` (which returns the splice text
/// plus an optional warning), splice it in place of the whole hunk, and return
/// the REMAINING conflict count. The re-scan keeps that count honest even when
/// a replacement itself contains marker-shaped lines. When `produce` returns a
/// warning it's pushed to the run log (the `(message …)` channel) — used to
/// flag a fused "keep both" join, computed from the same section materialization
/// as the splice text so the sides aren't read out of the buffer twice.
/// Shared core of `keep-lines` / `flush-lines`: rewrite the region from the
/// start of point's line to point-max, keeping (or dropping) the lines the
/// regex matches; returns the number of lines deleted. Operates "from point",
/// like Emacs; point lands at the region start. One delete + one insert, so
/// markers in the rewritten region collapse to its start — coarse, but the
/// classic bulk filter is rarely mixed with marker bookkeeping.
fn filter_lines(s: &SharedSession, rx: &regex::Regex, keep_matching: bool) -> Result<i64, Error> {
    let mut sess = s.borrow_mut();
    sess.buffer.beginning_of_line();
    let start = sess.buffer.point();
    let end = sess.buffer.point_max();
    let region = sess.buffer.substring(start, end);
    let mut deleted = 0i64;
    let kept: String = region
        .split_inclusive('\n')
        .filter(|line| {
            let keep = rx.is_match(line.trim_end_matches('\n')) == keep_matching;
            if !keep {
                deleted += 1;
            }
            keep
        })
        .collect();
    if kept != region {
        sess.buffer.delete_region(start, end);
        sess.buffer.goto_char(start);
        sess.buffer.insert(&kept);
    }
    let landing = start.min(sess.buffer.point_max());
    sess.buffer.goto_char(landing);
    Ok(deleted)
}

fn conflict_splice<F>(s: &SharedSession, n: Option<i64>, produce: F) -> Result<i64, Error>
where
    F: FnOnce(
        &dyn crate::store::TextStore,
        &crate::conflict::Hunk,
    ) -> Result<(String, Option<String>), Error>,
{
    let mut sess = s.borrow_mut();
    let (count, warning) = {
        let b = sess.buffer.as_mut();
        let hunks = crate::conflict::scan(b);
        let h = crate::conflict::pick(&hunks, n, b.point()).map_err(|e| err(&e))?;
        let (text, warning) = produce(&*b, h)?;
        let (start, end) = (h.start, h.end);
        b.delete_region(start, end);
        b.goto_char(start);
        b.insert(&text);
        (crate::conflict::scan(b).len() as i64, warning)
    };
    if let Some(w) = warning {
        sess.log.push(w);
    }
    Ok(count)
}

/// Apply a whole-buffer set of `(start, end, replacement)` splices bottom-up so
/// earlier spans stay valid as later ones shrink/grow, then return the remaining
/// conflict count. The multi-hunk counterpart of [`conflict_splice`], shared by
/// the buffer-wide resolvers (`conflict-keep-all`, `conflict-resolve-trivial`).
fn conflict_splice_all(
    b: &mut dyn crate::store::TextStore,
    plan: Vec<(usize, usize, String)>,
) -> i64 {
    for (start, end, text) in plan.into_iter().rev() {
        b.delete_region(start, end);
        b.goto_char(start);
        b.insert(&text);
    }
    crate::conflict::scan(b).len() as i64
}

pub fn register(ctx: &mut TulispContext, session: &SharedSession) {
    // ---- navigation ----
    {
        let s = session.clone();
        ctx.defun("point", move || -> i64 { s.borrow().buffer.point() as i64 });
    }
    {
        let s = session.clone();
        ctx.defun("point-min", move || -> i64 {
            s.borrow().buffer.point_min() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("point-max", move || -> i64 {
            s.borrow().buffer.point_max() as i64
        });
    }
    {
        let s = session.clone();
        // Accepts an integer position or a marker (Emacs `goto-char`).
        ctx.defun("goto-char", move |p: TulispObject| -> Result<i64, Error> {
            let mut b = s.borrow_mut();
            let pos = if let Ok(m) = Marker::from_tulisp(&p) {
                b.buffer
                    .marker_position(m.id)
                    .ok_or_else(|| err("goto-char: marker points nowhere"))?
            } else {
                i64::from_tulisp(&p)?.max(1) as usize
            };
            b.buffer.goto_char(pos);
            Ok(b.buffer.point() as i64)
        });
    }
    {
        let s = session.clone();
        ctx.defun("forward-char", move |n: Option<i64>| -> i64 {
            let mut b = s.borrow_mut();
            let p = b.buffer.point() as i64 + n.unwrap_or(1);
            b.buffer.goto_char(p.max(1) as usize);
            b.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("backward-char", move |n: Option<i64>| -> i64 {
            let mut b = s.borrow_mut();
            let p = b.buffer.point() as i64 - n.unwrap_or(1);
            b.buffer.goto_char(p.max(1) as usize);
            b.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("beginning-of-buffer", move || -> i64 {
            let mut b = s.borrow_mut();
            let m = b.buffer.point_min();
            b.buffer.goto_char(m);
            m as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("end-of-buffer", move || -> i64 {
            let mut b = s.borrow_mut();
            let m = b.buffer.point_max();
            b.buffer.goto_char(m);
            m as i64
        });
    }

    // ---- text ----
    {
        // (buffer-string) — the ACCESSIBLE portion, like Emacs: a narrowing
        // scopes it. (`write-file` still persists the whole buffer.)
        let s = session.clone();
        ctx.defun("buffer-string", move || -> String {
            let sess = s.borrow();
            sess.buffer
                .substring(sess.buffer.point_min(), sess.buffer.point_max())
        });
    }
    {
        let s = session.clone();
        ctx.defun("buffer-substring", move |a: i64, b: i64| -> String {
            s.borrow()
                .buffer
                .substring(a.max(1) as usize, b.max(1) as usize)
        });
    }
    {
        let s = session.clone();
        ctx.defun(
            "insert",
            move |text: String| -> Result<TulispObject, Error> {
                s.borrow_mut().buffer.insert(&text);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "delete-region",
            move |a: i64, b: i64| -> Result<TulispObject, Error> {
                s.borrow_mut()
                    .buffer
                    .delete_region(a.max(1) as usize, b.max(1) as usize);
                Ok(TulispObject::nil())
            },
        );
    }

    // ---- regex search / match (RE2) ----
    {
        let s = session.clone();
        ctx.defun(
            "re-search-forward",
            move |re: String,
                  bound: Option<i64>,
                  noerror: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let rx = cached_regex(&re)?;
                let bound = bound.map(|b| b.max(1) as usize);
                let hit = s.borrow_mut().buffer.re_search_forward(&rx, bound);
                match hit {
                    Some(p) => Ok(TulispObject::from(p as i64)),
                    None if noerror.is_some_and(|o| o.is_truthy()) => Ok(TulispObject::nil()),
                    None => Err(Error::lisp_error(format!("Search failed: {re}"))),
                }
            },
        );
    }
    {
        // (re-search-backward REGEXP &optional BOUND NOERROR) — the mirror of
        // re-search-forward: BOUND is the LOWER limit, point lands on the
        // match start, and with overlapping candidates the latest start wins.
        let s = session.clone();
        ctx.defun(
            "re-search-backward",
            move |re: String,
                  bound: Option<i64>,
                  noerror: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let rx = cached_regex(&re)?;
                let bound = bound.map(|b| b.max(1) as usize);
                let hit = s.borrow_mut().buffer.re_search_backward(&rx, bound);
                match hit {
                    Some(p) => Ok(TulispObject::from(p as i64)),
                    None if noerror.is_some_and(|o| o.is_truthy()) => Ok(TulispObject::nil()),
                    None => Err(Error::lisp_error(format!("Search failed: {re}"))),
                }
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "replace-match",
            // (replace-match NEWTEXT &optional FIXEDCASE LITERAL). LITERAL
            // non-nil inserts NEWTEXT verbatim — no `\&`/`\N` expansion, so
            // backslash-heavy replacements need no double escaping. FIXEDCASE
            // is accepted for parity but ignored: mime never case-adjusts a
            // replacement (it always behaves as FIXEDCASE = t).
            move |newtext: String,
                  _fixedcase: Option<TulispObject>,
                  literal: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let text = if literal.is_some_and(|o| o.is_truthy()) {
                    newtext.replace('\\', "\\\\")
                } else {
                    newtext
                };
                s.borrow_mut()
                    .buffer
                    .replace_match(&text)
                    .map_err(Error::lisp_error)?;
                Ok(TulispObject::nil())
            },
        );
    }
    {
        // (looking-back REGEXP &optional LIMIT) — t when text ending exactly
        // at point matches. Anchored with \z against the [LIMIT|point-min,
        // point) window, so the leftmost (longest) qualifying match decides,
        // like Emacs's greedy backward match. Like looking-at, records no
        // match data; boundary context left of LIMIT is cut (the documented
        // bound divergence).
        let s = session.clone();
        ctx.defun(
            "looking-back",
            move |re: String, limit: Option<i64>| -> Result<TulispObject, Error> {
                let rx = cached_regex(&format!("(?:{re})\\z"))?;
                let sess = s.borrow();
                let point = sess.buffer.point();
                let lo = match limit {
                    Some(l) => (l.max(1) as usize).min(point),
                    None => sess.buffer.point_min().min(point),
                };
                Ok(if rx.is_match(&sess.buffer.substring(lo, point)) {
                    TulispObject::t()
                } else {
                    TulispObject::nil()
                })
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "looking-at",
            move |re: String| -> Result<TulispObject, Error> {
                let rx = cached_regex(&re)?;
                Ok(if s.borrow().buffer.looking_at(&rx) {
                    TulispObject::t()
                } else {
                    TulispObject::nil()
                })
            },
        );
    }

    // ---- observability ----
    {
        let s = session.clone();
        ctx.defun(
            "report",
            move |key: String, value: TulispObject| -> Result<TulispObject, Error> {
                s.borrow_mut().reports.push((key, value.to_string()));
                Ok(value)
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun("message", move |msg: String| -> String {
            s.borrow_mut().log.push(msg.clone());
            msg
        });
    }
    {
        // (window &optional N POS) — a text viewport: N lines (default 4) before and
        // after POS (default point), the focus line marked with '‸' at the column,
        // plus line numbers and a header. The agent's eyes. POS lets you "look" at
        // any position (e.g. a saved marker) — several calls = several viewports.
        let s = session.clone();
        ctx.defun(
            "window",
            move |n: Option<i64>, pos: Option<i64>| -> String {
                let n = n.unwrap_or(4).max(0) as usize;
                let mut sess = s.borrow_mut();
                let saved = sess.buffer.point();
                let center =
                    pos.map_or(saved, |p| (p.max(1) as usize).min(sess.buffer.point_max()));
                let cur_line = sess.buffer.line_number_at_pos(center);
                sess.buffer.goto_char(center);
                sess.buffer.beginning_of_line();
                let col = center - sess.buffer.point();
                let total = sess.buffer.line_number_at_pos(sess.buffer.point_max());
                let lo = cur_line.saturating_sub(n).max(1);
                let hi = (cur_line + n).min(total);
                let pmax = sess.buffer.point_max();
                // An active restriction is flagged like Emacs's "Narrow"
                // modeline indicator, so a viewport over a narrowed buffer is
                // never mistaken for the whole file. Line numbers count from
                // the accessible region's start; char positions stay absolute.
                let narrow = if sess.buffer.narrowing().is_some() {
                    "  Narrow"
                } else {
                    ""
                };
                let mut out = format!(
                    "\u{2014} {}  line {} col {}  point {}/{}{narrow} \u{2014}\n",
                    sess.buffer.name(),
                    cur_line,
                    col,
                    center,
                    pmax - 1
                );
                let m = sess.buffer.point_min();
                sess.buffer.goto_char(m);
                sess.buffer.forward_line(lo as i64 - 1);
                for line_no in lo..=hi {
                    let start = sess.buffer.point();
                    sess.buffer.end_of_line();
                    let end = sess.buffer.point();
                    let mut text = sess.buffer.substring(start, end);
                    if line_no == cur_line {
                        let at = col.min(text.chars().count());
                        let byte = text.char_indices().nth(at).map_or(text.len(), |(b, _)| b);
                        text.insert(byte, '\u{2038}');
                    }
                    let gutter = if line_no == cur_line { '>' } else { ' ' };
                    out.push_str(&format!("{line_no:>5} {gutter} {text}\n"));
                    if line_no < hi {
                        sess.buffer.goto_char(end);
                        sess.buffer.forward_line(1);
                    }
                }
                sess.buffer.goto_char(saved);
                out
            },
        );
    }

    {
        // (occur REGEXP &optional NLINES LIMIT) — a buffer-wide match overview:
        // one rendered line per matching line of the accessible region (so it
        // composes with narrowing), grep-style, each with its line number and
        // the char position of the line's first match for a direct goto-char
        // follow-up. NLINES (default 0) adds context lines around each hit,
        // overlapping blocks merged, "--" between blocks; LIMIT (default 100)
        // caps the rendered matching lines ("… N more" tail), and very long
        // lines are clamped around the match — token discipline for
        // minified/log content. Orientation, not motion: point is preserved
        // (match data is not). Shares the windowed-search perf profile of
        // replace-regexp on a huge Quire: fine at code scale.
        let s = session.clone();
        ctx.defun(
            "occur",
            move |re: String,
                  nlines: Option<i64>,
                  limit: Option<i64>|
                  -> Result<String, Error> {
                let rx = cached_regex(&re)?;
                let nlines = nlines.unwrap_or(0).max(0) as usize;
                let limit = limit.unwrap_or(100).max(1) as usize;
                let mut sess = s.borrow_mut();
                let saved = sess.buffer.point();
                let pmin = sess.buffer.point_min();
                let pmax = sess.buffer.point_max();
                let min_line = sess.buffer.line_number_at_pos(pmin);
                let max_line = sess.buffer.line_number_at_pos(pmax);
                // Pass 1: collect (line, first-match pos, per-line count) for
                // the first LIMIT matching lines, counting the rest only for
                // the header totals.
                let mut hits: Vec<(usize, usize, usize)> = Vec::new();
                let (mut total_matches, mut total_lines, mut prev_line) = (0usize, 0usize, 0usize);
                sess.buffer.goto_char(pmin);
                while let Some(end) = sess.buffer.re_search_forward(&rx, None) {
                    let start = sess.buffer.last_match().map_or(end, |m| m.start);
                    total_matches += 1;
                    let line = sess.buffer.line_number_at_pos(start);
                    if line != prev_line {
                        prev_line = line;
                        total_lines += 1;
                        if hits.len() < limit {
                            hits.push((line, start, 1));
                        }
                    } else if let Some(last) = hits.last_mut().filter(|h| h.0 == line) {
                        last.2 += 1;
                    }
                    // An empty match leaves point in place — step over it.
                    if end == start {
                        if end >= pmax {
                            break;
                        }
                        sess.buffer.goto_char(end + 1);
                    }
                }
                let name = sess.buffer.name().to_string();
                if total_matches == 0 {
                    sess.buffer.goto_char(saved);
                    return Ok(format!("\u{2014} occur \"{re}\" in {name}: no matches \u{2014}\n"));
                }
                let mut out = format!(
                    "\u{2014} occur \"{re}\" in {name}: {total_matches} match{} on {total_lines} line{} \u{2014}\n",
                    if total_matches == 1 { "" } else { "es" },
                    if total_lines == 1 { "" } else { "s" },
                );
                // Pass 2: render the hit lines (plus NLINES of context, with
                // overlapping blocks merged so no line prints twice).
                let mut blocks: Vec<(usize, usize)> = Vec::new();
                for &(line, _, _) in &hits {
                    let lo = line.saturating_sub(nlines).max(min_line);
                    let hi = (line + nlines).min(max_line);
                    match blocks.last_mut() {
                        Some((_, bhi)) if lo <= *bhi + 1 => *bhi = (*bhi).max(hi),
                        _ => blocks.push((lo, hi)),
                    }
                }
                for (bi, &(lo, hi)) in blocks.iter().enumerate() {
                    if nlines > 0 && bi > 0 {
                        out.push_str("   --\n");
                    }
                    sess.buffer.goto_char(pmin);
                    sess.buffer.forward_line((lo - min_line) as i64);
                    for line_no in lo..=hi {
                        let start = sess.buffer.point();
                        sess.buffer.end_of_line();
                        let end = sess.buffer.point();
                        let text = sess.buffer.substring(start, end);
                        match hits.binary_search_by_key(&line_no, |h| h.0) {
                            Ok(i) => {
                                let (_, pos, n) = hits[i];
                                let times =
                                    if n > 1 { format!(" \u{d7}{n}") } else { String::new() };
                                let text = clamp_occur_line(&text, pos - start);
                                out.push_str(&format!("{line_no:>5} @{pos}{times}: {text}\n"));
                            }
                            Err(_) => {
                                let text = clamp_occur_line(&text, 0);
                                out.push_str(&format!("{line_no:>5} - {text}\n"));
                            }
                        }
                        if line_no < hi {
                            sess.buffer.goto_char(end);
                            sess.buffer.forward_line(1);
                        }
                    }
                }
                if total_lines > hits.len() {
                    out.push_str(&format!(
                        "  \u{2026} and {} more matching lines\n",
                        total_lines - hits.len()
                    ));
                }
                sess.buffer.goto_char(saved);
                Ok(out)
            },
        );
    }
    {
        // (buffer-file-name) — the visited file's path as recorded at open/rebase
        // time, or nil for a buffer with no backing file (Emacs parity).
        let s = session.clone();
        ctx.defun("buffer-file-name", move || -> TulispObject {
            s.borrow()
                .buffer
                .file_stamp()
                .map(|st| st.path.to_string_lossy().into_owned())
                .into()
        });
    }
    {
        // (buffer-stale-p) — non-nil if the visited file changed on disk since it
        // was opened/saved (external writer: modified, replaced, or deleted); nil
        // for a clean stamp or a buffer with no backing file. The value is the
        // drift description string. Lets a program detect the stale-read race
        // up front instead of discovering it when the save is refused.
        let s = session.clone();
        ctx.defun("buffer-stale-p", move || -> TulispObject {
            s.borrow()
                .buffer
                .file_stamp()
                .and_then(|st| st.check())
                .into()
        });
    }
    {
        // (revert-buffer) — discard the buffer's edits and re-read the visited
        // file from disk: the recovery path when the stale guard refuses a
        // save (an external writer landed since open). Point is preserved by
        // position (clamped to the new content); narrowing and markers are
        // dropped with the old text. Re-reads only the already-authorized
        // visited path, so it is safe in the sandboxed tier. There is
        // deliberately no force-save counterpart — overwriting an external
        // writer's work stays impossible; revert, re-apply, save.
        let s = session.clone();
        ctx.defun("revert-buffer", move || -> Result<bool, Error> {
            // Keeps the buffer's (possibly uniquified) name, drops the old
            // content's markers/narrowing, and resets the modified baseline —
            // see `engine::revert_in_place`, shared with auto-revert.
            crate::engine::revert_in_place(&mut s.borrow_mut()).map_err(|e| err(&e))?;
            Ok(true)
        });
    }

    // ---- mark & region ----
    {
        let s = session.clone();
        ctx.defun("set-mark", move |p: i64| -> i64 {
            let p = p.max(1);
            s.borrow_mut().buffer.set_mark(p as usize);
            p
        });
    }
    {
        let s = session.clone();
        ctx.defun("mark", move || -> Result<TulispObject, Error> {
            Ok(match s.borrow().buffer.mark() {
                Some(m) => TulispObject::from(m as i64),
                None => TulispObject::nil(),
            })
        });
    }
    {
        let s = session.clone();
        ctx.defun("region-beginning", move || -> Result<i64, Error> {
            let sess = s.borrow();
            let m = sess
                .buffer
                .mark()
                .ok_or_else(|| err("The mark is not set now"))?;
            Ok(sess.buffer.point().min(m) as i64)
        });
    }
    {
        let s = session.clone();
        ctx.defun("region-end", move || -> Result<i64, Error> {
            let sess = s.borrow();
            let m = sess
                .buffer
                .mark()
                .ok_or_else(|| err("The mark is not set now"))?;
            Ok(sess.buffer.point().max(m) as i64)
        });
    }
    {
        let s = session.clone();
        ctx.defun("exchange-point-and-mark", move || -> Result<i64, Error> {
            let mut sess = s.borrow_mut();
            let m = sess
                .buffer
                .mark()
                .ok_or_else(|| err("No mark set in this buffer"))?;
            let p = sess.buffer.point();
            sess.buffer.set_mark(p);
            sess.buffer.goto_char(m);
            Ok(sess.buffer.point() as i64)
        });
    }

    // ---- markers (durable positions; the multi-cursor / viewport primitive) ----
    {
        let s = session.clone();
        // (make-marker) — a marker that points nowhere until `set-marker`.
        ctx.defun("make-marker", move || -> Marker {
            Marker {
                id: s.borrow_mut().buffer.marker_create(None),
            }
        });
    }
    {
        let s = session.clone();
        // (point-marker) — a marker at point.
        ctx.defun("point-marker", move || -> Marker {
            let mut sess = s.borrow_mut();
            let p = sess.buffer.point();
            Marker {
                id: sess.buffer.marker_create(Some(p)),
            }
        });
    }
    {
        let s = session.clone();
        // (copy-marker &optional POS) — a new marker at POS (default point).
        ctx.defun("copy-marker", move |pos: Option<i64>| -> Marker {
            let mut sess = s.borrow_mut();
            let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
            Marker {
                id: sess.buffer.marker_create(Some(p)),
            }
        });
    }
    {
        let s = session.clone();
        // (set-marker MARKER POS) — point MARKER at POS, or detach it with nil.
        ctx.defun("set-marker", move |m: Marker, pos: Option<i64>| -> Marker {
            s.borrow_mut()
                .buffer
                .marker_set(m.id, pos.map(|p| p.max(1) as usize));
            m
        });
    }
    {
        let s = session.clone();
        // (marker-position MARKER) — its position, or nil if detached.
        ctx.defun(
            "marker-position",
            move |m: Marker| -> Result<TulispObject, Error> {
                Ok(match s.borrow().buffer.marker_position(m.id) {
                    Some(p) => TulispValue::from(p as i64).into_ref(None),
                    None => TulispObject::nil(),
                })
            },
        );
    }
    {
        // (markerp OBJECT) — t if OBJECT is a marker.
        ctx.defun("markerp", move |obj: TulispObject| -> bool {
            Marker::from_tulisp(&obj).is_ok()
        });
    }

    // ---- line navigation ----
    {
        let s = session.clone();
        ctx.defun("beginning-of-line", move || -> i64 {
            let mut sess = s.borrow_mut();
            sess.buffer.beginning_of_line();
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("end-of-line", move || -> i64 {
            let mut sess = s.borrow_mut();
            sess.buffer.end_of_line();
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("forward-line", move |n: Option<i64>| -> i64 {
            s.borrow_mut().buffer.forward_line(n.unwrap_or(1))
        });
    }
    {
        let s = session.clone();
        ctx.defun("line-number-at-pos", move |p: Option<i64>| -> i64 {
            let sess = s.borrow();
            let pos = p.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
            sess.buffer.line_number_at_pos(pos) as i64
        });
    }

    // ---- char access ----
    {
        let s = session.clone();
        ctx.defun(
            "char-after",
            move |p: Option<i64>| -> Result<TulispObject, Error> {
                let sess = s.borrow();
                let pos = p.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
                Ok(match sess.buffer.char_after(pos) {
                    Some(c) => TulispObject::from(c as i64),
                    None => TulispObject::nil(),
                })
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "char-before",
            move |p: Option<i64>| -> Result<TulispObject, Error> {
                let sess = s.borrow();
                let pos = p.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
                Ok(match sess.buffer.char_before(pos) {
                    Some(c) => TulispObject::from(c as i64),
                    None => TulispObject::nil(),
                })
            },
        );
    }

    // ---- exact search ----
    {
        let s = session.clone();
        ctx.defun(
            "search-forward",
            move |needle: String,
                  bound: Option<i64>,
                  noerror: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let bound = bound.map(|b| b.max(1) as usize);
                match s.borrow_mut().buffer.search_forward(&needle, bound) {
                    Some(p) => Ok(TulispObject::from(p as i64)),
                    None if noerror.is_some_and(|o| o.is_truthy()) => Ok(TulispObject::nil()),
                    None => Err(Error::lisp_error(format!("Search failed: {needle}"))),
                }
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "search-backward",
            move |needle: String,
                  bound: Option<i64>,
                  noerror: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let bound = bound.map(|b| b.max(1) as usize);
                match s.borrow_mut().buffer.search_backward(&needle, bound) {
                    Some(p) => Ok(TulispObject::from(p as i64)),
                    None if noerror.is_some_and(|o| o.is_truthy()) => Ok(TulispObject::nil()),
                    None => Err(Error::lisp_error(format!("Search failed: {needle}"))),
                }
            },
        );
    }

    // ---- narrowing & excursion ----
    {
        let s = session.clone();
        ctx.defun(
            "narrow-to-region",
            move |a: i64, b: i64| -> Result<TulispObject, Error> {
                s.borrow_mut()
                    .buffer
                    .narrow_to_region(a.max(1) as usize, b.max(1) as usize);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun("widen", move || -> Result<TulispObject, Error> {
            s.borrow_mut().buffer.widen();
            Ok(TulispObject::nil())
        });
    }
    {
        // (save-excursion BODY...) — run BODY, then restore point and mark.
        let s = session.clone();
        ctx.defspecial("save-excursion", move |ctx, args| {
            let (pt, mk) = {
                let sess = s.borrow();
                (sess.buffer.point(), sess.buffer.mark())
            };
            let res = ctx.eval_progn(args);
            let mut sess = s.borrow_mut();
            sess.buffer.goto_char(pt);
            sess.buffer.set_mark_opt(mk);
            res
        });
    }
    {
        // (save-restriction BODY...) — run BODY, then restore the narrowing.
        let s = session.clone();
        ctx.defspecial("save-restriction", move |ctx, args| {
            let saved = s.borrow().buffer.narrowing();
            let res = ctx.eval_progn(args);
            s.borrow_mut().buffer.set_restriction(saved);
            res
        });
    }

    // ---- kill ring ----
    {
        let s = session.clone();
        ctx.defun(
            "kill-region",
            move |a: i64, b: i64| -> Result<TulispObject, Error> {
                let (a, b) = (a.max(1) as usize, b.max(1) as usize);
                let mut sess = s.borrow_mut();
                let text = sess.buffer.substring(a, b);
                sess.kill_ring.push(text);
                sess.buffer.delete_region(a, b);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "copy-region-as-kill",
            move |a: i64, b: i64| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let text = sess.buffer.substring(a.max(1) as usize, b.max(1) as usize);
                sess.kill_ring.push(text);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun("yank", move || -> Result<TulispObject, Error> {
            let mut sess = s.borrow_mut();
            if let Some(text) = sess.kill_ring.last().cloned() {
                sess.buffer.insert(&text);
            }
            Ok(TulispObject::nil())
        });
    }

    // ---- more edit primitives ----
    {
        let s = session.clone();
        ctx.defun(
            "delete-char",
            move |n: i64| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let p = sess.buffer.point() as i64;
                sess.buffer
                    .delete_region(p.max(1) as usize, (p + n).max(1) as usize);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun("erase-buffer", move || -> Result<TulispObject, Error> {
            let mut sess = s.borrow_mut();
            let (lo, hi) = (sess.buffer.point_min(), sess.buffer.point_max());
            sess.buffer.delete_region(lo, hi);
            Ok(TulispObject::nil())
        });
    }
    {
        let s = session.clone();
        ctx.defun("bolp", move || -> Result<TulispObject, Error> {
            let sess = s.borrow();
            let p = sess.buffer.point();
            let at = p == sess.buffer.point_min() || sess.buffer.char_before(p) == Some('\n');
            Ok(if at {
                TulispObject::t()
            } else {
                TulispObject::nil()
            })
        });
    }
    {
        let s = session.clone();
        ctx.defun("eolp", move || -> Result<TulispObject, Error> {
            let sess = s.borrow();
            let p = sess.buffer.point();
            let at = p == sess.buffer.point_max() || sess.buffer.char_after(p) == Some('\n');
            Ok(if at {
                TulispObject::t()
            } else {
                TulispObject::nil()
            })
        });
    }
    {
        let s = session.clone();
        ctx.defun("bobp", move || -> Result<TulispObject, Error> {
            let sess = s.borrow();
            let at = sess.buffer.point() == sess.buffer.point_min();
            Ok(if at {
                TulispObject::t()
            } else {
                TulispObject::nil()
            })
        });
    }
    {
        let s = session.clone();
        ctx.defun("eobp", move || -> Result<TulispObject, Error> {
            let sess = s.borrow();
            let at = sess.buffer.point() == sess.buffer.point_max();
            Ok(if at {
                TulispObject::t()
            } else {
                TulispObject::nil()
            })
        });
    }

    // ---- text objects & insertion ----
    {
        let s = session.clone();
        ctx.defun("forward-word", move |n: Option<i64>| -> i64 {
            let mut sess = s.borrow_mut();
            for _ in 0..n.unwrap_or(1).max(0) {
                let mut p = sess.buffer.point();
                let max = sess.buffer.point_max();
                while p < max
                    && !sess
                        .buffer
                        .char_after(p)
                        .is_some_and(|c| c.is_alphanumeric())
                {
                    p += 1;
                }
                while p < max
                    && sess
                        .buffer
                        .char_after(p)
                        .is_some_and(|c| c.is_alphanumeric())
                {
                    p += 1;
                }
                sess.buffer.goto_char(p);
            }
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("backward-word", move |n: Option<i64>| -> i64 {
            let mut sess = s.borrow_mut();
            for _ in 0..n.unwrap_or(1).max(0) {
                let mut p = sess.buffer.point();
                let min = sess.buffer.point_min();
                while p > min
                    && !sess
                        .buffer
                        .char_before(p)
                        .is_some_and(|c| c.is_alphanumeric())
                {
                    p -= 1;
                }
                while p > min
                    && sess
                        .buffer
                        .char_before(p)
                        .is_some_and(|c| c.is_alphanumeric())
                {
                    p -= 1;
                }
                sess.buffer.goto_char(p);
            }
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun(
            "insert-char",
            move |ch: i64, count: Option<i64>| -> Result<TulispObject, Error> {
                let c = char::from_u32(ch.max(0) as u32)
                    .ok_or_else(|| err("insert-char: invalid character code"))?;
                let n = count.unwrap_or(1).max(0) as usize;
                s.borrow_mut().buffer.insert(&c.to_string().repeat(n));
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "newline",
            move |n: Option<i64>| -> Result<TulispObject, Error> {
                let n = n.unwrap_or(1).max(0) as usize;
                s.borrow_mut().buffer.insert(&"\n".repeat(n));
                Ok(TulispObject::nil())
            },
        );
    }

    // ---- match data (from the store's most recent search) ----
    {
        let s = session.clone();
        ctx.defun(
            "match-string",
            move |n: i64, _string: Option<String>| -> Result<TulispObject, Error> {
                let text = s
                    .borrow()
                    .buffer
                    .last_match()
                    .and_then(|md| md.groups.get(n.max(0) as usize).cloned())
                    .flatten();
                Ok(match text {
                    Some(t) => TulispValue::from(t).into_ref(None),
                    None => TulispObject::nil(),
                })
            },
        );
    }

    // match-beginning / match-end: the bounds of the last search's whole match
    // (1-based char positions). Sub-group bounds are not tracked, so a non-zero
    // subexp argument yields nil.
    {
        let s = session.clone();
        ctx.defun(
            "match-beginning",
            move |n: Option<i64>| -> Result<TulispObject, Error> {
                let pos = match n.unwrap_or(0) {
                    0 => s.borrow().buffer.last_match().map(|md| md.start as i64),
                    _ => None,
                };
                Ok(match pos {
                    Some(p) => TulispValue::from(p).into_ref(None),
                    None => TulispObject::nil(),
                })
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "match-end",
            move |n: Option<i64>| -> Result<TulispObject, Error> {
                let pos = match n.unwrap_or(0) {
                    0 => s.borrow().buffer.last_match().map(|md| md.end as i64),
                    _ => None,
                };
                Ok(match pos {
                    Some(p) => TulispValue::from(p).into_ref(None),
                    None => TulispObject::nil(),
                })
            },
        );
    }

    // ---- buffer-level replace commands (map-shaped bulk edits) ----
    // Both are single-pass streaming rewrites: the window [point, point-max)
    // is materialized ONCE, every match located in it, and the edits applied
    // in document order with a running offset (`apply_window_edits`) —
    // O(window + matches) instead of the search loop's O(window²)
    // re-materialization per match on a large Quire. Replacement text is
    // never re-matched, point lands after the last replacement (unchanged
    // when nothing matches), and markers adjust per edit exactly as the
    // search-and-replace loop did.
    {
        let s = session.clone();
        ctx.defun(
            "replace-regexp",
            move |re: String, template: String| -> Result<TulispObject, Error> {
                let rx = cached_regex(&re)?;
                let mut sess = s.borrow_mut();
                let from = sess.buffer.point();
                let to = sess.buffer.point_max();
                // One char of context before point keeps `^`/`\b` honest at
                // the window edge, as in the store searches — but never from
                // before point-min, which counts as a real line beginning.
                let ctx_from = from
                    .saturating_sub(1)
                    .max(sess.buffer.point_min().min(from));
                let window = sess.buffer.substring(ctx_from, to);
                let skip = if ctx_from < from {
                    window.chars().next().map_or(0, char::len_utf8)
                } else {
                    0
                };
                // Locate every match left to right, expanding its replacement
                // from its own groups; an empty match steps one char forward.
                // A template with no backslash never reads its groups — skip
                // materializing them (bulk replaces can have 100k+ matches).
                let expands = template.contains('\\');
                let mut edits: Vec<(usize, usize, String)> = Vec::new();
                let mut at = skip;
                while let Some(caps) = rx.captures_at(&window, at) {
                    let whole = caps.get(0).expect("group 0 is the whole match");
                    let expanded = if expands {
                        let groups: Vec<Option<String>> = caps
                            .iter()
                            .map(|g| g.map(|m| m.as_str().to_string()))
                            .collect();
                        crate::buffer::expand_backrefs(&template, &groups)
                    } else {
                        template.clone()
                    };
                    edits.push((whole.start(), whole.end(), expanded));
                    at = if whole.end() > whole.start() {
                        whole.end()
                    } else {
                        match window[whole.end()..].chars().next() {
                            Some(c) => whole.end() + c.len_utf8(),
                            None => break,
                        }
                    };
                }
                Ok(TulispObject::from(apply_window_edits(
                    &mut sess, ctx_from, &window, edits,
                )))
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "replace-string",
            move |needle: String, to: String| -> Result<TulispObject, Error> {
                if needle.is_empty() {
                    return Err(err("replace-string: empty search string"));
                }
                let mut sess = s.borrow_mut();
                let from = sess.buffer.point();
                let pmax = sess.buffer.point_max();
                let window = sess.buffer.substring(from, pmax);
                let edits: Vec<(usize, usize, String)> = window
                    .match_indices(needle.as_str())
                    .map(|(b, m)| (b, b + m.len(), to.clone()))
                    .collect();
                Ok(TulispObject::from(apply_window_edits(
                    &mut sess, from, &window, edits,
                )))
            },
        );
    }

    // ---- line positions & columns ----
    {
        let s = session.clone();
        ctx.defun("line-beginning-position", move || -> i64 {
            let mut sess = s.borrow_mut();
            let saved = sess.buffer.point();
            sess.buffer.beginning_of_line();
            let p = sess.buffer.point();
            sess.buffer.goto_char(saved);
            p as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("line-end-position", move || -> i64 {
            let mut sess = s.borrow_mut();
            let saved = sess.buffer.point();
            sess.buffer.end_of_line();
            let p = sess.buffer.point();
            sess.buffer.goto_char(saved);
            p as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("current-column", move || -> i64 {
            let mut sess = s.borrow_mut();
            let saved = sess.buffer.point();
            sess.buffer.beginning_of_line();
            let bol = sess.buffer.point();
            sess.buffer.goto_char(saved);
            (saved - bol) as i64
        });
    }
    {
        let s = session.clone();
        ctx.defun("goto-line", move |n: i64| -> i64 {
            let mut sess = s.borrow_mut();
            let min = sess.buffer.point_min();
            sess.buffer.goto_char(min);
            sess.buffer.forward_line((n - 1).max(0));
            sess.buffer.point() as i64
        });
    }

    // ---- transactions (atomic edits: roll the workspace back on error) ----
    {
        let s = session.clone();
        ctx.defspecial("with-transaction", move |ctx, args| {
            let snapshot = {
                let sess = s.borrow();
                sess.buffer.snapshot()
            };
            let res = ctx.eval_progn(args);
            if res.is_err() {
                s.borrow_mut().buffer = snapshot;
            }
            res
        });
    }

    // ---- regexp-quote: escape regex metacharacters so a string matches
    // literally (Emacs `regexp-quote`; escapes for the RE2 engine that
    // `re-search-forward` uses). Lets fuzzy matching be built in elisp. ----
    {
        ctx.defun("regexp-quote", |s: String| -> String { regex::escape(&s) });
    }

    // (float-time) — seconds since the epoch as a float (Emacs `float-time`),
    // for coarse in-script profiling.
    {
        ctx.defun("float-time", |_t: Option<TulispObject>| -> f64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
    }

    // ---- region case + match counting ----
    {
        let s = session.clone();
        ctx.defun(
            "upcase-region",
            move |a: i64, b: i64| -> Result<TulispObject, Error> {
                let (lo, hi) = (a.min(b).max(1) as usize, a.max(b).max(1) as usize);
                let mut sess = s.borrow_mut();
                let text = sess.buffer.substring(lo, hi).to_uppercase();
                sess.buffer.delete_region(lo, hi);
                sess.buffer.goto_char(lo);
                sess.buffer.insert(&text);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "downcase-region",
            move |a: i64, b: i64| -> Result<TulispObject, Error> {
                let (lo, hi) = (a.min(b).max(1) as usize, a.max(b).max(1) as usize);
                let mut sess = s.borrow_mut();
                let text = sess.buffer.substring(lo, hi).to_lowercase();
                sess.buffer.delete_region(lo, hi);
                sess.buffer.goto_char(lo);
                sess.buffer.insert(&text);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "count-matches",
            move |re: String, start: Option<i64>, end: Option<i64>| -> Result<i64, Error> {
                let rx = cached_regex(&re)?;
                let mut sess = s.borrow_mut();
                let saved = sess.buffer.point();
                if let Some(st) = start {
                    sess.buffer.goto_char(st.max(1) as usize);
                }
                let bound = end.map(|e| e.max(1) as usize);
                let mut count = 0i64;
                while let Some(end) = sess.buffer.re_search_forward(&rx, bound) {
                    count += 1;
                    let start = sess.buffer.last_match().map_or(end, |m| m.start);
                    // A zero-width match (the line anchors) leaves point in
                    // place — step over it so each empty match counts exactly
                    // once and the scan continues: the same stepping occur
                    // and the streaming replace use.
                    if end == start {
                        // Clamp the step limit into the accessible region: an
                        // END beyond point-max would otherwise never satisfy
                        // `end >= limit` while goto_char clamps the step back
                        // — an infinite loop on `(count-matches "$" 1 BIG)`.
                        let pmax = sess.buffer.point_max();
                        let limit = bound.map_or(pmax, |b| b.min(pmax));
                        if end >= limit {
                            break;
                        }
                        sess.buffer.goto_char(end + 1);
                    }
                }
                sess.buffer.goto_char(saved);
                Ok(count)
            },
        );
    }

    // ---- more line commands ----
    {
        let s = session.clone();
        ctx.defun("kill-line", move || -> Result<TulispObject, Error> {
            let mut sess = s.borrow_mut();
            let p = sess.buffer.point();
            sess.buffer.end_of_line();
            let eol = sess.buffer.point();
            sess.buffer.goto_char(p);
            // Kill to end of line; if already there, kill the newline (like Emacs).
            let end = if p == eol && p < sess.buffer.point_max() {
                p + 1
            } else {
                eol
            };
            let text = sess.buffer.substring(p, end);
            sess.kill_ring.push(text);
            sess.buffer.delete_region(p, end);
            Ok(TulispObject::nil())
        });
    }
    {
        // (kill-whole-line) — kill the entire line point is on, newline
        // included, onto the kill-ring; point lands at the line's former
        // start.
        let s = session.clone();
        ctx.defun("kill-whole-line", move || -> Result<TulispObject, Error> {
            let mut sess = s.borrow_mut();
            sess.buffer.beginning_of_line();
            let start = sess.buffer.point();
            sess.buffer.end_of_line();
            let eol = sess.buffer.point();
            let end = if eol < sess.buffer.point_max() {
                eol + 1 // take the newline too
            } else {
                eol
            };
            let text = sess.buffer.substring(start, end);
            sess.kill_ring.push(text);
            sess.buffer.delete_region(start, end);
            let landing = start.min(sess.buffer.point_max());
            sess.buffer.goto_char(landing);
            Ok(TulispObject::nil())
        });
    }
    {
        // (keep-lines REGEXP) — delete every line from the start of point's
        // line to point-max that does NOT match; returns the count deleted.
        // Emacs's classic bulk filter, here as one region rewrite.
        let s = session.clone();
        ctx.defun("keep-lines", move |re: String| -> Result<i64, Error> {
            let rx = cached_regex(&re)?;
            filter_lines(&s, &rx, true)
        });
    }
    {
        // (flush-lines REGEXP) — the inverse: delete every matching line.
        let s = session.clone();
        ctx.defun("flush-lines", move |re: String| -> Result<i64, Error> {
            let rx = cached_regex(&re)?;
            filter_lines(&s, &rx, false)
        });
    }
    {
        // (sort-lines &optional REVERSE BEG END) — sort the lines covering
        // [BEG, END) (defaults: the whole accessible region) lexicographically;
        // REVERSE non-nil sorts descending. Whole lines are reordered; a
        // region cut mid-line is widened to line boundaries first.
        let s = session.clone();
        ctx.defun(
            "sort-lines",
            move |reverse: Option<TulispObject>,
                  beg: Option<i64>,
                  end: Option<i64>|
                  -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let beg = beg.map_or_else(|| sess.buffer.point_min(), |b| b.max(1) as usize);
                let end = end.map_or_else(|| sess.buffer.point_max(), |e| e.max(1) as usize);
                // Widen to line boundaries.
                sess.buffer.goto_char(beg);
                sess.buffer.beginning_of_line();
                let start = sess.buffer.point();
                sess.buffer.goto_char(end.max(start));
                if sess.buffer.point() > sess.buffer.point_min()
                    && sess.buffer.char_before(sess.buffer.point()) != Some('\n')
                {
                    sess.buffer.end_of_line();
                }
                let stop = sess.buffer.point();
                let region = sess.buffer.substring(start, stop);
                let trailing_newline = region.ends_with('\n');
                let mut lines: Vec<&str> = region.split('\n').collect();
                if trailing_newline {
                    lines.pop(); // the empty tail after the final newline
                }
                lines.sort();
                if reverse.is_some_and(|r| r.is_truthy()) {
                    lines.reverse();
                }
                let mut sorted = lines.join("\n");
                if trailing_newline {
                    sorted.push('\n');
                }
                if sorted != region {
                    sess.buffer.delete_region(start, stop);
                    sess.buffer.goto_char(start);
                    sess.buffer.insert(&sorted);
                }
                sess.buffer.goto_char(start);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        // (back-to-indentation) — point to the first non-whitespace char of
        // the current line (or its end if blank); returns the new point.
        let s = session.clone();
        ctx.defun("back-to-indentation", move || -> i64 {
            let mut sess = s.borrow_mut();
            sess.buffer.beginning_of_line();
            while matches!(
                sess.buffer.char_after(sess.buffer.point()),
                Some(' ') | Some('\t')
            ) {
                let p = sess.buffer.point();
                sess.buffer.goto_char(p + 1);
            }
            sess.buffer.point() as i64
        });
    }
    {
        // (current-indentation) — the column of the current line's first
        // non-whitespace char (tabs count 1), without moving point.
        let s = session.clone();
        ctx.defun("current-indentation", move || -> i64 {
            let mut sess = s.borrow_mut();
            let p0 = sess.buffer.point();
            sess.buffer.beginning_of_line();
            let mut n = 0i64;
            while matches!(
                sess.buffer.char_after(sess.buffer.point()),
                Some(' ') | Some('\t')
            ) {
                let p = sess.buffer.point();
                sess.buffer.goto_char(p + 1);
                n += 1;
            }
            sess.buffer.goto_char(p0);
            n
        });
    }
    {
        // (forward-paragraph) — move past the current paragraph to the next
        // blank-line boundary (or point-max); returns the new point.
        let s = session.clone();
        ctx.defun("forward-paragraph", move || -> Result<i64, Error> {
            let rx = cached_regex("\n[ \t]*\n")?;
            let mut sess = s.borrow_mut();
            match sess.buffer.re_search_forward(&rx, None) {
                Some(_) => {
                    // Land on the blank line itself (after the first newline).
                    let p = sess.buffer.point();
                    sess.buffer.goto_char(p.saturating_sub(1).max(1));
                }
                None => {
                    let max = sess.buffer.point_max();
                    sess.buffer.goto_char(max);
                }
            }
            Ok(sess.buffer.point() as i64)
        });
    }
    {
        let s = session.clone();
        ctx.defun(
            "delete-trailing-whitespace",
            move || -> Result<TulispObject, Error> {
                let rx = regex::Regex::new(r"(?m)[ \t]+$").map_err(bad_regex)?;
                let mut sess = s.borrow_mut();
                let m = sess.buffer.point_min();
                sess.buffer.goto_char(m);
                loop {
                    let start = sess.buffer.point();
                    if sess.buffer.re_search_forward(&rx, None).is_none() {
                        break;
                    }
                    sess.buffer.replace_match("").map_err(Error::lisp_error)?;
                    if sess.buffer.point() <= start {
                        break;
                    }
                }
                Ok(TulispObject::nil())
            },
        );
    }

    // ---- checkpoints (workspace time travel — M0: full-text snapshots) ----
    {
        let s = session.clone();
        ctx.defun(
            "checkpoint",
            move |label: Option<String>| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let label = label.unwrap_or_else(|| format!("auto-{}", sess.checkpoints.len()));
                let cp = Checkpoint::capture(label.clone(), &*sess.buffer);
                sess.checkpoints.push(cp);
                Ok(TulispValue::from(label).into_ref(None))
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "restore-checkpoint",
            move |label: String| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let restored = sess
                    .checkpoints
                    .iter()
                    .rev()
                    .find(|c| c.label == label)
                    .map(|c| c.restore());
                match restored {
                    Some(store) => {
                        sess.buffer = store;
                        Ok(TulispObject::t())
                    }
                    None => Err(err(&format!("No checkpoint named {label}"))),
                }
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun("list-checkpoints", move || -> Vec<String> {
            s.borrow()
                .checkpoints
                .iter()
                .map(|c| c.label.clone())
                .collect()
        });
    }
    {
        let s = session.clone();
        ctx.defun(
            "checkpoint-diff",
            move |a: String, b: String| -> Result<String, Error> {
                let sess = s.borrow();
                let text = |label: &str| {
                    sess.checkpoints
                        .iter()
                        .rev()
                        .find(|c| c.label == label)
                        .map(|c| c.text())
                };
                let ta = text(&a).ok_or_else(|| err(&format!("No checkpoint named {a}")))?;
                let tb = text(&b).ok_or_else(|| err(&format!("No checkpoint named {b}")))?;
                Ok(crate::result::unified_diff(&ta, &tb))
            },
        );
    }

    // ---- merge conflicts (smerge-flavored; see src/conflict.rs) ----
    // Stateless: every command re-scans the accessible region, so hunk
    // numbers refresh after each edit (resolve top-down, or re-list). All
    // mutating commands return the REMAINING conflict count — the loop
    // condition and the "am I done" signal in one. Addressing: a 1-based
    // hunk index, or nil for the hunk containing point (smerge-keep-current).
    {
        // (conflict-count) — the number of well-formed conflict hunks.
        let s = session.clone();
        ctx.defun("conflict-count", move || -> i64 {
            crate::conflict::scan(s.borrow_mut().buffer.as_mut()).len() as i64
        });
    }
    {
        // (conflicts) — the hunks' start positions (1-based, goto-char-able),
        // in document order; nil when the buffer is clean. The structured
        // companion to conflict-count: count says how MANY, this says WHERE.
        // Each position lands INSIDE its hunk, so the at-point commands address
        // it directly (goto-char → conflict-context to inspect, conflict-keep to
        // resolve) without parsing the conflict-hunks overview text. The list is
        // a SNAPSHOT: resolving a hunk shifts every later position, so a resolve
        // loop must re-scan each pass — `(while (> (conflict-count) 0) (goto-char
        // (car (conflicts))) (conflict-keep …))` — or walk one snapshot bottom-up
        // (last hunk first), where the earlier positions stay valid. (The MCP
        // `conflicts` tool instead renders the human overview; for that text from
        // lisp use `(conflict-hunks)`.)
        let s = session.clone();
        ctx.defun("conflicts", move || -> Vec<i64> {
            crate::conflict::scan(s.borrow_mut().buffer.as_mut())
                .iter()
                .map(|h| h.start as i64)
                .collect()
        });
    }
    {
        // (conflict-hunks) — rendered overview: one line per hunk with its
        // number, char position + line, labels, and side sizes; appends a
        // warning when marker-shaped lines were left unparsed (malformed or
        // nested conflicts), so "no conflicts" is never silently wrong.
        let s = session.clone();
        ctx.defun("conflict-hunks", move || -> String {
            let mut sess = s.borrow_mut();
            let b = sess.buffer.as_mut();
            let (hunks, strays) = crate::conflict::scan_with_strays(b);
            crate::conflict::render(&*b, &hunks, strays)
        });
    }
    {
        // (conflict-goto &optional N) — move point to hunk N's start; with nil,
        // to the next conflict at or after point (smerge-next). NOTE the two
        // forms land at different spots: explicit N at the opener line's
        // start, nil just past the opener — *inside* the hunk, so the at-point
        // commands address it and the next call advances to the following
        // hunk (a hunk starting exactly at point, e.g. at point-min, is found
        // rather than skipped). Both positions are inside the hunk for
        // at-point addressing. Returns the new position, or nil when there is
        // no next conflict.
        let s = session.clone();
        ctx.defun(
            "conflict-goto",
            move |n: Option<i64>| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let b = sess.buffer.as_mut();
                let hunks = crate::conflict::scan(b);
                let target = match n {
                    Some(_) => Some(
                        crate::conflict::pick(&hunks, n, b.point())
                            .map_err(|e| err(&e))?
                            .start,
                    ),
                    None => {
                        let p = b.point();
                        hunks.iter().find(|h| h.start >= p).map(|h| h.ours.0)
                    }
                };
                if let Some(p) = target {
                    b.goto_char(p);
                }
                Ok(target.map(|p| p as i64).into())
            },
        );
    }
    {
        // (conflict-text SIDE &optional N) — the text of one side of hunk N
        // (or the hunk at point): "ours" | "theirs" | "base" (diff3 only), plus
        // the combinations "both" / "all" (what conflict-keep would insert).
        let s = session.clone();
        ctx.defun(
            "conflict-text",
            move |side: String, n: Option<i64>| -> Result<String, Error> {
                let mut sess = s.borrow_mut();
                let b = sess.buffer.as_mut();
                let hunks = crate::conflict::scan(b);
                let h = crate::conflict::pick(&hunks, n, b.point()).map_err(|e| err(&e))?;
                crate::conflict::side_text(&*b, h, &side).map_err(|e| err(&e))
            },
        );
    }
    {
        // (conflict-diff &optional N) — unified diff ours → theirs for hunk N
        // (or the hunk at point): what actually differs, without reading both
        // sides in full (smerge-refine's idea, token-lean). "" = identical.
        let s = session.clone();
        ctx.defun(
            "conflict-diff",
            move |n: Option<i64>| -> Result<String, Error> {
                let mut sess = s.borrow_mut();
                let b = sess.buffer.as_mut();
                let hunks = crate::conflict::scan(b);
                let h = crate::conflict::pick(&hunks, n, b.point()).map_err(|e| err(&e))?;
                let ours = b.substring(h.ours.0, h.ours.1);
                let theirs = b.substring(h.theirs.0, h.theirs.1);
                Ok(crate::result::unified_diff(&ours, &theirs))
            },
        );
    }
    {
        // (conflict-context &optional N LINES) — the decision view in one
        // call: hunk N (or the hunk at point) rendered in place with LINES
        // (default 3) lines of surrounding code, view-style gutter marking
        // the hunk's own lines. Deciding a resolution usually needs the code
        // AROUND the hunk; this replaces the conflict-hunks → parse @pos →
        // view round-trip. Read-only: point is preserved.
        let s = session.clone();
        ctx.defun(
            "conflict-context",
            move |n: Option<i64>, lines: Option<i64>| -> Result<String, Error> {
                let mut sess = s.borrow_mut();
                let b = sess.buffer.as_mut();
                let hunks = crate::conflict::scan(b);
                let h = crate::conflict::pick(&hunks, n, b.point())
                    .map_err(|e| err(&e))?
                    .clone();
                let idx = hunks.iter().position(|x| x.start == h.start).unwrap_or(0) + 1;
                let ctx_lines = lines.unwrap_or(3).max(0) as usize;
                let saved = b.point();
                let pmin = b.point_min();
                let min_line = b.line_number_at_pos(pmin);
                let max_line = b.line_number_at_pos(b.point_max());
                let hunk_lo = b.line_number_at_pos(h.start);
                // h.end sits just past the hunk; the char before it is on the
                // closer's line.
                let hunk_hi = b.line_number_at_pos(h.end.saturating_sub(1).max(h.start));
                let lo = hunk_lo.saturating_sub(ctx_lines).max(min_line);
                let hi = (hunk_hi + ctx_lines).min(max_line);
                let mut out = format!(
                    "\u{2014} conflict {idx}/{} @{} ({} \u{2194} {}) \u{2014}\n",
                    hunks.len(),
                    h.start,
                    h.ours_label,
                    h.theirs_label,
                );
                b.goto_char(pmin);
                b.forward_line((lo - min_line) as i64);
                for line_no in lo..=hi {
                    let start = b.point();
                    b.end_of_line();
                    let end = b.point();
                    let text = b.substring(start, end);
                    let gutter = if (hunk_lo..=hunk_hi).contains(&line_no) {
                        '>'
                    } else {
                        ' '
                    };
                    out.push_str(&format!("{line_no:>5} {gutter} {text}\n"));
                    if line_no < hi {
                        b.goto_char(end);
                        b.forward_line(1);
                    }
                }
                b.goto_char(saved);
                Ok(out)
            },
        );
    }
    {
        // (conflict-keep SIDE &optional N) — resolve hunk N (or the hunk at
        // point) by replacing the whole hunk with SIDE: "ours" | "theirs" |
        // "base" | "both" (ours then theirs) | "all" (ours, base, theirs —
        // smerge-keep-all). Returns the remaining conflict count.
        let s = session.clone();
        ctx.defun(
            "conflict-keep",
            move |side: String, n: Option<i64>| -> Result<i64, Error> {
                conflict_splice(&s, n, |b, h| {
                    crate::conflict::side_text_with_warning(b, h, &side).map_err(|e| err(&e))
                })
            },
        );
    }
    {
        // (conflict-keep-all SIDE) — resolve EVERY remaining hunk by keeping
        // SIDE ("ours" | "theirs" | "both" | "all" | "base"): one call instead
        // of looping conflict-keep by hand (which renumbers as it goes). Side
        // text is resolved for all hunks FIRST, so an unavailable side errors
        // before any edit (all-or-nothing); the edits then run bottom-up so
        // earlier spans stay valid. Returns the remaining count (0 on success).
        let s = session.clone();
        ctx.defun(
            "conflict-keep-all",
            move |side: String| -> Result<i64, Error> {
                let mut sess = s.borrow_mut();
                let (remaining, warnings) = {
                    let b = sess.buffer.as_mut();
                    let hunks = crate::conflict::scan(b);
                    let mut warnings = Vec::new();
                    let mut plan = Vec::with_capacity(hunks.len());
                    for h in &hunks {
                        let (text, warning) =
                            crate::conflict::side_text_with_warning(&*b, h, &side)
                                .map_err(|e| err(&e))?;
                        if let Some(w) = warning {
                            warnings.push(w);
                        }
                        plan.push((h.start, h.end, text));
                    }
                    (conflict_splice_all(b, plan), warnings)
                };
                for w in warnings {
                    sess.log.push(w);
                }
                Ok(remaining)
            },
        );
    }
    {
        // (conflict-replace TEXT &optional N) — resolve hunk N (or the hunk at
        // point) by splicing TEXT verbatim in place of the whole hunk — the
        // hand-crafted resolution. Returns the remaining conflict count.
        let s = session.clone();
        ctx.defun(
            "conflict-replace",
            move |text: String, n: Option<i64>| -> Result<i64, Error> {
                conflict_splice(&s, n, |_, _| Ok((text.clone(), None)))
            },
        );
    }
    {
        // (conflict-resolve-trivial) — sweep every hunk and resolve the safe
        // cases (the conservative core of smerge-resolve): sides identical →
        // either; ours == base → theirs; theirs == base → ours. Edits run
        // bottom-up so earlier spans stay valid. Returns the remaining count.
        let s = session.clone();
        ctx.defun("conflict-resolve-trivial", move || -> i64 {
            let mut sess = s.borrow_mut();
            let b = sess.buffer.as_mut();
            let hunks = crate::conflict::scan(b);
            let mut plan = Vec::new();
            for h in &hunks {
                let ours = b.substring(h.ours.0, h.ours.1);
                let theirs = b.substring(h.theirs.0, h.theirs.1);
                let base = h.base.map(|(s, e)| b.substring(s, e));
                let keep = if ours == theirs {
                    Some(ours)
                } else if base.as_deref() == Some(ours.as_str()) {
                    Some(theirs)
                } else if base.as_deref() == Some(theirs.as_str()) {
                    Some(ours)
                } else {
                    None
                };
                if let Some(text) = keep {
                    plan.push((h.start, h.end, text));
                }
            }
            conflict_splice_all(b, plan)
        });
    }

    // ---- structural / AST-aware editing (M7, tree-sitter) ----
    // Markdown, Rust, and Python; the language comes from the buffer name's
    // extension (`Lang::from_buffer_name`), overridable per buffer with
    // `treesit-set-language`, falling back to Markdown (`syntax_of`). The
    // parse persists on the Session keyed by content version — a run of
    // treesit calls parses once, an edit re-parses on the next call (full
    // re-parse; incremental InputEdits are a TODO in syntax.rs). Node spans
    // are reported in 1-based char positions, like the rest of the builtins,
    // and are WHOLE-DOCUMENT positions by design: the structural layer reads
    // the full document regardless of narrowing (the one deliberate exception
    // to narrowing composition — a restriction that cuts a function in half
    // must not change what the tree says the function is). Motion still can't
    // escape: goto_char clamps into the accessible region. A "defun" is the
    // language's enclosing construct: a Markdown `section`, a Rust
    // `function_item`/`impl_item`/type item, a Python
    // `function_definition`/`class_definition`.
    {
        let s = session.clone();
        // (treesit-language) — report and return the language the current
        // buffer parses as ("markdown" / "rust" / "python").
        ctx.defun("treesit-language", move || -> String {
            let mut sess = s.borrow_mut();
            let name = lang_of(&sess).name().to_string();
            sess.reports
                .push(("treesit-language".to_string(), name.clone()));
            name
        });
    }
    {
        let s = session.clone();
        // (treesit-set-language LANG) — override the current buffer's language
        // (a name or extension: "rust"/"rs", "python"/"py", "markdown"/"md").
        // For buffers whose name carries no extension — stdin pipes, scratch
        // buffers. Returns the canonical name; errors on an unknown language.
        ctx.defun(
            "treesit-set-language",
            move |token: String| -> Result<String, Error> {
                let lang = Lang::from_token(&token)
                    .ok_or_else(|| err(&format!("Unknown treesit language: {token}")))?;
                let mut sess = s.borrow_mut();
                let name = sess.buffer.name().to_string();
                if let Some(slot) = sess.lang_overrides.iter_mut().find(|(n, _)| *n == name) {
                    slot.1 = lang;
                } else {
                    sess.lang_overrides.push((name, lang));
                }
                Ok(lang.name().to_string())
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-root-type) — report and return the parse-tree root kind
        // ("document" / "source_file" / "module"). Proves the buffer parses.
        ctx.defun("treesit-root-type", move || -> String {
            let mut sess = s.borrow_mut();
            let kind = syntax_of(&mut sess).root_kind();
            sess.reports
                .push(("treesit-root-type".to_string(), kind.clone()));
            kind
        });
    }
    {
        let s = session.clone();
        // (treesit-has-error) — t if the buffer fails to parse cleanly for its
        // language (any ERROR/missing node). The cheap "did my edit break the
        // syntax?" check an agent runs after editing code.
        ctx.defun("treesit-has-error", move || -> bool {
            let mut sess = s.borrow_mut();
            let broken = syntax_of(&mut sess).has_error();
            sess.reports
                .push(("treesit-has-error".to_string(), broken.to_string()));
            broken
        });
    }
    {
        let s = session.clone();
        // (treesit-node-at &optional POS) — the smallest NAMED node covering POS
        // (default point), as a first-class node value (nil for an empty
        // tree). Reports its type and 1-based char start/end too. Feed the
        // value to the treesit-node-* family: parent / child / siblings /
        // child-by-field-name / type / start / end / text.
        ctx.defun(
            "treesit-node-at",
            move |pos: Option<i64>| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
                let version = sess.buffer.version();
                let syn = syntax_of(&mut sess);
                Ok(match syn.node_at(p) {
                    Some(h) => {
                        let node = TsNode::new(&syn, version, h);
                        let span = node.described()?;
                        sess.reports
                            .push(("treesit-node-type".to_string(), span.kind));
                        sess.reports
                            .push(("treesit-node-start".to_string(), span.start.to_string()));
                        sess.reports
                            .push(("treesit-node-end".to_string(), span.end.to_string()));
                        node.into_tulisp()
                    }
                    None => TulispObject::nil(),
                })
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-defun-at &optional POS) — the nearest enclosing defun at
        // POS (default point) as a node value; nil if POS is inside no defun.
        // The node-valued sibling of treesit-beginning-of-defun.
        ctx.defun(
            "treesit-defun-at",
            move |pos: Option<i64>| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
                let version = sess.buffer.version();
                let syn = syntax_of(&mut sess);
                Ok(syn
                    .defun_at(p)
                    .map(|h| TsNode::new(&syn, version, h))
                    .into_tulisp_opt())
            },
        );
    }
    // ---- the treesit-node-* family: relational navigation over node values.
    // Every accessor checks the node is CURRENT (see live_node); navigation
    // results are node values in the same parse, nil where the tree ends.
    // NAMED (where accepted, default nil) restricts to named nodes, skipping
    // anonymous tokens like punctuation — Emacs's signatures.
    {
        let s = session.clone();
        // (treesit-node-type NODE) — the node's kind ("function_item", …).
        ctx.defun(
            "treesit-node-type",
            move |n: TsNode| -> Result<String, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.described()?.kind)
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-start NODE) — 1-based char position (whole-document).
        ctx.defun(
            "treesit-node-start",
            move |n: TsNode| -> Result<i64, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.described()?.start as i64)
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-end NODE) — position just past the node, Emacs-style.
        ctx.defun("treesit-node-end", move |n: TsNode| -> Result<i64, Error> {
            live_node(&s.borrow(), &n)?;
            Ok(n.described()?.end as i64)
        });
    }
    {
        let s = session.clone();
        // (treesit-node-text NODE) — the node's source text.
        ctx.defun(
            "treesit-node-text",
            move |n: TsNode| -> Result<String, Error> {
                live_node(&s.borrow(), &n)?;
                n.syn
                    .text_of_handle(n.h)
                    .ok_or_else(|| err("internal: node could not be relocated in its parse"))
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-parent NODE) — the parent node, nil at the root.
        ctx.defun(
            "treesit-node-parent",
            move |n: TsNode| -> Result<TulispObject, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.syn.parent_of(n.h).map(|h| n.derive(h)).into_tulisp_opt())
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-child NODE I &optional NAMED) — the I-th (0-based)
        // child; a negative I counts from the end (-1 = last), Emacs-style.
        // NAMED counts named children only.
        ctx.defun(
            "treesit-node-child",
            move |n: TsNode, i: i64, named: Option<TulispObject>| -> Result<TulispObject, Error> {
                live_node(&s.borrow(), &n)?;
                let named = truthy(&named);
                let i = if i < 0 {
                    let count = n.syn.child_count_of(n.h, named).unwrap_or(0) as i64;
                    if count + i < 0 {
                        return Ok(TulispObject::nil());
                    }
                    (count + i) as usize
                } else {
                    i as usize
                };
                Ok(n.syn
                    .child_of(n.h, i, named)
                    .map(|h| n.derive(h))
                    .into_tulisp_opt())
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-child-count NODE &optional NAMED).
        ctx.defun(
            "treesit-node-child-count",
            move |n: TsNode, named: Option<TulispObject>| -> Result<i64, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.syn.child_count_of(n.h, truthy(&named)).unwrap_or(0) as i64)
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-next-sibling NODE &optional NAMED).
        ctx.defun(
            "treesit-node-next-sibling",
            move |n: TsNode, named: Option<TulispObject>| -> Result<TulispObject, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.syn
                    .next_sibling_of(n.h, truthy(&named))
                    .map(|h| n.derive(h))
                    .into_tulisp_opt())
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-prev-sibling NODE &optional NAMED).
        ctx.defun(
            "treesit-node-prev-sibling",
            move |n: TsNode, named: Option<TulispObject>| -> Result<TulispObject, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.syn
                    .prev_sibling_of(n.h, truthy(&named))
                    .map(|h| n.derive(h))
                    .into_tulisp_opt())
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-node-child-by-field-name NODE FIELD) — the child filling a
        // grammar field ("name", "body", "parameters", …); nil if unfilled.
        ctx.defun(
            "treesit-node-child-by-field-name",
            move |n: TsNode, field: String| -> Result<TulispObject, Error> {
                live_node(&s.borrow(), &n)?;
                Ok(n.syn
                    .child_by_field_of(n.h, &field)
                    .map(|h| n.derive(h))
                    .into_tulisp_opt())
            },
        );
    }
    {
        // (treesit-node-p X) — t iff X is a tree-sitter node value.
        ctx.defun("treesit-node-p", move |v: TulispObject| -> bool {
            TsNode::from_tulisp(&v).is_ok()
        });
    }
    // ---- node-EDIT ops: splice at a node's span, return the re-parsed result.
    // Thin wrappers over the store's delete/insert that take a NODE from the
    // CURRENT buffer (live_current_node), edit at its span, then re-parse so the
    // replacement node comes back for chaining. A node from another buffer or an
    // outdated parse is refused — its span would address the wrong text.
    {
        let s = session.clone();
        // (treesit-replace-node NODE TEXT) — replace NODE's whole span with the
        // literal TEXT. Returns the re-parsed node now at the replacement start.
        ctx.defun(
            "treesit-replace-node",
            move |n: TsNode, text: String| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let span = edit_span(&sess, &n)?;
                Ok(splice_and_reparse(&mut sess, span.start, span.end, &text))
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-wrap-node NODE BEFORE AFTER) — insert BEFORE at NODE's start
        // and AFTER at its end, wrapping it (e.g. an expr in `Some(` … `)`).
        // Returns the re-parsed node at the wrapped region's start.
        ctx.defun(
            "treesit-wrap-node",
            move |n: TsNode, before: String, after: String| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let span = edit_span(&sess, &n)?;
                {
                    // AFTER first (the end insert leaves the start untouched),
                    // then BEFORE at the start.
                    let b = sess.buffer.as_mut();
                    b.goto_char(span.end);
                    b.insert(&after);
                    b.goto_char(span.start);
                    b.insert(&before);
                }
                Ok(reparse_at(&mut sess, span.start))
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-raise-node NODE) — replace NODE's PARENT with NODE (paredit
        // raise-sexp): the node's text takes the parent's place, DELETING the
        // node's siblings. Errors at the root (no parent). Returns the raised node.
        ctx.defun(
            "treesit-raise-node",
            move |n: TsNode| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                live_current_node(&sess, &n)?;
                let parent = n.syn.parent_of(n.h).ok_or_else(|| {
                    err("treesit-raise-node: the node has no parent (it is the root)")
                })?;
                let pspan = n
                    .syn
                    .describe(parent)
                    .ok_or_else(|| err("internal: parent could not be relocated"))?;
                within_region(&sess, &pspan)?;
                let text = n
                    .syn
                    .text_of_handle(n.h)
                    .ok_or_else(|| err("internal: node could not be relocated"))?;
                Ok(splice_and_reparse(&mut sess, pspan.start, pspan.end, &text))
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-kill-node NODE) — delete NODE's span, pushing its text to the
        // kill-ring (yank-able). Point lands at the deletion start; returns nil.
        ctx.defun(
            "treesit-kill-node",
            move |n: TsNode| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let span = edit_span(&sess, &n)?;
                let text = n
                    .syn
                    .text_of_handle(n.h)
                    .ok_or_else(|| err("internal: node could not be relocated"))?;
                sess.kill_ring.push(text);
                let b = sess.buffer.as_mut();
                b.delete_region(span.start, span.end);
                b.goto_char(span.start);
                Ok(TulispObject::nil())
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-insert-sibling NODE TEXT &optional BEFORE) — insert TEXT just
        // after NODE (or before it when BEFORE is non-nil), as an adjacent
        // sibling. Returns the re-parsed node at the insertion point.
        ctx.defun(
            "treesit-insert-sibling",
            move |n: TsNode,
                  text: String,
                  before: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let span = edit_span(&sess, &n)?;
                let pos = if truthy(&before) {
                    span.start
                } else {
                    span.end
                };
                {
                    let b = sess.buffer.as_mut();
                    b.goto_char(pos);
                    b.insert(&text);
                }
                Ok(reparse_at(&mut sess, pos))
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-beginning-of-defun) — move point to the start of the
        // enclosing defun and return the new point. If point is in no defun,
        // leave it put and return it unchanged.
        ctx.defun("treesit-beginning-of-defun", move || -> i64 {
            let mut sess = s.borrow_mut();
            let p = sess.buffer.point();
            if let Some(d) = syntax_of(&mut sess).enclosing_defun(p) {
                sess.buffer.goto_char(d.start);
            }
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        // (treesit-end-of-defun) — move point to the end of the enclosing
        // defun and return the new point. If point is in no defun, leave it
        // put and return it unchanged.
        ctx.defun("treesit-end-of-defun", move || -> i64 {
            let mut sess = s.borrow_mut();
            let p = sess.buffer.point();
            if let Some(d) = syntax_of(&mut sess).enclosing_defun(p) {
                sess.buffer.goto_char(d.end);
            }
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        // (treesit-defun-name &optional POS) — the name of the enclosing defun
        // at POS (default point): function/class/type name, Markdown heading
        // text. Reports and returns it; nil if no enclosing defun or anonymous.
        ctx.defun(
            "treesit-defun-name",
            move |pos: Option<i64>| -> TulispObject {
                let mut sess = s.borrow_mut();
                let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
                match syntax_of(&mut sess).enclosing_defun_name(p) {
                    Some(name) => {
                        sess.reports
                            .push(("treesit-defun-name".to_string(), name.clone()));
                        TulispValue::from(name).into_ref(None)
                    }
                    None => TulispObject::nil(),
                }
            },
        );
    }
    {
        let s = session.clone();
        // (treesit-narrow-to-defun &optional POS) — narrow the buffer to the
        // enclosing defun at POS (default point), scoping every subsequent
        // edit/search to that one function/class/section (compose with
        // save-restriction / widen). Returns t, or nil (no narrowing) if POS
        // is inside no defun. REPLACES any existing restriction, like Emacs's
        // narrowing commands — the defun is found in the whole document (see
        // the section comment), so this can deliberately re-narrow outside
        // the current restriction.
        ctx.defun("treesit-narrow-to-defun", move |pos: Option<i64>| -> bool {
            let mut sess = s.borrow_mut();
            let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
            match syntax_of(&mut sess).enclosing_defun(p) {
                Some(d) => {
                    sess.buffer.narrow_to_region(d.start, d.end);
                    true
                }
                None => false,
            }
        });
    }
    {
        let s = session.clone();
        // (treesit-list-defuns) — the buffer outline: report every defun in
        // document order (nested ones included) as "KIND START END NAME" and
        // return the list of names. How an agent surveys a source file
        // without reading it whole.
        ctx.defun("treesit-list-defuns", move || -> Vec<String> {
            let mut sess = s.borrow_mut();
            let defuns = syntax_of(&mut sess).defuns();
            let mut names = Vec::with_capacity(defuns.len());
            for d in defuns {
                sess.reports.push((
                    "defun".to_string(),
                    format!("{} {} {} {}", d.kind, d.start, d.end, d.name),
                ));
                names.push(d.name);
            }
            names
        });
    }
    {
        let s = session.clone();
        // (treesit-goto-defun NAME) — move point to the start of the first
        // defun (document order) named NAME — "go to fn parse_args" without
        // knowing where it is. Reports its span and returns the new point;
        // nil (point unmoved) if no defun has that name.
        ctx.defun("treesit-goto-defun", move |name: String| -> TulispObject {
            let mut sess = s.borrow_mut();
            match syntax_of(&mut sess).find_defun(&name) {
                Some(d) => {
                    sess.reports.push((
                        "treesit-defun".to_string(),
                        format!("{} {} {} {}", d.kind, d.start, d.end, d.name),
                    ));
                    sess.buffer.goto_char(d.start);
                    TulispValue::from(sess.buffer.point() as i64).into_ref(None)
                }
                None => TulispObject::nil(),
            }
        });
    }
    {
        let s = session.clone();
        // (treesit-query PATTERN) — run a tree-sitter query (.scm pattern
        // syntax) over the buffer: structural search ("every call to foo",
        // "all pub fns") instead of regex. Reports each capture as
        // "@CAPTURE KIND START END" and returns the captures as a list of
        // first-class NODE values (feed treesit-node-text / -start / the
        // navigation family; the report rows carry the capture names).
        // Errors if the pattern does not compile for the buffer's language.
        ctx.defun(
            "treesit-query",
            move |pattern: String| -> Result<Vec<TsNode>, Error> {
                let mut sess = s.borrow_mut();
                let version = sess.buffer.version();
                let syn = syntax_of(&mut sess);
                let caps = syn
                    .query(&pattern)
                    .map_err(|e| err(&format!("treesit-query: {e}")))?;
                let mut nodes = Vec::with_capacity(caps.len());
                for (name, h) in caps {
                    let node = TsNode::new(&syn, version, h);
                    let span = node.described()?;
                    sess.reports.push((
                        "capture".to_string(),
                        format!("@{name} {} {} {}", span.kind, span.start, span.end),
                    ));
                    nodes.push(node);
                }
                Ok(nodes)
            },
        );
    }
}

/// Shared body of `find-file` / `find-file-noselect`: a buffer already
/// visiting `path` (canonical compare — dedup by FILE, not basename) is
/// reused; otherwise the file is opened and installed under its basename,
/// uniquified Emacs-style (`doc.txt<2>`) when a different file already owns
/// that name. `select` makes the buffer current.
fn find_file_buffer(s: &SharedSession, path: &str, select: bool) -> Result<String, Error> {
    let p = std::path::Path::new(path);
    let mut sess = s.borrow_mut();
    if let Some(name) = sess.buffer_visiting(p) {
        if select {
            sess.set_buffer(&name).map_err(|e| err(&e))?;
        }
        return Ok(name);
    }
    let mut store: Box<dyn crate::store::TextStore> =
        Box::new(crate::Quire::open(p).map_err(|e| err(&format!("find-file {path}: {e}")))?);
    let unique = sess.unique_buffer_name(store.name());
    if unique != store.name() {
        store.set_name(&unique);
    }
    Ok(sess.install_buffer(store, select))
}

/// Apply a batch of non-overlapping window edits in document order — the
/// shared tail of the streaming `replace-regexp` / `replace-string`. `edits`
/// are ascending `(byte_start, byte_end, replacement)` spans in `window`;
/// `window_start` is the absolute char position of the window's first byte.
/// One forward pass maps the byte offsets to char offsets, then each span is
/// spliced as delete+insert — markers and the narrowing bound adjust per edit,
/// exactly as `replace_match` would — with a running length delta keeping the
/// positions current. Returns the edit count; point ends after the last
/// replacement (untouched when there are none).
fn apply_window_edits(
    sess: &mut crate::engine::Session,
    window_start: usize,
    window: &str,
    edits: Vec<(usize, usize, String)>,
) -> i64 {
    if edits.is_empty() {
        return 0; // nothing to map or splice — skip the window walk
    }
    let bytes: Vec<usize> = edits.iter().flat_map(|(s, e, _)| [*s, *e]).collect();
    // The single mapping pass requires ascending, non-overlapping spans; a
    // violator would have its offsets silently collapsed to the window end.
    debug_assert!(
        bytes.windows(2).all(|w| w[0] <= w[1]),
        "window edits must be ascending and non-overlapping"
    );
    let mut chars = Vec::with_capacity(bytes.len());
    let (mut bi, mut count) = (0, 0usize);
    for (b, _) in window.char_indices() {
        while bi < bytes.len() && bytes[bi] == b {
            chars.push(count);
            bi += 1;
        }
        count += 1;
    }
    while bi < bytes.len() {
        chars.push(count); // offsets sitting at the window's very end
        bi += 1;
    }
    let mut delta = 0i64;
    for (i, (_, _, repl)) in edits.iter().enumerate() {
        let start = ((window_start + chars[2 * i]) as i64 + delta) as usize;
        let end = ((window_start + chars[2 * i + 1]) as i64 + delta) as usize;
        if end > start {
            sess.buffer.delete_region(start, end);
        }
        sess.buffer.goto_char(start);
        if !repl.is_empty() {
            sess.buffer.insert(repl);
        }
        delta += repl.chars().count() as i64 - (end - start) as i64;
    }
    edits.len() as i64
}

/// The language the current buffer parses as: the buffer's
/// `treesit-set-language` override if present, else extension detection on the
/// buffer name, else Markdown (mime-rs's home turf — and the scaffold's
/// historical behavior for nameless buffers).
fn lang_of(sess: &crate::engine::Session) -> Lang {
    let name = sess.buffer.name();
    sess.lang_overrides
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, l)| *l)
        .or_else(|| Lang::from_buffer_name(name))
        .unwrap_or(Lang::Markdown)
}

/// The current buffer's parse for the `treesit-*` builtins — cached on the
/// Session and reused while (buffer, language, content version) are
/// unchanged, so a run of treesit calls parses once. An edit re-stamps the
/// store version and the next call re-parses in full (incremental re-parse
/// via InputEdits remains the syntax.rs TODO).
fn syntax_of(sess: &mut crate::engine::Session) -> std::rc::Rc<Syntax> {
    let lang = lang_of(sess);
    let version = sess.buffer.version();
    if let Some((l, v, syn)) = &sess.syntax_cache
        && *l == lang
        && *v == version
    {
        return std::rc::Rc::clone(syn);
    }
    let syn = std::rc::Rc::new(Syntax::parse(sess.buffer.text(), lang));
    sess.syntax_cache = Some((lang, version, std::rc::Rc::clone(&syn)));
    syn
}

/// Stricter [`live_node`] for the node-EDIT ops: the node must reflect the
/// CURRENT buffer (its parse version equals the current buffer's), because the
/// op mutates the current buffer at the node's span — a node from an inactive
/// buffer (which `live_node` would accept) addresses the wrong text.
fn live_current_node(sess: &crate::engine::Session, n: &TsNode) -> Result<(), Error> {
    if n.version == sess.buffer.version() {
        Ok(())
    } else {
        Err(err(
            "outdated or non-current node: the current buffer changed since the \
             node's parse — re-fetch it (treesit-node-at / treesit-defun-at / \
             treesit-query)",
        ))
    }
}

/// Refuse to splice a span that escapes the buffer's accessible region. The
/// treesit layer reads the WHOLE document (positions are document-absolute,
/// ignoring narrowing), so a node fetched outside an active restriction has a
/// span `delete_region` would honor but `goto_char` would clamp to the
/// narrowing edge — the two halves of the splice would disagree and corrupt the
/// buffer. So an out-of-region edit is rejected, not silently mangled.
fn within_region(
    sess: &crate::engine::Session,
    span: &crate::syntax::NodeSpan,
) -> Result<(), Error> {
    if span.start >= sess.buffer.point_min() && span.end <= sess.buffer.point_max() {
        Ok(())
    } else {
        Err(err(
            "node lies outside the buffer's accessible region — widen (or re-fetch \
             a node inside the narrowing) before editing it",
        ))
    }
}

/// Resolve a node for an edit op: it must be current and its span must lie within
/// the accessible region. Returns the span to splice.
fn edit_span(sess: &crate::engine::Session, n: &TsNode) -> Result<crate::syntax::NodeSpan, Error> {
    live_current_node(sess, n)?;
    let span = n.described()?;
    within_region(sess, &span)?;
    Ok(span)
}

/// The parse-tree node now covering 1-based char `pos`, as a fresh node value in
/// a re-parse of the (just-mutated) current buffer — `nil` if none. The tail of
/// every node-edit op: the splice re-stamped the version, so this re-parses and
/// hands back the replacement, letting edits chain (replace → navigate result).
fn reparse_at(sess: &mut crate::engine::Session, pos: usize) -> TulispObject {
    let version = sess.buffer.version();
    let syn = syntax_of(sess);
    syn.node_at(pos)
        .map(|h| TsNode::new(&syn, version, h))
        .into_tulisp_opt()
}

/// Splice `text` over the current buffer's char span `[start, end)` — delete then
/// insert at `start`, via the store mutators (which re-stamp the version, adjust
/// markers, and clear match-data) — and return the re-parsed node at `start`.
fn splice_and_reparse(
    sess: &mut crate::engine::Session,
    start: usize,
    end: usize,
    text: &str,
) -> TulispObject {
    {
        let b = sess.buffer.as_mut();
        if end > start {
            b.delete_region(start, end);
        }
        b.goto_char(start);
        if !text.is_empty() {
            b.insert(text);
        }
    }
    reparse_at(sess, start)
}

/// Register the *orchestration* builtin group — multiple buffers, file I/O,
/// directory listing, and program arguments — on top of the core vocabulary.
/// Only the TRUSTED tier calls this (see [`crate::engine::Capabilities`]); the
/// sandboxed, agent-facing tier never does. This is the seam M10 fills in; today
/// it is empty (the core group is the whole vocabulary).
pub fn register_orchestration(ctx: &mut TulispContext, session: &SharedSession) {
    // ---- multiple buffers ----
    // These close over the SharedSession exactly like the core editing builtins.
    // The ~90 core primitives all act on `sess.buffer` (the current buffer);
    // `set-buffer` swaps which store that is, so per-buffer editing just works.
    {
        let s = session.clone();
        // (generate-new-buffer NAME) — create an empty in-memory buffer, name
        // uniquified Emacs-style if taken; returns the actual name. Does not
        // switch to it.
        ctx.defun("generate-new-buffer", move |name: String| -> String {
            s.borrow_mut().generate_new_buffer(&name)
        });
    }
    {
        let s = session.clone();
        // (set-buffer NAME) — make NAME the current buffer; returns NAME. Errors
        // if no such buffer exists.
        ctx.defun(
            "set-buffer",
            move |name: String| -> Result<TulispObject, Error> {
                s.borrow_mut().set_buffer(&name).map_err(|e| err(&e))?;
                Ok(TulispValue::from(name).into_ref(None))
            },
        );
    }
    {
        let s = session.clone();
        // (current-buffer) — the current buffer's name.
        ctx.defun("current-buffer", move || -> String {
            s.borrow().current_buffer_name()
        });
    }
    {
        let s = session.clone();
        // (buffer-name) — the current buffer's name (alias of current-buffer
        // here, since mime identifies buffers by name).
        ctx.defun("buffer-name", move || -> String {
            s.borrow().current_buffer_name()
        });
    }
    {
        let s = session.clone();
        // (buffer-list) — all buffer names: current first, then inactive in
        // creation order. The Vec converts to a Lisp list of strings.
        ctx.defun("buffer-list", move || -> Vec<String> {
            s.borrow().buffer_names()
        });
    }
    {
        let s = session.clone();
        // (get-buffer NAME) — NAME if such a buffer exists (current or inactive),
        // else nil.
        ctx.defun(
            "get-buffer",
            move |name: String| -> Result<TulispObject, Error> {
                Ok(if s.borrow().has_buffer(&name) {
                    TulispValue::from(name).into_ref(None)
                } else {
                    TulispObject::nil()
                })
            },
        );
    }
    {
        let s = session.clone();
        // (kill-buffer NAME) — remove the inactive buffer NAME; returns t.
        // Killing the current buffer is an error for now.
        ctx.defun(
            "kill-buffer",
            move |name: String| -> Result<TulispObject, Error> {
                s.borrow_mut().kill_buffer(&name).map_err(|e| err(&e))?;
                Ok(TulispObject::t())
            },
        );
    }
    {
        let s = session.clone();
        // (with-current-buffer NAME BODY...) — evaluate BODY with NAME current,
        // then restore the previously-current buffer *even if BODY errors*. NAME
        // (the first arg) is evaluated; BODY is the rest. Returns BODY's value.
        ctx.defspecial("with-current-buffer", move |ctx, args| {
            let name_form = args.car_and_then(|f| Ok(f.clone()))?;
            let name = ctx.eval(&name_form)?.as_string()?;
            let body = args.cdr_and_then(|b| Ok(b.clone()))?;

            let previous = s.borrow().current_buffer_name();
            s.borrow_mut().set_buffer(&name).map_err(|e| err(&e))?;
            // Capture BODY's result, restore the previous buffer regardless, then
            // surface the result (value or error).
            let res = ctx.eval_progn(&body);
            s.borrow_mut().set_buffer(&previous).map_err(|e| err(&e))?;
            res
        });
    }

    // ---- file I/O (trusted tier only → UNRESTRICTED filesystem) ----
    // These run only on the trusted, local CLI tier, so they get the user's full
    // filesystem reach — NO `safety::check_path` root/allowlist check. (The
    // sandboxed agent-facing tier never registers this group.) Writes still go
    // through `safety::write_atomic` so a save is atomic (temp file + rename) and
    // never mutates a file in place under a live mmap.
    {
        let s = session.clone();
        // (find-file PATH) — open PATH (mmap-backed Quire) into a new buffer and
        // make it current; returns the buffer name. A buffer already VISITING
        // that file (canonical-path compare) is switched to instead of opening
        // a duplicate, like Emacs `find-file`; a different file that merely
        // shares the basename gets a uniquified name (`doc.txt<2>`). IO errors
        // propagate as a tulisp Error.
        ctx.defun("find-file", move |path: String| -> Result<String, Error> {
            find_file_buffer(&s, &path, true)
        });
    }
    {
        let s = session.clone();
        // (find-file-noselect PATH) — open PATH into a buffer WITHOUT making it
        // current (Emacs `find-file-noselect`); returns the buffer name. The
        // same visiting-buffer reuse and name uniquification as find-file.
        ctx.defun(
            "find-file-noselect",
            move |path: String| -> Result<String, Error> { find_file_buffer(&s, &path, false) },
        );
    }
    {
        let s = session.clone();
        // (insert-file-contents PATH) — read PATH and insert its text at point in
        // the CURRENT buffer (no new buffer); returns the char count inserted.
        ctx.defun(
            "insert-file-contents",
            move |path: String| -> Result<i64, Error> {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| err(&format!("insert-file-contents {path}: {e}")))?;
                let n = text.chars().count() as i64;
                s.borrow_mut().buffer.insert(&text);
                Ok(n)
            },
        );
    }
    {
        let s = session.clone();
        // (write-file PATH) — write the CURRENT buffer's text to PATH atomically;
        // returns the byte count written. Streams the buffer via `write_to`, so a
        // multi-GB Quire is never materialized into one allocation just to save.
        ctx.defun("write-file", move |path: String| -> Result<i64, Error> {
            let p = std::path::Path::new(&path);
            // Is this our OWN visited file? If so, apply the same stale-read
            // guard as save_buffer — refuse to overwrite it when it drifted on
            // disk since we opened it, rather than silently clobbering an
            // external writer. Writing to any *other* path overwrites nothing
            // of ours, so it stays unguarded (the export/write-anywhere case).
            let own_file = {
                let sess = s.borrow();
                match sess.buffer.file_stamp() {
                    Some(st) if crate::engine::same_file(p, &st.path) => {
                        if let Some(reason) = st.check() {
                            return Err(err(&format!(
                                "refusing to write-file: {path} was {reason} after it \
                                 was opened; run (revert-buffer) to re-read it, or \
                                 write to a different path"
                            )));
                        }
                        true
                    }
                    _ => false,
                }
            };
            let written = {
                let sess = s.borrow();
                let mut written = 0usize;
                crate::safety::write_atomic_with(p, |w| {
                    written = sess.buffer.write_to(w)?;
                    Ok(())
                })
                .map_err(|e| err(&format!("write-file {path}: {e}")))?;
                written
            };
            // Overwrote our own visited file → re-stamp the session onto the
            // fresh bytes (mirrors save_to's rebase) so the guard doesn't later
            // trip against this very write and force a needless re-open. Writing
            // elsewhere left visiting — and the stamp — untouched. Best-effort:
            // a failed re-stamp just leaves the guard conservatively armed.
            if own_file {
                let _ = s.borrow_mut().buffer.rebase_to_file(p);
            }
            Ok(written as i64)
        });
    }
    {
        let s = session.clone();
        // (write-region START END PATH) — write the buffer substring [START, END)
        // to PATH atomically; returns the byte count written.
        ctx.defun(
            "write-region",
            move |start: i64, end: i64, path: String| -> Result<i64, Error> {
                let text = {
                    let sess = s.borrow();
                    sess.buffer
                        .substring(start.max(1) as usize, end.max(1) as usize)
                };
                crate::safety::write_atomic(std::path::Path::new(&path), text.as_bytes())
                    .map_err(|e| err(&format!("write-region {path}: {e}")))?;
                Ok(text.len() as i64)
            },
        );
    }
    {
        // (directory-files DIR) — the entry names in DIR (names only, not full
        // paths, like Emacs), sorted. IO errors propagate as a tulisp Error.
        ctx.defun(
            "directory-files",
            move |dir: String| -> Result<Vec<String>, Error> {
                let mut names = Vec::new();
                let entries = std::fs::read_dir(&dir)
                    .map_err(|e| err(&format!("directory-files {dir}: {e}")))?;
                for entry in entries {
                    let entry = entry.map_err(|e| err(&format!("directory-files {dir}: {e}")))?;
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
                names.sort();
                Ok(names)
            },
        );
    }

    // ---- program arguments (trusted CLI) ----
    {
        let s = session.clone();
        // (arg "KEY") — the value the trusted CLI passed for KEY, or nil if it
        // wasn't given. A bare `--flag` reads back as the string "t", so a flag
        // and a string option are queried the same way.
        ctx.defun("arg", move |key: String| -> Result<TulispObject, Error> {
            Ok(match s.borrow().args.iter().find(|(k, _)| *k == key) {
                Some((_, v)) => TulispValue::from(v.clone()).into_ref(None),
                None => TulispObject::nil(),
            })
        });
    }
    {
        let s = session.clone();
        // (args) — the whole argument list as an alist `((KEY . VALUE) …)`, in the
        // order the CLI gave them. Each element is a cons of two strings.
        ctx.defun("args", move || -> TulispObject {
            let pairs: Vec<TulispObject> = s
                .borrow()
                .args
                .iter()
                .map(|(k, v)| {
                    TulispObject::cons(
                        TulispValue::from(k.clone()).into_ref(None),
                        TulispValue::from(v.clone()).into_ref(None),
                    )
                })
                .collect();
            TulispObject::from(pairs)
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::Workspace;
    use crate::buffer::Buffer;
    use crate::result::RunReport;

    #[test]
    fn cached_regex_reuses_compiles_and_propagates_errors() {
        let a = super::cached_regex("a+b").unwrap();
        let b = super::cached_regex("a+b").unwrap(); // served from the cache
        assert!(a.is_match("aaab"));
        assert!(b.is_match("ab"));
        assert!(super::cached_regex("(unclosed").is_err());
    }

    /// A trusted workspace (so `register_orchestration` runs) over a "main"
    /// buffer with the given text.
    fn trusted(text: &str) -> Workspace {
        Workspace::new_trusted(Box::new(Buffer::from_string("main", text)))
    }

    fn report(r: &RunReport, key: &str) -> String {
        r.reports
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn match_beginning_and_end_bracket_the_whole_match() {
        let mut ws = trusted("hello WORLD hello");
        let r = ws
            .run(
                r#"(search-forward "WORLD")
                   (report "beg" (match-beginning))
                   (report "end" (match-end))
                   (report "str" (match-string 0))"#,
            )
            .unwrap();
        // match-beginning is the start, match-end one past the end (point lands
        // there after a forward search), and they bracket exactly the match.
        assert_eq!(report(&r, "beg"), "7");
        assert_eq!(report(&r, "end"), "12");
        assert_eq!(report(&r, "str"), "\"WORLD\"");
        // A non-zero subexp is not tracked -> nil.
        let r = ws.run(r#"(report "g1" (match-beginning 1))"#).unwrap();
        assert_eq!(report(&r, "g1"), "nil");
    }

    #[test]
    fn two_buffers_stay_isolated_with_their_own_text_and_point() {
        let mut ws = trusted("main-body");
        // Put point somewhere non-trivial in `main`, then create+edit `other`.
        ws.run(
            r#"(goto-char 5)
               (generate-new-buffer "other")
               (set-buffer "other")
               (insert "other-text")
               (goto-char 3)"#,
        )
        .unwrap();
        // `other` is current and holds only its own edits.
        let r = ws
            .run(r#"(report "txt" (buffer-string)) (report "pt" (point))"#)
            .unwrap();
        assert_eq!(report(&r, "txt"), "\"other-text\"");
        assert_eq!(report(&r, "pt"), "3");

        // Switching back to `main` preserves its text AND its point (5) — the
        // edits to `other` never touched it.
        let r = ws
            .run(r#"(set-buffer "main") (report "txt" (buffer-string)) (report "pt" (point))"#)
            .unwrap();
        assert_eq!(report(&r, "txt"), "\"main-body\"");
        assert_eq!(report(&r, "pt"), "5");
    }

    #[test]
    fn with_current_buffer_evaluates_in_name_and_restores_after() {
        let mut ws = trusted("MAIN");
        ws.run(r#"(generate-new-buffer "side") (set-buffer "side") (insert "SIDE") (set-buffer "main")"#)
            .unwrap();
        // BODY runs in "side" (sees "SIDE"); afterwards "main" is current again.
        let r = ws
            .run(
                r#"(report "in" (with-current-buffer "side" (buffer-string)))
                   (report "cur" (current-buffer))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "in"), "\"SIDE\"");
        // `report` stringifies its value the way tulisp prints it, so a string
        // return renders quoted.
        assert_eq!(report(&r, "cur"), "\"main\"");
    }

    #[test]
    fn with_current_buffer_restores_even_when_body_errors() {
        let mut ws = trusted("MAIN");
        ws.run(r#"(generate-new-buffer "scratch")"#).unwrap();
        // BODY switches into "scratch", mutates it, then signals an error; the
        // error is caught, but the current buffer must already be back to "main".
        let r = ws
            .run(
                r#"(condition-case e
                      (with-current-buffer "scratch" (insert "X") (error "boom"))
                    (error nil))
                   (report "cur" (current-buffer))
                   (set-buffer "scratch")
                   (report "scratch" (buffer-string))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "cur"), "\"main\"");
        // The pre-error edit inside BODY still happened (it is not rolled back —
        // only the *current buffer* is restored), proving BODY did run in scratch.
        assert_eq!(report(&r, "scratch"), "\"X\"");
    }

    #[test]
    fn buffer_list_current_get_and_kill_buffer_behave() {
        let mut ws = trusted("MAIN");
        ws.run(r#"(generate-new-buffer "a") (generate-new-buffer "b")"#)
            .unwrap();
        // buffer-list: current first, then inactive in creation order.
        let r = ws
            .run(
                r#"(report "list" (buffer-list))
                   (report "cur" (current-buffer))
                   (report "name" (buffer-name))
                   (report "got" (get-buffer "a"))
                   (report "miss" (get-buffer "nope"))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "list"), "(\"main\" \"a\" \"b\")");
        assert_eq!(report(&r, "cur"), "\"main\"");
        assert_eq!(report(&r, "name"), "\"main\"");
        assert_eq!(report(&r, "got"), "\"a\""); // existing buffer → its name
        assert_eq!(report(&r, "miss"), "nil"); // absent → nil

        // kill-buffer removes an inactive buffer; it then drops out of buffer-list.
        let r = ws
            .run(r#"(report "k" (kill-buffer "a")) (report "list" (buffer-list))"#)
            .unwrap();
        assert_eq!(report(&r, "k"), "t");
        assert_eq!(report(&r, "list"), "(\"main\" \"b\")");
    }

    #[test]
    fn kill_buffer_errors_on_missing_and_on_current() {
        let mut ws = trusted("MAIN");
        // Missing buffer → error.
        match ws.run(r#"(kill-buffer "ghost")"#) {
            Err(e) => assert!(e.contains("no buffer named ghost"), "got: {e}"),
            Ok(_) => panic!("killing a missing buffer should error"),
        }
        // The current buffer cannot be killed (no replacement policy yet).
        match ws.run(r#"(kill-buffer "main")"#) {
            Err(e) => assert!(e.contains("current buffer"), "got: {e}"),
            Ok(_) => panic!("killing the current buffer should error"),
        }
    }

    #[test]
    fn set_buffer_errors_on_unknown_name() {
        let mut ws = trusted("MAIN");
        match ws.run(r#"(set-buffer "nope")"#) {
            Err(e) => assert!(e.contains("no buffer named nope"), "got: {e}"),
            Ok(_) => panic!("set-buffer on an unknown name should error"),
        }
    }

    #[test]
    fn kill_buffer_drops_the_buffers_language_override() {
        let mut ws = trusted("MAIN");
        // Override an inactive buffer's language, kill it, then reuse the name:
        // the fresh buffer must get default detection, not the dead buffer's
        // override.
        ws.run(
            r#"(generate-new-buffer "scratch") (set-buffer "scratch")
               (treesit-set-language "rust") (set-buffer "main")
               (kill-buffer "scratch")"#,
        )
        .unwrap();
        let r = ws
            .run(
                r#"(generate-new-buffer "scratch") (set-buffer "scratch")
                   (treesit-language)"#,
            )
            .unwrap();
        assert_eq!(report(&r, "treesit-language"), "markdown");
    }

    #[test]
    fn generate_new_buffer_uniquifies_duplicate_names() {
        let mut ws = trusted("MAIN");
        let r = ws
            .run(
                r#"(report "a" (generate-new-buffer "dup"))
                   (report "b" (generate-new-buffer "dup"))
                   (report "c" (generate-new-buffer "dup"))"#,
            )
            .unwrap();
        // First takes the bare name; later ones get Emacs-style <N> suffixes.
        assert_eq!(report(&r, "a"), "\"dup\"");
        assert_eq!(report(&r, "b"), "\"dup<2>\"");
        assert_eq!(report(&r, "c"), "\"dup<3>\"");
    }

    #[test]
    fn sandboxed_tier_lacks_the_orchestration_buffer_builtins() {
        // The sandboxed (agent-facing) workspace never registers the
        // orchestration group, so `set-buffer` is not defined: calling it fails
        // as a void/undefined symbol rather than switching buffers.
        let mut ws = Workspace::new(Box::new(Buffer::from_string("main", "x")));
        let e = match ws.run(r#"(set-buffer "x")"#) {
            Err(e) => e,
            Ok(_) => panic!("sandboxed tier must not expose set-buffer"),
        };
        assert!(
            e.contains("void") && e.contains("set-buffer"),
            "expected a void/undefined error for set-buffer, got: {e}"
        );
        // generate-new-buffer is likewise absent on the sandboxed tier.
        assert!(
            ws.run(r#"(generate-new-buffer "y")"#).is_err(),
            "sandboxed tier must not expose generate-new-buffer"
        );
    }

    /// A unique temp directory for an I/O test (process- and test-scoped so the
    /// parallel harness doesn't collide). Created fresh; the caller cleans up.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mime-io-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn find_file_dedups_by_path_and_uniquifies_colliding_basenames() {
        let dir = temp_dir("find-file-dedup");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/doc.txt"), "alpha").unwrap();
        std::fs::write(dir.join("b/doc.txt"), "beta").unwrap();
        let (pa, pb) = (
            dir.join("a/doc.txt").to_string_lossy().into_owned(),
            dir.join("b/doc.txt").to_string_lossy().into_owned(),
        );

        let mut ws = trusted("main-body");
        let r = ws
            .run(&format!(
                r#"(report "first" (find-file "{pa}"))
                   (report "second" (find-file "{pb}"))
                   (report "second-txt" (buffer-string))
                   (report "revisit" (find-file-noselect "{pa}"))
                   (set-buffer "doc.txt")
                   (report "first-txt" (buffer-string))"#
            ))
            .unwrap();
        // Two files sharing a basename are two buffers — the second is
        // uniquified, not an alias of the first.
        assert_eq!(report(&r, "first"), "\"doc.txt\"");
        assert_eq!(report(&r, "second"), "\"doc.txt<2>\"");
        assert_eq!(report(&r, "second-txt"), "\"beta\"");
        // Revisiting the FILE (not the name) finds the original buffer.
        assert_eq!(report(&r, "revisit"), "\"doc.txt\"");
        assert_eq!(report(&r, "first-txt"), "\"alpha\"");

        // revert-buffer keeps a uniquified name — no duplicate "doc.txt".
        let r = ws
            .run(
                r#"(set-buffer "doc.txt<2>")
                   (revert-buffer)
                   (report "name" (current-buffer))
                   (report "list" (buffer-list))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "name"), "\"doc.txt<2>\"");
        assert_eq!(report(&r, "list"), "(\"doc.txt<2>\" \"main\" \"doc.txt\")");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revert_buffer_recovers_from_a_stale_save_refusal() {
        let dir = temp_dir("revert");
        let file = dir.join("doc.txt");
        std::fs::write(&file, "v1 content\n").unwrap();

        // Sandboxed tier on purpose: revert-buffer re-reads only the
        // already-authorized visited path, so agents can recover too.
        let mut ws = Workspace::new(Box::new(crate::Quire::open(&file).unwrap()));
        ws.run(r#"(goto-char (point-max)) (insert "edit\n")"#)
            .unwrap();

        // An external writer lands; the stale guard refuses the save.
        std::fs::write(&file, "v2 external\n").unwrap();
        let e = ws.save_to(&file).unwrap_err().to_string();
        assert!(e.contains("revert-buffer"), "recovery hint missing: {e}");

        // Revert: the buffer now matches the disk, point clamped, stamp
        // fresh — and a re-applied edit saves cleanly.
        let r = ws
            .run(
                r#"(report "reverted" (if (revert-buffer) 1 0))
                   (report "txt" (buffer-string))
                   (report "stale" (if (buffer-stale-p) 1 0))
                   (goto-char (point-max))
                   (insert "edit\n")"#,
            )
            .unwrap();
        assert_eq!(report(&r, "reverted"), "1");
        assert_eq!(report(&r, "txt"), "\"v2 external\\n\"");
        assert_eq!(report(&r, "stale"), "0");
        ws.save_to(&file).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v2 external\nedit\n"
        );

        // A revert keeps the buffer's (possibly uniquified) name, and live
        // marker handles DETACH rather than aliasing newly created markers
        // (the fresh registry is padded to the old id space).
        let r = ws
            .run(
                r#"(setq m (copy-marker 3))
                   (revert-buffer)
                   (report "old-marker" (if (marker-position m) 1 0))
                   (setq m2 (copy-marker 5))
                   (report "new-marker" (marker-position m2))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "old-marker"), "0", "detached, not aliased");
        assert_eq!(report(&r, "new-marker"), "5");

        // No visited file → a proper error.
        let mut scratch = Workspace::new(Box::new(crate::Buffer::from_string("s", "x")));
        let e = match scratch.run("(revert-buffer)") {
            Err(e) => e,
            Ok(_) => panic!("revert-buffer without a visited file must error"),
        };
        assert!(e.contains("no visited file"), "got: {e}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_to_the_visited_path_restamps_and_doesnt_self_trip_the_guard() {
        let dir = temp_dir("write-file-restamp");
        let file = dir.join("doc.txt");
        std::fs::write(&file, "v1\n").unwrap();
        // write-file is a trusted-tier (CLI/script) builtin — the tier this
        // self-trip bites when a script drives an edit→write→edit→save loop.
        let mut ws = Workspace::new_trusted(Box::new(crate::Quire::open(&file).unwrap()));
        let path = file.to_string_lossy().into_owned();

        // Edit, then write the buffer back over its OWN visited file. The write
        // advances the file's mtime/size, but re-stamping the session means the
        // external-change guard does NOT then read its own write as drift.
        let r = ws
            .run(&format!(
                r#"(goto-char (point-max)) (insert "a\n")
                   (report "wrote" (write-file "{path}"))
                   (report "stale" (if (buffer-stale-p) 1 0))"#
            ))
            .unwrap();
        assert_eq!(report(&r, "wrote"), "5"); // "v1\na\n"
        assert_eq!(report(&r, "stale"), "0", "own write must not read as drift");

        // A second edit→save to the same path then succeeds — no needless
        // re-open forced by the guard tripping on the prior write.
        ws.run(r#"(goto-char (point-max)) (insert "b\n")"#).unwrap();
        ws.save_to(&file)
            .expect("save after self-write must not be refused");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1\na\nb\n");

        // Writing ELSEWHERE leaves visiting (and the stamp) untouched: the
        // session still guards its original file, not the export target.
        let other = dir.join("export.txt");
        let other_path = other.to_string_lossy().into_owned();
        let r = ws
            .run(&format!(
                r#"(report "wrote" (write-file "{other_path}"))
                   (report "stale" (if (buffer-stale-p) 1 0))"#
            ))
            .unwrap();
        assert_eq!(report(&r, "wrote"), "7"); // "v1\na\nb\n"
        assert_eq!(report(&r, "stale"), "0", "still tracking the visited file");
        // Drift the export target — irrelevant, it is not the visited file.
        std::fs::write(&other, "clobbered\n").unwrap();
        let r = ws
            .run(r#"(report "stale" (if (buffer-stale-p) 1 0))"#)
            .unwrap();
        assert_eq!(report(&r, "stale"), "0");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_refuses_to_clobber_an_externally_changed_visited_file() {
        let dir = temp_dir("write-file-stale");
        let file = dir.join("doc.txt");
        std::fs::write(&file, "theirs v1\n").unwrap();
        let mut ws = Workspace::new_trusted(Box::new(crate::Quire::open(&file).unwrap()));
        let path = file.to_string_lossy().into_owned();
        ws.run(r#"(goto-char (point-max)) (insert "ours\n")"#)
            .unwrap();

        // An external writer lands after open (atomic rename → new inode, so
        // the Quire's mmap of the old inode stays intact); writing back over the
        // visited file must refuse (like save_buffer) rather than clobber it.
        crate::safety::write_atomic(&file, b"theirs v2 external\n").unwrap();
        let err = match ws.run(&format!(r#"(write-file "{path}")"#)) {
            Err(e) => e,
            Ok(_) => panic!("write-file over a drifted visited file must fail"),
        };
        // Refused for the right reason — the rename changed the inode.
        assert!(
            err.contains("refusing to write-file") && err.contains("replaced on disk"),
            "got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "theirs v2 external\n",
            "their bytes are intact"
        );

        // Exporting the same buffer ELSEWHERE is still fine — nothing of theirs
        // is at that path, so the guard does not apply.
        let other = dir.join("export.txt");
        let other_path = other.to_string_lossy().into_owned();
        ws.run(&format!(r#"(write-file "{other_path}")"#)).unwrap();
        assert_eq!(std::fs::read_to_string(&other).unwrap(), ws.text());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_refuses_on_an_in_place_mtime_size_drift() {
        // The other drift branch of FileStamp::check: an external in-place
        // rewrite keeps the inode but changes size/mtime. (A different LENGTH
        // makes the size check fire regardless of mtime granularity.) The guard
        // must still refuse — only the reason differs from the new-inode case.
        let dir = temp_dir("write-file-stale-inplace");
        let file = dir.join("doc.txt");
        std::fs::write(&file, "theirs v1\n").unwrap();
        let mut ws = Workspace::new_trusted(Box::new(crate::Quire::open(&file).unwrap()));
        let path = file.to_string_lossy().into_owned();
        ws.run(r#"(goto-char (point-max)) (insert "ours\n")"#)
            .unwrap();

        // In-place (same inode), longer content → size differs.
        std::fs::write(&file, "theirs version two, rather longer\n").unwrap();
        let err = match ws.run(&format!(r#"(write-file "{path}")"#)) {
            Err(e) => e,
            Ok(_) => panic!("write-file over an in-place-changed file must fail"),
        };
        assert!(
            err.contains("refusing to write-file") && err.contains("modified on disk"),
            "got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "theirs version two, rather longer\n",
            "their bytes are intact"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_file_opens_into_a_current_buffer_and_reuses_on_revisit() {
        let dir = temp_dir("find-file");
        let file = dir.join("doc.txt");
        std::fs::write(&file, "file body here").unwrap();
        let path = file.to_string_lossy().into_owned();

        let mut ws = trusted("main-body");
        // find-file opens the file into a new buffer named after the file and
        // makes it current; its text is present and current-buffer is its name.
        let r = ws
            .run(&format!(
                r#"(report "name" (find-file "{path}"))
                   (report "txt" (buffer-string))
                   (report "cur" (current-buffer))
                   (report "list" (buffer-list))"#
            ))
            .unwrap();
        assert_eq!(report(&r, "name"), "\"doc.txt\"");
        assert_eq!(report(&r, "txt"), "\"file body here\"");
        assert_eq!(report(&r, "cur"), "\"doc.txt\"");
        // The original "main" buffer was stashed inactive — both are present.
        assert_eq!(report(&r, "list"), "(\"doc.txt\" \"main\")");

        // Revisiting the same file reuses the existing buffer (no duplicate): the
        // buffer-list is unchanged and still has exactly the two buffers.
        let r = ws
            .run(&format!(
                r#"(set-buffer "main")
                   (report "name" (find-file "{path}"))
                   (report "cur" (current-buffer))
                   (report "list" (buffer-list))"#
            ))
            .unwrap();
        assert_eq!(report(&r, "name"), "\"doc.txt\"");
        assert_eq!(report(&r, "cur"), "\"doc.txt\"");
        assert_eq!(report(&r, "list"), "(\"doc.txt\" \"main\")");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn buffer_file_name_and_stale_p_track_the_visited_file() {
        let dir = temp_dir("stale-p");
        let file = dir.join("doc.txt");
        std::fs::write(&file, "v1").unwrap();
        let path = file.to_string_lossy().into_owned();

        let mut ws = trusted("main-body");
        // A file-less buffer has no visited file and is never stale; right after
        // find-file the visited file is recorded and clean.
        let r = ws
            .run(&format!(
                r#"(report "no-file" (buffer-file-name))
                   (report "no-stale" (buffer-stale-p))
                   (find-file "{path}")
                   (report "name" (buffer-file-name))
                   (report "fresh" (buffer-stale-p))"#
            ))
            .unwrap();
        assert_eq!(report(&r, "no-file"), "nil");
        assert_eq!(report(&r, "no-stale"), "nil");
        assert_eq!(report(&r, "name"), format!("\"{path}\""));
        assert_eq!(report(&r, "fresh"), "nil");

        // An external in-place write flips buffer-stale-p to the drift reason.
        std::fs::write(&file, "v2, externally modified").unwrap();
        let r = ws.run(r#"(report "stale" (buffer-stale-p))"#).unwrap();
        let stale = report(&r, "stale");
        assert!(stale.contains("modified"), "got: {stale}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn occur_renders_hits_with_line_pos_counts_and_preserves_point() {
        // "beta" hits: line 1 @7, line 2 @12 (twice), line 4 @28.
        let mut ws = trusted("alpha beta\nbeta beta\ngamma\nbeta\n");
        let r = ws
            .run(r#"(goto-char 5) (message (occur "beta")) (report "point" (point))"#)
            .unwrap();
        let out = &r.log[0];
        assert!(
            out.contains("4 matches on 3 lines"),
            "header totals, got: {out}"
        );
        assert!(out.contains("    1 @7: alpha beta"), "got: {out}");
        assert!(
            out.contains("    2 @12 ×2: beta beta"),
            "per-line count, got: {out}"
        );
        assert!(out.contains("    4 @28: beta"), "got: {out}");
        assert!(!out.contains("gamma"), "non-matching line leaked: {out}");
        // Orientation, not motion: point is back where the program left it.
        assert_eq!(report(&r, "point"), "5");

        // No matches → a one-line header, point still preserved.
        let r = ws.run(r#"(message (occur "zeta"))"#).unwrap();
        assert!(r.log[0].contains("no matches"), "got: {}", r.log[0]);
    }

    #[test]
    fn occur_respects_narrowing_limit_and_context() {
        let mut ws = trusted("alpha beta\nbeta beta\ngamma\nbeta\n");
        // Narrowed to line 2 only, occur sees just that line's matches.
        let r = ws
            .run(r#"(save-restriction (narrow-to-region 12 22) (message (occur "beta")))"#)
            .unwrap();
        assert!(
            r.log[0].contains("2 matches on 1 line —"),
            "narrowing scope, got: {}",
            r.log[0]
        );
        assert!(!r.log[0].contains("alpha"), "got: {}", r.log[0]);

        // LIMIT 1 renders one matching line and counts the rest in the tail.
        let r = ws.run(r#"(message (occur "beta" 0 1))"#).unwrap();
        assert!(
            r.log[0].contains("… and 2 more matching lines"),
            "limit tail, got: {}",
            r.log[0]
        );

        // NLINES 1 around the line-4 hit pulls in line 3 as "-" context, and
        // the two blocks (lines 1-3 merged, then nothing left) never repeat a
        // line; a context-only line shows the "-" gutter.
        let r = ws.run(r#"(message (occur "gamma" 1))"#).unwrap();
        let out = &r.log[0];
        assert!(
            out.contains("    2 - beta beta"),
            "context above, got: {out}"
        );
        assert!(out.contains("    3 @22: gamma"), "hit line, got: {out}");
        assert!(out.contains("    4 - beta"), "context below, got: {out}");
    }

    #[test]
    fn line_labels_are_narrowing_relative_and_goto_line_round_trips() {
        // Five lines; narrow to lines 3-4. Line numbers count from the
        // accessible region's start (Emacs line-number-at-pos semantics), so
        // the labels window/occur display feed straight back into goto-line.
        let mut ws = trusted("l1\nl2\nl3\nl4\nl5\n");
        let r = ws
            .run(
                r#"(narrow-to-region 7 13)
                   (goto-char 10)
                   (report "ln" (line-number-at-pos (point)))
                   (report "back" (progn (goto-line 2) (point)))
                   (message (window 0))
                   (message (occur "l4"))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "ln"), "2");
        assert_eq!(report(&r, "back"), "10", "goto-line 2 = start of l4");
        let win = &r.log[0];
        assert!(win.contains("line 2"), "window header relative, got: {win}");
        assert!(win.contains("Narrow"), "restriction flagged, got: {win}");
        assert!(
            win.contains("    2 > "),
            "window renders THE line, got: {win}"
        );
        assert!(
            !win.contains("l5"),
            "window stays inside the narrowing: {win}"
        );
        assert!(
            r.log[1].contains("    2 @10: l4"),
            "occur label, got: {}",
            r.log[1]
        );
    }

    #[test]
    fn zero_width_nodes_negative_indexes_and_cross_buffer_access() {
        // Zero-width nodes (incomplete code) answer through every accessor —
        // a query capturing one used to PANIC the process.
        let mut ws = trusted("def f():");
        let r = ws
            .run(
                r#"(treesit-set-language "python")
                   (let* ((blk (car (treesit-query "(block) @b"))))
                     (report "type" (treesit-node-type blk))
                     (report "width" (- (treesit-node-end blk)
                                        (treesit-node-start blk)))
                     (report "parent" (treesit-node-type (treesit-node-parent blk))))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "type"), "\"block\"");
        assert_eq!(report(&r, "width"), "0");
        assert_eq!(report(&r, "parent"), "\"function_definition\"");

        // Negative child index counts from the end (Emacs); too-negative → nil.
        let mut ws = trusted("fn f() { 1; }");
        let r = ws
            .run(
                r#"(treesit-set-language "rust")
                   (let ((d (treesit-defun-at 1)))
                     (report "last" (treesit-node-text (treesit-node-child d -1)))
                     (report "too-neg" (if (treesit-node-child d -99) 1 0)))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "last"), "\"{ 1; }\"");
        assert_eq!(report(&r, "too-neg"), "0");

        // A node from a non-current buffer still answers (its parse reflects
        // a LIVE buffer); it only outdates when its own buffer changes.
        let mut ws = trusted("fn alpha() {}");
        let r = ws
            .run(
                r#"(treesit-set-language "rust")
                   (setq n (treesit-defun-at 1))
                   (generate-new-buffer "side")
                   (set-buffer "side")
                   (report "cross" (treesit-node-type n))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "cross"), "\"function_item\"");
        let e = match ws.run(r#"(set-buffer "main") (insert "x") (treesit-node-type n)"#) {
            Err(e) => e,
            Ok(_) => panic!("must outdate when its own buffer changes"),
        };
        assert!(e.contains("outdated node"), "got: {e}");
    }

    #[test]
    fn node_values_navigate_the_tree_and_expire_on_edit() {
        let mut ws = trusted("fn norm(x: i64) -> i64 {\n    x.abs()\n}\n");
        let r = ws
            .run(
                r#"(treesit-set-language "rust")
                   (let* ((leaf (treesit-node-at (- (point-max) 8))) ; inside abs
                          (defun-node (treesit-defun-at (point-min)))
                          (name (treesit-node-child-by-field-name defun-node "name"))
                          (params (treesit-node-child-by-field-name defun-node "parameters")))
                     (report "leaf-type" (treesit-node-type leaf))
                     (report "fn-type" (treesit-node-type defun-node))
                     (report "fn-start" (treesit-node-start defun-node))
                     (report "name-text" (treesit-node-text name))
                     (report "params-text" (treesit-node-text params))
                     (report "kids" (treesit-node-child-count defun-node))
                     (report "named-kids" (treesit-node-child-count defun-node t))
                     (report "root-type"
                             (treesit-node-type (treesit-node-parent defun-node)))
                     (report "no-parent"
                             (if (treesit-node-parent (treesit-node-parent defun-node))
                                 1 0))
                     ;; siblings: name -> parameters in the named view
                     (report "sib"
                             (treesit-node-text (treesit-node-next-sibling name t)))
                     (report "sib-back"
                             (treesit-node-text
                              (treesit-node-prev-sibling
                               (treesit-node-next-sibling name t) t)))
                     ;; raw view: child 0 of the fn is the "fn" keyword token
                     (report "kw" (treesit-node-text (treesit-node-child defun-node 0)))
                     (setq stale defun-node))"#,
            )
            .unwrap();
        assert!(report(&r, "leaf-type").contains("field_identifier"));
        assert_eq!(report(&r, "fn-type"), "\"function_item\"");
        assert_eq!(report(&r, "fn-start"), "1");
        assert_eq!(report(&r, "name-text"), "\"norm\"");
        assert_eq!(report(&r, "params-text"), "\"(x: i64)\"");
        assert_eq!(report(&r, "named-kids"), "4"); // name params return_type body
        assert!(
            report(&r, "kids").parse::<i64>().unwrap() > 4,
            "raw view includes tokens"
        );
        assert_eq!(report(&r, "root-type"), "\"source_file\"");
        assert_eq!(report(&r, "no-parent"), "0", "root has no parent");
        assert_eq!(report(&r, "sib"), "\"(x: i64)\"");
        assert_eq!(report(&r, "sib-back"), "\"norm\"");
        assert_eq!(report(&r, "kw"), "\"fn\"");

        // An edit outdates every node from the old parse: accessors refuse
        // rather than serve positions from a stale tree.
        let e = match ws.run(r#"(insert "// hi\n") (treesit-node-type stale)"#) {
            Err(e) => e,
            Ok(_) => panic!("an outdated node must not answer"),
        };
        assert!(e.contains("outdated node"), "got: {e}");
    }

    #[test]
    fn treesit_replace_node_swaps_the_span_and_returns_the_reparse() {
        let mut ws = trusted("fn f() {\n    foo(bar)\n}\n");
        let r = ws
            .run(
                r#"(treesit-set-language "rust")
                   (goto-char (point-min)) (search-forward "bar")
                   (report "ret" (treesit-node-text
                     (treesit-replace-node (treesit-node-at (- (point) 1)) "baz")))"#,
            )
            .unwrap();
        assert_eq!(ws.text(), "fn f() {\n    foo(baz)\n}\n");
        assert_eq!(report(&r, "ret"), "\"baz\"", "returns the re-parsed node");
    }

    #[test]
    fn treesit_wrap_node_brackets_the_span() {
        let mut ws = trusted("fn f() -> i64 {\n    x\n}\n");
        ws.run(
            r#"(treesit-set-language "rust")
               (goto-char (point-min)) (search-forward "x")
               (treesit-wrap-node (treesit-node-at (- (point) 1)) "Some(" ")")"#,
        )
        .unwrap();
        assert_eq!(ws.text(), "fn f() -> i64 {\n    Some(x)\n}\n");
    }

    #[test]
    fn treesit_raise_node_replaces_its_parent() {
        // The identifier's parent is the parenthesized_expression `(inner)`;
        // raising it strips the parens.
        let mut ws = trusted("fn f() {\n    (inner)\n}\n");
        ws.run(
            r#"(treesit-set-language "rust")
               (goto-char (point-min)) (search-forward "inner")
               (treesit-raise-node (treesit-node-at (- (point) 1)))"#,
        )
        .unwrap();
        assert_eq!(ws.text(), "fn f() {\n    inner\n}\n");
    }

    #[test]
    fn treesit_kill_node_deletes_the_span_and_yanks() {
        let mut ws = trusted("fn f() {\n    foo(bar)\n}\n");
        ws.run(
            r#"(treesit-set-language "rust")
               (goto-char (point-min)) (search-forward "bar")
               (treesit-kill-node (treesit-node-at (- (point) 1)))"#,
        )
        .unwrap();
        assert_eq!(ws.text(), "fn f() {\n    foo()\n}\n", "span deleted");
        // The killed text went to the kill-ring → yank restores it.
        ws.run(r#"(goto-char (point-max)) (yank)"#).unwrap();
        assert_eq!(ws.text(), "fn f() {\n    foo()\n}\nbar");
    }

    #[test]
    fn treesit_insert_sibling_adds_text_after_the_node() {
        let mut ws = trusted("fn f() {}\n");
        ws.run(
            r#"(treesit-set-language "rust")
               (treesit-insert-sibling (treesit-defun-at (point-min)) "\nfn g() {}" nil)"#,
        )
        .unwrap();
        assert_eq!(ws.text(), "fn f() {}\nfn g() {}\n");
    }

    #[test]
    fn node_edit_ops_refuse_an_outdated_node() {
        let mut ws = trusted("fn f() {\n    x\n}\n");
        let e = match ws.run(
            r#"(treesit-set-language "rust")
               (setq n (treesit-node-at 1))
               (insert "// edit\n")            ; bumps the version → n is stale
               (treesit-replace-node n "y")"#,
        ) {
            Err(e) => e,
            Ok(_) => panic!("editing through an outdated node must fail"),
        };
        assert!(e.contains("outdated or non-current node"), "got: {e}");
    }

    #[test]
    fn node_edit_ops_refuse_a_node_outside_the_narrowing() {
        // The treesit layer reads the whole document, so a node can be fetched
        // OUTSIDE an active narrowing — but its span would corrupt the buffer
        // (delete honors absolute coords, goto clamps to the restriction), so the
        // edit is refused and the buffer is left untouched.
        let original = "fn a() {\n    foo(bar)\n}\nfn b() {\n    qux\n}\n";
        let mut ws = trusted(original);
        let e = match ws.run(
            r#"(treesit-set-language "rust")
               (goto-char (point-min)) (search-forward "bar")
               (setq n (treesit-node-at (- (point) 1)))     ; node in fn a
               (search-forward "fn b")
               (narrow-to-region (point) (point-max))       ; restrict to fn b
               (treesit-replace-node n "BAZ")"#,
        ) {
            Err(e) => e,
            Ok(_) => panic!("editing a node outside the narrowing must fail"),
        };
        assert!(
            e.contains("outside the buffer's accessible region"),
            "got: {e}"
        );
        assert_eq!(
            ws.text(),
            original,
            "buffer must be untouched (no corruption)"
        );
    }

    #[test]
    fn node_edit_returns_a_node_usable_for_a_chained_edit() {
        // The headline reason the ops return a node: replace, then act on the
        // RE-PARSED result without re-fetching.
        let mut ws = trusted("fn f() {\n    foo(bar)\n}\n");
        ws.run(
            r#"(treesit-set-language "rust")
               (goto-char (point-min)) (search-forward "bar")
               (let ((n (treesit-replace-node (treesit-node-at (- (point) 1)) "baz")))
                 (treesit-wrap-node n "Some(" ")"))"#,
        )
        .unwrap();
        assert_eq!(ws.text(), "fn f() {\n    foo(Some(baz))\n}\n");
    }

    #[test]
    fn treesit_raise_node_errors_at_the_root() {
        let mut ws = trusted("foo\n");
        let e = match ws.run(
            r#"(treesit-set-language "rust")
               (setq n (treesit-node-at 1))
               (while (treesit-node-parent n) (setq n (treesit-node-parent n)))
               (treesit-raise-node n)"#, // n is now the root → no parent
        ) {
            Err(e) => e,
            Ok(_) => panic!("raising the root must fail"),
        };
        assert!(e.contains("no parent"), "got: {e}");
    }

    #[test]
    fn conflict_overview_navigation_and_inspection() {
        let mut ws = trusted(
            "intro\n<<<<<<< HEAD\nours-1\n=======\ntheirs-1\n>>>>>>> branch\nmid\n\
             <<<<<<< HEAD\nsame\n=======\nsame\n>>>>>>> branch\ntail\n",
        );
        let r = ws
            .run(
                r#"(report "count" (conflict-count))
                   (message (conflict-hunks))
                   (report "g1" (conflict-goto))
                   (report "ours" (conflict-text "ours"))
                   (report "g2" (conflict-goto))
                   (report "g3" (conflict-goto))
                   (message (conflict-diff 1))
                   (message (conflict-diff 2))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "count"), "2");
        let overview = &r.log[0];
        assert!(overview.contains("2 conflicts"), "got: {overview}");
        assert!(
            overview.contains("1 @7 L2: HEAD ↔ branch"),
            "got: {overview}"
        );
        // conflict-goto with nil lands just past the next hunk's opener (so
        // point is INSIDE it), then advances, then nil at the end;
        // conflict-text with nil N reads the hunk at point.
        assert_eq!(report(&r, "g1"), "20"); // after "intro\n<<<<<<< HEAD\n"
        assert_eq!(report(&r, "ours"), "\"ours-1\\n\"");
        assert!(report(&r, "g2").parse::<i64>().unwrap() > 20);
        assert_eq!(report(&r, "g3"), "nil");
        // The decision view: a real difference diffs, identical sides don't.
        assert!(r.log[1].contains("-ours-1") && r.log[1].contains("+theirs-1"));
        assert_eq!(r.log[2], "");
        // base on a two-way hunk is a proper error.
        let e = match ws.run(r#"(conflict-text "base" 1)"#) {
            Err(e) => e,
            Ok(_) => panic!("base on a two-way hunk must error"),
        };
        assert!(e.contains("no base section"), "got: {e}");
    }

    #[test]
    fn conflict_context_renders_the_hunk_with_surrounding_lines() {
        let mut ws =
            trusted("a\nb\nc\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\nx\ny\nz\n");
        let r = ws
            .run(
                r#"(goto-char 1)
                   (message (conflict-context 1 2))
                   (report "point" (point))"#,
            )
            .unwrap();
        let view = &r.log[0];
        assert!(
            view.contains("conflict 1/1 @7 (HEAD \u{2194} branch)"),
            "got: {view}"
        );
        // Two context lines on each side, the hunk's own lines marked.
        assert!(
            !view.contains("\n    1   a"),
            "line 1 is beyond the context"
        );
        assert!(view.contains("    2   b\n"), "got: {view}");
        assert!(view.contains("    4 > <<<<<<< HEAD\n"), "got: {view}");
        assert!(view.contains("    6 > =======\n"), "got: {view}");
        assert!(view.contains("    8 > >>>>>>> branch\n"), "got: {view}");
        assert!(view.contains("   10   y\n"), "got: {view}");
        assert!(!view.contains("   11"), "line 11 is beyond the context");
        assert_eq!(report(&r, "point"), "1", "read-only: point preserved");
    }

    #[test]
    fn conflict_goto_finds_a_hunk_at_point_min_and_iterates() {
        // A file that BEGINS with a conflict: the nil form must find it (not
        // skip past), land inside it for at-point addressing, and a second
        // call must advance — the iteration idiom visits every hunk once.
        let mut ws = trusted(
            "<<<<<<< A\nfirst\n=======\nf2\n>>>>>>> B\nmid\n<<<<<<< A\nsecond\n=======\ns2\n>>>>>>> B\n",
        );
        let r = ws
            .run(
                r#"(goto-char (point-min))
                   (report "g1" (conflict-goto))
                   (report "in1" (conflict-text "ours"))
                   (report "g2" (conflict-goto))
                   (report "in2" (conflict-text "ours"))
                   (report "g3" (conflict-goto))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "g1"), "11"); // past "<<<<<<< A\n"
        assert_eq!(report(&r, "in1"), "\"first\\n\"");
        assert!(report(&r, "g2").parse::<i64>().unwrap() > 11);
        assert_eq!(report(&r, "in2"), "\"second\\n\"");
        assert_eq!(report(&r, "g3"), "nil");
        // Explicit N=0 is a proper 1-based-index error, not a silent nil.
        let e = match ws.run(r#"(conflict-goto 0)"#) {
            Err(e) => e,
            Ok(_) => panic!("N=0 must error: indices are 1-based"),
        };
        assert!(e.contains("no conflict 0"), "got: {e}");
    }

    #[test]
    fn conflict_goto_explicit_n_lands_on_the_opener() {
        // Explicit N addresses by index and lands at the hunk START (the
        // opener line) — unlike nil, which lands just past it; both are
        // inside the hunk for the at-point commands.
        let mut ws = trusted("pre\n<<<<<<< A\no\n=======\nt\n>>>>>>> B\n");
        let r = ws
            .run(
                r#"(report "g" (conflict-goto 1))
                   (report "p" (point))
                   (report "ours" (conflict-text "ours"))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "g"), "5"); // start of "<<<<<<< A"
        assert_eq!(report(&r, "p"), "5");
        assert_eq!(report(&r, "ours"), "\"o\\n\"");
    }

    #[test]
    fn conflict_ops_compose_with_narrowing() {
        let text =
            "<<<<<<< A\no1\n=======\nt1\n>>>>>>> B\nmid\n<<<<<<< A\no2\n=======\nt2\n>>>>>>> B\n";
        let mut ws = trusted(text);
        // Narrowed to the second hunk: it is the only one visible, addressed
        // as N=1, and resolving it leaves the first hunk untouched outside.
        let r = ws
            .run(
                r#"(narrow-to-region 39 73)
                   (report "count" (conflict-count))
                   (report "left" (conflict-keep "theirs" 1))
                   (widen)
                   (report "all" (conflict-count))
                   (message (buffer-string))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "count"), "1");
        assert_eq!(
            report(&r, "left"),
            "0",
            "no conflicts left in the narrowing"
        );
        assert_eq!(report(&r, "all"), "1", "the first hunk is still there");
        assert!(r.log[0].contains("mid\nt2\n"), "got: {}", r.log[0]);

        // Narrowing into the MIDDLE of a hunk hides it entirely: the opener
        // is outside, so neither a hunk nor a stray is reported (documented).
        let mut ws = trusted("<<<<<<< A\no1\n=======\nt1\n>>>>>>> B\n");
        let r = ws
            .run(
                r#"(narrow-to-region 11 24)
                   (report "count" (conflict-count))
                   (message (conflict-hunks))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "count"), "0");
        assert!(r.log[0].contains("no conflicts"), "got: {}", r.log[0]);
        assert!(!r.log[0].contains("unparsed"), "got: {}", r.log[0]);
    }

    #[test]
    fn kill_line_stops_at_the_narrowing_boundary() {
        // "abc\ndef\n" narrowed to "bc\nde": kill-line from 'd' kills exactly
        // to point-max (mid-line), and at point-max it is a no-op.
        let mut ws = trusted("abc\ndef\n");
        let r = ws
            .run(
                r#"(narrow-to-region 2 7)
                   (goto-char 5)
                   (kill-line)
                   (report "txt" (buffer-string))
                   (report "p" (point))
                   (kill-line)
                   (report "txt2" (buffer-string))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "txt"), "\"bc\\n\"", "killed de up to point-max");
        assert_eq!(report(&r, "p"), "5");
        assert_eq!(report(&r, "txt2"), "\"bc\\n\"", "no-op at point-max");
    }

    #[test]
    fn conflict_combination_sides_and_line_ending_fidelity() {
        // diff3 hunk: the combination sides read and keep correctly.
        let mut ws = trusted("<<<<<<< A\no\n||||||| base\nb\n=======\nt\n>>>>>>> B\ntail\n");
        let r = ws
            .run(
                r#"(report "both" (conflict-text "both" 1))
                   (report "all" (conflict-text "all" 1))
                   (report "left" (conflict-keep "all" 1))
                   (message (buffer-string))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "both"), "\"o\\nt\\n\"");
        assert_eq!(report(&r, "all"), "\"o\\nb\\nt\\n\"");
        assert_eq!(report(&r, "left"), "0");
        assert_eq!(r.log[0], "o\nb\nt\ntail\n");

        // On a two-way hunk with an empty ours, both/all degrade to theirs.
        let mut ws = trusted("<<<<<<< A\n=======\nt\n>>>>>>> B\n");
        let r = ws
            .run(r#"(report "both" (conflict-text "both" 1)) (report "all" (conflict-text "all" 1))"#)
            .unwrap();
        assert_eq!(report(&r, "both"), "\"t\\n\"");
        assert_eq!(report(&r, "all"), "\"t\\n\"");

        // Mixed line endings: ours CRLF, theirs LF — "both" keeps each side's
        // endings verbatim (content-faithful, no normalization).
        let mut ws = trusted("<<<<<<< A\no\r\n=======\nt\n>>>>>>> B\n");
        let r = ws
            .run(r#"(report "left" (conflict-keep "both" 1)) (message (buffer-string))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0");
        assert_eq!(r.log[0], "o\r\nt\n");
    }

    #[test]
    fn conflict_keep_adjusts_markers_and_replace_empty_deletes() {
        // Markers around the hunk survive a keep with adjusted positions.
        let mut ws = trusted("ab\n<<<<<<< A\no\n=======\nt\n>>>>>>> B\nyz\n");
        let r = ws
            .run(
                r#"(setq m1 (copy-marker 2))
                   (setq m2 (copy-marker 36))
                   (conflict-keep "ours" 1)
                   (report "m1" (marker-position m1))
                   (report "m2" (marker-position m2))
                   (report "at-m2" (buffer-substring (marker-position m2) (+ (marker-position m2) 1)))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "m1"), "2", "marker before the hunk is untouched");
        // The after-hunk marker collapses to the hunk start on delete, then —
        // Emacs insertion-type nil — stays BEFORE the kept text on insert.
        assert_eq!(report(&r, "m2"), "4");
        assert_eq!(report(&r, "at-m2"), "\"o\"");

        // conflict-replace with "" removes the hunk entirely.
        let mut ws = trusted("pre\n<<<<<<< A\no\n=======\nt\n>>>>>>> B\npost\n");
        let r = ws
            .run(r#"(report "left" (conflict-replace "" 1)) (message (buffer-string))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0");
        assert_eq!(r.log[0], "pre\npost\n");
    }

    #[test]
    fn conflict_keep_and_replace_resolve_and_count_down() {
        let mut ws = trusted(
            "intro\n<<<<<<< HEAD\nours-1\n=======\ntheirs-1\n>>>>>>> branch\nmid\n\
             <<<<<<< HEAD\nours-2\n=======\ntheirs-2\n>>>>>>> branch\ntail\n",
        );
        let r = ws
            .run(
                r#"(report "left" (conflict-keep "theirs" 1))
                   (report "done" (conflict-replace "merged\n" 1))
                   (report "txt" (buffer-string))"#,
            )
            .unwrap();
        // Each resolution returns the remaining count; hunk numbers refresh,
        // so the second hunk is addressed as 1 after the first resolves.
        assert_eq!(report(&r, "left"), "1");
        assert_eq!(report(&r, "done"), "0");
        assert_eq!(
            report(&r, "txt"),
            "\"intro\\ntheirs-1\\nmid\\nmerged\\ntail\\n\""
        );

        // At-point addressing: keep "both" on the hunk containing point.
        let mut ws = trusted("<<<<<<< A\no\n=======\nt\n>>>>>>> B\n");
        let r = ws
            .run(r#"(goto-char 12) (report "left" (conflict-keep "both")) (report "txt" (buffer-string))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0");
        assert_eq!(report(&r, "txt"), "\"o\\nt\\n\"");
    }

    #[test]
    fn conflict_keep_both_warns_when_a_shared_suffix_fuses_the_sides() {
        // Each side is the BODY of a brace block that the post-marker `}` was
        // meant to close; keeping both concatenates them, so the single `}`
        // closes only theirs and ours' block is left open — broken but
        // reported resolved. The keep still happens; a warning rides the log.
        let mut ws = trusted(
            "fn pick() {\n<<<<<<< HEAD\n  if a {\n    one()\n=======\n  if b {\n    two()\n\
             >>>>>>> branch\n  }\n}\n",
        );
        let r = ws
            .run(r#"(report "left" (conflict-keep "both" 1))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0", "keep still resolves the hunk");
        assert!(
            r.log.iter().any(|m| m.contains("leave a bracket open")),
            "fused-keep warning missing; log = {:?}",
            r.log
        );

        // The common, safe case — sides that each close what they open — keeps
        // both with no warning (the join is balanced).
        let mut ws = trusted("<<<<<<< HEAD\nfoo()\n=======\nbar()\n>>>>>>> branch\n");
        let r = ws
            .run(r#"(report "left" (conflict-keep "both" 1))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0");
        assert!(r.log.is_empty(), "balanced join must not warn: {:?}", r.log);
    }

    #[test]
    fn conflict_keep_all_warns_on_fused_diff3_sides() {
        // diff3: ours, base, AND theirs each open a brace the post-marker `}`
        // was meant to close. Keeping "all" concatenates all three, so the
        // single `}` closes only the last — the warning must fire (the base
        // section participates in the dangle count too).
        let mut ws = trusted(
            "fn pick() {\n<<<<<<< HEAD\n  if a {\n    one()\n\
             ||||||| base\n  if z {\n    zero()\n=======\n  if b {\n    two()\n\
             >>>>>>> branch\n  }\n}\n",
        );
        let r = ws
            .run(r#"(report "left" (conflict-keep "all" 1))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0", "keep still resolves the hunk");
        assert!(
            r.log.iter().any(|m| m.contains("leave a bracket open")),
            "fused-keep warning missing for \"all\"; log = {:?}",
            r.log
        );
    }

    #[test]
    fn conflicts_lists_hunk_positions_for_a_pure_lisp_resolve_loop() {
        // A clean buffer has no conflicts: (conflicts) is nil (empty list).
        let mut clean = trusted("just some text\nno markers here\n");
        let r = clean.run(r#"(report "n" (length (conflicts)))"#).unwrap();
        assert_eq!(report(&r, "n"), "0");

        let mut ws = trusted(
            "a\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\nmid\n\
             <<<<<<< HEAD\np\n=======\nq\n>>>>>>> b\nz\n",
        );
        // WHERE the conflicts are, as goto-char-able positions — the structured
        // companion to conflict-count's HOW MANY. First position is "a\n" past,
        // i.e. char 3 (the opener line's start), and it sits inside its hunk.
        let r = ws
            .run(
                r#"(report "n" (length (conflicts)))
                   (report "first" (car (conflicts)))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "n"), "2");
        assert_eq!(report(&r, "first"), "3");

        // A whole resolve loop driven from lisp alone — no MCP front door:
        // jump to each hunk's position and resolve it at point until none remain.
        let r = ws
            .run(
                r#"(while (> (conflict-count) 0)
                     (goto-char (car (conflicts)))
                     (conflict-keep "theirs"))
                   (report "left" (length (conflicts)))
                   (report "txt" (buffer-string))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "left"), "0");
        assert_eq!(report(&r, "txt"), "\"a\\ny\\nmid\\nq\\nz\\n\"");
    }

    #[test]
    fn conflict_resolve_trivial_sweeps_only_the_safe_hunks() {
        let mut ws = trusted(
            "<<<<<<< A\nx\n=======\nx\n>>>>>>> B\n\
             <<<<<<< A\no\n||||||| base\no\n=======\nt\n>>>>>>> B\n\
             <<<<<<< A\no2\n||||||| base\nb2\n=======\nb2\n>>>>>>> B\n\
             <<<<<<< A\nreal\n=======\nconflict\n>>>>>>> B\n",
        );
        let r = ws
            .run(r#"(report "left" (conflict-resolve-trivial)) (report "txt" (buffer-string))"#)
            .unwrap();
        // Identical sides → x; ours==base → theirs (t); theirs==base → ours
        // (o2); the genuine conflict survives untouched.
        assert_eq!(report(&r, "left"), "1");
        let txt = report(&r, "txt");
        assert!(
            txt.starts_with("\"x\\nt\\no2\\n<<<<<<< A\\nreal\\n"),
            "got: {txt}"
        );
    }

    #[test]
    fn conflict_keep_all_resolves_every_remaining_hunk_at_once() {
        let mut ws = trusted(
            "<<<<<<< A\no1\n=======\nt1\n>>>>>>> B\n\
             mid\n\
             <<<<<<< A\no2\n=======\nt2\n>>>>>>> B\n",
        );
        // One call takes ours on BOTH hunks (applied bottom-up internally, so
        // no manual renumbering); returns the remaining count.
        let r = ws
            .run(r#"(report "left" (conflict-keep-all "ours")) (report "txt" (buffer-string))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0");
        assert_eq!(report(&r, "txt"), "\"o1\\nmid\\no2\\n\"");
    }

    #[test]
    fn conflict_keep_all_is_all_or_nothing_on_an_unavailable_side() {
        // "base" needs a diff3 hunk; the FIRST hunk here has none. All side-texts
        // are resolved before any splice, so the call errors before touching the
        // buffer. The diff3 hunk is placed SECOND on purpose: an impl that
        // spliced as it scanned bottom-up (no planning phase) would resolve it
        // before hitting the error and leave the buffer half-resolved — this
        // ordering, plus the byte-for-byte check, catches that.
        let mut ws = trusted(
            "<<<<<<< A\no1\n=======\nt1\n>>>>>>> B\n\
             <<<<<<< A\no2\n||||||| base\nb2\n=======\nt2\n>>>>>>> B\n",
        );
        let before = ws.run(r#"(report "txt" (buffer-string))"#).unwrap();
        assert!(ws.run(r#"(conflict-keep-all "base")"#).is_err());
        // Buffer untouched — both hunks still present, byte-for-byte.
        let after = ws
            .run(r#"(report "n" (conflict-count)) (report "txt" (buffer-string))"#)
            .unwrap();
        assert_eq!(report(&after, "n"), "2");
        assert_eq!(report(&after, "txt"), report(&before, "txt"));
    }

    #[test]
    fn conflict_keep_all_collects_fused_keep_warnings() {
        // Two hunks whose "both" join each leaves a brace open. conflict-keep-all
        // resolves every hunk AND drains a fused-keep warning per hunk to the log
        // — exercising its own warning-collection path (collect during planning,
        // push after the buffer borrow ends), which conflict-keep does not share.
        let mut ws = trusted(
            "fn a() {\n<<<<<<< HEAD\n  if x {\n    p()\n=======\n  if y {\n    q()\n\
             >>>>>>> branch\n  }\n}\n\
             fn b() {\n<<<<<<< HEAD\n  if x {\n    p()\n=======\n  if y {\n    q()\n\
             >>>>>>> branch\n  }\n}\n",
        );
        let r = ws
            .run(r#"(report "left" (conflict-keep-all "both"))"#)
            .unwrap();
        assert_eq!(report(&r, "left"), "0", "every hunk resolved");
        assert_eq!(
            r.log
                .iter()
                .filter(|m| m.contains("leave a bracket open"))
                .count(),
            2,
            "one fused-keep warning per hunk; log = {:?}",
            r.log
        );
    }

    #[test]
    fn insert_file_contents_inserts_at_point_in_the_current_buffer() {
        let dir = temp_dir("insert-file");
        let file = dir.join("frag.txt");
        std::fs::write(&file, "INSERTED").unwrap();
        let path = file.to_string_lossy().into_owned();

        let mut ws = trusted("ab");
        // Insert at point (between 'a' and 'b' after goto-char 2) — no new buffer,
        // current stays "main"; returns the char count inserted.
        let r = ws
            .run(&format!(
                r#"(goto-char 2)
                   (report "n" (insert-file-contents "{path}"))
                   (report "txt" (buffer-string))
                   (report "cur" (current-buffer))"#
            ))
            .unwrap();
        assert_eq!(report(&r, "n"), "8"); // "INSERTED" is 8 chars
        assert_eq!(report(&r, "txt"), "\"aINSERTEDb\"");
        assert_eq!(report(&r, "cur"), "\"main\"");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_round_trips_the_whole_buffer() {
        let dir = temp_dir("write-file");
        let file = dir.join("out.txt");
        let path = file.to_string_lossy().into_owned();

        let mut ws = trusted("save me whole");
        let r = ws
            .run(&format!(r#"(report "n" (write-file "{path}"))"#))
            .unwrap();
        // The byte count is reported and the file on disk matches the buffer.
        assert_eq!(report(&r, "n"), "13"); // "save me whole"
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "save me whole");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_region_round_trips_a_substring() {
        let dir = temp_dir("write-region");
        let file = dir.join("region.txt");
        let path = file.to_string_lossy().into_owned();

        let mut ws = trusted("abcdefgh");
        // [3, 6) is "cde".
        let r = ws
            .run(&format!(r#"(report "n" (write-region 3 6 "{path}"))"#))
            .unwrap();
        assert_eq!(report(&r, "n"), "3");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "cde");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_files_lists_entry_names_sorted() {
        let dir = temp_dir("dir-files");
        // Populate out of order; directory-files returns names only, sorted.
        std::fs::write(dir.join("beta.txt"), "b").unwrap();
        std::fs::write(dir.join("alpha.txt"), "a").unwrap();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let path = dir.to_string_lossy().into_owned();

        let mut ws = trusted("main");
        let r = ws
            .run(&format!(r#"(report "files" (directory-files "{path}"))"#))
            .unwrap();
        // Names only (not full paths), sorted; the subdir is listed too.
        assert_eq!(
            report(&r, "files"),
            "(\"alpha.txt\" \"beta.txt\" \"subdir\")"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn program_args_are_readable_via_arg() {
        let mut ws = trusted("main");
        ws.set_program_args(vec![
            ("date".into(), "May 1".into()),
            ("with_badges".into(), "t".into()),
        ]);
        // A string option, a flag (reads back as "t"), and an absent key (nil).
        let r = ws
            .run(
                r#"(report "date" (arg "date"))
                   (report "badges" (arg "with_badges"))
                   (report "missing" (arg "missing"))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "date"), "\"May 1\"");
        assert_eq!(report(&r, "badges"), "\"t\"");
        assert_eq!(report(&r, "missing"), "nil");
    }

    #[test]
    fn args_returns_the_whole_alist() {
        let mut ws = trusted("main");
        ws.set_program_args(vec![
            ("infile".into(), "in.md".into()),
            ("with_badges".into(), "t".into()),
        ]);
        let r = ws.run(r#"(report "all" (args))"#).unwrap();
        // An alist of (KEY . VALUE) string conses, in the order given.
        assert_eq!(
            report(&r, "all"),
            "((\"infile\" . \"in.md\") (\"with_badges\" . \"t\"))"
        );
    }

    #[test]
    fn sandboxed_tier_lacks_file_io_and_arg_builtins() {
        // The sandboxed (agent-facing) tier never registers the orchestration
        // group, so neither the file-I/O builtins nor `arg` exist: they fail as
        // void/undefined symbols rather than touching the filesystem or args.
        let mut ws = Workspace::new(Box::new(Buffer::from_string("main", "x")));
        // Even with args set on the session, the sandboxed tier has no `arg` to
        // reach them.
        ws.set_program_args(vec![("y".into(), "v".into())]);

        let e = match ws.run(r#"(find-file "x")"#) {
            Err(e) => e,
            Ok(_) => panic!("sandboxed tier must not expose find-file"),
        };
        assert!(
            e.contains("void") && e.contains("find-file"),
            "expected a void/undefined error for find-file, got: {e}"
        );
        let e = match ws.run(r#"(arg "y")"#) {
            Err(e) => e,
            Ok(_) => panic!("sandboxed tier must not expose arg"),
        };
        assert!(
            e.contains("void") && e.contains("arg"),
            "expected a void/undefined error for arg, got: {e}"
        );
        // The remaining file-I/O builtins are absent too.
        for prog in [
            r#"(insert-file-contents "x")"#,
            r#"(write-file "x")"#,
            r#"(write-region 1 2 "x")"#,
            r#"(directory-files "x")"#,
            r#"(args)"#,
        ] {
            assert!(
                ws.run(prog).is_err(),
                "sandboxed tier must not expose {prog}"
            );
        }
    }
}
