//! Run a tulisp program against a buffer, returning a structured result.
//!
//! M0 runs cold (a fresh `TulispContext` per program). Warm buffers + warm
//! definitions across programs (the daemon) arrive in M3.
use crate::result::{RunReport, unified_diff};
use crate::store::TextStore;
use std::cell::RefCell;
use std::rc::Rc;
use tulisp::TulispContext;

/// State shared between a running program and the editor builtins. The editor
/// primitives close over an `Rc<RefCell<Session>>` (tulisp is single-threaded,
/// so interior mutability via `RefCell` is sound for these leaf operations).
pub struct Session {
    pub buffer: Box<dyn TextStore>,
    pub kill_ring: Vec<String>,
    pub reports: Vec<(String, String)>,
    pub log: Vec<String>,
}

pub type SharedSession = Rc<RefCell<Session>>;

/// Evaluate `program` (Emacs Lisp / tulisp) against `buffer`; return the diff,
/// reports, and final state. The error string is the formatted tulisp error.
pub fn run_program(buffer: Box<dyn TextStore>, program: &str) -> Result<RunReport, String> {
    let before = buffer.text().to_string();
    let len_before = before.chars().count();
    let name = buffer.name().to_string();

    let session: SharedSession = Rc::new(RefCell::new(Session {
        buffer,
        kill_ring: Vec::new(),
        reports: Vec::new(),
        log: Vec::new(),
    }));

    let mut ctx = TulispContext::new();
    crate::builtins::register(&mut ctx, &session);
    crate::strings::register(&mut ctx);

    ctx.eval_string(program).map_err(|e| e.format(&ctx))?;

    let s = session.borrow();
    let after = s.buffer.text().to_string();
    let len_after = after.chars().count();
    Ok(RunReport {
        buffer_name: name,
        dirty: after != before,
        diff: unified_diff(&before, &after),
        point: s.buffer.point(),
        len_before,
        len_after,
        reports: s.reports.clone(),
        log: s.log.clone(),
        final_text: after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn run(text: &str, program: &str) -> RunReport {
        run_program(Box::new(Buffer::from_string("t", text)), program).expect("program should run")
    }

    #[test]
    fn regex_replace_loop() {
        let r = run(
            "a world b world",
            r#"(while (re-search-forward "world" nil t) (replace-match "W"))"#,
        );
        assert_eq!(r.final_text, "a W b W");
        assert!(r.dirty);
    }

    #[test]
    fn exact_search_replace() {
        let r = run(
            "foo bar foo",
            r#"(while (search-forward "foo" nil t) (replace-match "X"))"#,
        );
        assert_eq!(r.final_text, "X bar X");
    }

    #[test]
    fn mark_and_region() {
        let r = run(
            "hello world",
            r#"(goto-char 1) (set-mark 6) (report "len" (- (region-end) (region-beginning)))"#,
        );
        assert_eq!(r.reports, vec![("len".to_string(), "5".to_string())]);
    }

    #[test]
    fn line_navigation_and_char_after() {
        let r = run(
            "one\ntwo\nthree\n",
            r#"(goto-char 1) (forward-line 2)
               (report "line" (line-number-at-pos (point)))
               (report "ch" (char-after (point)))"#,
        );
        assert_eq!(r.reports[0], ("line".to_string(), "3".to_string()));
        assert_eq!(r.reports[1], ("ch".to_string(), "116".to_string())); // 't'
    }

    #[test]
    fn report_count_via_loop() {
        let r = run(
            "x x x x",
            r#"(let ((n 0))
                 (while (search-forward "x" nil t) (setq n (1+ n)))
                 (report "n" n))"#,
        );
        assert_eq!(r.reports, vec![("n".to_string(), "4".to_string())]);
    }

    #[test]
    fn narrowing_restricts_search() {
        let r = run(
            "aaa BBB aaa",
            r#"(narrow-to-region 5 8) (goto-char (point-min))
               (report "pmin" (point-min)) (report "pmax" (point-max))
               (report "found" (if (search-forward "aaa" nil t) 1 0))
               (widen)"#,
        );
        assert_eq!(r.reports[0], ("pmin".to_string(), "5".to_string()));
        assert_eq!(r.reports[1], ("pmax".to_string(), "8".to_string()));
        assert_eq!(r.reports[2], ("found".to_string(), "0".to_string()));
    }

    #[test]
    fn save_excursion_restores_point() {
        let r = run(
            "hello",
            r#"(goto-char 1)
               (save-excursion (goto-char 5) (report "in" (point)))
               (report "out" (point))"#,
        );
        assert_eq!(r.reports[0], ("in".to_string(), "5".to_string()));
        assert_eq!(r.reports[1], ("out".to_string(), "1".to_string()));
    }

    #[test]
    fn save_restriction_restores() {
        let r = run(
            "aaaaaaa",
            r#"(save-restriction (narrow-to-region 2 4) (report "in" (point-max)))
               (report "out" (point-max))"#,
        );
        assert_eq!(r.reports[0], ("in".to_string(), "4".to_string()));
        assert_eq!(r.reports[1], ("out".to_string(), "8".to_string()));
    }

    #[test]
    fn kill_and_yank() {
        let r = run(
            "hello world",
            r#"(kill-region 1 7) (goto-char (point-max)) (yank)"#,
        );
        assert_eq!(r.final_text, "worldhello ");
    }

    #[test]
    fn erase_buffer_clears() {
        let r = run("abc", "(erase-buffer)");
        assert_eq!(r.final_text, "");
    }
}
