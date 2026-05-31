//! Run a tulisp program against a buffer, returning a structured result.
//!
//! A [`Workspace`] is the *warm* unit: it owns one `TulispContext` plus the
//! shared [`Session`] and survives many programs, so buffer/checkpoint/kill-ring
//! state and agent-defined `defun`s persist across `run` calls. [`run_program`]
//! is the cold one-shot built on top of it (`Workspace::new(buffer).run(prog)`).
use crate::result::{RunReport, unified_diff};
use crate::store::TextStore;
use std::cell::RefCell;
use std::rc::Rc;
use tulisp::TulispContext;

/// State shared between a running program and the editor builtins. The editor
/// primitives close over an `Rc<RefCell<Session>>` (tulisp is single-threaded,
/// so interior mutability via `RefCell` is sound for these leaf operations).
/// A saved workspace snapshot, backed by `TextStore::snapshot` — O(1)/O(log n)
/// and ~KB for Quire (structural sharing); a full clone for the in-memory Buffer.
pub struct Checkpoint {
    pub label: String,
    snap: Box<dyn TextStore>,
}

impl Checkpoint {
    pub fn capture(label: String, store: &dyn TextStore) -> Self {
        Checkpoint {
            label,
            snap: store.snapshot(),
        }
    }
    /// A fresh, independent store restored from this checkpoint; the checkpoint
    /// stays reusable.
    pub fn restore(&self) -> Box<dyn TextStore> {
        self.snap.snapshot()
    }
    pub fn text(&self) -> String {
        self.snap.text().to_string()
    }
}

pub struct Session {
    pub buffer: Box<dyn TextStore>,
    pub checkpoints: Vec<Checkpoint>,
    pub kill_ring: Vec<String>,
    pub reports: Vec<(String, String)>,
    pub log: Vec<String>,
}

pub type SharedSession = Rc<RefCell<Session>>;

/// A warm editing session: a long-lived `TulispContext` over a shared
/// [`Session`]. The daemon keeps one of these per session id; each
/// [`Workspace::run`] evaluates a program against the accumulated state.
///
/// Because the context is reused, a `defun` defined by one program is callable
/// by the next, and buffer/checkpoint/kill-ring mutations carry over. The
/// context is *not* `Send`/`Sync` (tulisp is single-threaded), so the daemon
/// confines each `Workspace` behind a `Mutex` and a session is a single writer.
pub struct Workspace {
    ctx: TulispContext,
    session: SharedSession,
    /// Read-only sessions reject any program that would change the buffer (the
    /// "reference material attached unwritable" case). The edit is rolled back
    /// from a pre-run snapshot and `run` returns an error instead of a report.
    read_only: bool,
}

impl Workspace {
    /// Build the session + context and register the editor builtins and string
    /// library ONCE. Subsequent `run` calls reuse them — the warm path.
    pub fn new(buffer: Box<dyn TextStore>) -> Workspace {
        Workspace::with_mode(buffer, false)
    }

    /// A read-only workspace: programs may navigate, search, and `report`, but
    /// any program that leaves the buffer modified is rolled back and rejected.
    pub fn new_read_only(buffer: Box<dyn TextStore>) -> Workspace {
        Workspace::with_mode(buffer, true)
    }

    fn with_mode(buffer: Box<dyn TextStore>, read_only: bool) -> Workspace {
        let session: SharedSession = Rc::new(RefCell::new(Session {
            buffer,
            checkpoints: Vec::new(),
            kill_ring: Vec::new(),
            reports: Vec::new(),
            log: Vec::new(),
        }));

        let mut ctx = TulispContext::new();
        crate::builtins::register(&mut ctx, &session);
        crate::strings::register(&mut ctx);

        Workspace {
            ctx,
            session,
            read_only,
        }
    }

    /// Whether this session refuses buffer mutations.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Evaluate `program` against the warm state and return a per-program
    /// report. `reports`/`log` are cleared first so the report reflects only
    /// this run; the diff is "buffer at run start → buffer at run end" (warm,
    /// not against the original file). The error string is the formatted tulisp
    /// error.
    pub fn run(&mut self, program: &str) -> Result<RunReport, String> {
        let (before, name) = {
            let mut s = self.session.borrow_mut();
            s.reports.clear();
            s.log.clear();
            (s.buffer.text().to_string(), s.buffer.name().to_string())
        };
        let len_before = before.chars().count();
        // For a read-only session, keep a cheap pre-run snapshot (structural
        // sharing for Quire) so a mutating program can be rolled back.
        let guard = self
            .read_only
            .then(|| self.session.borrow().buffer.snapshot());

        self.ctx
            .eval_string(program)
            .map_err(|e| e.format(&self.ctx))?;

        // Enforce read-only after the fact: if the program left the buffer
        // changed, restore the snapshot and reject it. (Programs that only
        // navigate/search/report are unaffected.)
        if let Some(snap) = guard
            && self.session.borrow().buffer.text() != before
        {
            self.session.borrow_mut().buffer = snap;
            return Err("session is read-only: program modified the buffer".to_string());
        }

        let s = self.session.borrow();
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

    /// The current buffer text — used by the daemon's `save` op.
    pub fn text(&self) -> String {
        self.session.borrow().buffer.text().to_string()
    }
}

/// Evaluate `program` (Emacs Lisp / tulisp) against `buffer` once, cold; return
/// the diff, reports, and final state. A thin wrapper over [`Workspace`] — the
/// `mimectl --local` one-shot path.
pub fn run_program(buffer: Box<dyn TextStore>, program: &str) -> Result<RunReport, String> {
    Workspace::new(buffer).run(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn run(text: &str, program: &str) -> RunReport {
        run_program(Box::new(Buffer::from_string("t", text)), program).expect("program should run")
    }

    #[test]
    fn read_only_rejects_mutation_and_preserves_buffer() {
        let mut ws = Workspace::new_read_only(Box::new(Buffer::from_string("ref", "keep me")));
        assert!(ws.is_read_only());
        // A mutating program is rejected...
        match ws.run(r#"(goto-char (point-max)) (insert " EDITED")"#) {
            Err(e) => assert!(e.contains("read-only"), "got: {e}"),
            Ok(_) => panic!("read-only session should reject a mutating program"),
        }
        // ...and the buffer is rolled back to its original text.
        assert_eq!(ws.text(), "keep me");
    }

    #[test]
    fn read_only_allows_navigation_and_report() {
        let mut ws = Workspace::new_read_only(Box::new(Buffer::from_string("ref", "hello world")));
        // Pure navigation/search/report does not mutate, so it is permitted.
        let r = ws
            .run(r#"(goto-char 1) (report "found" (if (search-forward "world" nil t) 1 0))"#)
            .expect("read-only allows non-mutating programs");
        assert!(!r.dirty);
        assert_eq!(r.reports, vec![("found".to_string(), "1".to_string())]);
    }

    #[test]
    fn warm_buffer_edits_persist_across_runs() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hello")));
        let r1 = ws
            .run(r#"(goto-char (point-max)) (insert " world")"#)
            .unwrap();
        assert_eq!(r1.final_text, "hello world");
        // The 2nd run sees the 1st's edit and diffs against it (not the original).
        let r2 = ws.run(r#"(upcase-region 1 6)"#).unwrap();
        assert_eq!(r2.final_text, "HELLO world");
        assert_eq!(r2.len_before, 11);
        assert!(r2.diff.contains("-hello world"));
        assert!(r2.diff.contains("+HELLO world"));
    }

    #[test]
    fn warm_defun_is_callable_in_a_later_run() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "abc")));
        // 1st run only defines a helper — no buffer change.
        let r1 = ws.run(r#"(defun shout (s) (insert (upcase s)))"#).unwrap();
        assert!(!r1.dirty);
        // 2nd run calls the warm defun.
        let r2 = ws.run(r#"(goto-char (point-max)) (shout "xyz")"#).unwrap();
        assert_eq!(r2.final_text, "abcXYZ");
    }

    #[test]
    fn warm_reports_and_log_reset_each_run() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "x")));
        let r1 = ws.run(r#"(report "a" 1)"#).unwrap();
        assert_eq!(r1.reports.len(), 1);
        // 2nd run reports nothing — the report list is per-program, not cumulative.
        let r2 = ws.run(r#"(goto-char 1)"#).unwrap();
        assert!(r2.reports.is_empty());
    }

    #[test]
    fn warm_kill_ring_carries_over() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hello world")));
        ws.run(r#"(kill-region 1 7)"#).unwrap(); // "hello " into the kill-ring
        let r2 = ws.run(r#"(goto-char (point-max)) (yank)"#).unwrap();
        assert_eq!(r2.final_text, "worldhello ");
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

    #[test]
    fn forward_word_moves_to_word_ends() {
        let r = run(
            "  foo bar  baz",
            r#"(goto-char 1) (forward-word 2) (report "p" (point))"#,
        );
        assert_eq!(r.reports[0], ("p".to_string(), "10".to_string()));
    }

    #[test]
    fn insert_char_and_newline() {
        let r = run("", r#"(insert-char 65 3) (newline) (insert-char 66 2)"#);
        assert_eq!(r.final_text, "AAA\nBB");
    }

    #[test]
    fn match_string_captures_groups() {
        let r = run(
            "John Doe",
            r#"(re-search-forward "(\\w+) (\\w+)" nil t)
               (let ((a (match-string 1)) (b (match-string 2)))
                 (erase-buffer) (insert b) (insert " ") (insert a))"#,
        );
        assert_eq!(r.final_text, "Doe John");
    }

    #[test]
    fn replace_regexp_all() {
        let r = run(
            "a1 b2 c3",
            r##"(goto-char (point-min)) (report "n" (replace-regexp "[0-9]" "#"))"##,
        );
        assert_eq!(r.final_text, "a# b# c#");
        assert_eq!(r.reports[0], ("n".to_string(), "3".to_string()));
    }

    #[test]
    fn replace_string_is_literal() {
        let r = run(
            "foo.bar.baz",
            r#"(goto-char (point-min)) (replace-string "." "/")"#,
        );
        assert_eq!(r.final_text, "foo/bar/baz");
    }

    #[test]
    fn line_positions_and_goto_line() {
        let r = run(
            "hello\nworld\n",
            r#"(goto-char 9)
               (report "col" (current-column))
               (report "bol" (line-beginning-position))
               (report "eol" (line-end-position))
               (report "g2" (goto-line 2))"#,
        );
        assert_eq!(r.reports[0], ("col".to_string(), "2".to_string()));
        assert_eq!(r.reports[1], ("bol".to_string(), "7".to_string()));
        assert_eq!(r.reports[2], ("eol".to_string(), "12".to_string()));
        assert_eq!(r.reports[3], ("g2".to_string(), "7".to_string()));
    }

    #[test]
    fn checkpoint_and_restore() {
        let r = run(
            "original",
            r#"(checkpoint "c1") (erase-buffer) (insert "changed") (restore-checkpoint "c1")"#,
        );
        assert_eq!(r.final_text, "original");
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let r = run(
            "keep",
            r#"(condition-case e
                  (with-transaction (erase-buffer) (insert "gone") (error "boom"))
                (error nil))"#,
        );
        assert_eq!(r.final_text, "keep");
    }

    #[test]
    fn transaction_keeps_on_success() {
        let r = run("a", r#"(goto-char 2) (with-transaction (insert "b"))"#);
        assert_eq!(r.final_text, "ab");
    }

    #[test]
    fn checkpoint_diff_and_list() {
        let r = run(
            "one",
            r#"(checkpoint "a") (erase-buffer) (insert "two") (checkpoint "b")
               (report "diff" (checkpoint-diff "a" "b"))
               (report "labels" (length (list-checkpoints)))"#,
        );
        assert!(r.reports[0].1.contains("-one"));
        assert!(r.reports[0].1.contains("+two"));
        assert_eq!(r.reports[1], ("labels".to_string(), "2".to_string()));
    }

    #[test]
    fn upcase_region_works() {
        let r = run("hello world", r#"(upcase-region 1 6)"#);
        assert_eq!(r.final_text, "HELLO world");
    }

    #[test]
    fn count_matches_counts() {
        let r = run(
            "a a a a",
            r#"(goto-char (point-min)) (report "n" (count-matches "a"))"#,
        );
        assert_eq!(r.reports[0], ("n".to_string(), "4".to_string()));
    }

    #[test]
    fn search_fuzzy_ignores_case_and_whitespace() {
        let r = run(
            "The   Quick\nBROWN fox",
            r#"(goto-char (point-min)) (search-fuzzy "quick brown") (report "p" (point))"#,
        );
        assert_eq!(r.reports[0], ("p".to_string(), "18".to_string()));
    }
}
