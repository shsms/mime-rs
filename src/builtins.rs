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
        ctx.defun("find-file-noselect", move |path: String| -> Result<String, Error> {
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
                crate::Quire::open(p).map_err(|e| err(&format!("find-file-noselect {path}: {e}")))?,
            );
            Ok(sess.install_buffer(store, false))
        });
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
