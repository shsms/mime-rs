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
    /// The content version of the captured state ("equal versions imply equal
    /// text") — lets the undo ring skip duplicate captures.
    pub fn version(&self) -> u64 {
        self.snap.version()
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
    /// Per-buffer language overrides for the `treesit-*` builtins, keyed by
    /// buffer name — set by `(treesit-set-language LANG)`, consulted before
    /// extension detection. A `Vec` + linear scan, like `inactive`: buffer
    /// counts are tiny.
    pub lang_overrides: Vec<(String, crate::syntax::Lang)>,
    /// The persistent tree-sitter parse: the last `Syntax` the `treesit-*`
    /// builtins built, keyed by (language, store content version). The
    /// version is globally unique per text state (see `TextStore::version`),
    /// so it alone identifies the parsed text — no buffer name needed — and
    /// the language catches `treesit-set-language` / rename-driven detection
    /// changes. A run of treesit calls parses once; an edit re-stamps the
    /// version and the next call re-parses. One slot — agents work one
    /// buffer at a time, and a parse is only worth caching while it's hot.
    pub syntax_cache: Option<(crate::syntax::Lang, u64, Rc<crate::syntax::Syntax>)>,
    /// The buffer's content [`version`](crate::store::TextStore::version) at the
    /// last load or save — the point the warm buffer last matched its file on
    /// disk. The buffer is "modified since load/save" exactly when its current
    /// version differs; auto-revert only re-reads an *unmodified* drifted buffer
    /// (a modified one is the genuine conflict the stale-WARN still covers).
    /// Tracks the session's CURRENT buffer. Auto-revert fires only on the
    /// sandboxed MCP path, which is single-buffer, so this is sufficient; a
    /// per-buffer baseline would be needed before extending it to the trusted
    /// tier's inactive/switched buffers.
    pub synced_version: u64,
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
    /// name. Unlike `generate_new_buffer` the name is taken as-is — callers
    /// dedup and uniquify first (the trusted `find-file` reuses via
    /// [`buffer_visiting`] and renames collisions with [`unique_buffer_name`]).
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

    /// The name of the buffer (current or inactive) visiting `path`, if any —
    /// compared canonically when both sides resolve, so `find-file` dedups by
    /// the FILE rather than by basename (two `doc.txt`s in different
    /// directories are different buffers).
    pub fn buffer_visiting(&self, path: &std::path::Path) -> Option<String> {
        let visits = |b: &dyn TextStore| b.file_stamp().is_some_and(|st| same_file(path, &st.path));
        if visits(self.buffer.as_ref()) {
            return Some(self.buffer.name().to_string());
        }
        self.inactive
            .iter()
            .find(|b| visits(b.as_ref()))
            .map(|b| b.name().to_string())
    }

    /// `name` if free, else the first available `name<N>` (N ≥ 2), Emacs-style.
    pub(crate) fn unique_buffer_name(&self, name: &str) -> String {
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
        // The override dies with the buffer — a later buffer reusing the name
        // must get fresh extension detection, not the dead buffer's language.
        self.lang_overrides.retain(|(n, _)| n != name);
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
    /// Whether the most recent FAILED program left the buffer changed —
    /// surfaced by [`failure_context`](Self::failure_context) so a failure
    /// JSON can say whether partial edits persist (a warm run does not roll
    /// back on error; read-only and rehearse sessions do).
    last_failure_dirty: std::cell::Cell<bool>,
    /// Recency stamp the MCP front-end writes on every use — the basis for
    /// least-recently-used eviction when the warm-session cap is hit. The
    /// engine itself never reads it.
    last_used: std::cell::Cell<u64>,
    /// Automatic restore points, newest last — one per distinct text state
    /// captured just before a program runs, so [`undo_last`](Self::undo_last)
    /// can rewind a misfired edit in one call without any checkpoint
    /// discipline up front. Held apart from the session's user checkpoints:
    /// it neither shows in `list-checkpoints` nor is touched by rehearse's
    /// rollback. Bounded to [`UNDO_RING_CAP`]; no redo.
    undo_ring: Vec<Checkpoint>,
}

/// Depth of the automatic undo ring. Snapshots are O(1) for Quire and the
/// states are version-deduped, so this is a safety-net depth, not a cost knob.
const UNDO_RING_CAP: usize = 8;

/// The trust tier a workspace runs at, chosen by the front-end MODE at launch
/// (not by the program). `Sandboxed` (the agent-facing MCP / daemon) registers
/// only the core editing vocabulary; `Trusted` (the local `mime` CLI) also
/// registers the *orchestration* group — multiple buffers, file I/O, directory
/// listing, program arguments.
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
        let synced_version = buffer.version();
        let session: SharedSession = Rc::new(RefCell::new(Session {
            buffer,
            inactive: Vec::new(),
            checkpoints: Vec::new(),
            kill_ring: Vec::new(),
            reports: Vec::new(),
            log: Vec::new(),
            args: Vec::new(),
            lang_overrides: Vec::new(),
            syntax_cache: None,
            synced_version,
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
            last_failure_dirty: std::cell::Cell::new(false),
            last_used: std::cell::Cell::new(0),
            undo_ring: Vec::new(),
        }
    }

    /// Stamp this workspace as just-used (see `last_used`).
    pub fn touch(&self, stamp: u64) {
        self.last_used.set(stamp);
    }

    /// The most recent [`touch`](Self::touch) stamp; 0 = never used.
    pub fn last_used(&self) -> u64 {
        self.last_used.get()
    }

    /// Capture the current buffer state onto the undo ring unless its top
    /// already holds this exact text state (same version) — so read-only
    /// programs and repeated probes don't churn the ring. The MCP front-end
    /// calls this before every (non-rehearse) program.
    pub fn push_undo(&mut self) {
        let v = self.session.borrow().buffer.version();
        if self.undo_ring.last().is_some_and(|c| c.version() == v) {
            return;
        }
        let cp = {
            let s = self.session.borrow();
            Checkpoint::capture(format!("undo-{v}"), s.buffer.as_ref())
        };
        self.undo_ring.push(cp);
        if self.undo_ring.len() > UNDO_RING_CAP {
            self.undo_ring.remove(0);
        }
    }

    /// Rewind the buffer to the most recent undo-ring state and pop it — each
    /// call steps one mutating program further back. The restored snapshot
    /// carries point/mark/narrowing; there is no redo. `Err` when the ring is
    /// empty.
    pub fn undo_last(&mut self) -> Result<(), String> {
        // The top may BE the current state — captured before a read or a
        // program that ended up clean; rewinding to it would be a no-op
        // that burns a step, so skip those first.
        let cur = self.session.borrow().buffer.version();
        while self.undo_ring.last().is_some_and(|c| c.version() == cur) {
            self.undo_ring.pop();
        }
        let cp = self
            .undo_ring
            .pop()
            .ok_or("nothing to undo (no earlier state on the undo ring)")?;
        self.session.borrow_mut().buffer = cp.restore();
        Ok(())
    }

    /// The current buffer's length in chars (whole document).
    pub fn char_len(&self) -> usize {
        self.session.borrow().buffer.char_len()
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

        let (report, value) = match self.eval_and_report(program, false) {
            Ok(rv) => rv,
            // A program that mutated and THEN died must not leave its edits in
            // a read-only session either — restore the snapshot before
            // propagating, and the failure is clean (not dirty).
            Err(e) => {
                if let Some(snap) = guard {
                    self.session.borrow_mut().buffer = snap;
                    self.last_failure_dirty.set(false);
                }
                return Err(e);
            }
        };

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
        // The rollback above means a failed rehearsal never leaves edits; its
        // reports/log DO remain readable via `failure_context`, by design.
        self.last_failure_dirty.set(false);
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
        // Pre-state: a cheap snapshot (structural sharing for Quire; a clone
        // for the in-memory Buffer) is the diff baseline, and the version
        // stamp decides whether any text changed at all — "equal versions
        // imply equal text" (store.rs) — so a CLEAN run (every view / search /
        // occur / read_region) never materializes, copies, or diffs the
        // document. A clean run never grows the add buffer either, so the
        // snapshot costs no copy-on-write there.
        let (snap, version_before, len_before, name) = {
            let mut s = self.session.borrow_mut();
            s.reports.clear();
            s.log.clear();
            (
                s.buffer.snapshot(),
                s.buffer.version(),
                s.buffer.char_len(),
                s.buffer.name().to_string(),
            )
        };

        let value = match self.ctx.eval_string(program) {
            Ok(v) => v.to_string(),
            // Record whether the dying program left edits behind in the
            // primary buffer (a warm run does not roll back), so the failure
            // JSON can say so. An unmoved version proves clean without
            // touching the text; a moved one falls back to the exact compare
            // (an edit-then-revert program is NOT dirty — nothing persists).
            Err(e) => {
                let s = self.session.borrow();
                let b = primary_buffer(&s, &name);
                let dirty = b.version() != version_before && b.text() != snap.text();
                drop(s);
                self.last_failure_dirty.set(dirty);
                return Err(e.format(&self.ctx));
            }
        };

        let s = self.session.borrow();
        let primary = primary_buffer(&s, &name);
        // Same ladder as the failure path: version unmoved ⇒ clean fast path;
        // moved ⇒ exact compare, and only a real text change pays for the
        // diff + the final_text copy.
        let (dirty, diff, final_text) = if primary.version() == version_before {
            (false, String::new(), None)
        } else {
            let before = snap.text();
            let after = primary.text().to_string();
            if after == before {
                (false, String::new(), None)
            } else {
                (true, unified_diff(before, &after), Some(after))
            }
        };
        let report = RunReport {
            buffer_name: name,
            dirty,
            rehearsed,
            diff,
            // The PRIMARY's point, like every other field — pairing the exit
            // buffer's point with the primary's final_text handed consumers a
            // position in a different buffer (possibly out of range).
            point: primary.point(),
            len_before,
            len_after: primary.char_len(),
            reports: s.reports.clone(),
            log: s.log.clone(),
            final_text,
        };
        Ok((report, value))
    }

    /// What the most recent FAILED program left behind: the reports and log it
    /// accumulated before dying (cleared at the start of every run, so they
    /// always belong to the last one; an error does not clear them), plus
    /// whether its edits persist — `true` only for a warm writable run that
    /// mutated before erroring (read-only and rehearse sessions roll back).
    /// The failure JSONs carry all three so diagnostics need not be packed
    /// into the error string.
    pub fn failure_context(&self) -> (Vec<(String, String)>, Vec<String>, bool) {
        let s = self.session.borrow();
        (
            s.reports.clone(),
            s.log.clone(),
            self.last_failure_dirty.get(),
        )
    }

    /// The current buffer's name.
    pub fn buffer_name(&self) -> String {
        self.session.borrow().buffer.name().to_string()
    }

    /// The current buffer's visited file (its `FileStamp` path), if any.
    pub fn visited_path(&self) -> Option<std::path::PathBuf> {
        self.session
            .borrow()
            .buffer
            .file_stamp()
            .map(|st| st.path.clone())
    }

    /// Whether a narrowing restricts the current buffer.
    pub fn is_narrowed(&self) -> bool {
        self.session.borrow().buffer.narrowing().is_some()
    }

    /// Whether the visited file changed on disk since open/rebase (the
    /// stale-read guard's view); `false` for an unvisited buffer. A current
    /// stat catches drift now; the store's sticky `drifted` flag keeps reporting
    /// it after a fresh read already saw the change (so an mtime reset can't
    /// make a corrupted read look clean again).
    pub fn is_stale(&self) -> bool {
        let s = self.session.borrow();
        s.buffer.drifted() || s.buffer.file_stamp().is_some_and(|st| st.check().is_some())
    }

    /// Whether the buffer has unsaved edits since its last load/save (its content
    /// version moved off the synced baseline).
    pub fn is_modified(&self) -> bool {
        let s = self.session.borrow();
        s.buffer.version() != s.synced_version
    }

    /// Auto-revert (Emacs `auto-revert-mode`): if the visited file drifted on
    /// disk and the buffer has NO unsaved edits, silently re-read it so the next
    /// read/edit sees the current file instead of stale-or-corrupt bytes. Returns
    /// whether it reverted. A read-only or MODIFIED buffer is never touched — a
    /// read-only buffer is an unwritable reference the engine must not swap, and
    /// a modified one is the genuine conflict the stale-WARN path covers.
    pub fn auto_revert_if_clean(&mut self) -> bool {
        if self.is_read_only() || self.is_modified() || !self.is_stale() {
            return false;
        }
        revert_in_place(&mut self.session.borrow_mut()).is_ok()
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
    /// Refuses (`Err`) if `path` is the visited file and it changed externally
    /// since open/rebase — see [`stale_visit`]. A failed rebase also leaves the
    /// stamp unrefreshed, so a later save to the same path is conservatively
    /// refused too.
    pub fn save_to(&mut self, path: &std::path::Path) -> std::io::Result<usize> {
        let bytes = {
            let s = self.session.borrow();
            // The stale-read guard: refuse to overwrite the visited file if it
            // drifted under us (an external writer landed since open/rebase).
            // Saving to a *different* path overwrites nothing of theirs — fine.
            if let Some(reason) = stale_visit(s.buffer.as_ref(), path) {
                return Err(std::io::Error::other(format!(
                    "refusing to save: {} was {reason} after it was opened; \
                     (revert-buffer) discards the buffer's edits and re-reads \
                     the file, or save to a different path",
                    path.display()
                )));
            }
            let mut written = 0usize;
            crate::safety::write_atomic_with(path, |w| {
                written = s.buffer.write_to(w)?;
                Ok(())
            })?;
            written
        };
        let _ = self.session.borrow_mut().buffer.rebase_to_file(path);
        // The buffer now matches its file on disk → reset the modified baseline
        // so a clean post-save buffer doesn't read as modified.
        let v = self.session.borrow().buffer.version();
        self.session.borrow_mut().synced_version = v;
        Ok(bytes)
    }
}

/// Re-read the buffer's visited file from disk, discarding the warm buffer's
/// edits — the body shared by the `revert-buffer` builtin and auto-revert. Point
/// is kept by position (clamped to the new content); narrowing and markers are
/// dropped with the old text (the fresh registry is padded so old marker handles
/// read nil rather than aliasing new ones). The buffer keeps its (possibly
/// uniquified) name, and the load/save baseline is reset so the reverted buffer
/// reads as unmodified.
pub(crate) fn revert_in_place(sess: &mut Session) -> Result<(), String> {
    let path = sess
        .buffer
        .file_stamp()
        .map(|st| st.path.clone())
        .ok_or_else(|| "revert-buffer: buffer has no visited file".to_string())?;
    let point = sess.buffer.point();
    let name = sess.buffer.name().to_string();
    let markers = sess.buffer.marker_count();
    let mut store = crate::Quire::open(&path)
        .map_err(|e| format!("revert-buffer: cannot re-read {}: {e}", path.display()))?;
    crate::store::TextStore::set_name(&mut store, &name);
    sess.buffer = Box::new(store);
    for _ in 0..markers {
        sess.buffer.marker_create(None);
    }
    let max = sess.buffer.point_max();
    sess.buffer.goto_char(point.min(max));
    sess.synced_version = sess.buffer.version();
    Ok(())
}

/// The PRIMARY buffer of a run — the one current when the program started,
/// found again by name at exit. The closing report (`diff` / `dirty` /
/// `len_*` / `final_text`) describes IT, not whatever buffer happens to be
/// current when the program ends: a trusted program finishing on another
/// buffer (a find-file'd document, a scratch buffer) used to get the
/// primary's before-text diffed against that other buffer — an --infile run
/// ending on a find-file'd document rendered the whole document as one giant
/// insertion unless the script set-buffer'd home first. A primary killed
/// mid-program falls back to the buffer current at exit rather than diffing
/// against nothing.
fn primary_buffer<'a>(s: &'a Session, name: &str) -> &'a dyn TextStore {
    if s.buffer.name() == name {
        return s.buffer.as_ref();
    }
    s.inactive
        .iter()
        .find(|b| b.name() == name)
        .map(|b| b.as_ref())
        .unwrap_or(s.buffer.as_ref())
}

/// `Some(reason)` when `store` visits `path` and the file on disk has drifted
/// from the stamp recorded at open/rebase — saving there would overwrite an
/// external writer's work. Compared canonically when both sides resolve (the
/// stamp's path is as-given at open time); when either side no longer exists
/// (e.g. the visited file was deleted) an exact path match still counts.
fn stale_visit(store: &dyn TextStore, path: &std::path::Path) -> Option<String> {
    let stamp = store.file_stamp()?;
    if same_file(path, &stamp.path) {
        stamp.check()
    } else {
        None
    }
}

/// One notion of path identity for the whole engine: canonical when both
/// sides resolve (symlinks, `..`), exact otherwise (e.g. a deleted visited
/// file). `find-file` dedup and the stale-save guard MUST agree on this, or a
/// buffer could dedup as visiting a file the guard treats as a different one.
pub(crate) fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
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
    fn closing_report_tracks_the_primary_buffer_not_the_exit_buffer() {
        let mut ws =
            Workspace::new_trusted(Box::new(crate::Buffer::from_string("main", "primary text")));
        // Edit the primary, then END on a different buffer.
        let r = ws
            .run(
                r#"(goto-char (point-max)) (insert "!")
                   (generate-new-buffer "side") (set-buffer "side")
                   (insert "side stuff")"#,
            )
            .unwrap();
        assert_eq!(r.buffer_name, "main");
        assert_eq!(
            r.final_text.as_deref(),
            Some("primary text!"),
            "primary's text, not side's"
        );
        assert_eq!(r.len_before, 12);
        assert_eq!(r.len_after, 13);
        assert_eq!(r.point, 14, "the PRIMARY's point, not the exit buffer's");
        assert!(r.dirty);
        assert!(r.diff.contains("+primary text!"), "diff: {}", r.diff);
        assert!(!r.diff.contains("side stuff"), "diff: {}", r.diff);

        // Come home; a run that edits only OTHER buffers reports its primary
        // ("main") clean — no diff, original text.
        ws.run(r#"(set-buffer "main")"#).unwrap();
        let r = ws.run(r#"(set-buffer "side") (insert "more")"#).unwrap();
        assert_eq!(r.buffer_name, "main");
        assert!(!r.dirty, "main untouched");
        assert_eq!(r.diff, "");
        // A clean run carries no final_text (the fast path never
        // materializes the document); the workspace still has it.
        assert_eq!(r.final_text, None);
        ws.run(r#"(set-buffer "main")"#).unwrap();
        assert_eq!(ws.text(), "primary text!");
    }

    #[test]
    fn treesit_parse_caches_per_content_version() {
        let mut ws = Workspace::new(Box::new(crate::Buffer::from_string("t.md", "# A\nbody\n")));
        ws.run("(treesit-list-defuns)").unwrap();
        let stamp = |ws: &Workspace| {
            let s = ws.session.borrow();
            s.syntax_cache.as_ref().map(|(_, v, _)| *v).unwrap()
        };
        let v1 = stamp(&ws);
        // A second treesit call with no edit reuses the cached parse.
        ws.run("(treesit-root-type)").unwrap();
        assert_eq!(stamp(&ws), v1, "no edit: the parse is reused");
        // An edit re-stamps the content version; the next call re-parses and
        // sees the new structure (no stale tree is served).
        let r = ws
            .run(r##"(goto-char (point-max)) (insert "# B\nmore\n") (treesit-list-defuns)"##)
            .unwrap();
        assert_ne!(stamp(&ws), v1, "edit: a fresh parse");
        assert!(
            r.reports.iter().any(|(_, v)| v.contains("B")),
            "the new section is visible: {:?}",
            r.reports
        );
    }

    #[test]
    fn treesit_reads_the_whole_document_but_motion_respects_narrowing() {
        // The deliberate exception to narrowing composition: the structural
        // layer parses the FULL document (a restriction cutting a function in
        // half must not change what the tree says the function is), while
        // motion clamps into the accessible region and narrow-to-defun
        // REPLACES the restriction like Emacs narrowing commands do.
        let text = "fn one() {\n    1;\n}\n\nfn two() {\n    2;\n}\n";
        let mut ws = Workspace::new(Box::new(crate::Buffer::from_string("t.rs", text)));
        let r = ws
            .run(
                r#"(narrow-to-region 1 12) ; a slice of fn one
                   (goto-char 5)
                   (report "begin" (treesit-beginning-of-defun))
                   (report "end" (treesit-end-of-defun))
                   (report "outline" (length (treesit-list-defuns)))
                   (report "renarrow" (if (treesit-narrow-to-defun 30) 1 0))
                   (report "pmin" (point-min))"#,
            )
            .unwrap();
        assert_eq!(report(&r, "begin"), "1");
        // fn one ends past the restriction; motion clamps to point-max.
        assert_eq!(report(&r, "end"), "12");
        // The outline sees BOTH defuns despite the narrowing.
        assert_eq!(report(&r, "outline"), "2");
        // narrow-to-defun at a position outside the restriction re-narrows
        // there (whole-document by design).
        assert_eq!(report(&r, "renarrow"), "1");
        assert!(
            report(&r, "pmin").parse::<usize>().unwrap() > 12,
            "restriction moved to fn two: {:?}",
            r.reports
        );
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
    fn save_to_refuses_an_externally_changed_visited_file() {
        // The stale-read guard: an external writer lands between open and save;
        // saving over the visited file must fail and leave their bytes intact.
        let tmp = std::env::temp_dir().join(format!("mime-stale-{}.txt", std::process::id()));
        std::fs::write(&tmp, "theirs v1\n").unwrap();
        let mut ws = Workspace::new(Box::new(Quire::open(&tmp).unwrap()));
        ws.run(r#"(goto-char (point-max)) (insert "ours\n")"#)
            .unwrap();

        std::fs::write(&tmp, "theirs v2 (external writer)\n").unwrap();
        let err = ws
            .save_to(&tmp)
            .expect_err("saving over a drifted file must fail");
        assert!(err.to_string().contains("refusing to save"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            "theirs v2 (external writer)\n"
        );

        // Saving the same buffer elsewhere is fine — nothing of theirs is lost.
        let other = std::env::temp_dir().join(format!("mime-stale-b-{}.txt", std::process::id()));
        ws.save_to(&other)
            .expect("saving to a different path is allowed");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), ws.text());

        // After a clean save+rebase cycle the guard re-arms on the new stamp.
        std::fs::remove_file(&tmp).ok();
        std::fs::remove_file(&other).ok();
        let err = ws
            .save_to(&other)
            .expect_err("deleting the now-visited file is drift");
        assert!(err.to_string().contains("deleted"), "got: {err}");
    }

    #[test]
    fn auto_revert_refreshes_a_clean_drifted_buffer_but_not_a_modified_one() {
        let tmp = std::env::temp_dir().join(format!("mime-autorevert-{}.txt", std::process::id()));
        std::fs::write(&tmp, "v1\n").unwrap();
        let mut ws = Workspace::new(Box::new(Quire::open(&tmp).unwrap()));
        assert_eq!(ws.text(), "v1\n");
        assert!(!ws.is_modified(), "fresh open is unmodified");

        // External change, buffer still clean → auto-revert re-reads the file.
        crate::safety::write_atomic(&tmp, b"v2 external\n").unwrap();
        assert!(ws.is_stale(), "drift detected");
        assert!(ws.auto_revert_if_clean(), "a clean + stale buffer reverts");
        assert_eq!(ws.text(), "v2 external\n", "buffer now matches the file");
        assert!(!ws.is_stale(), "fresh stamp after the revert");
        assert!(!ws.is_modified());

        // Now EDIT (→ modified), then drift again → auto-revert must NOT fire.
        ws.run(r#"(goto-char (point-max)) (insert "mine\n")"#)
            .unwrap();
        assert!(ws.is_modified(), "an edit makes it modified");
        crate::safety::write_atomic(&tmp, b"v3 external\n").unwrap();
        assert!(ws.is_stale());
        assert!(
            !ws.auto_revert_if_clean(),
            "a modified buffer is never auto-reverted"
        );
        assert_eq!(ws.text(), "v2 external\nmine\n", "the edits are preserved");
        assert!(ws.is_stale(), "still flagged for the user to resolve");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn save_resets_the_modified_baseline() {
        let tmp = std::env::temp_dir().join(format!("mime-synced-{}.txt", std::process::id()));
        std::fs::write(&tmp, "a\n").unwrap();
        let mut ws = Workspace::new(Box::new(Quire::open(&tmp).unwrap()));
        assert!(!ws.is_modified());
        ws.run(r#"(goto-char (point-max)) (insert "b\n")"#).unwrap();
        assert!(ws.is_modified(), "edit → modified");
        ws.save_to(&tmp).unwrap();
        assert!(!ws.is_modified(), "save → clean baseline");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn auto_revert_never_swaps_a_read_only_buffer() {
        // A read-only buffer is an unwritable reference: auto-revert must not
        // swap it even when its file drifts (that would defeat the contract).
        let tmp = std::env::temp_dir().join(format!("mime-ro-revert-{}.txt", std::process::id()));
        std::fs::write(&tmp, "ref v1\n").unwrap();
        let mut ws = Workspace::new_read_only(Box::new(Quire::open(&tmp).unwrap()));
        crate::safety::write_atomic(&tmp, b"ref v2 external (longer)\n").unwrap();
        assert!(ws.is_stale(), "drift detected");
        assert!(
            !ws.auto_revert_if_clean(),
            "a read-only buffer is never auto-reverted"
        );
        assert_eq!(
            ws.text(),
            "ref v1\n",
            "the read-only reference is untouched"
        );
        assert!(ws.is_stale(), "still flagged on the stale-WARN path");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn failure_context_keeps_a_failed_runs_reports_and_log() {
        let mut ws = Workspace::new(Box::new(Buffer::from_string("main", "x")));
        let e = match ws.run(r#"(report "step" 1) (message "got here") (error "boom")"#) {
            Err(e) => e,
            Ok(_) => panic!("program must fail"),
        };
        assert!(e.contains("boom"), "got: {e}");
        // The diagnostics the program emitted before dying survive the error;
        // a navigate-and-report program leaves no edits → not dirty.
        let (reports, log, dirty) = ws.failure_context();
        assert_eq!(reports, vec![("step".to_string(), "1".to_string())]);
        assert_eq!(log, vec!["got here".to_string()]);
        assert!(!dirty);
        // A program that edits and THEN dies reports its lasting partial edit.
        assert!(
            ws.run(r#"(insert "partial ") (error "late boom")"#)
                .is_err()
        );
        let (_, _, dirty) = ws.failure_context();
        assert!(dirty, "warm runs keep pre-error edits — flagged");
        assert!(ws.text().starts_with("partial "));
        // ...and the next run owns the slate again.
        ws.run("(point)").unwrap();
        let (reports, log, _) = ws.failure_context();
        assert!(reports.is_empty() && log.is_empty());
    }

    #[test]
    fn read_only_rolls_back_a_program_that_edits_then_dies() {
        let mut ws = Workspace::new_read_only(Box::new(Buffer::from_string("ref", "keep me")));
        let e = match ws.run(r#"(insert "EDIT") (error "mid-flight")"#) {
            Err(e) => e,
            Ok(_) => panic!("program must fail"),
        };
        assert!(e.contains("mid-flight"), "got: {e}");
        // The pre-error mutation must not survive in a read-only session...
        assert_eq!(ws.text(), "keep me");
        // ...so the failure is reported clean, not dirty.
        let (_, _, dirty) = ws.failure_context();
        assert!(!dirty);
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
        assert_eq!(r1.final_text.as_deref(), Some("hi!"));
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
        assert_eq!(r1.final_text.as_deref(), Some("hello world"));
        // The 2nd run sees the 1st's edit and diffs against it (not the original).
        let r2 = ws.run(r#"(upcase-region 1 6)"#).unwrap();
        assert_eq!(r2.final_text.as_deref(), Some("HELLO world"));
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
        assert_eq!(r2.final_text.as_deref(), Some("abcXYZ"));
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
        assert_eq!(r2.final_text.as_deref(), Some("worldhello "));
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
        assert_eq!(r.final_text.as_deref(), Some("hello world"));
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
        assert_eq!(r.final_text.as_deref(), Some("hello world"));
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
        assert_eq!(r.final_text.as_deref(), Some("worldhello "));
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
        assert_eq!(r.final_text.as_deref(), Some("keep me EDITED"));
        assert_eq!(ws.text(), "keep me");
    }

    #[test]
    fn regex_replace_loop() {
        let r = run(
            "a world b world",
            r#"(while (re-search-forward "world" nil t) (replace-match "W"))"#,
        );
        assert_eq!(r.final_text.as_deref(), Some("a W b W"));
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
        assert_eq!(r.final_text.as_deref(), Some("XXhello world"));
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
        assert_eq!(r.final_text.as_deref(), Some("X bar X"));
    }

    #[test]
    fn regex_line_anchors_mean_line_boundaries() {
        // Emacs `^` / `$` anchor LINES; the searches must find every line
        // start/end, not just the search window's own ends, and a mid-line
        // point must not pass for a line beginning.
        let r = run(
            "alpha\nbeta\ngamma\n",
            r#"(goto-char 3) ; mid "alpha"
               (report "starts" (count-matches "^[a-z]"))
               (report "ends" (count-matches "[a-z]$"))
               (goto-char 3)
               (report "at-line-start" (if (looking-at "^") 1 0))
               (goto-char 7) ; start of "beta"
               (report "beta-start" (if (looking-at "^beta") 1 0))"#,
        );
        assert_eq!(report(&r, "starts"), "2"); // beta, gamma (from point)
        assert_eq!(report(&r, "ends"), "3"); // a, a, a — every line end
        assert_eq!(report(&r, "at-line-start"), "0");
        assert_eq!(report(&r, "beta-start"), "1");
    }

    #[test]
    fn restriction_edges_are_line_boundaries_and_empty_matches_count_once() {
        // Emacs: point-min counts as a line beginning even mid-line, and a
        // zero-width anchor match counts once per position (no truncation,
        // no double count).
        let r = run(
            "xxfoo\nfoo bar\n",
            r#"(narrow-to-region 3 11)
               (goto-char (point-min))
               (report "bol" (if (looking-at "^foo") 1 0))
               (report "n" (replace-regexp "^foo" "F"))
               (widen)
               (report "starts" (count-matches "^" 1))
               (report "ends" (count-matches "$" 1))"#,
        );
        assert_eq!(report(&r, "bol"), "1", "point-min is a line beginning");
        // Both the restriction-start "foo" and the real line-start one.
        assert_eq!(report(&r, "n"), "2");
        assert_eq!(r.final_text.as_deref(), Some("xxF\nF bar\n"));
        // Three ^ positions (two line starts + the position after the final
        // newline), three $ positions — each zero-width match counted once.
        assert_eq!(report(&r, "starts"), "3");
        assert_eq!(report(&r, "ends"), "3");
    }

    #[test]
    fn count_matches_with_an_end_beyond_point_max_terminates() {
        // The zero-width step limit must clamp to point-max — an oversized
        // END used to make `(count-matches "$" 1 BIG)` loop forever.
        let r = run("abc", r#"(report "n" (count-matches "$" 1 100))"#);
        assert_eq!(report(&r, "n"), "1");
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
        assert_eq!(r.final_text.as_deref(), Some("worldhello "));
    }

    #[test]
    fn erase_buffer_clears() {
        let r = run("abc", "(erase-buffer)");
        assert_eq!(r.final_text.as_deref(), Some(""));
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
        assert_eq!(r.final_text.as_deref(), Some("AAA\nBB"));
    }

    #[test]
    fn match_string_captures_groups() {
        let r = run(
            "John Doe",
            r#"(re-search-forward "(\\w+) (\\w+)" nil t)
               (let ((a (match-string 1)) (b (match-string 2)))
                 (erase-buffer) (insert b) (insert " ") (insert a))"#,
        );
        assert_eq!(r.final_text.as_deref(), Some("Doe John"));
    }

    #[test]
    fn replace_regexp_all() {
        let r = run(
            "a1 b2 c3",
            r##"(goto-char (point-min)) (report "n" (replace-regexp "[0-9]" "#"))"##,
        );
        assert_eq!(r.final_text.as_deref(), Some("a# b# c#"));
        assert_eq!(r.reports[0], ("n".to_string(), "3".to_string()));
    }

    #[test]
    fn replace_string_is_literal() {
        let r = run(
            "foo.bar.baz",
            r#"(goto-char (point-min)) (replace-string "." "/")"#,
        );
        assert_eq!(r.final_text.as_deref(), Some("foo/bar/baz"));
    }

    #[test]
    fn bulk_replace_counts_lands_point_and_respects_anchors_and_markers() {
        let r = run(
            "foo x\nfoo y\nbar foo\n",
            r#"(goto-char 1)
               (let ((m (copy-marker 19))) ; the final "o" of "bar foo"
                 (report "n" (replace-regexp "^foo" "F"))
                 (report "point" (point))
                 (report "marker" (marker-position m))
                 (report "none" (replace-string "absent" "X")))"#,
        );
        // Only the two line-start "foo"s — the third is mid-line.
        assert_eq!(report(&r, "n"), "2");
        assert_eq!(r.final_text.as_deref(), Some("F x\nF y\nbar foo\n"));
        assert_eq!(report(&r, "point"), "6"); // after the second replacement
        assert_eq!(report(&r, "marker"), "15"); // tracked both shrinks
        assert_eq!(report(&r, "none"), "0");
    }

    #[test]
    fn bulk_replace_starts_at_point_and_keeps_replacement_text_unmatched() {
        let r = run(
            "ab ab ab",
            r#"(goto-char 4) ; skip the first "ab"
               (report "n" (replace-string "ab" "ab-ab"))"#,
        );
        // The two matches after point each expand; the "ab"s inside the
        // replacements are not re-matched.
        assert_eq!(report(&r, "n"), "2");
        assert_eq!(r.final_text.as_deref(), Some("ab ab-ab ab-ab"));
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
        // Restoring the pre-edit checkpoint reinstates the snapshot's
        // version stamp, so the run as a whole reports CLEAN: no diff,
        // no final_text — and the buffer text is back to the original.
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "original")));
        let r = ws
            .run(r#"(checkpoint "c1") (erase-buffer) (insert "changed") (restore-checkpoint "c1")"#)
            .unwrap();
        assert!(!r.dirty);
        assert_eq!(r.final_text, None);
        assert_eq!(ws.text(), "original");
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        // The rollback swaps the pre-transaction snapshot (and its version
        // stamp) back in — the run reports clean and the text is intact.
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "keep")));
        let r = ws
            .run(
                r#"(condition-case e
                  (with-transaction (erase-buffer) (insert "gone") (error "boom"))
                (error nil))"#,
            )
            .unwrap();
        assert!(!r.dirty);
        assert_eq!(r.final_text, None);
        assert_eq!(ws.text(), "keep");
    }

    #[test]
    fn transaction_keeps_on_success() {
        let r = run("a", r#"(goto-char 2) (with-transaction (insert "b"))"#);
        assert_eq!(r.final_text.as_deref(), Some("ab"));
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
        assert_eq!(r.final_text.as_deref(), Some("HELLO world"));
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
        // Explicit POS argument inside the H2 body paragraph. The return is a
        // first-class node value; its accessors read it back.
        let p = MD.find("More").unwrap() + 1;
        let r = run(
            MD,
            &format!(
                r#"(let ((n (treesit-node-at {p})))
                     (report "n" n)
                     (report "type" (treesit-node-type n))
                     (report "is-node" (if (treesit-node-p n) 1 0))
                     (report "not-node" (if (treesit-node-p 5) 1 0)))"#
            ),
        );
        assert_eq!(report(&r, "n"), "#<node inline @31..46>");
        assert_eq!(report(&r, "type"), "\"inline\"");
        assert_eq!(report(&r, "is-node"), "1");
        assert_eq!(report(&r, "not-node"), "0");
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

    /// Like [`run`] but with a chosen buffer name — the treesit language tests
    /// need an extension for `Lang::from_buffer_name` to see.
    fn run_named(name: &str, text: &str, program: &str) -> RunReport {
        run_program(Box::new(Buffer::from_string(name, text)), program).expect("program should run")
    }

    // Two tiny code sources for the language-aware M7 builtin tests.
    const RS_SRC: &str =
        "fn alpha() -> i64 {\n    1\n}\n\nfn beta() -> i64 {\n    alpha() + 1\n}\n";
    const PY_SRC: &str = "def f():\n    return 1\n\ndef g():\n    return 2\n";

    #[test]
    fn treesit_language_detected_from_buffer_name() {
        let r = run_named("lib.rs", RS_SRC, "(treesit-language) (treesit-root-type)");
        assert_eq!(report(&r, "treesit-language"), "rust");
        assert_eq!(report(&r, "treesit-root-type"), "source_file");
    }

    #[test]
    fn treesit_set_language_overrides_a_nameless_buffer() {
        // Buffer "t" has no extension → Markdown by default; the override
        // re-parses it as Python.
        let r = run(
            PY_SRC,
            r#"(treesit-set-language "py") (treesit-language) (treesit-root-type)"#,
        );
        assert_eq!(report(&r, "treesit-language"), "python");
        assert_eq!(report(&r, "treesit-root-type"), "module");
        let e = run_program(
            Box::new(Buffer::from_string("t", "x")),
            r#"(treesit-set-language "cobol")"#,
        );
        assert!(e.is_err(), "unknown language must error");
    }

    #[test]
    fn treesit_goto_defun_and_defun_name() {
        let r = run_named(
            "lib.rs",
            RS_SRC,
            r#"(report "p" (treesit-goto-defun "beta")) (report "name" (treesit-defun-name))
               (report "missing" (treesit-goto-defun "gamma"))"#,
        );
        // "fn alpha() -> i64 {\n    1\n}\n\n" is 29 chars; beta starts at 30.
        assert_eq!(report(&r, "p"), "30");
        assert_eq!(report(&r, "name"), "\"beta\"");
        assert_eq!(report(&r, "missing"), "nil");
        assert_eq!(r.point, 30); // the missing lookup left point alone
    }

    #[test]
    fn treesit_narrow_to_defun_scopes_an_edit() {
        // Replace inside `alpha` only: beta's literal `1` is outside the
        // narrowing and must survive.
        let r = run_named(
            "lib.rs",
            RS_SRC,
            r#"(treesit-goto-defun "alpha") (treesit-narrow-to-defun)
               (replace-string "1" "42") (widen)"#,
        );
        assert_eq!(
            r.final_text.as_deref(),
            Some("fn alpha() -> i64 {\n    42\n}\n\nfn beta() -> i64 {\n    alpha() + 1\n}\n")
        );
    }

    #[test]
    fn treesit_list_defuns_outlines_the_buffer() {
        let r = run_named(
            "app.py",
            PY_SRC,
            r#"(report "names" (string-join (treesit-list-defuns) ","))"#,
        );
        assert_eq!(report(&r, "names"), "\"f,g\"");
        // Each defun also self-reports its kind and span.
        assert_eq!(report(&r, "defun"), "function_definition 1 22 f");
    }

    #[test]
    fn treesit_has_error_flags_a_breaking_edit() {
        let r = run_named(
            "lib.rs",
            "fn ok() {}\n",
            r#"(report "before" (if (treesit-has-error) "y" "n"))
               (end-of-buffer) (insert "fn broken(")
               (report "after" (if (treesit-has-error) "y" "n"))"#,
        );
        assert_eq!(report(&r, "before"), "\"n\"");
        assert_eq!(report(&r, "after"), "\"y\"");
    }

    #[test]
    fn treesit_query_runs_structural_search() {
        let r = run_named(
            "app.py",
            PY_SRC,
            r#"(report "n" (length (treesit-query "(function_definition name: (identifier) @fn)")))"#,
        );
        assert_eq!(report(&r, "n"), "2");
        // Captures self-report as "@CAPTURE KIND START END"; `f` is char [5, 6).
        assert_eq!(report(&r, "capture"), "@fn identifier 5 6");
        // A bad pattern is a lisp error, not a panic.
        let e = run_program(
            Box::new(Buffer::from_string("app.py", PY_SRC)),
            r#"(treesit-query "(unbalanced")"#,
        );
        assert!(e.is_err());
    }

    #[test]
    fn clean_runs_report_no_diff_and_no_final_text() {
        // The version-stamp fast path: a run that only navigates / reads
        // must not report a diff or carry final_text — for a warm Quire
        // this is what keeps view/search/occur O(viewport) instead of
        // O(document).
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "hello world")));
        let r = ws
            .run(r#"(goto-char 4) (report "found" (if (search-forward "world" nil t) 1 0))"#)
            .unwrap();
        assert!(!r.dirty);
        assert_eq!(r.diff, "");
        assert_eq!(r.final_text, None);
        assert_eq!(r.len_before, 11);
        assert_eq!(r.len_after, 11);
    }

    #[test]
    fn edit_then_revert_within_one_run_reports_clean() {
        // The version moves (two mutations) but the text round-trips —
        // the exact-compare fallback must report clean, like the old
        // full-text compare did.
        let mut ws = Workspace::new(Box::new(Buffer::from_string("t", "stable")));
        let r = ws
            .run(r#"(goto-char (point-max)) (insert "x") (delete-region (- (point) 1) (point))"#)
            .unwrap();
        assert!(!r.dirty);
        assert_eq!(r.final_text, None);
        assert_eq!(ws.text(), "stable");
    }

    #[test]
    fn quire_clean_and_dirty_runs_match_the_oracle_reports() {
        // Differential guard for the fast path over the file-backed store:
        // Quire and the in-memory oracle must agree on dirty/diff/lens/
        // final_text for a clean read, a real edit, and an edit-and-revert.
        let path = std::env::temp_dir().join(format!("mime-fastpath-{}.txt", std::process::id()));
        std::fs::write(&path, "alpha\nbeta — gamma\n").unwrap();
        let mut oracle =
            Workspace::new(Box::new(Buffer::from_string("t", "alpha\nbeta — gamma\n")));
        let mut quire = Workspace::new(Box::new(Quire::open(&path).unwrap()));
        for prog in [
            r#"(report "hit" (if (search-forward "beta" nil t) 1 0))"#,
            r#"(goto-char (point-min)) (search-forward "alpha" nil t) (insert "!")"#,
            r#"(goto-char (point-max)) (insert "x") (delete-region (- (point) 1) (point))"#,
        ] {
            let o = oracle.run(prog).unwrap();
            let q = quire.run(prog).unwrap();
            assert_eq!(q.dirty, o.dirty, "prog: {prog}");
            assert_eq!(q.diff, o.diff, "prog: {prog}");
            assert_eq!(q.len_before, o.len_before, "prog: {prog}");
            assert_eq!(q.len_after, o.len_after, "prog: {prog}");
            assert_eq!(q.final_text, o.final_text, "prog: {prog}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn re_search_backward_finds_latest_match_and_moves_to_start() {
        let r = run(
            "foo1 bar foo2 baz",
            r#"(end-of-buffer)
               (report "pos" (re-search-backward "foo[0-9]"))
               (replace-match "X")"#,
        );
        // Latest match (foo2 at 10) wins; point lands on its start.
        assert_eq!(report(&r, "pos"), "10");
        assert_eq!(r.final_text.as_deref(), Some("foo1 bar X baz"));
    }

    #[test]
    fn re_search_backward_prefers_latest_start_with_overlaps() {
        // "aa" in "aaa": a leftmost-biased sweep would report 1; the
        // Emacs-style probe must land on the latest start, 2.
        let r = run(
            "aaa",
            r#"(end-of-buffer) (report "pos" (re-search-backward "aa"))"#,
        );
        assert_eq!(report(&r, "pos"), "2");
    }

    #[test]
    fn re_search_backward_honors_bound_and_noerror() {
        let r = run(
            "ab ab ab",
            r#"(end-of-buffer)
               (report "hit" (if (re-search-backward "ab" 5 t) (point) 0))
               (goto-char 3)
               (report "miss" (if (re-search-backward "zz" nil t) 1 0))"#,
        );
        // BOUND is the lower limit of the window [5, point): the latest
        // match inside it starts at 7.
        assert_eq!(report(&r, "hit"), "7");
        assert_eq!(report(&r, "miss"), "0");
    }

    #[test]
    fn re_search_backward_quire_matches_oracle() {
        let path = std::env::temp_dir().join(format!("mime-rsb-{}.txt", std::process::id()));
        let text = "alpha — beta\ngamma alpha délta\nalpha end\n";
        std::fs::write(&path, text).unwrap();
        let mut oracle = Workspace::new(Box::new(Buffer::from_string("t", text)));
        let mut quire = Workspace::new(Box::new(Quire::open(&path).unwrap()));
        let prog = r#"(end-of-buffer)
                      (report "p1" (re-search-backward "alpha"))
                      (report "p2" (re-search-backward "alpha" nil t))
                      (replace-match "OMEGA")"#;
        let o = oracle.run(prog).unwrap();
        let q = quire.run(prog).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(report(&q, "p1"), report(&o, "p1"));
        assert_eq!(report(&q, "p2"), report(&o, "p2"));
        assert_eq!(q.final_text, o.final_text);
        assert_eq!(q.point, o.point);
    }

    #[test]
    fn looking_back_matches_text_ending_at_point() {
        let r = run(
            "hello world",
            r#"(goto-char 6)
               (report "yes" (if (looking-back "hel+o") 1 0))
               (report "no" (if (looking-back "world") 1 0))
               (report "lim" (if (looking-back "hello" 2) 1 0))"#,
        );
        assert_eq!(report(&r, "yes"), "1");
        assert_eq!(report(&r, "no"), "0");
        // Window [2, 6) = "ello" — the full "hello" no longer fits.
        assert_eq!(report(&r, "lim"), "0");
    }

    #[test]
    fn keep_and_flush_lines_filter_from_point() {
        let r = run(
            "head\nkeep a\ndrop b\nkeep c\n",
            r#"(goto-char 6) ; on the "keep a" line
               (report "flushed" (flush-lines "drop"))"#,
        );
        // "head" sits above point's line and is untouched.
        assert_eq!(r.final_text.as_deref(), Some("head\nkeep a\nkeep c\n"));
        assert_eq!(report(&r, "flushed"), "1");

        let r = run(
            "head\nkeep a\ndrop b\nkeep c\n",
            r#"(goto-char 6)
               (report "dropped" (keep-lines "keep"))"#,
        );
        assert_eq!(r.final_text.as_deref(), Some("head\nkeep a\nkeep c\n"));
        assert_eq!(report(&r, "dropped"), "1");
    }

    #[test]
    fn sort_lines_orders_the_region() {
        let r = run("pear\napple\nmango\n", "(sort-lines)");
        assert_eq!(r.final_text.as_deref(), Some("apple\nmango\npear\n"));
        let r = run("pear\napple\nmango\n", "(sort-lines t)");
        assert_eq!(r.final_text.as_deref(), Some("pear\nmango\napple\n"));
    }

    #[test]
    fn kill_whole_line_takes_the_newline_and_yanks_back() {
        let r = run(
            "one\ntwo\nthree\n",
            r#"(goto-char 6) ; mid "two"
               (kill-whole-line)
               (report "after" (buffer-string))
               (end-of-buffer) (yank)"#,
        );
        assert_eq!(report(&r, "after"), "\"one\\nthree\\n\"");
        assert_eq!(r.final_text.as_deref(), Some("one\nthree\ntwo\n"));
    }

    #[test]
    fn indentation_helpers_and_forward_paragraph() {
        let r = run(
            "    indented line\n\nnext para\n",
            r#"(goto-char 9)
               (report "indent" (current-indentation))
               (report "bti" (back-to-indentation))
               (goto-char 1)
               (report "para" (forward-paragraph))"#,
        );
        assert_eq!(report(&r, "indent"), "4");
        assert_eq!(report(&r, "bti"), "5");
        // The paragraph boundary is the start of the blank line after
        // "indented line" (char 19: 17 line chars + its newline).
        assert_eq!(report(&r, "para"), "19");
    }

    #[test]
    fn replace_match_literal_skips_backref_expansion() {
        let r = run(
            "ab ab",
            r#"(re-search-forward "(a)(b)")
               (replace-match "\\1-\\2")          ; expands: a-b
               (re-search-forward "(a)(b)")
               (replace-match "\\1-\\2" nil t)"#, // literal: \1-\2
        );
        assert_eq!(r.final_text.as_deref(), Some("a-b \\1-\\2"));
    }

    #[test]
    fn forward_line_to_a_genuine_top_boundary_is_a_complete_move() {
        let path = std::env::temp_dir().join(format!("mime-fl-{}.txt", std::process::id()));
        let text = "one\ntwo\nthree\n";
        std::fs::write(&path, text).unwrap();
        let mut oracle = Workspace::new(Box::new(Buffer::from_string("t", text)));
        let mut quire = Workspace::new(Box::new(Quire::open(&path).unwrap()));
        let prog = r#"(goto-char 9) ; on "three"
            (report "full" (forward-line -2))   ; lands on line 1 → complete
            (report "stuck" (forward-line -1))  ; already at the start → short
            (report "p" (point))
            (narrow-to-region 5 14)             ; starts at "two" — a real line beginning
            (goto-char 9)
            (report "narrowed" (forward-line -1))
            (report "p2" (point))
            (widen)
            (narrow-to-region 6 14)             ; starts MID-line — unreachable beginning
            (goto-char 9)
            (report "midline" (forward-line -1))
            (widen)"#;
        let o = oracle.run(prog).unwrap();
        let q = quire.run(prog).unwrap();
        std::fs::remove_file(&path).ok();
        for key in ["full", "stuck", "p", "narrowed", "p2", "midline"] {
            assert_eq!(report(&o, key), report(&q, key), "stores agree on {key}");
        }
        assert_eq!(report(&o, "full"), "0", "reaching the buffer start counts");
        assert_eq!(report(&o, "stuck"), "1");
        assert_eq!(report(&o, "p"), "1");
        assert_eq!(
            report(&o, "narrowed"),
            "0",
            "a restriction at a line beginning counts"
        );
        assert_eq!(report(&o, "p2"), "5");
        assert_eq!(
            report(&o, "midline"),
            "1",
            "a mid-line restriction stays short"
        );
    }
}
