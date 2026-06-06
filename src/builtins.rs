//! Editor primitives, registered on a `TulispContext` as Rust closures.
//! Emacs-Lisp names over an implicit current buffer (held in the shared
//! `Session`). M0 subset: navigation, edit, regex search/replace, reporting.
//! Subagents extend this with region/mark, kill-ring, markers, and narrowing.
use crate::engine::{Checkpoint, SharedSession};
use crate::syntax::{Lang, Syntax};
use tulisp::{Error, Shared, TulispContext, TulispConvertible, TulispObject, TulispValue};

fn bad_regex(e: regex::Error) -> Error {
    Error::lisp_error(format!("Invalid regexp: {e}"))
}

/// Compile a regex, caching by pattern string. The search builtins are commonly
/// called many times with a small set of repeated patterns, where `Regex::new`
/// would otherwise dominate those calls. `Regex` is `Arc`-backed, so the cached
/// clone is cheap. The session is single-threaded.
pub(crate) fn cached_regex(re: &str) -> Result<regex::Regex, Error> {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, regex::Regex>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    CACHE.with(|c| {
        if let Some(rx) = c.borrow().get(re) {
            return Ok(rx.clone());
        }
        let rx = regex::Regex::new(re).map_err(bad_regex)?;
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
/// hunk, compute its replacement via `text` (a side's content, or a literal),
/// splice it in place of the whole hunk, and return the REMAINING conflict
/// count. The re-scan keeps that count honest even when a replacement itself
/// contains marker-shaped lines.
fn conflict_splice<F>(s: &SharedSession, n: Option<i64>, text: F) -> Result<i64, Error>
where
    F: FnOnce(&dyn crate::store::TextStore, &crate::conflict::Hunk) -> Result<String, String>,
{
    let mut sess = s.borrow_mut();
    let b = sess.buffer.as_mut();
    let hunks = crate::conflict::scan(b);
    let h = crate::conflict::pick(&hunks, n, b.point()).map_err(|e| err(&e))?;
    let text = text(&*b, h).map_err(|e| err(&e))?;
    let (start, end) = (h.start, h.end);
    b.delete_region(start, end);
    b.goto_char(start);
    b.insert(&text);
    Ok(crate::conflict::scan(b).len() as i64)
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
        let s = session.clone();
        ctx.defun(
            "replace-match",
            move |newtext: String| -> Result<TulispObject, Error> {
                s.borrow_mut()
                    .buffer
                    .replace_match(&newtext)
                    .map_err(Error::lisp_error)?;
                Ok(TulispObject::nil())
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
    {
        let s = session.clone();
        ctx.defun(
            "replace-regexp",
            move |re: String, to: String| -> Result<TulispObject, Error> {
                let rx = cached_regex(&re)?;
                let mut sess = s.borrow_mut();
                let mut count = 0i64;
                loop {
                    let start = sess.buffer.point();
                    if sess.buffer.re_search_forward(&rx, None).is_none() {
                        break;
                    }
                    sess.buffer.replace_match(&to).map_err(Error::lisp_error)?;
                    count += 1;
                    if sess.buffer.point() <= start {
                        break; // no forward progress (empty match) — avoid looping
                    }
                }
                Ok(TulispObject::from(count))
            },
        );
    }
    {
        let s = session.clone();
        ctx.defun(
            "replace-string",
            move |from: String, to: String| -> Result<TulispObject, Error> {
                // Literal replacement: escape backslashes so replace-match does no
                // \N / \& expansion.
                let literal = to.replace('\\', "\\\\");
                let mut sess = s.borrow_mut();
                let mut count = 0i64;
                loop {
                    let start = sess.buffer.point();
                    if sess.buffer.search_forward(&from, None).is_none() {
                        break;
                    }
                    sess.buffer
                        .replace_match(&literal)
                        .map_err(Error::lisp_error)?;
                    count += 1;
                    if sess.buffer.point() <= start {
                        break;
                    }
                }
                Ok(TulispObject::from(count))
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
                loop {
                    let p0 = sess.buffer.point();
                    if sess.buffer.re_search_forward(&rx, bound).is_none() {
                        break;
                    }
                    count += 1;
                    if sess.buffer.point() <= p0 {
                        break;
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
        // (conflict-keep SIDE &optional N) — resolve hunk N (or the hunk at
        // point) by replacing the whole hunk with SIDE: "ours" | "theirs" |
        // "base" | "both" (ours then theirs) | "all" (ours, base, theirs —
        // smerge-keep-all). Returns the remaining conflict count.
        let s = session.clone();
        ctx.defun(
            "conflict-keep",
            move |side: String, n: Option<i64>| -> Result<i64, Error> {
                conflict_splice(&s, n, |b, h| crate::conflict::side_text(b, h, &side))
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
                conflict_splice(&s, n, |_, _| Ok(text.clone()))
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
            for h in hunks.iter().rev() {
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
                    b.delete_region(h.start, h.end);
                    b.goto_char(h.start);
                    b.insert(&text);
                }
            }
            crate::conflict::scan(b).len() as i64
        });
    }

    // ---- structural / AST-aware editing (M7, tree-sitter) ----
    // Markdown, Rust, and Python; the language comes from the buffer name's
    // extension (`Lang::from_buffer_name`), overridable per buffer with
    // `treesit-set-language`, falling back to Markdown (`syntax_of`). Every
    // call re-parses the buffer fresh (incremental re-parse is a TODO in
    // syntax.rs). Node spans are reported in 1-based char positions, like the
    // rest of the builtins. A "defun" is the language's enclosing construct:
    // a Markdown `section`, a Rust `function_item`/`impl_item`/type item, a
    // Python `function_definition`/`class_definition`.
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
            let kind = syntax_of(&sess).root_kind();
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
            let broken = syntax_of(&sess).has_error();
            sess.reports
                .push(("treesit-has-error".to_string(), broken.to_string()));
            broken
        });
    }
    {
        let s = session.clone();
        // (treesit-node-at &optional POS) — the smallest NAMED node covering POS
        // (default point). Reports its type and 1-based char start/end; returns
        // the type string (nil if the tree is empty).
        ctx.defun(
            "treesit-node-at",
            move |pos: Option<i64>| -> Result<TulispObject, Error> {
                let mut sess = s.borrow_mut();
                let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
                let node = syntax_of(&sess).named_node_at(p);
                Ok(match node {
                    Some(n) => {
                        sess.reports
                            .push(("treesit-node-type".to_string(), n.kind.clone()));
                        sess.reports
                            .push(("treesit-node-start".to_string(), n.start.to_string()));
                        sess.reports
                            .push(("treesit-node-end".to_string(), n.end.to_string()));
                        TulispValue::from(n.kind).into_ref(None)
                    }
                    None => TulispObject::nil(),
                })
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
            if let Some(d) = syntax_of(&sess).enclosing_defun(p) {
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
            if let Some(d) = syntax_of(&sess).enclosing_defun(p) {
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
                match syntax_of(&sess).enclosing_defun_name(p) {
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
        // is inside no defun.
        ctx.defun("treesit-narrow-to-defun", move |pos: Option<i64>| -> bool {
            let mut sess = s.borrow_mut();
            let p = pos.map_or_else(|| sess.buffer.point(), |p| p.max(1) as usize);
            match syntax_of(&sess).enclosing_defun(p) {
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
            let defuns = syntax_of(&sess).defuns();
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
            match syntax_of(&sess).find_defun(&name) {
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
        // "@CAPTURE KIND START END" and returns the matching list of
        // "START END" strings (split-string to consume). Errors if the
        // pattern does not compile for the buffer's language.
        ctx.defun(
            "treesit-query",
            move |pattern: String| -> Result<Vec<String>, Error> {
                let mut sess = s.borrow_mut();
                let caps = syntax_of(&sess)
                    .query(&pattern)
                    .map_err(|e| err(&format!("treesit-query: {e}")))?;
                let mut spans = Vec::with_capacity(caps.len());
                for (name, n) in caps {
                    sess.reports.push((
                        "capture".to_string(),
                        format!("@{name} {} {} {}", n.kind, n.start, n.end),
                    ));
                    spans.push(format!("{} {}", n.start, n.end));
                }
                Ok(spans)
            },
        );
    }
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

/// Parse the current buffer for the `treesit-*` builtins (fresh each call).
fn syntax_of(sess: &crate::engine::Session) -> Syntax {
    Syntax::parse(sess.buffer.text(), lang_of(sess))
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
        // make it current; returns the buffer name. If a buffer with that file's
        // name is already open, switch to it instead of opening a duplicate (like
        // Emacs `find-file`). IO errors propagate as a tulisp Error.
        ctx.defun("find-file", move |path: String| -> Result<String, Error> {
            let p = std::path::Path::new(&path);
            // The name a freshly-opened Quire would carry (its file name); reuse
            // an already-open buffer with that name rather than opening it twice.
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            let mut sess = s.borrow_mut();
            if sess.has_buffer(&name) {
                sess.set_buffer(&name).map_err(|e| err(&e))?;
                return Ok(name);
            }
            let store: Box<dyn crate::store::TextStore> = Box::new(
                crate::Quire::open(p).map_err(|e| err(&format!("find-file {path}: {e}")))?,
            );
            Ok(sess.install_buffer(store, true))
        });
    }
    {
        let s = session.clone();
        // (find-file-noselect PATH) — open PATH into a buffer WITHOUT making it
        // current (Emacs `find-file-noselect`); returns the buffer name. Reuses an
        // already-open buffer of the same name, like find-file.
        // TODO: dedup by canonical path, not basename — two files sharing a
        // basename in different dirs collide (the second aliases the first).
        // Fix: key buffers by visited path; uniquify the name (Emacs `doc.txt<2>`).
        // Same limitation in `find-file` above.
        ctx.defun(
            "find-file-noselect",
            move |path: String| -> Result<String, Error> {
                let p = std::path::Path::new(&path);
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                let mut sess = s.borrow_mut();
                if sess.has_buffer(&name) {
                    return Ok(name);
                }
                let store: Box<dyn crate::store::TextStore> = Box::new(
                    crate::Quire::open(p)
                        .map_err(|e| err(&format!("find-file-noselect {path}: {e}")))?,
                );
                Ok(sess.install_buffer(store, false))
            },
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
            let sess = s.borrow();
            let mut written = 0usize;
            crate::safety::write_atomic_with(std::path::Path::new(&path), |w| {
                written = sess.buffer.write_to(w)?;
                Ok(())
            })
            .map_err(|e| err(&format!("write-file {path}: {e}")))?;
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
