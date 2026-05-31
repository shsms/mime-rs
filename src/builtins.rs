//! Editor primitives, registered on a `TulispContext` as Rust closures.
//! Emacs-Lisp names over an implicit current buffer (held in the shared
//! `Session`). M0 subset: navigation, edit, regex search/replace, reporting.
//! Subagents extend this with region/mark, kill-ring, markers, and narrowing.
use crate::engine::SharedSession;
use tulisp::{Error, TulispContext, TulispObject, TulispValue};

fn bad_regex(e: regex::Error) -> Error {
    Error::lisp_error(format!("Invalid regexp: {e}"))
}

fn err(msg: &str) -> Error {
    Error::lisp_error(msg.to_string())
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
        ctx.defun("goto-char", move |p: i64| -> i64 {
            let mut b = s.borrow_mut();
            b.buffer.goto_char(p.max(1) as usize);
            b.buffer.point() as i64
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
}
