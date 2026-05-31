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
    /// The non-current buffers, in creation order. `buffer` is always *the
    /// current buffer*; switching (`set_buffer`) swaps one of these into it and
    /// stashes the previous current one back here. A `Vec` + linear scan by
    /// `.name()` is plenty — buffer counts are tiny — and it gives `buffer-list`
    /// a stable order. Only the trusted (orchestration) tier ever grows this;
    /// the sandboxed tier always sees exactly one buffer.
    pub inactive: Vec<Box<dyn TextStore>>,
    pub checkpoints: Vec<Checkpoint>,
    pub kill_ring: Vec<String>,
    pub reports: Vec<(String, String)>,
    pub log: Vec<String>,
    /// Program arguments passed in by the trusted CLI: a key→value list in the
    /// order given, read back by the `(arg "KEY")` builtin. A bare `--flag`
    /// stores `(flag, "t")`. Only the trusted (orchestration) tier ever reads
    /// these; the sandboxed agent-facing tier leaves them empty and has no `arg`
    /// builtin to reach them anyway.
    pub args: Vec<(String, String)>,
}

impl Session {
    /// `true` if any buffer (current or inactive) already has this name.
    pub fn has_buffer(&self, name: &str) -> bool {
        self.buffer.name() == name || self.inactive.iter().any(|b| b.name() == name)
    }

    /// The current buffer's name.
    pub fn current_buffer_name(&self) -> String {
        self.buffer.name().to_string()
    }

    /// All buffer names: the current buffer first, then the inactive buffers in
    /// creation order (the stable order `buffer-list` reports).
    pub fn buffer_names(&self) -> Vec<String> {
        std::iter::once(self.buffer.name().to_string())
            .chain(self.inactive.iter().map(|b| b.name().to_string()))
            .collect()
    }

    /// Make the buffer named `name` current. A no-op if it is already current;
    /// otherwise the present current buffer is stashed into `inactive` and the
    /// named one is swapped in (carrying its own point/mark/narrowing, since
    /// those live in the store). `Err` if no such buffer exists.
    pub fn set_buffer(&mut self, name: &str) -> Result<(), String> {
        if self.buffer.name() == name {
            return Ok(());
        }
        let idx = self
            .inactive
            .iter()
            .position(|b| b.name() == name)
            .ok_or_else(|| format!("no buffer named {name}"))?;
        // Swap the target out of `inactive` and the old current in its place,
        // then exchange that slot with the current buffer.
        std::mem::swap(&mut self.buffer, &mut self.inactive[idx]);
        Ok(())
    }

    /// Create an empty in-memory buffer named `name` and register it as
    /// inactive, returning the *actual* name used. If `name` is already taken
    /// (current or inactive), it is uniquified Emacs-style — `name<2>`,
    /// `name<3>`, … — until free. Does NOT switch to it (like Emacs
    /// `generate-new-buffer`).
    pub fn generate_new_buffer(&mut self, name: &str) -> String {
        let actual = self.unique_buffer_name(name);
        self.inactive
            .push(Box::new(crate::Buffer::from_string(actual.clone(), "")));
        actual
    }

    /// Install an already-built store (e.g. a `Quire` opened from a file) as a
    /// buffer, the analog of [`generate_new_buffer`] for a store that exists. If
    /// `make_current`, the present current buffer is stashed into `inactive` and
    /// `store` becomes current; otherwise `store` joins `inactive`. Returns its
    /// name. Unlike `generate_new_buffer` the name is taken as-is (callers that
    /// want reuse-by-name check `has_buffer` first); the trusted `find-file`
    /// builtin does exactly that.
    pub fn install_buffer(&mut self, store: Box<dyn TextStore>, make_current: bool) -> String {
        let name = store.name().to_string();
        if make_current {
            let previous = std::mem::replace(&mut self.buffer, store);
            self.inactive.push(previous);
        } else {
            self.inactive.push(store);
        }
        name
    }

    /// `name` if free, else the first available `name<N>` (N ≥ 2), Emacs-style.
    fn unique_buffer_name(&self, name: &str) -> String {
        if !self.has_buffer(name) {
            return name.to_string();
        }
        (2..)
            .map(|n| format!("{name}<{n}>"))
            .find(|candidate| !self.has_buffer(candidate))
            .expect("an unbounded search always finds a free name")
    }

    /// Remove the inactive buffer named `name`. Killing the *current* buffer is
    /// an error for now (there is no policy yet for choosing its replacement);
    /// `Err` too if no such buffer exists.
    pub fn kill_buffer(&mut self, name: &str) -> Result<(), String> {
        if self.buffer.name() == name {
            return Err(format!("cannot kill the current buffer {name}"));
        }
        let idx = self
            .inactive
            .iter()
            .position(|b| b.name() == name)
            .ok_or_else(|| format!("no buffer named {name}"))?;
        self.inactive.remove(idx);
        Ok(())
    }
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
    /// The trust tier; `Trusted` additionally registers the orchestration builtin
    /// group. Fixed at construction — an agent cannot escalate it.
    capabilities: Capabilities,
}

/// The trust tier a workspace runs at, chosen by the front-end MODE at launch
/// (not by the program). `Sandboxed` (the agent-facing MCP / daemon) registers
/// only the core editing vocabulary; `Trusted` (the local `mime` CLI) also
/// registers the *orchestration* group — multiple buffers, file I/O, directory
/// listing, program arguments. See plan.org §"Single binary & capability tiers".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capabilities {
    Sandboxed,
    Trusted,
}

impl Workspace {
    /// Build the session + context and register the editor builtins and string
    /// library ONCE. Subsequent `run` calls reuse them — the warm path.
    pub fn new(buffer: Box<dyn TextStore>) -> Workspace {
        Workspace::with_mode(buffer, false, Capabilities::Sandboxed)
    }

    /// A read-only workspace: programs may navigate, search, and `report`, but
    /// any program that leaves the buffer modified is rolled back and rejected.
    pub fn new_read_only(buffer: Box<dyn TextStore>) -> Workspace {
        Workspace::with_mode(buffer, true, Capabilities::Sandboxed)
    }

    /// A trusted, writable workspace — the local `mime` CLI tier, which also gets
    /// the orchestration builtin group (multiple buffers, file I/O, arguments).
    pub fn new_trusted(buffer: Box<dyn TextStore>) -> Workspace {
        Workspace::with_mode(buffer, false, Capabilities::Trusted)
    }

    /// This workspace's trust tier.
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Install the program arguments the trusted CLI collected, readable from a
    /// program via `(arg "KEY")`. Each entry is a `(KEY, VALUE)` pair; a bare
    /// `--flag` is stored as `(flag, "t")`. Only meaningful on the trusted tier
    /// (the sandboxed tier registers no `arg` builtin), but harmless to call on
    /// either. Replaces any previously-set arguments.
    pub fn set_program_args(&self, args: Vec<(String, String)>) {
        self.session.borrow_mut().args = args;
    }

    fn with_mode(
        buffer: Box<dyn TextStore>,
        read_only: bool,
        capabilities: Capabilities,
    ) -> Workspace {
        let session: SharedSession = Rc::new(RefCell::new(Session {
            buffer,
            inactive: Vec::new(),
            checkpoints: Vec::new(),
            kill_ring: Vec::new(),
            reports: Vec::new(),
            log: Vec::new(),
            args: Vec::new(),
        }));

        let mut ctx = TulispContext::new();
        crate::builtins::register(&mut ctx, &session);
        crate::strings::register(&mut ctx);
        // The trusted tier additionally gets the orchestration group; the
        // sandboxed agent-facing tier never registers it.
        if capabilities == Capabilities::Trusted {
            crate::builtins::register_orchestration(&mut ctx, &session);
        }

        Workspace {
            ctx,
            session,
            read_only,
            capabilities,
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
        self.run_value(program).map(|(report, _value)| report)
    }

    /// Like [`run`], but also returns the program's final value rendered the way
    /// tulisp prints it (strings quoted, lists as `(a b c)`, `nil` for nil) —
    /// the warm interactive path the `mimectl repl` verb prints alongside the
    /// diff. The buffer effects persist exactly as in [`run`]; only the extra
    /// return value distinguishes the two.
    pub fn run_value(&mut self, program: &str) -> Result<(RunReport, String), String> {
        // For a read-only session, keep a cheap pre-run snapshot (structural
        // sharing for Quire) so a mutating program can be rolled back.
        let guard = self
            .read_only
            .then(|| self.session.borrow().buffer.snapshot());

        let (report, value) = self.eval_and_report(program, false)?;

        // Enforce read-only after the fact: if the program left the buffer
        // changed, restore the snapshot and reject it. (Programs that only
        // navigate/search/report are unaffected.)
        if let Some(snap) = guard
            && report.dirty
        {
            self.session.borrow_mut().buffer = snap;
            return Err("session is read-only: program modified the buffer".to_string());
        }
        Ok((report, value))
    }

    /// Dry-run `program` and return the report it *would* produce, then roll the
    /// session back so nothing persists — the "try before you commit" path. The
    /// returned [`RunReport`] still shows `dirty`/`diff`/`reports`/`log` for the
    /// hypothetical edit (with `rehearsed = true`), but afterwards [`text`] is
    /// unchanged and the kill-ring/checkpoints are exactly as before.
    ///
    /// Rollback reuses the [`Checkpoint`] mechanism: a pre-run `snapshot()` of
    /// the buffer (which carries text *and* point/mark/narrowing, since those
    /// live in the store) is swapped back in, undoing every buffer-level effect
    /// in one move. Session-level state a rehearsal must not keep — entries the
    /// program pushed onto the kill-ring or `checkpoints` — is truncated back to
    /// its pre-run length. A rehearsal works the same whether or not the session
    /// is read-only: it never persists, so there is nothing to reject.
    ///
    /// `defun`s the program defined in the `TulispContext` are intentionally
    /// kept: tulisp has no cheap context rollback, the bindings are harmless
    /// (callable, but inert until something runs them), and keeping them lets an
    /// agent rehearse a helper definition and then a `run` that uses it.
    pub fn rehearse(&mut self, program: &str) -> Result<RunReport, String> {
        // Snapshot everything a rehearsal must restore *before* the program runs.
        let (snap, kill_len, cp_len) = {
            let s = self.session.borrow();
            (s.buffer.snapshot(), s.kill_ring.len(), s.checkpoints.len())
        };

        // Run with the same machinery as `run`; on a tulisp error we still roll
        // back, so a failed rehearsal leaves no trace either.
        let result = self.eval_and_report(program, true);

        let mut s = self.session.borrow_mut();
        s.buffer = snap;
        s.kill_ring.truncate(kill_len);
        s.checkpoints.truncate(cp_len);
        result.map(|(report, _value)| report)
    }

    /// Shared core of [`run`]/[`rehearse`]: clear the per-program `reports`/`log`,
    /// evaluate `program`, and build the [`RunReport`] (diff = buffer-at-start →
    /// buffer-at-end) plus the program's final value rendered as tulisp prints
    /// it. Neither rolls back here — the caller decides whether the effects
    /// persist. `rehearsed` flags the report's origin.
    fn eval_and_report(
        &mut self,
        program: &str,
        rehearsed: bool,
    ) -> Result<(RunReport, String), String> {
        let (before, name) = {
            let mut s = self.session.borrow_mut();
            s.reports.clear();
            s.log.clear();
            (s.buffer.text().to_string(), s.buffer.name().to_string())
        };
        let len_before = before.chars().count();

        let value = self
            .ctx
            .eval_string(program)
            .map_err(|e| e.format(&self.ctx))?
            .to_string();

        let s = self.session.borrow();
        let after = s.buffer.text().to_string();
        let len_after = after.chars().count();
        let report = RunReport {
            buffer_name: name,
            dirty: after != before,
            rehearsed,
            diff: unified_diff(&before, &after),
            point: s.buffer.point(),
            len_before,
            len_after,
            reports: s.reports.clone(),
            log: s.log.clone(),
            final_text: after,
        };
        Ok((report, value))
    }

    /// The current buffer text — used by the daemon's `save` op.
    pub fn text(&self) -> String {
        self.session.borrow().buffer.text().to_string()
    }

    /// Persist the buffer to `path` atomically (temp file + rename), then re-base
    /// the store onto the just-written file so the pre-save mmap backing and the
    /// add buffer are reclaimed (a no-op for the in-memory `Buffer`). Returns the
    /// byte count written. The rebase is best-effort: if re-opening the saved file
    /// fails, the (still correct) pre-save backing is kept and the save stands.
    pub fn save_to(&mut self, path: &std::path::Path) -> std::io::Result<usize> {
        let bytes = {
            let s = self.session.borrow();
            let mut written = 0usize;
            crate::safety::write_atomic_with(path, |w| {
                written = s.buffer.write_to(w)?;
                Ok(())
            })?;
            written
        };
        let _ = self.session.borrow_mut().buffer.rebase_to_file(path);
        Ok(bytes)
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
    use crate::quire::Quire;

    fn run(text: &str, program: &str) -> RunReport {
        run_program(Box::new(Buffer::from_string("t", text)), program).expect("program should run")
    }

    #[test]
    fn warm_quire_matches_oracle_across_separate_runs() {
        // Warm-Quire consistency guard: a session that snapshots between programs
        // (the diff baseline) must stay byte-for-byte equal to the in-memory
        // oracle — both the spine (full_text) and a windowed readout
        // (collect_range, behind buffer-substring/read_region), over a realistic
        // multibyte, mmap-backed document and several snapshot-bracketed edits.
        // NOTE: an *intermittent*, save-safe readout discrepancy was observed
        // editing plan.org through the warm MCP server (buffer-substring/read_region
        // returning the wrong window while full_text/save stayed correct) that this
        // synthetic case does not yet reproduce — tracked in the friction log.
        // Multibyte content throughout (em dashes, tildes) so char positions and
        // byte offsets diverge — the regime where a piece-tree offset bug bites.
        let mut text = String::new();
        for i in 0..800 {
            text.push_str(&format!("line {i:04} — körner ~filler~ ‸content here\n"));
        }
        text.push_str("ANCHOR-TARGET — marker line\n");
        for i in 0..800 {
            text.push_str(&format!("more {i:04} — naïve ~filler~ café content\n"));
        }
        text.push_str("UNIQUE-TAIL-END-SENTINEL\n");

        // Each program is its own warm run, so the Workspace snapshots the buffer
        // between them (sharing Quire's Arc backings), then the next edit grows the
        // add buffer (must copy-on-write off the snapshot's share).
        let progs = [
            r#"(goto-char (point-min)) (search-forward "line 0100" nil t) (insert " INSERTED-ONE ")"#,
            "(buffer-string)", // mirrors save_buffer materializing text()
            r#"(goto-char (point-min)) (search-forward "more 0100" nil t) (insert " INSERTED-TWO ")"#,
            r#"(goto-char (point-min)) (if (search-forward "ANCHOR-TARGET marker line" nil t) (replace-match "REPLACED-ANCHOR-WITH-A-RATHER-LONGER-STRING"))"#,
            "(buffer-string)",
            r#"(goto-char (point-min)) (search-forward "line 0700" nil t) (insert " INSERTED-THREE ")"#,
        ];

        // Quire opened from a real file → mmap-backed original (the open_file path
        // the MCP server uses), which from_string-based tests don't exercise.
        let path =
            std::env::temp_dir().join(format!("mime-warm-regression-{}.txt", std::process::id()));
        std::fs::write(&path, &text).unwrap();

        let mut oracle = Workspace::new(Box::new(Buffer::from_string("t", &text)));
        let mut quire = Workspace::new(Box::new(Quire::open(&path).unwrap()));
        for p in progs {
            oracle.run(p).unwrap();
            quire.run(p).unwrap();
        }
        // The spine (full_text) and a windowed readout (collect_range, the path
        // behind buffer-substring/read_region/search) must both match the oracle.
        let (q, o) = (quire.text().to_string(), oracle.text().to_string());
        let probe = "(message (buffer-substring (max 1 (- (point-max) 300)) (point-max)))";
        let qr = quire
            .run(probe)
            .unwrap()
            .log
            .first()
            .cloned()
            .unwrap_or_default();
        let or = oracle
            .run(probe)
            .unwrap()
            .log
            .first()
            .cloned()
            .unwrap_or_default();
        std::fs::remove_file(&path).ok();
        assert_eq!(q, o, "spine (full_text) diverged from the oracle");
        assert_eq!(
            qr, or,
            "windowed readout (collect_range) diverged from the oracle"
        );
    }

    fn report(r: &RunReport, key: &str) -> String {
        r.reports
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn save_to_persists_rebases_and_edits_again() {
        // save_to writes the buffer and re-bases the Quire onto the new file; the
        // buffer is unchanged, and editing the rebased buffer + saving again keeps
        // disk and buffer in sync (the "keep editing with the saved file as base").
        let tmp = std::env::temp_dir().join(format!("mime-saveto-{}.txt", std::process::id()));
        std::fs::write(&tmp, "alpha\nbeta\ngamma\n").unwrap();
        let mut ws = Workspace::new(Box::new(Quire::open(&tmp).unwrap()));

        ws.run(r#"(goto-char (point-min)) (search-forward "beta" nil t) (insert "_X")"#)
            .unwrap();
        let want = ws.text();
        let n = ws.save_to(&tmp).unwrap();
        assert_eq!(n, want.len());
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            want,
            "disk matches buffer"
        );
        assert_eq!(
            ws.text(),
            want,
            "rebase leaves the buffer content unchanged"
        );

        // Edit the now-rebased buffer and save again — must stay consistent.
        ws.run(r#"(goto-char (point-max)) (insert "omega\n")"#)
            .unwrap();
        let want2 = ws.text();
        ws.save_to(&tmp).unwrap();
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), want2);
        assert_eq!(ws.text(), want2);
        std::fs::remove_file(&tmp).ok();
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
    fn run_value_returns_the_rendered_result_and_persists() {
        // `run_value` (the warm REPL path) returns the program's final value as
        // tulisp prints it — strings quoted, lists parenthesized — and still
        // persists the buffer edit, so a later run sees it.
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hi")));
        let (r1, v1) = ws
            .run_value(r#"(goto-char (point-max)) (insert "!") (+ 1 2)"#)
            .unwrap();
        assert_eq!(v1, "3");
        assert_eq!(r1.final_text, "hi!");
        // A string result renders quoted; a later run sees the persisted edit.
        let (_r2, v2) = ws.run_value(r#"(buffer-string)"#).unwrap();
        assert_eq!(v2, "\"hi!\"");
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
    fn rehearse_reports_the_edit_but_leaves_the_buffer_unchanged() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hello")));
        // The rehearsal's report describes what *would* happen...
        let r = ws
            .rehearse(r#"(goto-char (point-max)) (insert " world") (report "did" 1)"#)
            .unwrap();
        assert!(r.rehearsed);
        assert!(r.dirty);
        assert_eq!(r.final_text, "hello world");
        assert_eq!(r.len_before, 5);
        assert_eq!(r.len_after, 11);
        assert!(r.diff.contains("-hello"));
        assert!(r.diff.contains("+hello world"));
        assert_eq!(r.reports, vec![("did".to_string(), "1".to_string())]);
        // ...but the live buffer is untouched.
        assert_eq!(ws.text(), "hello");
    }

    #[test]
    fn rehearse_then_run_persists_normally() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hello")));
        // A rehearsal first — discarded.
        ws.rehearse(r#"(goto-char (point-max)) (insert " THROWN-AWAY")"#)
            .unwrap();
        assert_eq!(ws.text(), "hello");
        // A normal run after a rehearsal still persists, and sees the original.
        let r = ws
            .run(r#"(goto-char (point-max)) (insert " world")"#)
            .unwrap();
        assert!(!r.rehearsed);
        assert_eq!(r.final_text, "hello world");
        assert_eq!(ws.text(), "hello world");
    }

    #[test]
    fn rehearse_restores_point_mark_and_narrowing() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "abcdefgh")));
        // Establish point/mark/narrowing state the rehearsal will perturb.
        ws.run(r#"(goto-char 3) (set-mark 5)"#).unwrap();
        ws.rehearse(r#"(narrow-to-region 1 4) (goto-char 2) (insert "XYZ")"#)
            .unwrap();
        // Point, mark, and the (un-narrowed) bounds are all back to pre-rehearsal.
        let r = ws
            .run(r#"(report "point" (point)) (report "pmax" (point-max)) (report "mark" (mark))"#)
            .unwrap();
        assert_eq!(report(&r, "point"), "3");
        assert_eq!(report(&r, "pmax"), "9"); // 8 chars + 1, i.e. not narrowed
        assert_eq!(report(&r, "mark"), "5");
        assert_eq!(ws.text(), "abcdefgh");
    }

    #[test]
    fn rehearse_does_not_keep_kill_ring_or_checkpoints() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hello world")));
        // Seed one kill-ring entry and one checkpoint before the rehearsal.
        ws.run(r#"(kill-region 1 7) (checkpoint "keep")"#).unwrap(); // kills "hello "
        // The rehearsal pushes onto both — and must not keep either.
        ws.rehearse(r#"(kill-region 1 4) (checkpoint "throwaway") (checkpoint "throwaway2")"#)
            .unwrap();
        let r = ws
            .run(r#"(report "cps" (list-checkpoints)) (goto-char (point-max)) (yank)"#)
            .unwrap();
        // Only the pre-rehearsal checkpoint remains.
        assert_eq!(report(&r, "cps"), "(\"keep\")");
        // The yank pulled the pre-rehearsal kill ("hello "), not the rehearsal's.
        assert_eq!(r.final_text, "worldhello ");
    }

    #[test]
    fn rehearse_rolls_back_even_when_the_program_errors() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "keep")));
        // A program that mutates then errors: the rehearsal returns Err, but the
        // buffer is still rolled back to its pre-rehearsal text.
        let res = ws.rehearse(r#"(erase-buffer) (insert "gone") (error "boom")"#);
        assert!(res.is_err());
        assert_eq!(ws.text(), "keep");
    }

    #[test]
    fn rehearse_on_read_only_session_still_reports() {
        // A rehearsal never persists, so it is allowed (and useful) even on a
        // read-only session — the agent can preview an edit it could not commit.
        let mut ws = Workspace::new_read_only(Box::new(Buffer::from_string("ref", "keep me")));
        let r = ws
            .rehearse(r#"(goto-char (point-max)) (insert " EDITED")"#)
            .unwrap();
        assert!(r.rehearsed);
        assert!(r.dirty);
        assert_eq!(r.final_text, "keep me EDITED");
        assert_eq!(ws.text(), "keep me");
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
    fn marker_tracks_position_across_edits() {
        // A marker set inside the text rides edits made before it; `goto-char`
        // accepts the marker, and `markerp` tells a marker from an integer.
        let r = run(
            "hello world",
            r#"(let ((m (copy-marker 9)))
                 (goto-char 1)
                 (insert "XX")
                 (report "pos" (marker-position m))
                 (goto-char m)
                 (report "point" (point))
                 (report "is-marker" (if (markerp m) 1 0))
                 (report "not-marker" (if (markerp 5) 1 0)))"#,
        );
        assert_eq!(r.final_text, "XXhello world");
        assert_eq!(report(&r, "pos"), "11"); // 9, shifted right by the 2-char insert
        assert_eq!(report(&r, "point"), "11"); // goto-char followed the marker
        assert_eq!(report(&r, "is-marker"), "1");
        assert_eq!(report(&r, "not-marker"), "0");
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
    fn length_changing_replace_under_narrowing_tracks_point_max() {
        // A replace that changes length inside a restriction must shrink/grow
        // point-max, like insert/delete do — otherwise the bound goes stale and
        // the region silently pulls in text from outside (regression).
        let r = run(
            "AA[xx]BB outside",
            r#"(narrow-to-region 1 7)                ; "AA[xx]" accessible
               (goto-char (point-min)) (replace-string "xx" "")
               (report "pmax" (point-max))
               (report "acc" (buffer-substring (point-min) (point-max)))"#,
        );
        assert_eq!(r.reports[0], ("pmax".to_string(), "5".to_string()));
        assert_eq!(r.reports[1], ("acc".to_string(), "\"AA[]\"".to_string()));
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

    // A small Markdown document the M7 (tree-sitter) builtin tests parse.
    const MD: &str = "# Title\n\nHello para.\n\n## Sub\n\nMore text here.\n";

    #[test]
    fn treesit_root_type_is_document() {
        let r = run(MD, "(report \"root\" (treesit-root-type))");
        // `report` stringifies the returned tulisp value, so a string return
        // renders quoted; the builtin's own self-report (a raw key/value) does not.
        assert_eq!(report(&r, "root"), "\"document\"");
        assert_eq!(report(&r, "treesit-root-type"), "document");
    }

    #[test]
    fn treesit_node_at_reports_type_and_char_span() {
        // Point inside "Title" (char 4) → the heading's inline content,
        // char span [3, 8).
        let r = run(MD, "(goto-char 4) (treesit-node-at)");
        assert_eq!(report(&r, "treesit-node-type"), "inline");
        assert_eq!(report(&r, "treesit-node-start"), "3");
        assert_eq!(report(&r, "treesit-node-end"), "8");
    }

    #[test]
    fn treesit_node_at_accepts_explicit_pos() {
        // Explicit POS argument inside the H2 body paragraph.
        let p = MD.find("More").unwrap() + 1;
        let r = run(MD, &format!("(report \"t\" (treesit-node-at {p}))"));
        // Returns the node type (here the paragraph's inline content); reported
        // through `report` it renders as a quoted tulisp string.
        assert_eq!(report(&r, "t"), "\"inline\"");
        assert_eq!(report(&r, "treesit-node-type"), "inline");
    }

    #[test]
    fn treesit_beginning_and_end_of_defun_move_point() {
        // Point in the H2 body → its enclosing section is "## Sub\n\nMore text
        // here.\n", char [23, 47).
        let p = MD.find("More").unwrap() + 1;
        let r = run(
            MD,
            &format!(
                "(goto-char {p})
                 (report \"beg\" (treesit-beginning-of-defun))
                 (goto-char {p})
                 (report \"end\" (treesit-end-of-defun))"
            ),
        );
        assert_eq!(report(&r, "beg"), "23");
        assert_eq!(report(&r, "end"), "47");
    }

    #[test]
    fn treesit_beginning_of_defun_outside_section_keeps_point() {
        // A buffer that opens with blank lines: point 1 is before any section, so
        // navigation is a no-op and returns point unchanged.
        let r = run(
            "\n\n# H\n\nbody\n",
            "(goto-char 1) (treesit-beginning-of-defun)",
        );
        assert_eq!(r.point, 1);
    }
}
