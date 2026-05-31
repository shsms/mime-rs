//! Editor primitives, registered on a `TulispContext` as Rust closures.
//! Emacs-Lisp names over an implicit current buffer (held in the shared
//! `Session`). M0 subset: navigation, edit, regex search/replace, reporting.
//! Subagents extend this with region/mark, kill-ring, markers, and narrowing.
use crate::engine::{Checkpoint, SharedSession};
use crate::syntax::Syntax;
use tulisp::{Error, Shared, TulispContext, TulispConvertible, TulispObject, TulispValue};

fn bad_regex(e: regex::Error) -> Error {
    Error::lisp_error(format!("Invalid regexp: {e}"))
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

/// Build an RE2 pattern that matches `needle` case-insensitively and treats any
/// run of whitespace as matching any run of whitespace (mime's `find_fuzzy`).
fn fuzzy_regex(needle: &str) -> String {
    let mut out = String::from("(?i)");
    let mut lit = String::new();
    let mut prev_ws = false;
    for c in needle.chars() {
        if c.is_whitespace() {
            if !lit.is_empty() {
                out.push_str(&regex::escape(&lit));
                lit.clear();
            }
            if !prev_ws {
                out.push_str(r"\s+");
            }
            prev_ws = true;
        } else {
            lit.push(c);
            prev_ws = false;
        }
    }
    if !lit.is_empty() {
        out.push_str(&regex::escape(&lit));
    }
    out
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
        let s = session.clone();
        ctx.defun("buffer-string", move || -> String {
            s.borrow().buffer.text().to_string()
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
                let rx = regex::Regex::new(&re).map_err(bad_regex)?;
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
                let rx = regex::Regex::new(&re).map_err(bad_regex)?;
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
                let mut out = format!(
                    "\u{2014} {}  line {} col {}  point {}/{} \u{2014}\n",
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

    // ---- buffer-level replace commands (map-shaped bulk edits) ----
    {
        let s = session.clone();
        ctx.defun(
            "replace-regexp",
            move |re: String, to: String| -> Result<TulispObject, Error> {
                let rx = regex::Regex::new(&re).map_err(bad_regex)?;
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

    // ---- fuzzy search (case- and whitespace-run-insensitive) ----
    {
        let s = session.clone();
        ctx.defun(
            "search-fuzzy",
            move |needle: String,
                  bound: Option<i64>,
                  noerror: Option<TulispObject>|
                  -> Result<TulispObject, Error> {
                let rx = regex::Regex::new(&fuzzy_regex(&needle)).map_err(bad_regex)?;
                let bound = bound.map(|b| b.max(1) as usize);
                match s.borrow_mut().buffer.re_search_forward(&rx, bound) {
                    Some(p) => Ok(TulispObject::from(p as i64)),
                    None if noerror.is_some_and(|o| o.is_truthy()) => Ok(TulispObject::nil()),
                    None => Err(Error::lisp_error(format!("Fuzzy search failed: {needle}"))),
                }
            },
        );
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
                let rx = regex::Regex::new(&re).map_err(bad_regex)?;
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

    // ---- structural / AST-aware editing (M7, tree-sitter Markdown) ----
    // Every call re-parses the buffer fresh (incremental re-parse is a TODO in
    // syntax.rs). Node spans are reported in 1-based char positions, like the
    // rest of the builtins.
    {
        let s = session.clone();
        // (treesit-root-type) — report and return the parse-tree root kind
        // ("document" for Markdown). Proves the buffer parses.
        ctx.defun("treesit-root-type", move || -> String {
            let mut sess = s.borrow_mut();
            let kind = Syntax::parse(sess.buffer.text()).root_kind();
            sess.reports
                .push(("treesit-root-type".to_string(), kind.clone()));
            kind
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
                let node = Syntax::parse(sess.buffer.text()).named_node_at(p);
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
        // (treesit-beginning-of-defun) — move point to the start of the enclosing
        // top-level construct (a Markdown `section`) and return the new point. If
        // point is in no section, leave it put and return it unchanged.
        ctx.defun("treesit-beginning-of-defun", move || -> i64 {
            let mut sess = s.borrow_mut();
            let p = sess.buffer.point();
            if let Some(sec) = Syntax::parse(sess.buffer.text()).enclosing_section(p) {
                sess.buffer.goto_char(sec.start);
            }
            sess.buffer.point() as i64
        });
    }
    {
        let s = session.clone();
        // (treesit-end-of-defun) — move point to the end of the enclosing
        // top-level construct (a Markdown `section`) and return the new point. If
        // point is in no section, leave it put and return it unchanged.
        ctx.defun("treesit-end-of-defun", move || -> i64 {
            let mut sess = s.borrow_mut();
            let p = sess.buffer.point();
            if let Some(sec) = Syntax::parse(sess.buffer.text()).enclosing_section(p) {
                sess.buffer.goto_char(sec.end);
            }
            sess.buffer.point() as i64
        });
    }
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
}

#[cfg(test)]
mod tests {
    use crate::Workspace;
    use crate::buffer::Buffer;
    use crate::result::RunReport;

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
}
