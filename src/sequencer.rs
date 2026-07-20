//! Git sequencer — drives rebase / cherry-pick / revert as a sequence of
//! pick → 3-way-merge → commit steps, entirely in-process via `git2`
//! (libgit2). See todo.org "Rebase / cherry-pick driver".
//!
//! The discriminating capability the whole feature rests on is a 3-way merge
//! that writes conflict markers into the worktree, so a conflicted step routes
//! straight into the existing conflict vocabulary ([`crate::conflict`]) — no
//! new resolution surface.
//!
//! Mechanics: HEAD is detached to a moving `current` tip (starting at `onto`);
//! each step cherry-picks onto `current` via [`git2::Repository::cherrypick`]
//! with a diff3 checkout, so a clean step lands a new commit and a conflicted
//! one leaves markers in the worktree. In-progress state lives in
//! `.git/mime-sequencer.json` so `continue`/`abort` are re-entrant across
//! process restarts; the branch ref only moves at `finish`. `abort` puts HEAD
//! back on the branch at its current tip.
//!
//! Precondition: the operation assumes it is the SOLE writer of the branch for
//! its duration. `begin` refuses on a dirty/colliding worktree or detached
//! HEAD; `finish` refuses to land if the branch moved or was deleted under it
//! (use `abort`). It does NOT defend against a *concurrent* external rewrite
//! racing a single call — an agent driving its own repo, the intended use, is
//! single-writer.
//!
//! Network and arbitrary-code channels are unused by construction: no
//! remotes/transports, no hooks or filters; repos are confined to `MIME_ROOTS`
//! at the tool boundary (see todo.org for the security checklist).

use crate::buffer::Buffer;
use git2::{
    BlameOptions, CherrypickOptions, Delta, DiffFormat, Index, Oid, Repository, ResetType,
    RevertOptions, Sort, build::CheckoutBuilder,
};
use serde_json::json;
use std::path::Path;

type Error = git2::Error;

fn estr(msg: &str) -> Error {
    Error::from_str(msg)
}

/// Hard-reset HEAD/index/worktree to `oid` — the recurring teardown idiom.
fn hard_reset(repo: &Repository, oid: Oid) -> Result<(), Error> {
    repo.reset(&repo.find_object(oid, None)?, ResetType::Hard, None)
}

/// [`hard_reset`], but carry the autostashed paths' CURRENT worktree bytes
/// across the reset. The plan never rewrites these paths, so an edit the user
/// made there during the operation is theirs — the teardown reset must not
/// silently revert it.
fn hard_reset_keeping_autostash(repo: &Repository, oid: Oid, st: &State) -> Result<(), Error> {
    if st.autostash.is_empty() {
        return hard_reset(repo, oid);
    }
    let workdir = repo.workdir().ok_or_else(|| estr("bare repository"))?;
    // Only a clean read or a clean not-found participates — an unreadable
    // path (permissions, transient I/O) is left to the plain reset rather
    // than mistaken for a deletion.
    let read_opt = |p: &str| -> Option<Option<Vec<u8>>> {
        match std::fs::read(workdir.join(p)) {
            Ok(b) => Some(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        }
    };
    let kept: Vec<(String, Option<Vec<u8>>)> = st
        .autostash
        .iter()
        .filter_map(|p| read_opt(p).map(|bytes| (p.clone(), bytes)))
        .collect();
    hard_reset(repo, oid)?;
    for (p, bytes) in kept {
        let abs = workdir.join(&p);
        if read_opt(&p) == Some(bytes.clone()) {
            continue;
        }
        match bytes {
            Some(b) => {
                if let Some(dir) = abs.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&abs, b);
            }
            None => {
                let _ = std::fs::remove_file(&abs);
            }
        }
    }
    Ok(())
}

/// The recovery ref a `begin` stamps with the pre-op tip of `branch` (a full
/// refname). Lives under `refs/mime-backup/` so it stays out of `git branch` yet
/// is trivially recoverable (`git reset --hard <ref>`).
fn backup_ref(branch: &str) -> String {
    backup_slot(branch, 0)
}

/// Slot `n` of the branch's backup ring — /0 is the most recent op's pre-tip.
fn backup_slot(branch: &str, n: usize) -> String {
    format!(
        "refs/mime-backup/{}/{n}",
        branch.strip_prefix("refs/heads/").unwrap_or(branch)
    )
}

/// How many pre-op tips a branch keeps. One rope was not enough: in a
/// multi-rebase session the next op overwrote the only backup, so every op
/// after the first ran without a net.
const BACKUP_RING: usize = 3;

/// Rotate the ring and stamp `orig` as the newest slot. A pre-ring FLAT ref
/// (refs/mime-backup/<branch>) occupies the path the ring's directory needs,
/// so it is folded in as the previous newest and deleted.
fn rotate_backup_ring(repo: &Repository, branch: &str, orig: Oid) -> Result<(), Error> {
    let flat = format!(
        "refs/mime-backup/{}",
        branch.strip_prefix("refs/heads/").unwrap_or(branch)
    );
    let mut slots: Vec<Option<Oid>> = (0..BACKUP_RING)
        .map(|n| repo.refname_to_id(&backup_slot(branch, n)).ok())
        .collect();
    if let Ok(old) = repo.refname_to_id(&flat) {
        slots[0] = Some(old);
        if let Ok(mut r) = repo.find_reference(&flat) {
            r.delete()?;
        }
    }
    for n in (0..BACKUP_RING - 1).rev() {
        if let Some(oid) = slots[n] {
            repo.reference(
                &backup_slot(branch, n + 1),
                oid,
                true,
                "mime sequencer: backup rotate",
            )?;
        }
    }
    repo.reference(
        &backup_slot(branch, 0),
        orig,
        true,
        "mime sequencer: pre-op backup",
    )?;
    Ok(())
}

/// Whether the working tree or index has uncommitted changes to TRACKED files.
/// Untracked/ignored files are allowed, matching what `git rebase` refuses on.
fn is_dirty(repo: &Repository) -> Result<bool, Error> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    Ok(!repo.statuses(Some(&mut opts))?.is_empty())
}

/// What to do with a planned commit. `Squash`/`Fixup` meld into the preceding
/// commit (squash concatenates messages, fixup keeps the first); a leading
/// squash/fixup is rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Pick,
    Edit,
    Reword,
    Squash,
    Fixup,
    Split,
    Drop,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Edit => "edit",
            Action::Reword => "reword",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Split => "split",
            Action::Drop => "drop",
        }
    }
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "pick" => Action::Pick,
            "edit" => Action::Edit,
            "reword" => Action::Reword,
            "squash" => Action::Squash,
            "fixup" => Action::Fixup,
            "split" => Action::Split,
            "drop" => Action::Drop,
            _ => return None,
        })
    }
}

/// One planned step: a commit to replay, how, an optional message change (a full
/// `message` and/or ordered `message_edits`, for reword/squash/fixup/edit), and
/// for a `split` the output parts (`split_into`).
#[derive(Clone, Debug)]
pub struct Step {
    pub commit: Oid,
    pub action: Action,
    pub message: Option<String>,
    pub message_edits: Vec<MsgEdit>,
    pub split_into: Vec<SplitPart>,
}

/// A rebase plan: replay `steps` (in order) onto `onto`.
#[derive(Clone, Debug)]
pub struct Plan {
    pub onto: Oid,
    pub steps: Vec<Step>,
}

/// One edit applied (in order) to a step's commit message — the alternative to
/// retyping the whole message, so the rest (e.g. the sign-off) is preserved.
#[derive(Clone, Debug)]
pub enum MsgEdit {
    /// Replace EVERY occurrence of `find` with `with` (with = "" deletes).
    Replace { find: String, with: String },
    /// Append `text` as a trailing line.
    Append { text: String },
}

/// The raw form the MCP layer extracts from a step's `message_edits`;
/// `MsgEdit::from_spec` validates it into a `MsgEdit`.
pub struct MsgEditSpec {
    pub find: Option<String>,
    pub replace: Option<String>,
    pub append: Option<String>,
}

/// One output commit of a `split`: a message and the paths whose changes go into
/// it. At most one part per split may set `rest` (omit `paths` in the MCP form) to
/// collect every changed path not claimed by another part.
#[derive(Clone, Debug)]
pub struct SplitPart {
    pub message: String,
    pub paths: Vec<String>,
    pub hunks: Vec<HunkSel>,
    pub rest: bool,
}

/// A new-side line span selecting whole diff-hunks of `path`: a hunk of that
/// file joins the part when its post-commit line range overlaps `[lo, hi]`
/// (1-based inclusive) — the line numbers grep/blame hand back. Lets one part
/// claim some of a file's changes and another the rest.
#[derive(Clone, Debug)]
pub struct HunkSel {
    pub path: String,
    pub lo: u32,
    pub hi: u32,
}

/// A raw plan step as the MCP layer extracts it: (commit, action, message,
/// message_edits, split parts). `cmd_rebase` resolves/validates each into a `Step`.
pub type PlanItem = (
    String,
    String,
    Option<String>,
    Vec<MsgEditSpec>,
    Vec<SplitPart>,
);

/// The `autosquash` argument to `cmd_rebase`: an explicit sparse directive
/// list, or marker-driven derivation — git's `--autosquash` proper — from
/// `fixup!`/`squash!` commit subjects in onto..HEAD.
#[derive(Debug, PartialEq, Eq)]
pub enum Autosquash {
    /// Explicit `[{commit, into, action}]` directives (revspecs, unresolved).
    Directives(Vec<(String, String, String)>),
    /// Derive the directives from `fixup!`/`squash!` subject markers.
    Markers,
}

impl MsgEdit {
    pub fn from_spec(spec: &MsgEditSpec) -> Result<MsgEdit, String> {
        match (&spec.find, &spec.append) {
            (Some(find), None) => {
                if find.is_empty() {
                    return Err("message_edits: `find` must be non-empty".to_string());
                }
                Ok(MsgEdit::Replace {
                    find: find.clone(),
                    with: spec.replace.clone().unwrap_or_default(),
                })
            }
            (None, Some(text)) => Ok(MsgEdit::Append { text: text.clone() }),
            (Some(_), Some(_)) => {
                Err("message_edits: an item has both `find` and `append` — use one".to_string())
            }
            (None, None) => Err(
                "message_edits: an item needs `find` (with optional `replace`) or `append`"
                    .to_string(),
            ),
        }
    }
}

/// Apply `edits` to `msg` in order. A `find` that is absent is an error (a
/// typo'd anchor fails loudly rather than silently doing nothing); one that
/// matches replaces EVERY occurrence — zero-or-all, never a silent partial
/// application. Replacements never re-match text they inserted.
fn apply_msg_edits(msg: String, edits: &[MsgEdit]) -> Result<String, Error> {
    let (out, counts) = apply_msg_edits_counted(&msg, edits);
    for (e, n) in edits.iter().zip(&counts) {
        if let (MsgEdit::Replace { find, .. }, 0) = (e, *n) {
            return Err(estr(&format!(
                "message edit: text not found in the commit message: {find:?}"
            )));
        }
    }
    Ok(out)
}

/// The counting core of [`apply_msg_edits`]: per-edit replacement counts
/// instead of the absent-find error — the range rewrite tolerates zero
/// matches in ONE commit (the count says so) and checks range-wide totals
/// itself.
fn apply_msg_edits_counted(msg: &str, edits: &[MsgEdit]) -> (String, Vec<usize>) {
    let mut msg = msg.to_string();
    let mut counts = Vec::with_capacity(edits.len());
    for e in edits {
        match e {
            MsgEdit::Replace { find, with } => {
                let mut n = 0;
                let mut at = 0;
                while let Some(i) = msg[at..].find(find.as_str()) {
                    let pos = at + i;
                    msg.replace_range(pos..pos + find.len(), with);
                    at = pos + with.len();
                    n += 1;
                }
                counts.push(n);
            }
            MsgEdit::Append { text } => {
                if !msg.is_empty() && !msg.ends_with('\n') {
                    msg.push('\n');
                }
                msg.push_str(text);
                if !text.ends_with('\n') {
                    msg.push('\n');
                }
                counts.push(1);
            }
        }
    }
    (msg, counts)
}

/// How a step's diff is applied: forward (rebase/cherry-pick) or inverted
/// (revert). Both stop on conflict and route through the same resolution.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Pick,
    Revert,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Pick => "pick",
            Mode::Revert => "revert",
        }
    }
    fn parse(s: &str) -> Mode {
        match s {
            "revert" => Mode::Revert,
            _ => Mode::Pick,
        }
    }
}

/// The result of `start`/`continue`: either the operation finished (with the
/// new tip), or it stopped on a conflict the agent must resolve.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Done {
        head: Oid,
        /// Autostashed paths whose restore was skipped because the user
        /// edited them during the operation — the parked bytes stay on the
        /// `-autostash` ref. Empty on a clean restore (or no autostash).
        kept: Vec<String>,
    },
    Conflict {
        step: usize,
        files: Vec<String>,
    },
    /// An `edit` step applied and committed; the operation is paused with that
    /// commit checked out for the agent to amend, then `git_continue`.
    Paused {
        step: usize,
        head: Oid,
    },
}

/// A snapshot of an in-progress operation, for `git_status`.
#[derive(Debug)]
pub struct Status {
    pub next: usize,
    pub total: usize,
    pub current: Oid,
    pub conflicts: Vec<String>,
    /// Paused at an `edit` step, waiting for the agent to amend + continue.
    pub editing: bool,
}

/// A dry-run of a plan: the commits it WOULD produce (oldest→newest, with
/// summaries) and the resulting tree, computed entirely in the object DB without
/// moving HEAD/refs or touching the worktree. Unlike a real run, the rehearsal
/// does NOT stop at the first conflicting step: the step's change is skipped and
/// the preview continues, so ONE pass lists every step that needs attention
/// (later conflicts can be knock-on effects of an earlier skipped change).
#[derive(Debug)]
pub struct Preview {
    pub commits: Vec<(Oid, String)>,
    pub final_tree: Oid,
    pub conflicts: Vec<PreviewConflict>,
}

/// One step a real run would stop on, with the WHY an agent needs to repair
/// the plan: per conflicted file, the commit that last set those lines at
/// this point of the replayed history — usually the right fold target.
#[derive(Debug)]
pub struct PreviewConflict {
    pub step: usize,
    pub commit: Oid,
    pub summary: String,
    pub files: Vec<String>,
    /// Rendered "file: last set by <short> <subject>" lines.
    pub why: Vec<String>,
}

/// In-progress operation state, persisted to `.git/mime-sequencer.json`.
struct State {
    branch: String,
    orig: Oid,
    onto: Oid,
    current: Oid,
    next: usize,
    steps: Vec<Step>,
    mode: Mode,
    /// True while paused at a landed `edit` step, awaiting the amend on continue.
    editing: bool,
    /// Paths of uncommitted changes autostashed at begin (their bytes live on
    /// the `-autostash` backup ref); restored by finish/abort. Empty = none.
    autostash: Vec<String>,
}

fn state_path(repo: &Repository) -> std::path::PathBuf {
    repo.path().join("mime-sequencer.json")
}

fn save_state(repo: &Repository, st: &State) -> Result<(), Error> {
    let steps: Vec<_> = st
        .steps
        .iter()
        .map(|s| {
            let edits: Vec<_> = s
                .message_edits
                .iter()
                .map(|e| match e {
                    // Persist with the SAME keys the MCP input uses ({find,
                    // replace, append}) so the three forms can't drift.
                    MsgEdit::Replace { find, with } => json!({"find": find, "replace": with}),
                    MsgEdit::Append { text } => json!({"append": text}),
                })
                .collect();
            let split: Vec<_> = s
                .split_into
                .iter()
                .map(|p| {
                    let hunks: Vec<_> = p
                        .hunks
                        .iter()
                        .map(|h| json!({"path": h.path, "lo": h.lo, "hi": h.hi}))
                        .collect();
                    json!({"message": p.message, "paths": p.paths, "hunks": hunks, "rest": p.rest})
                })
                .collect();
            json!({"commit": s.commit.to_string(), "action": s.action.as_str(), "message": s.message, "message_edits": edits, "split_into": split})
        })
        .collect();
    let v = json!({
        "branch": st.branch,
        "orig": st.orig.to_string(),
        "onto": st.onto.to_string(),
        "current": st.current.to_string(),
        "next": st.next,
        "steps": steps,
        "mode": st.mode.as_str(),
        "editing": st.editing,
        "autostash": st.autostash,
    });
    std::fs::write(state_path(repo), serde_json::to_vec_pretty(&v).unwrap())
        .map_err(|e| estr(&format!("cannot write sequencer state: {e}")))
}

fn load_state(repo: &Repository) -> Result<State, Error> {
    let data =
        std::fs::read(state_path(repo)).map_err(|_| estr("no sequencer operation in progress"))?;
    let v: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|e| estr(&format!("corrupt sequencer state: {e}")))?;
    let oid = |k: &str| -> Result<Oid, Error> {
        Oid::from_str(
            v[k].as_str()
                .ok_or_else(|| estr("corrupt sequencer state"))?,
        )
    };
    let steps = v["steps"]
        .as_array()
        .ok_or_else(|| estr("corrupt sequencer state"))?
        .iter()
        .map(|s| {
            Ok(Step {
                commit: Oid::from_str(s["commit"].as_str().ok_or_else(|| estr("corrupt step"))?)?,
                action: Action::parse(s["action"].as_str().unwrap_or(""))
                    .ok_or_else(|| estr("corrupt step action"))?,
                message: s["message"].as_str().map(str::to_string),
                message_edits: s["message_edits"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|e| {
                                if let Some(t) = e["append"].as_str() {
                                    MsgEdit::Append {
                                        text: t.to_string(),
                                    }
                                } else {
                                    MsgEdit::Replace {
                                        find: e["find"].as_str().unwrap_or("").to_string(),
                                        // `with` is the pre-unification key: fall
                                        // back so a mid-op upgrade doesn't turn a
                                        // replacement into a deletion.
                                        with: e["replace"]
                                            .as_str()
                                            .or_else(|| e["with"].as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                    }
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                split_into: s["split_into"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|p| SplitPart {
                                message: p["message"].as_str().unwrap_or("").to_string(),
                                paths: p["paths"]
                                    .as_array()
                                    .map(|ps| {
                                        ps.iter()
                                            .filter_map(|x| x.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                hunks: p["hunks"]
                                    .as_array()
                                    .map(|hs| {
                                        hs.iter()
                                            .filter_map(|h| {
                                                Some(HunkSel {
                                                    path: h["path"].as_str()?.to_string(),
                                                    lo: h["lo"].as_u64()? as u32,
                                                    hi: h["hi"].as_u64()? as u32,
                                                })
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                rest: p["rest"].as_bool().unwrap_or(false),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(State {
        branch: v["branch"].as_str().unwrap_or("").to_string(),
        orig: oid("orig")?,
        onto: oid("onto")?,
        current: oid("current")?,
        next: v["next"].as_u64().unwrap_or(0) as usize,
        steps,
        mode: Mode::parse(v["mode"].as_str().unwrap_or("pick")),
        editing: v["editing"].as_bool().unwrap_or(false),
        autostash: v["autostash"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The worktree paths a conflicted index touches, de-duplicated, in first-seen
/// order.
fn conflict_paths(index: &Index) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(conflicts) = index.conflicts() {
        for c in conflicts.flatten() {
            if let Some(entry) = c.our.or(c.their).or(c.ancestor)
                && let Ok(p) = std::str::from_utf8(&entry.path)
            {
                let s = p.to_string();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Whether `bytes` still reads as conflicted: a well-formed conflict hunk (the
/// full `<<<<<<<` → `=======` → `>>>>>>>` combination) OR a stray opener line a
/// partial cleanup left behind. Reuses `conflict.rs`'s grammar rather than a
/// substring heuristic, so it understands diff3/run-length and won't fire on,
/// say, a lone `=======` (a Markdown heading). Decodes UTF-8 (lossily only if
/// invalid) — markers are ASCII, so non-UTF-8 content can't hide them.
fn has_conflict_markers(bytes: Vec<u8>) -> bool {
    // Move the Vec into the String on the valid-UTF-8 path (no copy); only the
    // rare non-UTF-8 file pays a lossy re-decode.
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    let (hunks, strays) = crate::conflict::scan_with_strays(&mut Buffer::from_string("", text));
    !hunks.is_empty() || strays > 0
}

/// A diff3-conflict-style checkout builder — the worktree rendering a
/// conflicted step needs so [`crate::conflict`] can parse it.
fn diff3_checkout() -> CheckoutBuilder<'static> {
    let mut co = CheckoutBuilder::new();
    co.force().conflict_style_diff3(true);
    co
}

/// Begin a rebase: replay `plan.steps` onto `plan.onto`, rewriting the current
/// branch to the result. Returns once the plan completes or a step conflicts.
pub fn start(repo: &Repository, plan: Plan) -> Result<Outcome, Error> {
    begin(repo, plan, Mode::Pick)
}

/// Cherry-pick `commits` (in order) onto the current branch tip — a rebase
/// whose base is HEAD, so the new commits append rather than rewrite.
pub fn cherry_pick(repo: &Repository, commits: Vec<Oid>) -> Result<Outcome, Error> {
    let onto = repo.head()?.peel_to_commit()?.id();
    let steps = commits.into_iter().map(pick_step).collect();
    begin(repo, Plan { onto, steps }, Mode::Pick)
}

/// Revert `commits` (in order) on top of the current branch tip — like
/// cherry-pick, but each step applies the commit's inverse.
pub fn revert(repo: &Repository, commits: Vec<Oid>) -> Result<Outcome, Error> {
    let onto = repo.head()?.peel_to_commit()?.id();
    let steps = commits.into_iter().map(pick_step).collect();
    begin(repo, Plan { onto, steps }, Mode::Revert)
}

fn pick_step(commit: Oid) -> Step {
    Step {
        commit,
        action: Action::Pick,
        message: None,
        message_edits: Vec::new(),
        split_into: Vec::new(),
    }
}

fn begin(repo: &Repository, plan: Plan, mode: Mode) -> Result<Outcome, Error> {
    if let Some(first) = plan.steps.iter().find(|s| s.action != Action::Drop)
        && matches!(first.action, Action::Squash | Action::Fixup)
    {
        return Err(estr("the first applied step cannot be squash/fixup"));
    }
    // Pre-validate message_edits before mutating. They only make sense for actions
    // that build a message from a base; reject them on any other action so a
    // silently-dropped edit can't masquerade as applied. For reword/edit the base
    // is known up front (the provided message, else the commit's own), so a typo'd
    // `find` fails BEFORE we mutate rather than stranding a half-applied op.
    // (squash/fixup meld a dynamic base, so their finds are only checked at land.)
    for s in &plan.steps {
        if !s.message_edits.is_empty()
            && !matches!(
                s.action,
                Action::Reword | Action::Edit | Action::Squash | Action::Fixup
            )
        {
            return Err(estr(&format!(
                "message_edits apply only to reword/squash/fixup/edit, not {}",
                s.action.as_str()
            )));
        }
        if matches!(s.action, Action::Reword | Action::Edit) {
            // Dry-run the edits against the same base make_commit will use (the
            // provided message if any, else the commit's own), reusing the exact
            // land-time logic so a bad edit fails BEFORE we mutate. Checking each
            // `find` independently against the static base would disagree with the
            // sequential apply: it would miss a later edit whose anchor an earlier
            // edit deletes, and falsely reject one whose anchor an earlier creates.
            let base = match &s.message {
                Some(m) => m.clone(),
                None => repo
                    .find_commit(s.commit)?
                    .message()
                    .unwrap_or("")
                    .to_string(),
            };
            apply_msg_edits(base, &s.message_edits)?;
        }
        // Best-effort early check: validate the split against the commit's OWN
        // parent→commit diff — the authoritative path set for the common case, so a
        // bad partition usually fails before any mutation. NOT a guarantee: if
        // `onto` or an earlier step already contains some of the change, the
        // replayed net-diff differs and split_commits re-validates against it,
        // surfacing any mismatch mid-op (recoverable via git_abort).
        if s.action == Action::Split {
            let c = repo.find_commit(s.commit)?;
            if c.parent_count() == 0 {
                return Err(estr("split: cannot split a root commit"));
            }
            let base_tree = c.parent(0)?.tree()?;
            let target = c.tree()?;
            if s.split_into.iter().any(|p| !p.hunks.is_empty()) {
                hunk_assignment(repo, &base_tree, &target, &s.split_into)?;
            } else {
                let touched = changed_paths(repo, &base_tree, &target)?;
                split_assignment(&touched, &s.split_into)?;
            }
        }
    }
    if state_path(repo).exists() {
        return Err(estr(
            "a sequencer operation is already in progress (continue or abort it first)",
        ));
    }
    // git2's head().name() is Some("HEAD") when detached, so the name check
    // below can't detect it — ask explicitly. We rewrite the current branch, so
    // a detached HEAD has nothing to land onto.
    if repo.head_detached()? {
        return Err(estr("HEAD is detached — check out a branch first"));
    }
    // Uncommitted work the hard reset below would silently destroy: changes to
    // paths this operation REWRITES refuse (like `git rebase`; restoring their
    // bytes over the rewrite would silently mix old and new content). Unstaged
    // changes confined to untouched paths are AUTOSTASHED instead — parked on
    // the `-autostash` backup ref and restored when the operation finishes or
    // aborts (a path edited again during a pause keeps the later edit).
    let mut autostash_diff = None;
    if is_dirty(repo)? {
        let head_tree = repo.head()?.peel_to_tree()?;
        let mut dopts = git2::DiffOptions::new();
        dopts.context_lines(0);
        let dirty_diff =
            repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut dopts))?;
        let dirty = diff_paths(&dirty_diff);
        let touched = plan_touched_paths(repo, &plan)?;
        let mut clash: Vec<&String> = dirty.intersection(&touched).collect();
        clash.sort();
        if !clash.is_empty() {
            return Err(estr(&format!(
                "the working tree has uncommitted changes to paths this operation \
                 rewrites ({}) — commit them (or fold them in via git_absorb / \
                 git_fixup) first",
                clash
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        // The autostash restores WORKTREE bytes only. Staged changes would
        // come back with the staged/unstaged split flattened — and content
        // that is staged but reverted in the worktree would not be captured
        // at all. Refuse those instead of quietly losing index state.
        let idx_tree_id = repo.index()?.write_tree()?;
        if idx_tree_id != head_tree.id() {
            let idx_tree = repo.find_tree(idx_tree_id)?;
            let staged = repo.diff_tree_to_tree(Some(&head_tree), Some(&idx_tree), None)?;
            let mut names: Vec<String> = diff_paths(&staged).into_iter().collect();
            names.sort();
            return Err(estr(&format!(
                "the index has staged changes ({}) — the autostash cannot carry \
                 staged state; commit or unstage them first",
                names.join(", ")
            )));
        }
        autostash_diff = Some(dirty_diff);
    }
    // is_dirty ignores untracked files, but the hard reset to `onto` would still
    // clobber an untracked file colliding with a path in onto's tree — git rebase
    // refuses that, so we do too.
    let onto_tree = repo.find_commit(plan.onto)?.tree()?;
    let mut uopts = git2::StatusOptions::new();
    uopts
        .include_untracked(true)
        // Recurse so a fully-untracked directory yields per-file entries —
        // a bare `dir/` entry cannot be resolved against the tree (bypath
        // needs a full path), so only recursion lets the checks below see
        // and NAME each colliding file.
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    for e in repo.statuses(Some(&mut uopts))?.iter() {
        if e.status().contains(git2::Status::WT_NEW)
            && let Some(p) = e.path()
        {
            if onto_tree.get_path(Path::new(p)).is_ok() {
                return Err(estr(&format!(
                    "untracked file {p} would be overwritten by the checkout — move or remove it first"
                )));
            }
            // A FILE in onto where the untracked path needs a directory:
            // the checkout would delete the whole untracked directory to
            // write the file. get_path cannot descend through a blob, so
            // check each proper ancestor explicitly.
            let mut anc = Path::new(p).parent();
            while let Some(a) = anc {
                if !a.as_os_str().is_empty()
                    && let Ok(entry) = onto_tree.get_path(a)
                    && entry.kind() != Some(git2::ObjectType::Tree)
                {
                    return Err(estr(&format!(
                        "untracked file {p} would be deleted by the checkout ({} is a \
                         file in the target tree) — move or remove it first",
                        a.display()
                    )));
                }
                anc = a.parent();
            }
        }
    }
    let head = repo.head()?;
    let branch = head
        .name()
        .ok_or_else(|| estr("HEAD has no branch name"))?
        .to_string();
    let orig = head.peel_to_commit()?.id();

    // Stamp a recovery ref at the pre-op tip BEFORE touching anything, so the
    // original branch state is always reachable (history rewriting is otherwise
    // only recoverable via the reflog) — even after a clean finish. A ring of
    // three: /0 this op's pre-tip, /1 and /2 the ops before it, so a
    // multi-rebase session keeps its earlier ropes too.
    rotate_backup_ring(repo, &branch, orig)?;

    // Park the autostash: a full-worktree dangling commit on the -autostash
    // ref carries the dirty files' bytes across processes (a conflict pause
    // may be finished by a later invocation).
    let mut autostash: Vec<String> = Vec::new();
    if let Some(diff) = &autostash_diff {
        let head_commit = repo.find_commit(orig)?;
        let head_tree = head_commit.tree()?;
        let wt_tree_id = apply_subset(repo, &head_tree, diff, |_| true, |_, _, _| true)?;
        let sig = match repo.signature() {
            Ok(s) => s,
            Err(_) => {
                let a = head_commit.author();
                git2::Signature::now(
                    a.name().unwrap_or("mime"),
                    a.email().unwrap_or("mime@invalid"),
                )?
            }
        };
        let stash = repo.commit(
            None,
            &sig,
            &sig,
            "mime autostash (uncommitted changes parked during a rewrite)",
            &repo.find_tree(wt_tree_id)?,
            &[&head_commit],
        )?;
        repo.reference(&autostash_ref(&branch), stash, true, "mime autostash")?;
        autostash = diff_paths(diff).into_iter().collect();
        autostash.sort();
    }

    // Detach onto the base and clean the worktree to it.
    repo.set_head_detached(plan.onto)?;
    hard_reset(repo, plan.onto)?;

    let st = State {
        branch,
        orig,
        onto: plan.onto,
        current: plan.onto,
        next: 0,
        steps: plan.steps,
        mode,
        editing: false,
        autostash,
    };
    save_state(repo, &st)?;
    drive(repo, st)
}

/// Every path a diff mentions (old and new side).
fn diff_paths(diff: &git2::Diff) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for i in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(i) else {
            continue;
        };
        for f in [delta.old_file().path(), delta.new_file().path()] {
            if let Some(p) = f.and_then(|p| p.to_str()) {
                out.insert(p.to_string());
            }
        }
    }
    out
}

/// Every path a plan can possibly rewrite: the net onto↔HEAD diff plus each
/// step commit's own diff (a step can touch a path with zero net change).
fn plan_touched_paths(
    repo: &Repository,
    plan: &Plan,
) -> Result<std::collections::HashSet<String>, Error> {
    let mut out = std::collections::HashSet::new();
    let onto_tree = repo.find_commit(plan.onto)?.tree()?;
    let head_tree = repo.head()?.peel_to_tree()?;
    let d = repo.diff_tree_to_tree(Some(&onto_tree), Some(&head_tree), None)?;
    out.extend(diff_paths(&d));
    for s in &plan.steps {
        let c = repo.find_commit(s.commit)?;
        let parent_tree = c.parent(0).ok().map(|p| p.tree()).transpose()?;
        let d = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&c.tree()?), None)?;
        out.extend(diff_paths(&d));
    }
    Ok(out)
}

/// The ref the autostash bytes are parked on — sibling of the `-worktree`
/// backup ref the worktree fixup uses.
fn autostash_ref(branch: &str) -> String {
    format!(
        "refs/mime-backup/{}-autostash",
        branch
            .strip_prefix("refs/heads/")
            .unwrap_or(branch)
            .replace('/', "-")
    )
}

/// Put the autostashed files back, byte-exact, from the `-autostash` ref
/// (paths absent from the stash tree were deleted in the worktree). Deletes
/// the ref afterwards. A path the user edited DURING the operation keeps the
/// later edit — the parked bytes then stay on the ref, and the caller's
/// result says so. Failures name the paths and leave the ref in place — the
/// bytes stay recoverable. Returns the kept (skipped) paths.
fn restore_autostash(repo: &Repository, st: &State) -> Result<Vec<String>, Error> {
    if st.autostash.is_empty() {
        return Ok(Vec::new());
    }
    let refname = autostash_ref(&st.branch);
    let stash = repo.refname_to_id(&refname).map_err(|_| {
        estr(&format!(
            "the autostash ref {refname} is missing — the parked uncommitted \
             changes cannot be restored ({})",
            st.autostash.join(", ")
        ))
    })?;
    let tree = repo.find_commit(stash)?.tree()?;
    let head_tree = repo.head()?.peel_to_tree()?;
    let workdir = repo.workdir().ok_or_else(|| estr("bare repository"))?;
    let blob_bytes = |entry: &git2::TreeEntry| -> Option<Vec<u8>> {
        repo.find_blob(entry.id())
            .ok()
            .map(|b| b.content().to_vec())
    };
    let mut failed: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for p in &st.autostash {
        let abs = workdir.join(p);
        let stash_entry = tree.get_path(Path::new(p)).ok();
        let stash_bytes = stash_entry.as_ref().and_then(&blob_bytes);
        let wt_bytes = std::fs::read(&abs).ok();
        if wt_bytes == stash_bytes {
            // The bytes already match — but a mode-only change (chmod) still
            // needs the stash entry's mode applied below.
            apply_stash_mode(&abs, stash_entry.as_ref());
            continue;
        }
        let head_bytes = head_tree
            .get_path(Path::new(p))
            .ok()
            .as_ref()
            .and_then(&blob_bytes);
        if wt_bytes != head_bytes {
            // The user changed this path during the operation — their later
            // edit wins; the parked bytes stay recoverable on the ref.
            kept.push(p.clone());
            continue;
        }
        let outcome = match &stash_entry {
            Some(entry) => (|| -> std::io::Result<()> {
                if let Some(dir) = abs.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                std::fs::write(&abs, stash_bytes.as_deref().unwrap_or_default())?;
                apply_stash_mode(&abs, Some(entry));
                Ok(())
            })(),
            None => match std::fs::remove_file(&abs) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        if let Err(e) = outcome {
            failed.push(format!("{p} ({e})"));
        }
    }
    if !failed.is_empty() {
        return Err(estr(&format!(
            "restoring the autostashed changes failed for: {} — the bytes stay \
             recoverable on {refname}",
            failed.join(", ")
        )));
    }
    if kept.is_empty()
        && let Ok(mut r) = repo.find_reference(&refname)
    {
        let _ = r.delete();
    }
    Ok(kept)
}

/// Set a restored file's exec bits to match the stash entry's mode exactly —
/// on for 100755, off otherwise. No-op off unix or when there is no entry.
fn apply_stash_mode(abs: &std::path::Path, entry: Option<&git2::TreeEntry>) {
    #[cfg(unix)]
    if let Some(entry) = entry
        && let Ok(meta) = std::fs::metadata(abs)
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = meta.permissions();
        if entry.filemode() == 0o100755 {
            perm.set_mode(perm.mode() | 0o111);
        } else {
            perm.set_mode(perm.mode() & !0o111);
        }
        let _ = std::fs::set_permissions(abs, perm);
    }
    #[cfg(not(unix))]
    let _ = (abs, entry);
}

/// Dry-run a plan: compute the commits it would produce, entirely in the object
/// DB (loose objects, no ref/worktree change), stopping at the first step that
/// would conflict. Lets a caller preview a reorder/fold — and confirm the result
/// tree is unchanged — before committing to it.
fn rehearse(repo: &Repository, plan: &Plan, mode: Mode) -> Result<Preview, Error> {
    // Mirror begin's leading-squash guard so the preview matches a real run.
    if let Some(first) = plan.steps.iter().find(|s| s.action != Action::Drop)
        && matches!(first.action, Action::Squash | Action::Fixup)
    {
        return Err(estr("the first applied step cannot be squash/fixup"));
    }
    let mut current = plan.onto;
    let mut commits = Vec::new();
    let mut conflicts = Vec::new();
    for (i, step) in plan.steps.iter().enumerate() {
        if step.action == Action::Drop {
            continue;
        }
        let pick = repo.find_commit(step.commit)?;
        let current_commit = repo.find_commit(current)?;
        // In-memory merge — produces an Index, never touches the worktree.
        let mut index = match mode {
            Mode::Pick => repo.cherrypick_commit(&pick, &current_commit, 0, None)?,
            Mode::Revert => repo.revert_commit(&pick, &current_commit, 0, None)?,
        };
        if index.has_conflicts() {
            // Skip the step's change and keep previewing: one rehearsal lists
            // EVERY step that needs attention, not just the first (a real run
            // still stops there). The why names the commit that last set the
            // conflicted lines at this point of the replay — usually the
            // fold target the step should have named.
            let files = conflict_paths(&index);
            // The context the step patches lives in its ORIGINAL history (a
            // fixup is built against the tip, where later commits already
            // reshaped the lines) — so blame from the step's original parent
            // down to the base, not from the half-replayed new history.
            let why = match pick.parent(0) {
                Ok(parent) => {
                    let touchers = last_touchers(repo, parent.id(), plan.onto, &files);
                    files
                        .iter()
                        .filter_map(|f| {
                            touchers.get(f).map(|(o, s)| {
                                format!(
                                    "{f}: the context it patches was last reshaped by {} {s}",
                                    short(*o)
                                )
                            })
                        })
                        .collect()
                }
                Err(_) => Vec::new(),
            };
            conflicts.push(PreviewConflict {
                step: i,
                commit: step.commit,
                summary: pick.summary().unwrap_or("").to_string(),
                files,
                why,
            });
            continue;
        }
        let tree = repo.find_tree(index.write_tree_to(repo)?)?;
        if step.action == Action::Split {
            // Preview the split's output commits (real objects, left dangling).
            let pick = repo.find_commit(step.commit)?;
            for c in build_split(repo, &pick, current, &tree, &step.split_into)? {
                let summary = repo.find_commit(c)?.summary().unwrap_or("").to_string();
                commits.push((c, summary));
                current = c;
            }
        } else {
            let new = make_commit(repo, mode, plan.onto, current, step, &tree)?;
            let summary = repo.find_commit(new)?.summary().unwrap_or("").to_string();
            // A squash/fixup re-parents onto the PREVIOUS commit's parent, so the
            // new commit supersedes it rather than adding one — mirror that in the
            // preview by replacing the last entry, so the list and "-> N commits"
            // count match a real apply (which folds).
            if matches!(step.action, Action::Squash | Action::Fixup) {
                commits.pop();
            }
            commits.push((new, summary));
            current = new;
        }
    }
    Ok(Preview {
        commits,
        final_tree: repo.find_commit(current)?.tree_id(),
        conflicts,
    })
}

/// Which commit (walking `from` down to `stop`, newest-first, capped at 200)
/// last touched each of `paths` — the "why" behind a rehearsal conflict. ONE
/// revwalk with one pathspec'd tree-diff per commit answers every conflicted
/// file of a step.
fn last_touchers(
    repo: &Repository,
    from: Oid,
    stop: Oid,
    paths: &[String],
) -> std::collections::HashMap<String, (Oid, String)> {
    let mut out = std::collections::HashMap::new();
    let mut remaining: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
    let Ok(mut walk) = repo.revwalk() else {
        return out;
    };
    if walk.push(from).is_err()
        || walk.hide(stop).is_err()
        || walk.set_sorting(Sort::TOPOLOGICAL).is_err()
    {
        return out;
    }
    for oid in walk.flatten().take(200) {
        if remaining.is_empty() {
            break;
        }
        let Ok(c) = repo.find_commit(oid) else { break };
        let Ok(tree) = c.tree() else { break };
        let parent_tree = c.parent(0).ok().and_then(|p| p.tree().ok());
        let mut dopts = git2::DiffOptions::new();
        for p in &remaining {
            dopts.pathspec(*p);
        }
        let Ok(diff) = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut dopts))
        else {
            break;
        };
        // Newest-first walk: the FIRST commit that touches a path owns it.
        let summary = c.summary().unwrap_or("").to_string();
        for i in 0..diff.deltas().len() {
            let Some(delta) = diff.get_delta(i) else {
                continue;
            };
            for f in [delta.old_file().path(), delta.new_file().path()] {
                let Some(p) = f.and_then(|p| p.to_str()) else {
                    continue;
                };
                if remaining.remove(p) {
                    out.insert(p.to_string(), (oid, summary.clone()));
                }
            }
        }
    }
    out
}

/// Resume after the agent resolved a conflict in the worktree: stage the
/// resolved paths, commit the stopped step, then continue the plan. `force`
/// skips the conflict-marker guard (for a resolution that legitimately contains
/// marker-like lines, e.g. a diff fixture).
/// Resume from an `edit` pause: fold the agent's worktree changes into the
/// paused commit (an amend), then drive the remaining steps.
fn amend_step(repo: &Repository, mut st: State) -> Result<Outcome, Error> {
    let current = repo.find_commit(st.current)?;
    let parent = current.parent(0)?;
    // Sync the index to the worktree: update_all stages modifications and
    // deletions of tracked files; add_all stages new files (honouring .gitignore).
    let mut index = repo.index()?;
    index.update_all(["*"], None)?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    // Autostashed paths never belong to the plan — a pause-time edit there
    // is the user's live change and must not be folded into this commit
    // (the restore logic keeps it in the worktree instead).
    if !st.autostash.is_empty() {
        let target = repo.find_object(st.current, None)?;
        // reset_default matches pathspecs with fnmatch globbing — escape the
        // metacharacters so a literal path like `notes[1]` matches itself
        // and nothing else.
        let literal: Vec<String> = st
            .autostash
            .iter()
            .map(|p| {
                p.chars()
                    .flat_map(|c| match c {
                        '*' | '?' | '[' | ']' | '\\' => vec!['\\', c],
                        _ => vec![c],
                    })
                    .collect()
            })
            .collect();
        repo.reset_default(Some(&target), &literal)?;
        index = repo.index()?;
    }
    let tree = repo.find_tree(index.write_tree()?)?;
    // Amend in place: same author/committer/message, the worktree's tree, the
    // same parent. An unedited worktree reproduces the identical commit (no-op).
    let amended = repo.commit(
        None,
        &current.author(),
        &current.committer(),
        current.message().unwrap_or(""),
        &tree,
        &[&parent],
    )?;
    repo.set_head_detached(amended)?;
    st.current = amended;
    st.editing = false;
    save_state(repo, &st)?;
    drive(repo, st)
}

pub fn continue_op(repo: &Repository, force: bool) -> Result<Outcome, Error> {
    let mut st = load_state(repo)?;
    if st.editing {
        // Resuming from an `edit` pause: the commit is already landed; amend it
        // with the agent's worktree changes, then continue.
        return amend_step(repo, st);
    }
    let step = st
        .steps
        .get(st.next)
        .ok_or_else(|| estr("nothing to continue"))?
        .clone();

    let workdir = repo
        .workdir()
        .ok_or_else(|| estr("a bare repo has no worktree to resolve in"))?
        .to_path_buf();
    // Stage exactly the conflicted paths from the worktree (like `git add` on
    // the resolved files) — NOT the whole worktree, so unrelated edits or
    // stray markers elsewhere never get folded into this commit.
    let mut index = repo.index()?;
    for path in conflict_paths(&index) {
        let full = workdir.join(&path);
        if full.exists() {
            // Refuse to commit a file that still reads as conflicted —
            // has_conflict_markers parses it (a full hunk OR a stray opener), so
            // a partial cleanup doesn't slip through. Reading must succeed (fail
            // closed); `force` overrides for the rare legitimate-marker resolution.
            if !force {
                let bytes = std::fs::read(&full)
                    .map_err(|e| estr(&format!("cannot read {path} to verify resolution: {e}")))?;
                if has_conflict_markers(bytes) {
                    return Err(estr(&format!(
                        "unresolved conflict markers remain in {path} — resolve them, then continue \
                         (or git_continue {{force: true}} if they are intentional)"
                    )));
                }
            }
            index.add_path(Path::new(&path))?;
        } else {
            // Resolved by removing the file (e.g. a modify/delete conflict kept
            // as a deletion) — add_path would error on the missing file.
            index.remove_path(Path::new(&path))?;
        }
    }
    if index.has_conflicts() {
        return Err(estr(
            "unresolved conflicts remain in the index — resolve them, then continue",
        ));
    }
    let tree = repo.find_tree(index.write_tree()?)?;
    index.write()?;
    let new = land(repo, &st, &step, &tree)?;
    repo.cleanup_state()?;
    st.current = new;
    st.next += 1;
    if step.action == Action::Edit {
        // A conflicted `edit` step: now that it's resolved and landed, pause for
        // the agent to amend it (same as a clean edit step in drive).
        st.editing = true;
        save_state(repo, &st)?;
        return Ok(Outcome::Paused {
            step: st.next - 1,
            head: st.current,
        });
    }
    save_state(repo, &st)?;
    drive(repo, st)
}

/// Skip the stopped step: discard its (conflicted) merge, then continue the
/// plan as if that commit had been dropped.
pub fn skip(repo: &Repository) -> Result<Outcome, Error> {
    let mut st = load_state(repo)?;
    if st.editing {
        // Skipping an edit pause = abandon the pending worktree edits and resume,
        // leaving the landed commit unchanged.
        hard_reset_keeping_autostash(repo, st.current, &st)?;
        let _ = repo.cleanup_state();
        st.editing = false;
        save_state(repo, &st)?;
        return drive(repo, st);
    }
    if st.next >= st.steps.len() {
        return Err(estr("nothing to skip"));
    }
    // Discard the in-progress merge residue, back to the last good tip.
    hard_reset_keeping_autostash(repo, st.current, &st)?;
    let _ = repo.cleanup_state();
    st.next += 1;
    save_state(repo, &st)?;
    drive(repo, st)
}

/// Abort: drop the replay and put HEAD back on the branch at its CURRENT tip.
/// We never move the branch ref during an op (only `finish` does), so resetting
/// to the branch's present tip — not the recorded `orig` — preserves any commits
/// another process added while we were paused, instead of clobbering them.
pub fn abort(repo: &Repository) -> Result<Vec<String>, Error> {
    let st = load_state(repo)?;
    // Where to land: the branch's current tip if it still exists (preserve a
    // concurrent advance), else the pre-op backup, else the recorded orig. If
    // the branch was deleted under us, recreating it here un-wedges the op
    // rather than erroring with the state file left behind.
    let tip = repo
        .refname_to_id(&st.branch)
        .or_else(|_| repo.refname_to_id(&backup_ref(&st.branch)))
        .unwrap_or(st.orig);
    repo.reference(&st.branch, tip, true, "mime sequencer: abort")?;
    repo.set_head(&st.branch)?;
    hard_reset_keeping_autostash(repo, tip, &st)?;
    let _ = repo.cleanup_state();
    let _ = std::fs::remove_file(state_path(repo));
    // An aborted op hands the autostashed uncommitted changes back too. The
    // abort itself is already complete — a restore failure must say so.
    restore_autostash(repo, &st).map_err(|e| {
        estr(&format!(
            "the abort completed (back on {}) — but {}",
            st.branch,
            e.message()
        ))
    })
}

/// The in-progress operation, or `None` when the tree is clean.
pub fn status(repo: &Repository) -> Result<Option<Status>, Error> {
    if !state_path(repo).exists() {
        return Ok(None);
    }
    let st = load_state(repo)?;
    let conflicts = repo.index().map(|i| conflict_paths(&i)).unwrap_or_default();
    Ok(Some(Status {
        next: st.next,
        total: st.steps.len(),
        current: st.current,
        conflicts,
        editing: st.editing,
    }))
}

/// Build the commit for `step` on top of `current_oid` and return its oid,
/// WITHOUT moving any ref or the worktree. Pick/reword add a new commit on
/// `current` (reword swaps the message); squash/fixup REPLACE `current` with a
/// commit on `current`'s parent, melding the message (squash concatenates, fixup
/// keeps `current`'s) — so the step folds into the preceding one; revert appends
/// a generated message. Authorship follows git: the picked commit's for
/// pick/reword/revert, the kept (earlier) commit's for squash/fixup. Shared by
/// `land_step` (which then moves HEAD) and `rehearse` (which discards the result).
fn make_commit(
    repo: &Repository,
    mode: Mode,
    onto: Oid,
    current_oid: Oid,
    step: &Step,
    tree: &git2::Tree,
) -> Result<Oid, Error> {
    let current = repo.find_commit(current_oid)?;
    let pick = repo.find_commit(step.commit)?;
    let pick_msg = || pick.message().unwrap_or("").to_string();
    if mode == Mode::Revert {
        // A revert always appends on `current` with a generated message; the
        // forward-action vocabulary (reword/squash/fixup) does not apply.
        let summary = pick.summary().unwrap_or("commit");
        let msg = format!(
            "Revert \"{summary}\"\n\nThis reverts commit {}.\n",
            pick.id()
        );
        return repo.commit(
            None,
            &pick.committer(),
            &pick.committer(),
            &msg,
            tree,
            &[&current],
        );
    }
    match step.action {
        Action::Pick => repo.commit(
            None,
            &pick.author(),
            &pick.committer(),
            &pick_msg(),
            tree,
            &[&current],
        ),
        // `edit` lands like a pick (optionally reworded); the amend happens on
        // continue, once the agent has changed the worktree.
        Action::Reword | Action::Edit => {
            let msg = step.message.clone().unwrap_or_else(pick_msg);
            let msg = apply_msg_edits(msg, &step.message_edits)?;
            repo.commit(
                None,
                &pick.author(),
                &pick.committer(),
                &msg,
                tree,
                &[&current],
            )
        }
        Action::Squash | Action::Fixup => {
            if current_oid == onto {
                return Err(estr("squash/fixup needs a preceding pick"));
            }
            let parent = current.parent(0)?;
            let cur_msg = current.message().unwrap_or("").to_string();
            let msg = match step.action {
                Action::Fixup => cur_msg,
                _ => step.message.clone().unwrap_or_else(|| {
                    format!("{}\n\n{}", cur_msg.trim_end(), pick_msg().trim_end())
                }),
            };
            let msg = apply_msg_edits(msg, &step.message_edits)?;
            repo.commit(
                None,
                &current.author(),
                &current.committer(),
                &msg,
                tree,
                &[&parent],
            )
        }
        Action::Drop => unreachable!("drop is handled before make_commit"),
        Action::Split => unreachable!("split builds its commits via land_split"),
    }
}

/// Commit the merged `tree` for `step` (via [`make_commit`]) and move the
/// detached HEAD onto it. Detached, not via "HEAD", so squash/fixup can parent
/// on `current`'s parent (the "HEAD" path rejects first-parent != HEAD); the
/// worktree already matches the merged tree.
fn land_step(repo: &Repository, st: &State, step: &Step, tree: &git2::Tree) -> Result<Oid, Error> {
    let new = make_commit(repo, st.mode, st.onto, st.current, step, tree)?;
    repo.set_head_detached(new)?;
    Ok(new)
}

/// Land one step's merged `tree`: a `split` fans out into several commits (HEAD
/// moves to the last), any other action lands a single commit. The one dispatch
/// point, so a future special-landing action is wired here, not in each driver.
fn land(repo: &Repository, st: &State, step: &Step, tree: &git2::Tree) -> Result<Oid, Error> {
    if step.action == Action::Split {
        land_split(repo, st, step, tree)
    } else {
        land_step(repo, st, step, tree)
    }
}

/// Paths that differ between the `base` and `target` trees (a commit's net change).
fn changed_paths(
    repo: &Repository,
    base: &git2::Tree,
    target: &git2::Tree,
) -> Result<Vec<String>, Error> {
    let diff = repo.diff_tree_to_tree(Some(base), Some(target), None)?;
    let mut paths: Vec<String> = diff
        .deltas()
        .map(|d| delta_path(Some(d)))
        .filter(|p| !p.is_empty())
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Validate a split's parts against the `touched` paths and return the catch-all
/// part's paths (every touched path no explicit part claimed). Errors on an empty
/// plan, an unchanged or doubly-claimed path, an empty part, more than one
/// catch-all, or (with no catch-all) any unassigned changed path.
fn split_assignment(touched: &[String], parts: &[SplitPart]) -> Result<Vec<String>, Error> {
    if parts.is_empty() {
        return Err(estr("split: `into` must list at least one part"));
    }
    let mut assigned: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // O(1) membership for the "does the commit change this path?" check below.
    let touched_set: std::collections::HashSet<&str> = touched.iter().map(String::as_str).collect();
    let mut rest = 0usize;
    for part in parts {
        if part.message.trim().is_empty() {
            return Err(estr("split: every part needs a non-empty message"));
        }
        if part.rest {
            rest += 1;
            continue;
        }
        if part.paths.is_empty() {
            return Err(estr(
                "split: a non-catch-all part needs at least one path (give one part no paths to be the catch-all)",
            ));
        }
        for p in &part.paths {
            if !touched_set.contains(p.as_str()) {
                return Err(estr(&format!("split: the commit does not change {p}")));
            }
            if !assigned.insert(p.as_str()) {
                return Err(estr(&format!(
                    "split: {p} is assigned to more than one part"
                )));
            }
        }
    }
    if rest > 1 {
        return Err(estr(
            "split: at most one part may be the catch-all (the one with no paths)",
        ));
    }
    let leftover: Vec<String> = touched
        .iter()
        .filter(|t| !assigned.contains(t.as_str()))
        .cloned()
        .collect();
    if rest == 0 && !leftover.is_empty() {
        return Err(estr(&format!(
            "split: these changed paths are unassigned (add them to a part, or give one part no paths as the catch-all): {}",
            leftover.join(", ")
        )));
    }
    if rest == 1 && leftover.is_empty() {
        return Err(estr("split: the catch-all part has no remaining changes"));
    }
    Ok(leftover)
}

/// Build a split's output commits: replay each part's paths (taken from `target`)
/// onto a chain rooted at `base`, in order. Returns the new oids; the last one's
/// tree equals `target`. Pure tree construction — touches no worktree.
fn split_commits(
    repo: &Repository,
    pick: &git2::Commit,
    base: Oid,
    target: &git2::Tree,
    parts: &[SplitPart],
) -> Result<Vec<Oid>, Error> {
    let base_tree = repo.find_commit(base)?.tree()?;
    let touched = changed_paths(repo, &base_tree, target)?;
    let leftover = split_assignment(&touched, parts)?;

    let mut current = base;
    let mut made = Vec::new();
    for part in parts {
        let paths: &[String] = if part.rest { &leftover } else { &part.paths };
        let cur = repo.find_commit(current)?;
        let prev_tree = cur.tree()?;
        let mut index = git2::Index::new()?;
        index.read_tree(&prev_tree)?;
        for p in paths {
            let path = Path::new(p);
            match target.get_path(path) {
                Ok(entry) => {
                    // Stat fields are irrelevant to write_tree_to; zero them.
                    let ie = git2::IndexEntry {
                        ctime: git2::IndexTime::new(0, 0),
                        mtime: git2::IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: entry.filemode() as u32,
                        uid: 0,
                        gid: 0,
                        file_size: 0,
                        id: entry.id(),
                        flags: 0,
                        flags_extended: 0,
                        path: p.clone().into_bytes(),
                    };
                    index.add(&ie)?;
                }
                // Absent in the applied tree → the commit deleted this path.
                Err(_) => {
                    index.remove_path(path)?;
                }
            }
        }
        let tree = repo.find_tree(index.write_tree_to(repo)?)?;
        let new = repo.commit(
            None,
            &pick.author(),
            &pick.committer(),
            &part.message,
            &tree,
            &[&cur],
        )?;
        made.push(new);
        current = new;
    }
    Ok(made)
}

/// Build a split's output commits, choosing the path- or hunk-level builder by
/// whether any part selects hunks. Both are pure tree construction — no worktree.
fn build_split(
    repo: &Repository,
    pick: &git2::Commit,
    base: Oid,
    target: &git2::Tree,
    parts: &[SplitPart],
) -> Result<Vec<Oid>, Error> {
    if parts.iter().any(|p| !p.hunks.is_empty()) {
        split_commits_hunked(repo, pick, base, target, parts)
    } else {
        split_commits(repo, pick, base, target, parts)
    }
}

/// The resolved ownership of a hunk-aware split: which part takes each file
/// whole, and which part takes each individual hunk of a hunk-split file. A
/// path is in exactly one of the two maps.
struct HunkPlan {
    /// path → part index, for files claimed whole (a part's `paths`, or a
    /// no-hunk delta / catch-all file).
    whole: std::collections::HashMap<String, usize>,
    /// (path, new_start, new_lines) → part index, for hunk-split files.
    per_hunk: std::collections::HashMap<(String, u32, u32), usize>,
}

/// Resolve and validate a hunk-aware split against the commit's own
/// `base_tree → target` diff. Errors on an empty plan, an empty message, a path
/// the commit doesn't change, a path claimed twice (or by both path and hunk), a
/// hunk claimed twice, a selector that overlaps no hunk, more than one catch-all,
/// an unassigned file/hunk with no catch-all, or a part that ends up empty.
fn hunk_assignment(
    repo: &Repository,
    base_tree: &git2::Tree,
    target: &git2::Tree,
    parts: &[SplitPart],
) -> Result<HunkPlan, Error> {
    if parts.is_empty() {
        return Err(estr("split: `into` must list at least one part"));
    }
    let mut rest_idx: Option<usize> = None;
    for (i, p) in parts.iter().enumerate() {
        if p.message.trim().is_empty() {
            return Err(estr("split: every part needs a non-empty message"));
        }
        // Honour the caller-computed `rest` flag (set only when BOTH keys are
        // absent), so a present-but-empty `paths`/`hunks` is rejected rather than
        // silently promoted to the catch-all — matching split_assignment.
        if p.rest {
            if rest_idx.is_some() {
                return Err(estr(
                    "split: at most one part may be the catch-all (the one with no paths or hunks)",
                ));
            }
            rest_idx = Some(i);
        } else if p.paths.is_empty() && p.hunks.is_empty() {
            return Err(estr(
                "split: a non-catch-all part needs at least one path or hunk (give one part no paths/hunks to be the catch-all)",
            ));
        }
    }

    // The commit's changed files, each with its hunks' new-side spans (empty for
    // a rename/mode/binary delta that carries no text hunk).
    let diff = repo.diff_tree_to_tree(Some(base_tree), Some(target), None)?;
    let mut files: Vec<(String, Vec<(u32, u32)>)> = Vec::new();
    for idx in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(idx) else {
            continue;
        };
        // Key paths the SAME way apply_subset does (delta_path, lossy) so a
        // non-UTF-8 path matches on both sides instead of being dropped.
        let path = delta_path(Some(delta));
        let mut hunks = Vec::new();
        if let Some(patch) = git2::Patch::from_diff(&diff, idx)? {
            for h in 0..patch.num_hunks() {
                let (dh, _) = patch.hunk(h)?;
                hunks.push((dh.new_start(), dh.new_lines()));
            }
        }
        files.push((path, hunks));
    }

    let mut whole: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut per_hunk: std::collections::HashMap<(String, u32, u32), usize> =
        std::collections::HashMap::new();

    // 1) explicit whole-file claims (`paths`).
    for (i, part) in parts.iter().enumerate() {
        for p in &part.paths {
            if !files.iter().any(|(f, _)| f == p) {
                return Err(estr(&format!("split: the commit does not change {p}")));
            }
            if whole.insert(p.clone(), i).is_some() {
                return Err(estr(&format!(
                    "split: {p} is assigned to more than one part"
                )));
            }
        }
    }

    // 2) explicit hunk claims: a selector takes every hunk whose new-side span
    // overlaps it.
    for (i, part) in parts.iter().enumerate() {
        for sel in &part.hunks {
            if whole.contains_key(&sel.path) {
                return Err(estr(&format!(
                    "split: {} is claimed both by path and by hunk",
                    sel.path
                )));
            }
            let Some((_, hunks)) = files.iter().find(|(f, _)| f == &sel.path) else {
                return Err(estr(&format!(
                    "split: the commit does not change {}",
                    sel.path
                )));
            };
            if hunks.is_empty() {
                return Err(estr(&format!(
                    "split: {} has no text hunks to select (a rename/mode/binary change — assign it by path)",
                    sel.path
                )));
            }
            let mut matched = false;
            for &(ns, nl) in hunks {
                // New-side span [ns, ns+nl-1]; a pure-deletion hunk (nl==0) is a
                // point at ns.
                let hi = if nl == 0 { ns } else { ns + nl - 1 };
                if sel.lo <= hi && ns <= sel.hi {
                    matched = true;
                    match per_hunk.insert((sel.path.clone(), ns, nl), i) {
                        Some(prev) if prev != i => {
                            return Err(estr(&format!(
                                "split: a hunk of {} is claimed by more than one part",
                                sel.path
                            )));
                        }
                        _ => {}
                    }
                }
            }
            if !matched {
                return Err(estr(&format!(
                    "split: no hunk of {} overlaps lines {}-{}",
                    sel.path, sel.lo, sel.hi
                )));
            }
        }
    }

    // 3) sweep everything not explicitly claimed into the catch-all (or error).
    for (path, hunks) in &files {
        if whole.contains_key(path) {
            continue;
        }
        if hunks.is_empty() {
            match rest_idx {
                Some(r) => {
                    whole.insert(path.clone(), r);
                }
                None => {
                    return Err(estr(&format!(
                        "split: {path} is unassigned (add it to a part's paths, or give one part no paths/hunks as the catch-all)"
                    )));
                }
            }
            continue;
        }
        for &(ns, nl) in hunks {
            if per_hunk.contains_key(&(path.clone(), ns, nl)) {
                continue;
            }
            match rest_idx {
                Some(r) => {
                    per_hunk.insert((path.clone(), ns, nl), r);
                }
                None => {
                    return Err(estr(&format!(
                        "split: a hunk of {path} (line {ns}) is unassigned (add it to a part, or give one part no paths/hunks as the catch-all)"
                    )));
                }
            }
        }
    }

    // Every part — catch-all included — must end up owning something.
    for (i, part) in parts.iter().enumerate() {
        let owns = whole.values().any(|&v| v == i) || per_hunk.values().any(|&v| v == i);
        if !owns {
            return Err(estr(&format!(
                "split: part \"{}\" ends up with no changes",
                part.message
            )));
        }
    }

    Ok(HunkPlan { whole, per_hunk })
}

/// Build a hunk-split's output commits: for each part in turn, apply the
/// cumulative set of file/hunk changes owned by parts `0..=k` onto the ORIGINAL
/// base tree, so part `k`'s tree is `base + parts[0..=k]` and the last equals
/// `target`. Applying a subset of a file's hunks can fail if a skipped hunk
/// shifted the context a kept one needs — that surfaces as an apply error
/// (recoverable via git_abort).
fn split_commits_hunked(
    repo: &Repository,
    pick: &git2::Commit,
    base: Oid,
    target: &git2::Tree,
    parts: &[SplitPart],
) -> Result<Vec<Oid>, Error> {
    let base_tree = repo.find_commit(base)?.tree()?;
    let plan = hunk_assignment(repo, &base_tree, target, parts)?;
    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(target), None)?;

    let whole = &plan.whole;
    let per_hunk = &plan.per_hunk;
    let mut current = base;
    let mut made = Vec::new();
    for (k, part) in parts.iter().enumerate() {
        // Apply, cumulatively, every file/hunk owned by parts 0..=k onto the base
        // tree. apply_subset gates whole-file add/delete at the delta level, so an
        // added file whose hunks aren't owned yet doesn't linger as an empty file.
        let tree_oid = apply_subset(
            repo,
            &base_tree,
            &diff,
            |path| whole.get(path).map(|&o| o <= k).unwrap_or(false),
            |path, ns, nl| {
                // A whole-file-owned file's hunks aren't in per_hunk; fall back to
                // its whole owner so they land with the file.
                per_hunk
                    .get(&(path.to_string(), ns, nl))
                    .or_else(|| whole.get(path))
                    .map(|&o| o <= k)
                    .unwrap_or(false)
            },
        )?;
        let tree = repo.find_tree(tree_oid)?;
        let parent = repo.find_commit(current)?;
        let new = repo.commit(
            None,
            &pick.author(),
            &pick.committer(),
            &part.message,
            &tree,
            &[&parent],
        )?;
        made.push(new);
        current = new;
    }
    Ok(made)
}

/// The delta's path (new side, falling back to old), for the apply callbacks.
fn delta_path(d: Option<git2::DiffDelta>) -> String {
    d.and_then(|d| d.new_file().path().or_else(|| d.old_file().path()))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Apply the subset of `diff` that the callbacks keep onto `base_tree`, returning
/// the resulting tree oid. `keep_hunk` decides each hunk of a text file;
/// `keep_whole` decides a no-hunk delta (rename/mode/binary). A file with NO kept
/// content is dropped at the DELTA level — so an add/delete that is entirely
/// excluded doesn't leave an empty file behind (which per-hunk rejection alone
/// would, since git creates the added file before its hunks run).
fn apply_subset(
    repo: &Repository,
    base_tree: &git2::Tree,
    diff: &git2::Diff,
    keep_whole: impl Fn(&str) -> bool,
    keep_hunk: impl Fn(&str, u32, u32) -> bool,
) -> Result<Oid, Error> {
    // Per file: is anything kept? (delta-level gate, computed before applying.)
    let mut keep_file: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for idx in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(idx) else {
            continue;
        };
        let path = delta_path(Some(delta));
        let any = match git2::Patch::from_diff(diff, idx)? {
            Some(p) if p.num_hunks() > 0 => {
                let mut kept = false;
                for h in 0..p.num_hunks() {
                    let (dh, _) = p.hunk(h)?;
                    if keep_hunk(&path, dh.new_start(), dh.new_lines()) {
                        kept = true;
                        break;
                    }
                }
                kept
            }
            _ => keep_whole(&path), // no-hunk delta (rename/mode/binary)
        };
        keep_file.insert(path, any);
    }

    let cur = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let cur_delta = cur.clone();
    let mut opts = git2::ApplyOptions::new();
    opts.delta_callback(move |d: Option<git2::DiffDelta>| {
        let path = delta_path(d);
        let keep = *keep_file.get(&path).unwrap_or(&true);
        *cur_delta.borrow_mut() = path;
        keep
    });
    opts.hunk_callback(move |h: Option<git2::DiffHunk>| {
        let Some(h) = h else { return false };
        keep_hunk(&cur.borrow(), h.new_start(), h.new_lines())
    });
    let mut index = repo.apply_to_tree(base_tree, diff, Some(&mut opts))?;
    index.write_tree_to(repo)
}

/// The change to relocate, resolved against `from`'s own diff: whole files
/// (`paths`), individual hunks (`hunks`), and the diff's no-hunk files.
struct MoveSel {
    whole: std::collections::HashSet<String>,
    hunks: std::collections::HashSet<(String, u32, u32)>,
    /// Every path the move touches — for the "changed in both commits" check.
    paths: std::collections::HashSet<String>,
}

impl MoveSel {
    /// Is this hunk (or, via `new_lines`==anything, its file) part of the move?
    fn takes(&self, path: &str, ns: u32, nl: u32) -> bool {
        self.whole.contains(path) || self.hunks.contains(&(path.to_string(), ns, nl))
    }
}

/// Validate the requested paths/hunks against `from`'s diff and resolve them to a
/// [`MoveSel`]. Errors on a path/selector the commit doesn't change, a path named
/// both ways, a selector overlapping no hunk, or a rename/mode/binary hunk-select.
fn resolve_move_selection(
    from_diff: &git2::Diff,
    paths: &[String],
    hunks: &[HunkSel],
    what: &str,
) -> Result<MoveSel, Error> {
    let mut files: Vec<(String, Vec<(u32, u32)>)> = Vec::new();
    for idx in 0..from_diff.deltas().len() {
        let Some(delta) = from_diff.get_delta(idx) else {
            continue;
        };
        let path = delta_path(Some(delta));
        let mut hs = Vec::new();
        if let Some(patch) = git2::Patch::from_diff(from_diff, idx)? {
            for h in 0..patch.num_hunks() {
                let (dh, _) = patch.hunk(h)?;
                hs.push((dh.new_start(), dh.new_lines()));
            }
        }
        files.push((path, hs));
    }

    let mut whole = std::collections::HashSet::new();
    for p in paths {
        if !files.iter().any(|(f, _)| f == p) {
            return Err(estr(&format!("{what}: the diff does not change {p}")));
        }
        whole.insert(p.clone());
    }
    let mut hunk_keys = std::collections::HashSet::new();
    for sel in hunks {
        if whole.contains(&sel.path) {
            return Err(estr(&format!(
                "{what}: {} is named both by path and by hunk",
                sel.path
            )));
        }
        let Some((_, hs)) = files.iter().find(|(f, _)| f == &sel.path) else {
            return Err(estr(&format!(
                "{what}: the diff does not change {}",
                sel.path
            )));
        };
        if hs.is_empty() {
            return Err(estr(&format!(
                "{what}: {} has no text hunks (a rename/mode/binary change — take it by path)",
                sel.path
            )));
        }
        let mut matched = false;
        for &(ns, nl) in hs {
            let hi = if nl == 0 { ns } else { ns + nl - 1 };
            if sel.lo <= hi && ns <= sel.hi {
                matched = true;
                hunk_keys.insert((sel.path.clone(), ns, nl));
            }
        }
        if !matched {
            return Err(estr(&format!(
                "{what}: no hunk of {} overlaps lines {}-{}",
                sel.path, sel.lo, sel.hi
            )));
        }
    }
    let mut all = whole.clone();
    for (p, _, _) in &hunk_keys {
        all.insert(p.clone());
    }
    Ok(MoveSel {
        whole,
        hunks: hunk_keys,
        paths: all,
    })
}

/// Relocate `from`'s changes to the adjacent commit `to`, then replay the rest of
/// the branch. The FINAL tree never changes — only which of the two commits
/// introduces the moved change — so a move can't alter the branch's end state.
/// Returns the sequencer outcome (a conflict during tail replay stops for the
/// conflict tools + git_continue, like any rebase).
fn move_changes(
    repo: &Repository,
    from: Oid,
    to: Oid,
    paths: &[String],
    hunks: &[HunkSel],
) -> Result<String, Error> {
    if paths.is_empty() && hunks.is_empty() {
        return Err(estr("move: name at least one path or hunk to move"));
    }
    let fc = repo.find_commit(from)?;
    let tc = repo.find_commit(to)?;
    let from_is_child = fc.parent_count() > 0 && fc.parent(0)?.id() == to;
    let to_is_child = tc.parent_count() > 0 && tc.parent(0)?.id() == from;
    if !from_is_child && !to_is_child {
        return Err(estr(
            "move: `from` and `to` must be adjacent (one the direct parent of the other)",
        ));
    }
    // older is the parent commit, newer its child; `from` is one of them.
    let (older, newer) = if from_is_child {
        (tc.clone(), fc.clone())
    } else {
        (fc.clone(), tc.clone())
    };
    let base_commit = older.parent(0)?; // older is a child (adjacency), so it has a parent
    let base_tree = base_commit.tree()?;
    let older_tree = older.tree()?;
    let newer_tree = newer.tree()?;

    // The move set lives in `from`'s own diff; reject anything `to` also changes.
    let from_parent_tree = fc.parent(0)?.tree()?;
    let from_diff = repo.diff_tree_to_tree(Some(&from_parent_tree), Some(&fc.tree()?), None)?;
    let sel = resolve_move_selection(&from_diff, paths, hunks, "move")?;
    let to_changed = changed_paths(repo, &tc.parent(0)?.tree()?, &tc.tree()?)?;
    for p in &sel.paths {
        if to_changed.iter().any(|c| c == p) {
            return Err(estr(&format!(
                "move: {p} is changed by both commits — can't unambiguously move it"
            )));
        }
    }

    // Rebuild `older`: forward move (from=older) drops the move set from its diff;
    // backward move (from=newer) adds it to older's tree. `newer` keeps the final
    // tree unchanged, so the branch end state is identical either way.
    let older_new_tree = if from_is_child {
        // Backward: older' = older + the moved subset of newer's (=from's) diff.
        apply_subset(
            repo,
            &older_tree,
            &from_diff,
            |p| sel.whole.contains(p),
            |p, ns, nl| sel.takes(p, ns, nl),
        )?
    } else {
        // Forward: older' = base + older's (=from's) diff minus the moved subset.
        apply_subset(
            repo,
            &base_tree,
            &from_diff,
            |p| !sel.whole.contains(p),
            |p, ns, nl| !sel.takes(p, ns, nl),
        )?
    };

    let older_prime = repo.commit(
        None,
        &older.author(),
        &older.committer(),
        older.message().unwrap_or(""),
        &repo.find_tree(older_new_tree)?,
        &[&base_commit],
    )?;
    let newer_prime = repo.commit(
        None,
        &newer.author(),
        &newer.committer(),
        newer.message().unwrap_or(""),
        &newer_tree,
        &[&repo.find_commit(older_prime)?],
    )?;

    // Replay the commits after `newer` onto the rebuilt pair via the sequencer,
    // reusing its conflict handling + backup ref. Empty tail = just move the branch.
    let tail = commits_since(repo, newer.id())?;
    let note = backup_note(repo);
    let plan = Plan {
        onto: newer_prime,
        steps: tail.into_iter().map(pick_step).collect(),
    };
    let out = start(repo, plan)?;
    Ok(format!("{}{note}", outcome_with_tree_note(repo, &out)))
}

/// Land a `split` step: produce its output commits and move HEAD to the last.
fn land_split(
    repo: &Repository,
    st: &State,
    step: &Step,
    target: &git2::Tree,
) -> Result<Oid, Error> {
    let pick = repo.find_commit(step.commit)?;
    let made = build_split(repo, &pick, st.current, target, &step.split_into)?;
    let last = *made.last().expect("split produced at least one commit");
    repo.set_head_detached(last)?;
    Ok(last)
}

/// Replay steps from `st.next` until the plan completes or a step conflicts.
fn drive(repo: &Repository, mut st: State) -> Result<Outcome, Error> {
    while st.next < st.steps.len() {
        let step = st.steps[st.next].clone();
        if step.action == Action::Drop {
            st.next += 1;
            save_state(repo, &st)?;
            continue;
        }

        let pick = repo.find_commit(step.commit)?;
        match st.mode {
            Mode::Pick => {
                let mut opts = CherrypickOptions::new();
                opts.checkout_builder(diff3_checkout());
                repo.cherrypick(&pick, Some(&mut opts))?;
            }
            Mode::Revert => {
                let mut opts = RevertOptions::new();
                opts.checkout_builder(diff3_checkout());
                repo.revert(&pick, Some(&mut opts))?;
            }
        }

        let mut index = repo.index()?;
        if index.has_conflicts() {
            let files = conflict_paths(&index);
            save_state(repo, &st)?;
            return Ok(Outcome::Conflict {
                step: st.next,
                files,
            });
        }

        let tree = repo.find_tree(index.write_tree()?)?;
        let new = land(repo, &st, &step, &tree)?;
        repo.cleanup_state()?;
        st.current = new;
        st.next += 1;
        if step.action == Action::Edit {
            st.editing = true;
        }
        // Persist after each landed step: a crash mid-run otherwise leaves HEAD
        // ahead of a stale next=0/current=onto state that would re-apply commits.
        save_state(repo, &st)?;
        if st.editing {
            // Pause with the just-landed commit checked out; the agent edits the
            // worktree, then git_continue amends it (git_skip leaves it as-is).
            return Ok(Outcome::Paused {
                step: st.next - 1,
                head: st.current,
            });
        }
    }
    let kept = finish(repo, &st)?;
    Ok(Outcome::Done {
        head: st.current,
        kept,
    })
}

/// Land the rebased history: move the branch ref to the new tip, reattach
/// HEAD, clean the worktree, drop the state. Returns the autostash paths
/// whose restore was skipped in favor of a pause-time edit.
fn finish(repo: &Repository, st: &State) -> Result<Vec<String>, Error> {
    // Only land if the branch still points where `begin` left it. If another
    // process moved it (would drop their commits) or deleted it (recreating it
    // would resurrect a ref the user removed), refuse and leave the replay for
    // git_abort to discard.
    match repo.find_reference(&st.branch) {
        Ok(r) if r.target() != Some(st.orig) => {
            return Err(estr(&format!(
                "{} moved during the operation — leaving it untouched; git_abort to discard the replay",
                st.branch
            )));
        }
        Err(_) => {
            return Err(estr(&format!(
                "{} was deleted during the operation — git_abort to discard the replay",
                st.branch
            )));
        }
        _ => {}
    }
    repo.reference(&st.branch, st.current, true, "rebase (mime sequencer)")?;
    repo.set_head(&st.branch)?;
    hard_reset_keeping_autostash(repo, st.current, st)?;
    let _ = repo.cleanup_state();
    let _ = std::fs::remove_file(state_path(repo));
    // Hand back any autostashed uncommitted changes. The paths were checked
    // disjoint from everything the plan rewrites, so this can't mix old and
    // new content; a path the user edited during the operation keeps the
    // later edit (the parked bytes stay on the ref). The rewrite itself has
    // already landed — a restore failure must say so, not read as a failed
    // operation.
    restore_autostash(repo, st).map_err(|e| {
        estr(&format!(
            "the rewrite LANDED (new tip {}) — but {}",
            short(st.current),
            e.message()
        ))
    })
}

// ---- read-only inspection -------------------------------------------------

/// Resolve a revspec (oid, ref, `HEAD~3`, …) to a commit oid.
pub fn resolve(repo: &Repository, spec: &str) -> Result<Oid, Error> {
    Ok(repo.revparse_single(spec)?.peel_to_commit()?.id())
}

/// The commits reachable from HEAD but not from `onto`, oldest-first — the
/// default rebase plan (`pick` each) for `onto..HEAD`.
pub fn commits_since(repo: &Repository, onto: Oid) -> Result<Vec<Oid>, Error> {
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    walk.hide(onto)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    walk.collect()
}

/// The patch-id that identifies `commit` as a cherry-pick candidate, or `None`
/// when the commit has no single well-defined patch to match on:
/// - a MERGE (more than one parent) — never an "already-applied" cherry-pick, and
///   its first-parent diff wouldn't capture what it integrated anyway;
/// - an EMPTY-diff commit — libgit2 hashes the empty diff to a constant, so every
///   empty commit would collide on one id and be mistaken for a copy of any other.
///
/// A non-merge commit diffs against its first parent (the empty tree for a root
/// commit); the id is a content hash stable across cherry-pick/reorder — the same
/// change yields the same id at a different oid, which is how we recognise a
/// commit as already-applied regardless of where it sits.
fn commit_patch_id(repo: &Repository, commit: Oid) -> Result<Option<Oid>, Error> {
    let c = repo.find_commit(commit)?;
    if c.parent_count() > 1 {
        return Ok(None);
    }
    let parent_tree = if c.parent_count() > 0 {
        Some(c.parent(0)?.tree()?)
    } else {
        None
    };
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&c.tree()?), None)?;
    if diff.deltas().len() == 0 {
        return Ok(None);
    }
    Ok(Some(diff.patchid(None)?))
}

/// Split `from..HEAD` (`from` = `onto` in the two-arg case) into (kept,
/// dropped): commits whose patch-id already
/// appears among the upstream-only commits (`HEAD..onto`) are dropped. This
/// matches `git rebase`'s default cherry-pick detection
/// (`--no-reapply-cherry-picks`): when a base rewrite (reorder/amend) leaves the
/// merge-base *below* the rewrite, `onto..HEAD` still contains the pre-rewrite
/// copies of commits now present — by patch-id — in the new base, and a naive
/// pick-all would replay them a second time (duplicates). Comparing against only
/// the onto-only side (not all of `onto`) mirrors git's `onto...HEAD` symmetric
/// difference, so distinct commits that merely share a base are never dropped.
fn partition_cherry_picks(
    repo: &Repository,
    onto: Oid,
    from: Oid,
) -> Result<(Vec<Oid>, Vec<Oid>), Error> {
    // Patch-ids of the commits reachable from `onto` but not HEAD — git's
    // "right side" of the onto...HEAD symmetric difference.
    let mut walk = repo.revwalk()?;
    walk.push(onto)?;
    walk.hide_head()?;
    let mut upstream = std::collections::HashSet::new();
    for oid in walk {
        if let Ok(Some(pid)) = commit_patch_id(repo, oid?) {
            upstream.insert(pid);
        }
    }
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for oid in commits_since(repo, from)? {
        // Conservative on both None (merge/empty, no patch to match) and Err (a
        // diff/patchid failure): keep the commit rather than risk dropping one we
        // can't positively identify as already-applied.
        match commit_patch_id(repo, oid) {
            Ok(Some(pid)) if upstream.contains(&pid) => dropped.push(oid),
            _ => kept.push(oid),
        }
    }
    Ok((kept, dropped))
}

fn short(oid: Oid) -> String {
    let s = oid.to_string();
    s[..s.len().min(10)].to_string()
}

/// A one-line-per-commit log of `range` (default: from HEAD), capped at `limit`.
pub fn log(repo: &Repository, range: Option<&str>, limit: usize) -> Result<String, Error> {
    let mut walk = repo.revwalk()?;
    match range {
        Some(r) => walk.push_range(r)?,
        None => walk.push_head()?,
    }
    let mut out = String::new();
    for (n, oid) in walk.enumerate() {
        if n >= limit {
            out.push_str(&format!("… (truncated at {limit}; pass a tighter range)\n"));
            break;
        }
        let c = repo.find_commit(oid?)?;
        out.push_str(&format!(
            "{} {}\n",
            short(c.id()),
            c.summary().unwrap_or("")
        ));
    }
    if out.is_empty() {
        out.push_str("(no commits)\n");
    }
    Ok(out)
}

/// A commit's metadata, message, and the files it changed vs its first parent.
pub fn show(repo: &Repository, oid: Oid) -> Result<String, Error> {
    let c = repo.find_commit(oid)?;
    let a = c.author();
    let body = c.message().unwrap_or("").trim_end().replace('\n', "\n    ");
    let mut out = format!(
        "commit {}\nAuthor: {} <{}>\n\n    {body}\n\nChanged files:\n",
        c.id(),
        a.name().unwrap_or(""),
        a.email().unwrap_or(""),
    );
    let new_tree = c.tree()?;
    let old_tree = if c.parent_count() > 0 {
        Some(c.parent(0)?.tree()?)
    } else {
        None
    };
    let diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)?;
    for d in diff.deltas() {
        let mark = match d.status() {
            Delta::Added => "A",
            Delta::Deleted => "D",
            Delta::Modified => "M",
            Delta::Renamed => "R",
            Delta::Copied => "C",
            _ => "?",
        };
        let p = delta_path(Some(d));
        let p = if p.is_empty() { "?" } else { &p };
        out.push_str(&format!("  {mark} {p}\n"));
    }
    // Full unified diff after the file summary: reconstruct the patch, prefixing
    // each content line with its +/-/space origin (hunk/file headers carry their
    // own text). Binary content that isn't UTF-8 renders as its header only.
    out.push_str("\nDiff:\n");
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        if matches!(line.origin(), '+' | '-' | ' ') {
            out.push(line.origin());
        }
        out.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })?;
    Ok(out)
}

/// "Which commit last touched each line" for `rel` (relative to the repo
/// workdir), collapsed into contiguous same-commit hunks: one `L<a>-<b>  <oid>
/// <summary>` line per hunk. `range` (1-based, inclusive) restricts the blame to
/// those lines; `None` blames the whole file. The discovery half of the
/// find-the-commit → fold-in workflow (feeds a git_rebase fixup/edit plan).
pub fn blame(
    repo: &Repository,
    rel: &Path,
    range: Option<(usize, usize)>,
    since: Option<Oid>,
) -> Result<String, Error> {
    let mut opts = BlameOptions::new();
    if let Some((lo, hi)) = range {
        opts.min_line(lo.max(1));
        opts.max_line(hi.max(lo.max(1)));
    }
    // Scope history to `since..`: lines last touched at or after `since` keep
    // their commit; older lines collapse to the `since` boundary — so the answer
    // is "which of MY commits owns this", not any commit in the whole history.
    if let Some(s) = since {
        opts.oldest_commit(s);
    }
    let blame = repo.blame_file(rel, Some(&mut opts))?;
    let mut out = String::new();
    for h in blame.iter() {
        let n = h.lines_in_hunk();
        if n == 0 {
            continue;
        }
        let start = h.final_start_line();
        let end = start + n - 1;
        // libgit2 still returns whole-file hunks with min/max set; drop the ones
        // that fall outside the requested window ourselves.
        if let Some((lo, hi)) = range
            && (end < lo || start > hi)
        {
            continue;
        }
        let oid = h.final_commit_id();
        let summary = repo
            .find_commit(oid)
            .ok()
            .and_then(|c| c.summary().map(str::to_string))
            .unwrap_or_default();
        let span = if start == end {
            format!("L{start}")
        } else {
            format!("L{start}-{end}")
        };
        out.push_str(&format!("  {span}\t{}\t{summary}\n", short(oid)));
    }
    if out.is_empty() {
        out.push_str("(no blame — file untracked, empty, or outside the range)\n");
    }
    Ok(out)
}

/// One uncommitted diff-hunk and the commit(s) that own the lines it changes.
struct WorktreeHunk {
    path: String,
    /// The hunk's identity in the diff it came from (new-side start/lines) —
    /// the key `apply_subset`'s callbacks match on.
    ns: u32,
    nl: u32,
    /// "L10-12" over the CURRENT file — the span blame/fixup reports use.
    span: String,
    /// Deduped owners of the old-side lines this hunk changes; empty = new
    /// lines (or an added file with no committed version to blame).
    owners: Vec<Oid>,
}

/// Map every hunk of `diff` (HEAD→worktree, context 0) to the commit(s) that
/// last set the lines it changes — the shared discovery behind the worktree
/// blame and absorb. Blames the OLD-side lines (as they stand in HEAD); a
/// pure insertion borrows the owner of the line it sits after.
fn worktree_hunk_owners(
    repo: &Repository,
    diff: &git2::Diff,
    since: Option<Oid>,
) -> Result<Vec<WorktreeHunk>, Error> {
    let mut blames: std::collections::HashMap<String, Option<git2::Blame>> =
        std::collections::HashMap::new();
    let mut out = Vec::new();
    for idx in 0..diff.deltas().len() {
        let Some(patch) = git2::Patch::from_diff(diff, idx)? else {
            continue;
        };
        let path = delta_path(diff.get_delta(idx));
        for h in 0..patch.num_hunks() {
            let (dh, _) = patch.hunk(h)?;
            let (os, ol, ns, nl) = (
                dh.old_start(),
                dh.old_lines(),
                dh.new_start(),
                dh.new_lines(),
            );
            let span = if nl == 0 {
                format!("L{ns}")
            } else {
                format!("L{ns}-{}", ns + nl - 1)
            };
            // Old lines this hunk changes; a pure insertion (ol==0) borrows the
            // line it follows.
            let lines: Vec<u32> = if ol == 0 {
                vec![os.max(1)]
            } else {
                (os..os + ol).collect()
            };
            // The file may be newly added → no committed version to blame.
            let blame = blames.entry(path.clone()).or_insert_with(|| {
                let mut bopts = BlameOptions::new();
                if let Some(s) = since {
                    bopts.oldest_commit(s);
                }
                repo.blame_file(Path::new(&path), Some(&mut bopts)).ok()
            });
            let mut owners: Vec<Oid> = Vec::new();
            if let Some(b) = blame {
                for ln in lines {
                    if let Some(bh) = b.get_line(ln as usize) {
                        let oid = bh.final_commit_id();
                        if !owners.contains(&oid) {
                            owners.push(oid);
                        }
                    }
                }
            }
            out.push(WorktreeHunk {
                path: path.clone(),
                ns,
                nl,
                span,
                owners,
            });
        }
    }
    Ok(out)
}

/// The HEAD→worktree diff (context 0) for one path or the whole tree — the
/// input both the worktree blame and absorb discover owners on.
fn worktree_diff<'r>(repo: &'r Repository, rel: Option<&Path>) -> Result<git2::Diff<'r>, Error> {
    let head_tree = repo.head()?.peel_to_tree()?;
    let mut dopts = git2::DiffOptions::new();
    if let Some(rel) = rel {
        dopts.pathspec(rel.to_string_lossy().as_ref());
    }
    dopts.context_lines(0);
    repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut dopts))
}

/// "Which commit owns each uncommitted change": per hunk (`rel` or the whole
/// worktree), the commit that last set the lines it touches. The
/// absorb-target discovery — each reported oid feeds a git_fixup/git_move for
/// that hunk (or git_absorb folds them all). With `group_by_commit`, hunks are
/// grouped under their owning commit — the natural absorb preview.
fn blame_worktree(
    repo: &Repository,
    rel: Option<&Path>,
    since: Option<Oid>,
    group_by_commit: bool,
) -> Result<String, Error> {
    let diff = worktree_diff(repo, rel)?;
    let hunks = worktree_hunk_owners(repo, &diff, since)?;
    let summary_of = |oid: Oid| {
        repo.find_commit(oid)
            .ok()
            .and_then(|c| c.summary().map(str::to_string))
            .unwrap_or_default()
    };
    // A path column only when several files can appear (whole-tree mode).
    let loc = |h: &WorktreeHunk| {
        if rel.is_some() {
            h.span.clone()
        } else {
            format!("{} {}", h.path, h.span)
        }
    };
    let mut out = String::new();
    if group_by_commit {
        let mut order: Vec<Oid> = Vec::new();
        let mut by: std::collections::HashMap<Oid, Vec<String>> = std::collections::HashMap::new();
        let mut unowned: Vec<String> = Vec::new();
        for h in &hunks {
            match h.owners.as_slice() {
                [] => unowned.push(format!("  {} (no prior owner — new lines)\n", loc(h))),
                [one] => {
                    if !order.contains(one) {
                        order.push(*one);
                    }
                    by.entry(*one).or_default().push(format!("  {}\n", loc(h)));
                }
                many => unowned.push(format!(
                    "  {} (split ownership: {})\n",
                    loc(h),
                    many.iter()
                        .map(|o| short(*o))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        for oid in order {
            out.push_str(&format!("{} {}\n", short(oid), summary_of(oid)));
            for line in &by[&oid] {
                out.push_str(line);
            }
        }
        if !unowned.is_empty() {
            out.push_str("(not attributable to one commit)\n");
            for line in unowned {
                out.push_str(&line);
            }
        }
    } else {
        for h in &hunks {
            if h.owners.is_empty() {
                out.push_str(&format!("  {}\t(no prior owner — new lines)\n", loc(h)));
            } else {
                for &oid in &h.owners {
                    out.push_str(&format!(
                        "  {}\t{}\t{}\n",
                        loc(h),
                        short(oid),
                        summary_of(oid)
                    ));
                }
            }
        }
    }
    if out.is_empty() {
        out.push_str(if rel.is_some() {
            "(no uncommitted changes to this path)\n"
        } else {
            "(no uncommitted changes)\n"
        });
    }
    Ok(out)
}

// ---- text rendering for the tool layer ------------------------------------

/// `outcome_text`, plus — on a COMPLETED rewrite — whether HEAD's tree is
/// byte-identical to the pre-op tip on the backup ring: the one-line answer
/// to "did this rewrite change WHAT the branch builds, or only how history
/// slices it?" that otherwise costs a hand-rolled rev-parse comparison.
/// Used by the pure-reslice operations (rebase, committed fixup, move) and
/// by continue/skip, which finish ANY paused op — so a cherry-pick/revert
/// resumed after a conflict reports too (its tree differing is expected).
/// Direct cherry-pick/revert results and worktree folds skip the note; they
/// change the tree by design, so it would be noise there.
fn outcome_with_tree_note(repo: &Repository, out: &Outcome) -> String {
    let text = outcome_text(out);
    match out {
        Outcome::Done { kept, .. } => {
            format!("{text}{}{}", tree_identity_note(repo), kept_note(kept))
        }
        // A pause with an autostash in flight: say where the uncommitted
        // changes went and when they come back.
        Outcome::Conflict { .. } | Outcome::Paused { .. } => match load_state(repo) {
            Ok(st) if !st.autostash.is_empty() => format!(
                "{text}\n  (your uncommitted changes are autostashed on {}; \
                     they are restored when the operation finishes or aborts — \
                     a parked file you edit again during the pause keeps your \
                     edit instead)",
                autostash_ref(&st.branch)
            ),
            _ => text,
        },
    }
}

/// One line naming the autostashed paths whose restore was skipped because
/// they were edited during the operation — the later edits were kept and
/// the parked bytes stayed on the `-autostash` ref. Empty on a clean restore.
fn kept_note(kept: &[String]) -> String {
    if kept.is_empty() {
        return String::new();
    }
    format!(
        "\n  {} edited during the operation — those edits were kept; the parked \
         bytes stay on the -autostash ref",
        kept.join(", ")
    )
}

/// One line comparing HEAD's tree with the pre-op tip's (backup ring slot 0).
/// Empty when there is nothing to compare against.
fn tree_identity_note(repo: &Repository) -> String {
    let Some(branch) = repo.head().ok().and_then(|h| h.name().map(str::to_string)) else {
        return String::new();
    };
    if !branch.starts_with("refs/heads/") {
        return String::new();
    }
    let Ok(old) = repo.refname_to_id(&backup_ref(&branch)) else {
        return String::new();
    };
    let old_tree = repo.find_commit(old).and_then(|c| c.tree());
    let new_tree = repo.head().and_then(|h| h.peel_to_tree());
    let (Ok(old_tree), Ok(new_tree)) = (old_tree, new_tree) else {
        return String::new();
    };
    if old_tree.id() == new_tree.id() {
        return "\n  tree identical to the pre-op tip — a pure history re-slice".to_string();
    }
    let paths: Vec<String> = repo
        .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
        .map(|d| {
            (0..d.deltas().len())
                .filter_map(|i| d.get_delta(i))
                .map(|delta| delta_path(Some(delta)))
                .collect()
        })
        .unwrap_or_default();
    let shown = if paths.len() > 8 {
        format!("{} … ({} paths)", paths[..8].join(", "), paths.len())
    } else {
        paths.join(", ")
    };
    format!(
        "\n  tree differs from the pre-op tip in: {shown} (expected when the \
         base moved or the plan drops/edits content; git_show the backup ref \
         to compare)"
    )
}

/// The agent-facing summary of an [`Outcome`].
pub fn outcome_text(out: &Outcome) -> String {
    match out {
        Outcome::Done { head, .. } => format!("done — new tip {}", short(*head)),
        Outcome::Conflict { step, files } => format!(
            "stopped on a conflict at step {} — resolve {} file(s) with the conflict tools \
             (conflicts / conflict-keep / …), then git_continue (or git_skip / git_abort):\n  {}",
            step + 1,
            files.len(),
            files.join("\n  "),
        ),
        Outcome::Paused { step, head } => format!(
            "paused at step {} for editing — commit {} is checked out. Edit the \
             worktree with the normal tools, then git_continue to fold the changes \
             into it (git_skip to leave it unchanged, git_abort to bail).",
            step + 1,
            short(*head),
        ),
    }
}

/// The agent-facing summary of [`status`].
pub fn status_text(st: Option<Status>) -> String {
    match st {
        None => "no sequencer operation in progress".to_string(),
        Some(s) => {
            let conflicts = if s.conflicts.is_empty() {
                String::new()
            } else {
                format!("\n  unresolved: {}", s.conflicts.join(", "))
            };
            let editing = if s.editing {
                " (paused for editing — amend the worktree, then git_continue)"
            } else {
                ""
            };
            format!(
                "operation in progress: step {}/{}, tip {}{editing}{conflicts}",
                s.next + 1,
                s.total,
                short(s.current),
            )
        }
    }
}

/// The agent-facing summary of a rehearsed [`Preview`].
pub fn preview_text(repo: &Repository, preview: &Preview) -> String {
    let mut out = String::from("rehearsal — no changes applied:\n");
    if preview.commits.is_empty() {
        out.push_str("  (no commits)\n");
    }
    for (oid, summary) in &preview.commits {
        out.push_str(&format!("  {} {summary}\n", short(*oid)));
    }
    if preview.conflicts.is_empty() {
        let head_tree = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_tree().ok())
            .map(|t| t.id());
        out.push_str(&format!(
            "  -> {} commit(s); resulting tree {} current HEAD\n",
            preview.commits.len(),
            if head_tree == Some(preview.final_tree) {
                "IDENTICAL to (pure reorder/fold)"
            } else {
                "DIFFERS from"
            },
        ));
    } else {
        for c in &preview.conflicts {
            out.push_str(&format!(
                "  step {} ({} {}) would conflict in: {}\n",
                c.step + 1,
                short(c.commit),
                c.summary,
                c.files.join(", ")
            ));
            for w in &c.why {
                out.push_str(&format!("    {w}\n"));
            }
        }
        out.push_str(&format!(
            "  -> {} conflicting step(s); a real run stops at the first. Each skipped \
             change is missing from the preview after its step, so later conflicts \
             may be knock-on. Repair the plan (the last-set commit is usually the \
             right target) and rehearse again.\n",
            preview.conflicts.len()
        ));
    }
    out
}

// ---- path-facing command wrappers (the MCP tool layer calls these) --------
//
// These open the repo, resolve revspecs, and stringify errors so the MCP layer
// never touches git2. The caller must have `safety::check_path`-confined the
// repo path first (MIME_ROOTS).

fn gerr(e: Error) -> String {
    e.message().to_string()
}

fn resolve_s(repo: &Repository, spec: &str) -> Result<Oid, String> {
    resolve(repo, spec).map_err(|e| format!("bad revision \"{spec}\": {}", e.message()))
}

fn open(repo_path: &std::path::Path) -> Result<Repository, String> {
    Repository::open(repo_path).map_err(|e| format!("cannot open git repo: {}", e.message()))
}

/// Where `begin` will stamp the pre-op tip, read from the current branch BEFORE
/// the op detaches HEAD — appended to a start's output so the recovery ref is
/// discoverable. Empty when HEAD isn't on a branch.
fn backup_note(repo: &Repository) -> String {
    match repo.head().ok().and_then(|h| h.name().map(str::to_string)) {
        Some(b) if b.starts_with("refs/heads/") => {
            format!(
                "\n  pre-op tip backed up at {} (ring of {BACKUP_RING}: /1, /2 hold the ops before)",
                backup_ref(&b)
            )
        }
        _ => String::new(),
    }
}

/// `git_rebase`: replay `onto..HEAD` (or an explicit `plan` of
/// `(commit, action, message)`) onto `onto`. With `rehearse`, only preview the
/// result (commit list + whether the tree is unchanged) — no changes applied.
/// Expand sparse autosquash directives into a full `onto..HEAD` plan: pick every
/// commit in order, but relocate each named commit to sit right after its target
/// with the fixup/squash action — git's `--autosquash` as data. Removes the
/// transcribe-every-untouched-commit chore. A directive whose relocation lands a
/// fixup/squash first is rejected downstream by the sequencer.
fn autosquash_steps(
    repo: &Repository,
    onto: Oid,
    directives: &[(Oid, Oid, Action)],
) -> Result<Vec<Step>, Error> {
    // Chained folds — a target that is itself relocated by another directive —
    // would strand the earlier fold beside the wrong commit once its target
    // moves. Reject rather than silently mis-fold; the caller can chain in
    // separate git_rebase runs. Also reject folding one commit twice.
    let moved: std::collections::HashSet<Oid> = directives.iter().map(|(c, _, _)| *c).collect();
    if moved.len() != directives.len() {
        return Err(estr("autosquash: a commit is folded more than once"));
    }
    if directives.iter().any(|(_, into, _)| moved.contains(into)) {
        return Err(estr(
            "autosquash: a fold target is itself being folded — fold chained fixups in separate steps",
        ));
    }
    let mut steps: Vec<Step> = commits_since(repo, onto)?
        .into_iter()
        .map(pick_step)
        .collect();
    for (commit, into, action) in directives {
        if commit == into {
            return Err(estr("autosquash: a commit cannot fold into itself"));
        }
        let pos = steps
            .iter()
            .position(|s| s.commit == *commit)
            .ok_or_else(|| {
                estr(&format!(
                    "autosquash: {} is not in onto..HEAD",
                    short(*commit)
                ))
            })?;
        let mut step = steps.remove(pos);
        step.action = *action;
        let ipos = steps
            .iter()
            .position(|s| s.commit == *into)
            .ok_or_else(|| {
                estr(&format!(
                    "autosquash: target {} is not in onto..HEAD",
                    short(*into)
                ))
            })?;
        steps.insert(ipos + 1, step);
    }
    Ok(steps)
}

/// Split a subject into its marker action and remainder: `fixup! X` →
/// (Fixup, "X"). ONE prefix only: a chained marker's remainder
/// (`fixup! fixup! X` → "fixup! X") names the intermediate marker's full
/// subject, and target resolution follows it. (git instead strips ALL
/// prefixes and rematches — same root target when the intermediate marker
/// exists; when it was already folded away, git guesses the base subject
/// while this errors the marker as an orphan.) `None` for a plain subject.
fn marker_split(subject: &str) -> Option<(Action, &str)> {
    if let Some(rest) = subject.strip_prefix("fixup! ") {
        Some((Action::Fixup, rest))
    } else {
        subject
            .strip_prefix("squash! ")
            .map(|rest| (Action::Squash, rest))
    }
}

/// Derive sparse autosquash directives from `fixup!`/`squash!` subject markers
/// in onto..HEAD — git's `--autosquash` as a plan derivation. Scans oldest-
/// first; each marker resolves its target among the PRECEDING non-marker
/// commits: nearest exact subject match, then nearest subject-prefix match
/// (a hand-written marker may truncate the subject), then the remainder as a
/// revspec (`fixup! 1a2b3c`). The exact tier matches EVERY preceding commit,
/// markers included — a chained `fixup! fixup! X` resolves to the earlier
/// `fixup! X` marker by full subject, never to an unrelated commit that
/// happens to share X's subject — and a marker hit flattens to that marker's
/// own target, so `autosquash_steps`' no-chain invariant holds. The prefix
/// tier matches plain subjects only.
/// Unmatched markers are an ERROR naming each orphan — git leaves them in
/// place silently, but a silent no-fold here would read as folded. `amend!`
/// (message-replacing fixup) is rejected rather than half-supported.
///
/// Directives come back newest-first: `autosquash_steps` inserts each directly
/// after its target, so processing newest-first lands the oldest marker first
/// and commit order — hence patch application order — survives.
fn marker_directives(repo: &Repository, onto: Oid) -> Result<Vec<(Oid, Oid, Action)>, Error> {
    // (oid, subject) pairs in range order — markers included — the match pool.
    let mut pool: Vec<(Oid, String)> = Vec::new();
    // marker oid → its resolved (non-marker) target, for chain flattening.
    let mut target_of: std::collections::HashMap<Oid, Oid> = std::collections::HashMap::new();
    let mut directives: Vec<(Oid, Oid, Action)> = Vec::new();
    let mut orphans: Vec<String> = Vec::new();
    for oid in commits_since(repo, onto)? {
        let c = repo.find_commit(oid)?;
        let subject = c.summary().unwrap_or("").to_string();
        if subject.starts_with("amend! ") {
            return Err(estr(&format!(
                "autosquash: {} is an amend! commit — message-replacing folds \
                 are not supported; git_reword the target and fold with an \
                 explicit directive instead",
                short(oid)
            )));
        }
        let Some((action, rest)) = marker_split(&subject) else {
            pool.push((oid, subject));
            continue;
        };
        // Exact tier: markers included, so a chain remainder finds its
        // intermediate marker. Prefix tier: PLAIN subjects only — every marker
        // subject starts with "fixup!"/"squash!", so a truncated remainder
        // ("fixup! fix") would hit the nearest marker instead of the commit it
        // means.
        let matched = pool
            .iter()
            .rev()
            .find(|(_, s)| s == rest)
            .or_else(|| {
                pool.iter()
                    .rev()
                    .find(|(_, s)| marker_split(s).is_none() && s.starts_with(rest))
            })
            .map(|&(t, _)| t)
            .or_else(|| {
                repo.revparse_single(rest)
                    .ok()
                    .and_then(|o| o.peel_to_commit().ok())
                    .map(|c| c.id())
            });
        let Some(mut target) = matched else {
            orphans.push(format!("{} \"{subject}\"", short(oid)));
            continue;
        };
        if let Some(&t) = target_of.get(&target) {
            target = t;
        }
        target_of.insert(oid, target);
        pool.push((oid, subject));
        directives.push((oid, target, action));
    }
    if !orphans.is_empty() {
        return Err(estr(&format!(
            "autosquash: no target found for {} — fold with an explicit \
             {{commit, into}} directive, or reword the marker subject",
            orphans.join(", ")
        )));
    }
    if directives.is_empty() {
        return Err(estr(
            "autosquash: no fixup!/squash! commits in onto..HEAD — nothing to fold",
        ));
    }
    directives.reverse();
    Ok(directives)
}

/// The rebase base for folding `source` into `target`: the parent of whichever is
/// the ancestor. Errors if they aren't on one line of history, or the ancestor is
/// a root commit.
fn fixup_onto(repo: &Repository, target: Oid, source: Oid) -> Result<Oid, Error> {
    if source == target {
        return Err(estr("fixup: source and target are the same commit"));
    }
    let older = if repo.graph_descendant_of(source, target)? {
        target
    } else if repo.graph_descendant_of(target, source)? {
        source
    } else {
        return Err(estr(
            "fixup: source and target aren't on the same line of history",
        ));
    };
    let c = repo.find_commit(older)?;
    if c.parent_count() == 0 {
        return Err(estr("fixup: cannot fold into a root commit"));
    }
    Ok(c.parent(0)?.id())
}

/// Fold `source`'s changes into `target` (which keeps its own — already
/// signed-off — message), auto-picking the rest of the branch. A one-call
/// autosquash for a committed source: no plan to transcribe.
pub fn cmd_fixup(
    repo_path: &std::path::Path,
    target: &str,
    source: &str,
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let target = resolve_s(&repo, target)?;
    let source = resolve_s(&repo, source)?;
    let onto = fixup_onto(&repo, target, source).map_err(gerr)?;
    let steps = autosquash_steps(&repo, onto, &[(source, target, Action::Fixup)]).map_err(gerr)?;
    let plan = Plan { onto, steps };
    if rehearse_only {
        return Ok(preview_text(
            &repo,
            &rehearse(&repo, &plan, Mode::Pick).map_err(gerr)?,
        ));
    }
    let note = backup_note(&repo);
    let out = start(&repo, plan).map_err(gerr)?;
    Ok(format!("{}{note}", outcome_with_tree_note(&repo, &out)))
}

/// A byte-exact snapshot of the worktree files a diff touches. The worktree
/// fixup runs the replay on a CLEAN tree (the sequencer hard-resets and
/// replays through the worktree), so the uncommitted state is captured first
/// and written back verbatim afterwards — after a successful fold the
/// restored files differ from the new tip by exactly the changes that were
/// NOT folded.
struct WorktreeSnapshot(Vec<(std::path::PathBuf, SnapshotFile)>);

/// One snapshotted file: its bytes + executable bit; `None` = the path is
/// deleted in the worktree.
type SnapshotFile = Option<(Vec<u8>, bool)>;

fn snapshot_worktree(repo: &Repository, diff: &git2::Diff) -> Result<WorktreeSnapshot, Error> {
    let workdir = repo.workdir().ok_or_else(|| estr("bare repository"))?;
    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::new();
    for idx in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(idx) else {
            continue;
        };
        for f in [delta.old_file().path(), delta.new_file().path()] {
            let Some(rel) = f else { continue };
            if !seen.insert(rel.to_path_buf()) {
                continue;
            }
            let abs = workdir.join(rel);
            let entry = match std::fs::read(&abs) {
                Ok(bytes) => {
                    #[cfg(unix)]
                    let exec = {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::metadata(&abs)
                            .map(|m| m.permissions().mode() & 0o111 != 0)
                            .unwrap_or(false)
                    };
                    #[cfg(not(unix))]
                    let exec = false;
                    Some((bytes, exec))
                }
                Err(_) => None, // deleted in the worktree
            };
            files.push((rel.to_path_buf(), entry));
        }
    }
    Ok(WorktreeSnapshot(files))
}

/// Write a snapshot back over the worktree. Failures are collected and
/// reported per path, never swallowed — a lost uncommitted change must be
/// loud (and is still recoverable from the `-worktree` backup ref).
fn restore_worktree(repo: &Repository, snap: &WorktreeSnapshot) -> Result<(), Error> {
    let workdir = repo.workdir().ok_or_else(|| estr("bare repository"))?;
    let mut failed: Vec<String> = Vec::new();
    for (rel, entry) in &snap.0 {
        let abs = workdir.join(rel);
        let outcome = match entry {
            Some((bytes, exec)) => {
                if let Some(dir) = abs.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let write = std::fs::write(&abs, bytes);
                #[cfg(unix)]
                if write.is_ok() && *exec {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&abs) {
                        let mut p = meta.permissions();
                        p.set_mode(p.mode() | 0o111);
                        let _ = std::fs::set_permissions(&abs, p);
                    }
                }
                write
            }
            None => match std::fs::remove_file(&abs) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        if let Err(e) = outcome {
            failed.push(format!("{} ({e})", rel.display()));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(estr(&format!(
            "restoring the worktree failed for: {} — the pre-op worktree is \
             recoverable from the -worktree backup ref",
            failed.join(", ")
        )))
    }
}

/// Fold selected UNCOMMITTED changes into `target` — `git add -p` for agents:
/// build a fixup commit from just those worktree hunks (a dangling commit;
/// the branch never points at it), relocate it under `target` exactly like
/// `cmd_fixup`, and hand the unfolded rest of the uncommitted work back
/// afterwards. An empty selection folds every uncommitted change. A fold
/// whose tail replay conflicts is aborted whole: branch and worktree come
/// back exactly as they were (no half-done rebase is ever left over parked
/// uncommitted work).
pub fn cmd_fixup_worktree(
    repo_path: &std::path::Path,
    target: &str,
    paths: &[String],
    hunks: &[HunkSel],
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let target = resolve_s(&repo, target)?;
    fixup_worktree(&repo, target, paths, hunks, rehearse_only).map_err(gerr)
}

/// Compare a branch before and after a rewrite, commit by commit — the
/// "did the rewrite change anything it should not have" answer, natural
/// after any rebase/fixup: `git_range_diff {old: refs/mime-backup/<branch>/0,
/// new: HEAD}`. Commits of `base..old` and `base..new` (base = merge base)
/// pair by patch-id, then by summary; each pair reports whether its PATCH
/// and its MESSAGE drifted (an approximation of git range-diff — patch-id
/// equality instead of a full diff-of-diffs).
pub fn cmd_range_diff(repo_path: &std::path::Path, old: &str, new: &str) -> Result<String, String> {
    let repo = open(repo_path)?;
    let old = resolve_s(&repo, old)?;
    let new = resolve_s(&repo, new)?;
    range_diff(&repo, old, new).map_err(gerr)
}

fn range_diff(repo: &Repository, old: Oid, new: Oid) -> Result<String, Error> {
    if old == new {
        return Ok("the two tips are the same commit — nothing to compare".to_string());
    }
    let base = repo.merge_base(old, new)?;
    let commits_between = |tip: Oid| -> Result<Vec<Oid>, Error> {
        let mut walk = repo.revwalk()?;
        walk.push(tip)?;
        walk.hide(base)?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        walk.collect()
    };
    let olds = commits_between(old)?;
    let news = commits_between(new)?;

    let summary_of = |oid: Oid| -> String {
        repo.find_commit(oid)
            .ok()
            .and_then(|c| c.summary().map(str::to_string))
            .unwrap_or_default()
    };
    let message_of = |oid: Oid| -> String {
        repo.find_commit(oid)
            .ok()
            .and_then(|c| c.message().map(str::to_string))
            .unwrap_or_default()
    };
    let files_of = |oid: Oid| -> Vec<String> {
        let Ok(c) = repo.find_commit(oid) else {
            return Vec::new();
        };
        let parent_tree = c.parent(0).ok().and_then(|p| p.tree().ok());
        let Ok(tree) = c.tree() else {
            return Vec::new();
        };
        match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None) {
            Ok(d) => {
                let mut v: Vec<String> = diff_paths(&d).into_iter().collect();
                v.sort();
                v
            }
            Err(_) => Vec::new(),
        }
    };

    // Pair new commits to old ones: identical patch-id first, then the first
    // unpaired old commit with the same summary (the reworded/reshaped pair).
    let old_pids: Vec<Option<Oid>> = olds
        .iter()
        .map(|o| commit_patch_id(repo, *o).unwrap_or(None))
        .collect();
    let mut old_taken = vec![false; olds.len()];
    let mut pair_of: Vec<Option<usize>> = Vec::with_capacity(news.len());
    for n in &news {
        let pid = commit_patch_id(repo, *n).unwrap_or(None);
        let by_pid =
            pid.and_then(|p| (0..olds.len()).find(|&i| !old_taken[i] && old_pids[i] == Some(p)));
        let found = by_pid.or_else(|| {
            let s = summary_of(*n);
            (0..olds.len()).find(|&i| !old_taken[i] && summary_of(olds[i]) == s)
        });
        if let Some(i) = found {
            old_taken[i] = true;
        }
        pair_of.push(found);
    }

    let mut out = format!(
        "{} commit(s) before → {} after (base {})\n",
        olds.len(),
        news.len(),
        short(base)
    );
    // Dropped old commits first, in their original order.
    for (i, o) in olds.iter().enumerate() {
        if !old_taken[i] {
            out.push_str(&format!(
                "  - {}  (dropped) {}\n",
                short(*o),
                summary_of(*o)
            ));
        }
    }
    for (n, pair) in news.iter().zip(&pair_of) {
        match pair {
            None => out.push_str(&format!("  + {}  (added) {}\n", short(*n), summary_of(*n))),
            Some(i) => {
                let o = olds[*i];
                let same_patch = {
                    let (a, b) = (
                        commit_patch_id(repo, o).unwrap_or(None),
                        commit_patch_id(repo, *n).unwrap_or(None),
                    );
                    a.is_some() && a == b
                };
                let same_msg = message_of(o) == message_of(*n);
                if same_patch && same_msg {
                    out.push_str(&format!(
                        "  = {} → {}  {}\n",
                        short(o),
                        short(*n),
                        summary_of(*n)
                    ));
                } else {
                    let mut drift: Vec<String> = Vec::new();
                    if !same_patch {
                        let (fa, fb) = (files_of(o), files_of(*n));
                        drift.push(if fa == fb {
                            format!("patch changed (same files: {})", fb.join(", "))
                        } else {
                            format!(
                                "patch changed (files before: {}; after: {})",
                                fa.join(", "),
                                fb.join(", ")
                            )
                        });
                    }
                    if !same_msg {
                        drift.push("message changed".to_string());
                    }
                    out.push_str(&format!(
                        "  ! {} → {}  {} — {}\n",
                        short(o),
                        short(*n),
                        summary_of(*n),
                        drift.join("; ")
                    ));
                }
            }
        }
    }
    // Also say whether the two final trees are identical.
    let (ot, nt) = (
        repo.find_commit(old)?.tree_id(),
        repo.find_commit(new)?.tree_id(),
    );
    out.push_str(if ot == nt {
        "  final trees identical — the rewrite only re-sliced history\n"
    } else {
        "  final trees DIFFER\n"
    });
    Ok(out)
}

/// Discard selected UNCOMMITTED hunks — the destructive sibling of the
/// worktree git_fixup: the same {paths, hunks} selectors, but the chosen
/// changes reset to HEAD content instead of folding into a commit. Always
/// recoverable: the FULL pre-discard worktree is stamped on the
/// `-worktree` backup ref before anything is touched. Selection is
/// mandatory (discarding "everything" must be said path by path).
pub fn cmd_discard(
    repo_path: &std::path::Path,
    paths: &[String],
    hunks: &[HunkSel],
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    if paths.is_empty() && hunks.is_empty() {
        return Err(
            "git_discard: select what to drop ({paths} and/or {hunks}) — there is \
             no discard-everything shorthand, deliberately"
                .to_string(),
        );
    }
    discard(&repo, paths, hunks, rehearse_only).map_err(gerr)
}

fn discard(
    repo: &Repository,
    paths: &[String],
    hunks: &[HunkSel],
    rehearse_only: bool,
) -> Result<String, Error> {
    let head = repo.head()?.peel_to_commit()?;
    let head_tree = head.tree()?;
    let diff = worktree_diff(repo, None)?;
    if diff.deltas().len() == 0 {
        return Err(estr("discard: the worktree has no uncommitted changes"));
    }
    let sel = resolve_move_selection(&diff, paths, hunks, "discard")?;

    // What would go: every selected hunk, listed per path.
    let mut dropped: Vec<String> = Vec::new();
    for idx in 0..diff.deltas().len() {
        let Some(patch) = git2::Patch::from_diff(&diff, idx)? else {
            continue;
        };
        let path = delta_path(diff.get_delta(idx));
        if patch.num_hunks() == 0 {
            if sel.whole.contains(&path) {
                dropped.push(format!("{path} (whole — rename/mode/binary)"));
            }
            continue;
        }
        for h in 0..patch.num_hunks() {
            let (dh, _) = patch.hunk(h)?;
            let (ns, nl) = (dh.new_start(), dh.new_lines());
            if sel.takes(&path, ns, nl) {
                let span = if nl == 0 {
                    format!("L{ns}")
                } else {
                    format!("L{ns}-{}", ns + nl - 1)
                };
                dropped.push(format!("{path} {span}"));
            }
        }
    }
    if dropped.is_empty() {
        return Err(estr("discard: the selection holds no changes"));
    }
    let listing = dropped
        .iter()
        .map(|d| format!("  {d}"))
        .collect::<Vec<_>>()
        .join("\n");
    if rehearse_only {
        return Ok(format!(
            "rehearse: would discard {} uncommitted hunk(s):\n{listing}\n(nothing changed)",
            dropped.len()
        ));
    }

    // The parachute FIRST: the full pre-discard worktree as a dangling commit
    // on the -worktree ref — a discard is destructive by intent, never by
    // accident.
    let sig = match repo.signature() {
        Ok(s) => s,
        Err(_) => {
            let a = head.author();
            git2::Signature::now(
                a.name().unwrap_or("mime"),
                a.email().unwrap_or("mime@invalid"),
            )?
        }
    };
    let wt_tree_id = apply_subset(repo, &head_tree, &diff, |_| true, |_, _, _| true)?;
    let wt_backup = repo.commit(
        None,
        &sig,
        &sig,
        "mime worktree backup (pre discard)",
        &repo.find_tree(wt_tree_id)?,
        &[&head],
    )?;
    let short_branch = repo.head()?.shorthand().unwrap_or("HEAD").replace('/', "-");
    let backup_ref = format!("refs/mime-backup/{short_branch}-worktree");
    repo.reference(&backup_ref, wt_backup, true, "mime discard backup")?;

    // The KEPT tree: HEAD plus every change NOT selected; the discarded spans
    // fall back to HEAD content. Touched paths take their bytes from it, and
    // their index entries reset to HEAD (like the other worktree ops, what
    // remains is unstaged).
    let kept_id = apply_subset(
        repo,
        &head_tree,
        &diff,
        |p| !sel.whole.contains(p),
        |p, ns, nl| !sel.takes(p, ns, nl),
    )?;
    let kept = repo.find_tree(kept_id)?;
    let workdir = repo.workdir().ok_or_else(|| estr("bare repository"))?;
    let touched: Vec<String> = {
        let mut v: Vec<String> = diff_paths(&diff).into_iter().collect();
        v.sort();
        v
    };
    for p in &touched {
        let abs = workdir.join(p);
        match kept.get_path(Path::new(p)) {
            Ok(entry) => {
                if let Some(dir) = abs.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let blob = repo.find_blob(entry.id())?;
                std::fs::write(&abs, blob.content()).map_err(|e| {
                    estr(&format!(
                        "cannot write {p}: {e} — the pre-discard worktree is on {backup_ref}"
                    ))
                })?;
                #[cfg(unix)]
                if entry.filemode() == 0o100755 {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&abs) {
                        let mut perm = meta.permissions();
                        perm.set_mode(perm.mode() | 0o111);
                        let _ = std::fs::set_permissions(&abs, perm);
                    }
                }
            }
            Err(_) => match std::fs::remove_file(&abs) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(estr(&format!(
                        "cannot remove {p}: {e} — the pre-discard worktree is on {backup_ref}"
                    )));
                }
                _ => {}
            },
        }
    }
    repo.reset_default(Some(head.as_object()), touched.iter())?;
    Ok(format!(
        "discarded {} uncommitted hunk(s):\n{listing}\n  pre-discard worktree recoverable from {backup_ref}",
        dropped.len()
    ))
}

/// Absorb: fold EVERY uncommitted hunk into the commit that owns its lines —
/// `git_blame {worktree}` composed with `git_fixup {hunks}` in one call
/// (magit-commit-absorb / git-absorb). Hunks without a single clear owner
/// (new lines, split ownership, owners at the `since` boundary or off the
/// branch line, root commits) stay in the worktree and are reported; the
/// clear ones fold in ONE replay. `rehearse_only` previews the grouping and
/// the resulting history without touching anything.
pub fn cmd_absorb(
    repo_path: &std::path::Path,
    since: Option<&str>,
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let since = since.map(|s| resolve_s(&repo, s)).transpose()?;
    absorb(&repo, since, rehearse_only).map_err(gerr)
}

/// Visit every commit of `range` (oldest-first) in the worktree and run
/// `command` at each — the pr-prep gate loop ("does every commit build?")
/// that otherwise gets hand-rolled as `for c in rev-list; checkout c; cargo
/// check` in a shell. Stops on the first failure, naming the commit and the
/// command's output tail; the original HEAD (branch or detached) is restored
/// afterwards either way. Refuses on a dirty worktree.
///
/// GATED: running an arbitrary command breaks the git tools' default
/// "no hooks, no exec" posture, so it must be enabled explicitly by whoever
/// LAUNCHES the server (not the agent): set MIME_EXEC=1 in the environment.
pub fn cmd_exec_over(
    repo_path: &std::path::Path,
    range: &str,
    command: &str,
) -> Result<String, String> {
    let allowed = std::env::var("MIME_EXEC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !allowed {
        return Err(
            "git_exec_over: command execution is disabled by default (the git tools \
             promise no hooks, no exec). Whoever launches the server can allow it by \
             setting MIME_EXEC=1 in mime's environment."
                .to_string(),
        );
    }
    let repo = open(repo_path)?;
    exec_over(&repo, range, command).map_err(gerr)
}

fn exec_over(repo: &Repository, range: &str, command: &str) -> Result<String, Error> {
    if is_dirty(repo)? {
        return Err(estr(
            "exec_over: the worktree has uncommitted changes — each commit is checked \
             out in place, which would clobber them. Commit, absorb, or stash first.",
        ));
    }
    let workdir = repo
        .workdir()
        .ok_or_else(|| estr("bare repository"))?
        .to_path_buf();
    let mut walk = repo.revwalk()?;
    walk.push_range(range)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let commits: Vec<Oid> = walk.collect::<Result<_, _>>()?;
    if commits.is_empty() {
        return Err(estr(&format!("exec_over: no commits in {range}")));
    }
    // is_dirty ignores untracked files, but the force checkouts would still
    // clobber an untracked file colliding with a path in any visited tree —
    // and restoring HEAD (where the path is untracked) would then delete it.
    // begin() refuses this for its one target tree; here every commit of the
    // range is a target.
    let mut uopts = git2::StatusOptions::new();
    uopts
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let untracked: Vec<String> = repo
        .statuses(Some(&mut uopts))?
        .iter()
        .filter(|e| e.status().contains(git2::Status::WT_NEW))
        .filter_map(|e| e.path().map(str::to_string))
        .collect();
    if !untracked.is_empty() {
        for oid in &commits {
            let tree = repo.find_commit(*oid)?.tree()?;
            if let Some(p) = untracked
                .iter()
                .find(|p| tree.get_path(Path::new(p)).is_ok())
            {
                return Err(estr(&format!(
                    "exec_over: untracked file {p} would be overwritten by checking \
                     out {} — move or remove it first",
                    short(*oid)
                )));
            }
        }
    }
    // Remember where HEAD points so it can be restored (branch by name;
    // detached by oid).
    let head = repo.head()?;
    let branch = head.is_branch().then(|| head.name().map(str::to_string));
    let detached = head.target();
    drop(head);

    let force_checkout = |repo: &Repository| -> Result<(), Error> {
        let mut co = git2::build::CheckoutBuilder::new();
        co.force();
        repo.checkout_head(Some(&mut co))
    };
    let summary_of = |oid: Oid| {
        repo.find_commit(oid)
            .ok()
            .and_then(|c| c.summary().map(str::to_string))
            .unwrap_or_default()
    };
    let mut result: Result<String, Error> = Ok(String::new());
    for oid in &commits {
        repo.set_head_detached(*oid)?;
        force_checkout(repo)?;
        let run = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&workdir)
            .output();
        match run {
            Ok(out) if out.status.success() => {
                let report = result.as_mut().expect("only set on failure");
                report.push_str(&format!("  ok  {} {}\n", short(*oid), summary_of(*oid)));
            }
            Ok(out) => {
                // The command's output tail rides in the error — enough to
                // see WHAT broke without re-running by hand.
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let lines: Vec<&str> = stdout.lines().chain(stderr.lines()).collect();
                let start = lines.len().saturating_sub(20);
                result = Err(estr(&format!(
                    "exec_over: {:?} failed at {} ({}) with {}\n{}",
                    command,
                    short(*oid),
                    summary_of(*oid),
                    out.status,
                    lines[start..].join("\n"),
                )));
                break;
            }
            Err(e) => {
                result = Err(estr(&format!("exec_over: cannot run {command:?}: {e}")));
                break;
            }
        }
    }
    // Put HEAD back where it was, whatever happened above.
    let put_back = (|| -> Result<(), Error> {
        match (branch.flatten(), detached) {
            (Some(name), _) => repo.set_head(&name)?,
            (None, Some(oid)) => repo.set_head_detached(oid)?,
            (None, None) => {}
        }
        force_checkout(repo)
    })();
    match (result, put_back) {
        (Ok(report), Ok(())) => Ok(format!(
            "ran {command:?} on {} commit(s) — all passed\n{report}",
            commits.len()
        )),
        (Err(e), Ok(())) => Err(estr(&format!("{e}\n(HEAD restored — nothing changed)"))),
        (Err(e), Err(r)) => Err(estr(&format!(
            "{e}; and restoring HEAD failed: {r} — the worktree is on a \
             detached commit; check out your branch to recover"
        ))),
        (Ok(_), Err(r)) => Err(estr(&format!(
            "exec_over: every commit passed, but restoring HEAD failed: {r} — \
             the worktree is on a detached commit; check out your branch to recover"
        ))),
    }
}

/// Apply one `message_edits` vocabulary to EVERY commit of `range` — the bulk
/// trailer strip/add or identifier rename after a symbol rename. A sparse
/// rewrite touching only messages: each commit is re-created with its own
/// tree (byte-identical by construction) and re-parented, so nothing can
/// conflict. Per-commit replacement counts ride in the report; a `find` that
/// matches NOWHERE in the range is an error and nothing changes.
pub fn cmd_msg_rewrite(
    repo_path: &std::path::Path,
    range: &str,
    specs: &[MsgEditSpec],
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    if specs.is_empty() {
        return Err("git_msg_rewrite: message_edits must not be empty".to_string());
    }
    let edits: Vec<MsgEdit> = specs
        .iter()
        .map(MsgEdit::from_spec)
        .collect::<Result<_, _>>()?;
    msg_rewrite(&repo, range, &edits, rehearse_only).map_err(gerr)
}

/// Reword ONE commit's message — `message` replaces it wholesale, or
/// `message_edits` tweak it in place — as a sparse rewrite: the commit and its
/// descendants are re-created with their own trees (byte-identical, nothing
/// can conflict), no plan transcription needed. The everyday follow-up to a
/// review comment.
pub fn cmd_reword(
    repo_path: &std::path::Path,
    commit: &str,
    message: Option<&str>,
    specs: &[MsgEditSpec],
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    if message.is_none() && specs.is_empty() {
        return Err("git_reword: pass `message` (wholesale) and/or `message_edits`".to_string());
    }
    let edits: Vec<MsgEdit> = specs
        .iter()
        .map(MsgEdit::from_spec)
        .collect::<Result<_, _>>()?;
    let target = resolve_s(&repo, commit)?;
    reword(&repo, target, message, &edits, rehearse_only).map_err(gerr)
}

fn reword(
    repo: &Repository,
    target: Oid,
    message: Option<&str>,
    edits: &[MsgEdit],
    rehearse_only: bool,
) -> Result<String, Error> {
    let head_ref = repo.head()?;
    if !head_ref.is_branch() {
        return Err(estr("reword: HEAD is detached — check out a branch"));
    }
    let branch = head_ref
        .name()
        .ok_or_else(|| estr("HEAD has no branch name"))?
        .to_string();
    let head = head_ref.peel_to_commit()?.id();
    drop(head_ref);
    if target != head && !repo.graph_descendant_of(head, target)? {
        return Err(estr("reword: the commit is not on the current branch"));
    }

    // The new message, validated before anything is created.
    let tc = repo.find_commit(target)?;
    let base = message
        .map(str::to_string)
        .unwrap_or_else(|| tc.message().unwrap_or("").to_string());
    let new_msg = apply_msg_edits(base, edits)?;
    if new_msg == tc.message().unwrap_or("") {
        return Err(estr("reword: the message is unchanged — nothing to do"));
    }
    let tail: Vec<Oid> = commits_since(repo, target)?;
    if let Some(merge) = tail
        .iter()
        .chain(std::iter::once(&target))
        .find(|o| matches!(repo.find_commit(**o), Ok(c) if c.parent_count() > 1))
    {
        return Err(estr(&format!(
            "reword: {} is a merge — the sparse rewrite handles linear history only",
            short(*merge)
        )));
    }
    if rehearse_only {
        return Ok(format!(
            "rehearse: would reword {} to:\n{}\n(the tree and every descendant's \
             tree stay byte-identical)",
            short(target),
            new_msg.trim_end()
        ));
    }

    // Re-create the target with its own tree + the new message, then chain
    // its descendants (same trees and messages, re-parented).
    rotate_backup_ring(repo, &branch, head)?;
    let parents: Vec<git2::Commit> = tc.parents().collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    let mut tip = repo.commit(
        None,
        &tc.author(),
        &tc.committer(),
        &new_msg,
        &tc.tree()?,
        &parent_refs,
    )?;
    let reworded = tip;
    for oid in tail {
        let c = repo.find_commit(oid)?;
        let parent = repo.find_commit(tip)?;
        tip = repo.commit(
            None,
            &c.author(),
            &c.committer(),
            c.message().unwrap_or(""),
            &c.tree()?,
            &[&parent],
        )?;
    }
    repo.reference(&branch, tip, true, "mime reword")?;
    Ok(format!(
        "reworded {} → {}; every tree is byte-identical{}",
        short(target),
        short(reworded),
        backup_note(repo)
    ))
}

fn msg_rewrite(
    repo: &Repository,
    range: &str,
    edits: &[MsgEdit],
    rehearse_only: bool,
) -> Result<String, Error> {
    let head_ref = repo.head()?;
    if !head_ref.is_branch() {
        return Err(estr("msg_rewrite: HEAD is detached — check out a branch"));
    }
    let branch = head_ref
        .name()
        .ok_or_else(|| estr("HEAD has no branch name"))?
        .to_string();
    let head = head_ref.peel_to_commit()?.id();
    drop(head_ref);

    let mut walk = repo.revwalk()?;
    walk.push_range(range)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let commits: Vec<Oid> = walk.collect::<Result<_, _>>()?;
    if commits.is_empty() {
        return Err(estr(&format!("msg_rewrite: no commits in {range}")));
    }
    if *commits.last().expect("non-empty") != head {
        return Err(estr(
            "msg_rewrite: the range must end at HEAD (e.g. main..HEAD) — \
             rewriting below the tip would strand the commits above it",
        ));
    }

    // First pass: compute every new message + its per-edit counts, so a
    // range-wide miss aborts before anything is created.
    let mut new_msgs: Vec<(Oid, String, Vec<usize>)> = Vec::new();
    let mut totals = vec![0usize; edits.len()];
    for oid in &commits {
        let c = repo.find_commit(*oid)?;
        if c.parent_count() > 1 {
            return Err(estr(&format!(
                "msg_rewrite: {} is a merge — the sparse rewrite handles \
                 linear history only",
                short(*oid)
            )));
        }
        let (msg, counts) = apply_msg_edits_counted(c.message().unwrap_or(""), edits);
        for (t, n) in totals.iter_mut().zip(&counts) {
            *t += n;
        }
        new_msgs.push((*oid, msg, counts));
    }
    for (e, t) in edits.iter().zip(&totals) {
        if let (MsgEdit::Replace { find, .. }, 0) = (e, *t) {
            return Err(estr(&format!(
                "msg_rewrite: {find:?} matches no commit message in {range} — \
                 nothing changed"
            )));
        }
    }

    let counts_line = |counts: &[usize]| {
        counts
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if rehearse_only {
        let mut out = format!(
            "rehearse: would rewrite the messages of {} commit(s) in {range} \
             (trees byte-identical by construction)\n",
            commits.len()
        );
        for (oid, _, counts) in &new_msgs {
            out.push_str(&format!(
                "  {}  replacements per edit: {}\n",
                short(*oid),
                counts_line(counts)
            ));
        }
        return Ok(out);
    }

    // Second pass: re-create each commit with its own tree and the rewritten
    // message, chaining parents. An untouched prefix reproduces identical
    // objects (same message, tree, parents ⇒ same oid), so it is a no-op.
    rotate_backup_ring(repo, &branch, head)?;
    let mut new_parent: Option<Oid> = None;
    let mut out = String::new();
    let mut tip = head;
    for (oid, msg, counts) in &new_msgs {
        let c = repo.find_commit(*oid)?;
        let parents: Vec<git2::Commit> = match new_parent {
            Some(p) => vec![repo.find_commit(p)?],
            None => c.parents().collect(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let new = repo.commit(
            None,
            &c.author(),
            &c.committer(),
            msg,
            &c.tree()?,
            &parent_refs,
        )?;
        out.push_str(&if new == *oid {
            format!("  {}  unchanged\n", short(*oid))
        } else {
            format!(
                "  {} → {}  replacements per edit: {}\n",
                short(*oid),
                short(new),
                counts_line(counts)
            )
        });
        new_parent = Some(new);
        tip = new;
    }
    repo.reference(&branch, tip, true, "mime msg_rewrite")?;
    Ok(format!(
        "rewrote the messages of {} commit(s) in {range}; every tree is \
         byte-identical{}\n{out}",
        commits.len(),
        backup_note(repo)
    ))
}

fn absorb(repo: &Repository, since: Option<Oid>, rehearse_only: bool) -> Result<String, Error> {
    let head = repo.head()?.peel_to_commit()?;
    let head_tree = head.tree()?;
    let diff = worktree_diff(repo, None)?;
    if diff.deltas().len() == 0 {
        return Err(estr(
            "absorb: the worktree has no uncommitted changes to fold \
             (stage a brand-new file first — untracked files are not seen)",
        ));
    }
    let hunks = worktree_hunk_owners(repo, &diff, since)?;

    // Partition: hunks with one clear owner group under it; the rest stay in
    // the worktree, each with its reason.
    let mut groups: Vec<(Oid, Vec<&WorktreeHunk>)> = Vec::new();
    let mut left: Vec<(String, String)> = Vec::new();
    for h in &hunks {
        let reason = match h.owners.as_slice() {
            [] => Some("no prior owner — new lines".to_string()),
            [one] => {
                if since == Some(*one) {
                    // With `oldest_commit`, older lines collapse to the
                    // boundary — indistinguishable from genuinely owned there.
                    Some("owned at or beyond the since boundary".to_string())
                } else if repo.find_commit(*one)?.parent_count() == 0 {
                    Some("owned by the root commit (nothing to fold into)".to_string())
                } else {
                    None
                }
            }
            many => Some(format!(
                "split ownership: {}",
                many.iter()
                    .map(|o| short(*o))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
        match reason {
            Some(r) => left.push((format!("{} {}", h.path, h.span), r)),
            None => {
                let owner = h.owners[0];
                match groups.iter_mut().find(|(t, _)| *t == owner) {
                    Some((_, hs)) => hs.push(h),
                    None => groups.push((owner, vec![h])),
                }
            }
        }
    }
    let nothing = |left: &[(String, String)]| {
        estr(&format!(
            "absorb: no uncommitted hunk has a single owning commit on the \
             branch — nothing to fold\n{}",
            left.iter()
                .map(|(loc, why)| format!("  {loc} ({why})"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    };
    if groups.is_empty() {
        return Err(nothing(&left));
    }

    // The oldest owning commit anchors the replay.
    let mut oldest = groups[0].0;
    for (t, _) in &groups[1..] {
        if repo.graph_descendant_of(oldest, *t)? {
            oldest = *t;
        }
    }
    let onto = repo.find_commit(oldest)?.parent(0)?.id();
    let base = commits_since(repo, onto)?;
    // An owner off the replayed line (e.g. inside a merged side branch) can't
    // take a fold — leave its hunks in the worktree.
    groups.retain(|(t, hs)| {
        if base.contains(t) {
            return true;
        }
        for h in hs {
            left.push((
                format!("{} {}", h.path, h.span),
                format!("owner {} is not on the branch line", short(*t)),
            ));
        }
        false
    });
    if groups.is_empty() {
        return Err(nothing(&left));
    }

    // One dangling fixup commit per owning commit (the branch never points at
    // them; the fold discards their identity, so any signature works).
    let sig = match repo.signature() {
        Ok(s) => s,
        Err(_) => {
            let a = head.author();
            git2::Signature::now(
                a.name().unwrap_or("mime"),
                a.email().unwrap_or("mime@invalid"),
            )?
        }
    };
    let mut fixup_for: Vec<(Oid, Oid)> = Vec::new();
    for (target, hs) in &groups {
        let keys: std::collections::HashSet<(String, u32, u32)> =
            hs.iter().map(|h| (h.path.clone(), h.ns, h.nl)).collect();
        let tree_id = apply_subset(
            repo,
            &head_tree,
            &diff,
            |_| false,
            |p, ns, nl| keys.contains(&(p.to_string(), ns, nl)),
        )?;
        if tree_id == head_tree.id() {
            continue;
        }
        let tc = repo.find_commit(*target)?;
        let fixup = repo.commit(
            None,
            &sig,
            &sig,
            &format!("fixup! {}", tc.summary().unwrap_or("")),
            &repo.find_tree(tree_id)?,
            &[&head],
        )?;
        fixup_for.push((*target, fixup));
    }

    // Picks in branch order, each group's fold right after its target.
    let mut steps: Vec<Step> = Vec::new();
    for c in base {
        steps.push(pick_step(c));
        if let Some((_, f)) = fixup_for.iter().find(|(t, _)| *t == c) {
            let mut fold = pick_step(*f);
            fold.action = Action::Fixup;
            steps.push(fold);
        }
    }
    let step_commits: Vec<Oid> = steps.iter().map(|s| s.commit).collect();
    let plan = Plan { onto, steps };

    let mut report = format!(
        "absorb: {} hunk(s) → {} commit(s){}",
        groups.iter().map(|(_, hs)| hs.len()).sum::<usize>(),
        groups.len(),
        if left.is_empty() {
            String::new()
        } else {
            format!("; {} hunk(s) stay in the worktree", left.len())
        }
    );
    for (t, hs) in &groups {
        let summary = repo
            .find_commit(*t)
            .ok()
            .and_then(|c| c.summary().map(str::to_string))
            .unwrap_or_default();
        report.push_str(&format!("\n  into {} {summary}:", short(*t)));
        for h in hs {
            report.push_str(&format!("\n    {} {}", h.path, h.span));
        }
    }
    if !left.is_empty() {
        report.push_str("\n  left in the worktree:");
        for (loc, why) in &left {
            report.push_str(&format!("\n    {loc} ({why})"));
        }
    }

    if rehearse_only {
        return Ok(format!(
            "{}\n{report}\n(the hunks left in the worktree stay uncommitted)",
            preview_text(repo, &rehearse(repo, &plan, Mode::Pick)?)
        ));
    }
    let snap = snapshot_worktree(repo, &diff)?;
    let wt_tree_id = apply_subset(repo, &head_tree, &diff, |_| true, |_, _, _| true)?;
    let done = run_plan_over_parked_worktree(
        repo,
        plan,
        &step_commits,
        snap,
        wt_tree_id,
        &sig,
        &head,
        "absorb",
    )?;
    Ok(format!("{report}\n{done}"))
}

fn fixup_worktree(
    repo: &Repository,
    target: Oid,
    paths: &[String],
    hunks: &[HunkSel],
    rehearse_only: bool,
) -> Result<String, Error> {
    let head = repo.head()?.peel_to_commit()?;
    let head_tree = head.tree()?;
    if head.id() != target && !repo.graph_descendant_of(head.id(), target)? {
        return Err(estr("fixup: target is not an ancestor of HEAD"));
    }
    let diff = repo.diff_tree_to_workdir_with_index(Some(&head_tree), None)?;
    if diff.deltas().len() == 0 {
        return Err(estr(
            "fixup: the worktree has no uncommitted changes to fold \
             (stage a brand-new file first — untracked files are not seen)",
        ));
    }
    // What to fold: the explicit selection, or everything uncommitted.
    let sel = if paths.is_empty() && hunks.is_empty() {
        None
    } else {
        Some(resolve_move_selection(&diff, paths, hunks, "fixup")?)
    };
    let fixup_tree_id = match &sel {
        None => apply_subset(repo, &head_tree, &diff, |_| true, |_, _, _| true)?,
        Some(sel) => apply_subset(
            repo,
            &head_tree,
            &diff,
            |p| sel.whole.contains(p),
            |p, ns, nl| sel.whole.contains(p) || sel.hunks.contains(&(p.to_string(), ns, nl)),
        )?,
    };
    if fixup_tree_id == head_tree.id() {
        return Err(estr("fixup: the selection holds no changes"));
    }
    let tc = repo.find_commit(target)?;
    if tc.parent_count() == 0 {
        return Err(estr("fixup: cannot fold into a root commit"));
    }
    let onto = tc.parent(0)?.id();
    // The fixup source is a DANGLING commit of the selected changes on top of
    // HEAD — the branch never points at it; the replay only needs the object.
    // Its identity is discarded by the fold (the target keeps author+message),
    // so fall back to the target's author when the repo has no user config.
    let sig = match repo.signature() {
        Ok(s) => s,
        Err(_) => {
            let a = tc.author();
            git2::Signature::now(
                a.name().unwrap_or("mime"),
                a.email().unwrap_or("mime@invalid"),
            )?
        }
    };
    let fixup_tree = repo.find_tree(fixup_tree_id)?;
    let fixup = repo.commit(
        None,
        &sig,
        &sig,
        &format!("fixup! {}", tc.summary().unwrap_or("")),
        &fixup_tree,
        &[&head],
    )?;
    // commits_since is oldest-first; the fold lands right after the target.
    let mut steps: Vec<Step> = commits_since(repo, onto)?
        .into_iter()
        .map(pick_step)
        .collect();
    let pos = steps
        .iter()
        .position(|s| s.commit == target)
        .ok_or_else(|| estr("fixup: target not found in onto..HEAD"))?;
    let mut fold = pick_step(fixup);
    fold.action = Action::Fixup;
    steps.insert(pos + 1, fold);
    let step_commits: Vec<Oid> = steps.iter().map(|s| s.commit).collect();
    let plan = Plan { onto, steps };
    if rehearse_only {
        return Ok(format!(
            "{}\n(the unfolded uncommitted changes stay in the worktree)",
            preview_text(repo, &rehearse(repo, &plan, Mode::Pick)?)
        ));
    }

    // Park the uncommitted work: a byte snapshot for the restore, PLUS a
    // dangling backup commit of the full worktree stamped on a ref — even a
    // failed restore cannot lose uncommitted changes.
    let snap = snapshot_worktree(repo, &diff)?;
    // With no selection the fixup tree already IS the full worktree tree —
    // don't re-apply the whole diff a second time just for the backup.
    let wt_tree_id = match &sel {
        None => fixup_tree_id,
        Some(_) => apply_subset(repo, &head_tree, &diff, |_| true, |_, _, _| true)?,
    };
    run_plan_over_parked_worktree(
        repo,
        plan,
        &step_commits,
        snap,
        wt_tree_id,
        &sig,
        &head,
        "fixup",
    )
}

/// Park the uncommitted work (the byte `snap` for the restore, plus a dangling
/// full-worktree backup commit on the `-worktree` ref), replay `plan` over a
/// clean tree, then hand the parked changes back. The shared execution tail of
/// the worktree fixup and absorb; `what` names the operation in messages. A
/// replay conflict aborts the WHOLE operation — branch and worktree come back
/// exactly as they were.
#[allow(clippy::too_many_arguments)]
fn run_plan_over_parked_worktree(
    repo: &Repository,
    plan: Plan,
    step_commits: &[Oid],
    snap: WorktreeSnapshot,
    wt_tree_id: Oid,
    sig: &git2::Signature,
    head: &git2::Commit,
    what: &str,
) -> Result<String, Error> {
    let wt_tree = repo.find_tree(wt_tree_id)?;
    let wt_backup = repo.commit(
        None,
        sig,
        sig,
        &format!("mime worktree backup (pre {what}-from-worktree)"),
        &wt_tree,
        &[head],
    )?;
    let short_branch = repo.head()?.shorthand().unwrap_or("HEAD").replace('/', "-");
    repo.reference(
        &format!("refs/mime-backup/{short_branch}-worktree"),
        wt_backup,
        true,
        "mime sequencer worktree backup",
    )?;
    // The sequencer replays through the worktree and refuses to start dirty:
    // run it on a clean tree, then hand the parked changes back.
    hard_reset(repo, head.id())?;
    let outcome = match begin(repo, plan, Mode::Pick) {
        Ok(o) => o,
        Err(e) => {
            // begin can die AFTER detaching HEAD and writing the state file
            // (an I/O error mid-replay, not a conflict): abort any
            // half-started operation first, so the "nothing changed"
            // contract holds for hard errors too.
            let mut e = e;
            if status(repo).ok().flatten().is_some()
                && let Err(a) = abort(repo)
            {
                e = estr(&format!(
                    "{e}; and aborting the half-started operation failed: {a} — \
                     git_abort to recover"
                ));
            }
            restore_worktree(repo, &snap)?;
            return Err(e);
        }
    };
    match outcome {
        Outcome::Done { .. } => {
            restore_worktree(repo, &snap)?;
            Ok(format!(
                "{}\n(the uncommitted changes NOT folded are back in the worktree){}",
                outcome_text(&outcome),
                backup_note(repo)
            ))
        }
        Outcome::Conflict { step, files } => {
            // Never strand a half-done rebase over parked uncommitted work:
            // abort the whole fold and put the worktree back.
            abort(repo)?;
            restore_worktree(repo, &snap)?;
            let culprit = step_commits
                .get(step)
                .map(|c| short(*c))
                .unwrap_or_default();
            Err(estr(&format!(
                "{what}: replaying {culprit} over the folded change conflicts \
                 in {} — nothing changed (worktree restored). A later commit \
                 reshapes those lines: pick THAT commit as the target \
                 (git_blame {{worktree: true}} names it), or commit the change \
                 and drive the conflict interactively via git_fixup {{source}}.",
                files.join(", ")
            )))
        }
        Outcome::Paused { step, .. } => {
            // No `edit` step exists in this plan; a pause would be a bug.
            Err(estr(&format!(
                "{what}: unexpected pause at step {step} — git_status/git_abort \
                 the operation; the uncommitted changes are on the -worktree \
                 backup ref"
            )))
        }
    }
}

pub fn cmd_rebase(
    repo_path: &std::path::Path,
    onto: &str,
    from: Option<&str>,
    plan: Option<Vec<PlanItem>>,
    autosquash: Option<Autosquash>,
    rehearse_only: bool,
    reapply_cherry_picks: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let onto_oid = resolve_s(&repo, onto)?;
    // Three-arg --onto: `from` (git's <upstream>) bounds the replayed range —
    // from..HEAD lands on `onto` — so a stacked branch transplants only its own
    // commits. Defaults to `onto`, the classic two-arg behaviour.
    if from.is_some() && plan.is_some() {
        return Err(
            "from bounds the default (or autosquash) enumeration of onto..HEAD; \
             an explicit plan already lists its commits — pass one or the other"
                .to_string(),
        );
    }
    let from_oid = match from {
        Some(f) => resolve_s(&repo, f)?,
        None => onto_oid,
    };
    // Commits omitted from a plan-less pick-all because they are already present
    // in `onto` by patch-id (git's default cherry-pick detection).
    let mut dropped: Vec<Oid> = Vec::new();
    // A bare replay leaves fixup!/squash! commits as they are — the note below
    // points at autosquash:true, which is usually what such a branch wants.
    let plan_less = plan.is_none() && autosquash.is_none();
    let steps = if let Some(Autosquash::Markers) = autosquash {
        let derived = marker_directives(&repo, from_oid).map_err(gerr)?;
        autosquash_steps(&repo, from_oid, &derived).map_err(gerr)?
    } else if let Some(Autosquash::Directives(directives)) = autosquash {
        // An empty list would fall through to a bare pick-all — without even
        // the plan-less path's cherry-pick detection. Error like Markers does.
        if directives.is_empty() {
            return Err("autosquash: empty directive list — nothing to fold".to_string());
        }
        let resolved = directives
            .iter()
            .map(|(commit, into, action)| {
                let act = if action.is_empty() { "fixup" } else { action };
                let action = Action::parse(act)
                    .filter(|a| matches!(a, Action::Fixup | Action::Squash))
                    .ok_or_else(|| {
                        format!("autosquash action must be fixup or squash, got \"{act}\"")
                    })?;
                Ok((resolve_s(&repo, commit)?, resolve_s(&repo, into)?, action))
            })
            .collect::<Result<Vec<_>, String>>()?;
        autosquash_steps(&repo, from_oid, &resolved).map_err(gerr)?
    } else {
        match plan {
            Some(items) => items
                .iter()
                .map(|(commit, action, message, edits, into)| {
                    let message_edits = edits.iter().map(MsgEdit::from_spec).collect::<Result<
                        Vec<_>,
                        String,
                    >>(
                    )?;
                    Ok(Step {
                        commit: resolve_s(&repo, commit)?,
                        action: Action::parse(action)
                            .ok_or_else(|| format!("unknown action \"{action}\""))?,
                        message: message.clone(),
                        message_edits,
                        split_into: into.clone(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
            None => {
                let commits = if reapply_cherry_picks {
                    commits_since(&repo, from_oid).map_err(gerr)?
                } else {
                    let (kept, drop) =
                        partition_cherry_picks(&repo, onto_oid, from_oid).map_err(gerr)?;
                    dropped = drop;
                    kept
                };
                commits.into_iter().map(pick_step).collect()
            }
        }
    };
    let mark_note = if plan_less {
        marker_note(&repo, &steps)
    } else {
        String::new()
    };
    let plan = Plan {
        onto: onto_oid,
        steps,
    };
    let drop_note = dropped_note(&repo, onto_oid, &dropped);
    if rehearse_only {
        return Ok(format!(
            "{}{drop_note}{mark_note}",
            preview_text(&repo, &rehearse(&repo, &plan, Mode::Pick).map_err(gerr)?),
        ));
    }
    let note = backup_note(&repo);
    let out = start(&repo, plan).map_err(gerr)?;
    Ok(format!(
        "{}{drop_note}{mark_note}{note}",
        outcome_with_tree_note(&repo, &out)
    ))
}

/// A report of the commits a plan-less rebase skipped as already-applied, so the
/// drop is never silent. Empty when nothing was dropped.
/// A note appended to a plan-less replay whose range carries fixup!/squash!
/// marker commits: they are picked unchanged, so name the one argument that
/// would fold them instead — the point where an agent learns it exists.
fn marker_note(repo: &Repository, steps: &[Step]) -> String {
    let n = steps
        .iter()
        .filter(|s| {
            repo.find_commit(s.commit)
                .ok()
                .is_some_and(|c| marker_split(c.summary().unwrap_or("")).is_some())
        })
        .count();
    if n == 0 {
        return String::new();
    }
    format!(
        "\nnote: {n} fixup!/squash! commit(s) replayed as-is — \
         autosquash:true folds them into their targets"
    )
}

fn dropped_note(repo: &Repository, onto: Oid, dropped: &[Oid]) -> String {
    if dropped.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "\ndropped {} commit(s) already in {} (matched by patch-id; \
         pass reapply_cherry_picks:true to replay them):",
        dropped.len(),
        short(onto),
    );
    for oid in dropped {
        let summary = repo
            .find_commit(*oid)
            .ok()
            .and_then(|c| c.summary().map(str::to_string))
            .unwrap_or_default();
        s.push_str(&format!("\n  {} {summary}", short(*oid)));
    }
    s
}

fn resolve_all(repo: &Repository, specs: &[String]) -> Result<Vec<Oid>, String> {
    specs.iter().map(|s| resolve_s(repo, s)).collect()
}

pub fn cmd_cherry_pick(repo_path: &std::path::Path, commits: &[String]) -> Result<String, String> {
    let repo = open(repo_path)?;
    let oids = resolve_all(&repo, commits)?;
    let note = backup_note(&repo);
    Ok(format!(
        "{}{note}",
        outcome_text(&cherry_pick(&repo, oids).map_err(gerr)?)
    ))
}

pub fn cmd_revert(repo_path: &std::path::Path, commits: &[String]) -> Result<String, String> {
    let repo = open(repo_path)?;
    let oids = resolve_all(&repo, commits)?;
    let note = backup_note(&repo);
    Ok(format!(
        "{}{note}",
        outcome_text(&revert(&repo, oids).map_err(gerr)?)
    ))
}

pub fn cmd_continue(repo_path: &std::path::Path, force: bool) -> Result<String, String> {
    let repo = open(repo_path)?;
    let out = continue_op(&repo, force).map_err(gerr)?;
    Ok(outcome_with_tree_note(&repo, &out))
}

pub fn cmd_skip(repo_path: &std::path::Path) -> Result<String, String> {
    let repo = open(repo_path)?;
    let out = skip(&repo).map_err(gerr)?;
    Ok(outcome_with_tree_note(&repo, &out))
}

pub fn cmd_abort(repo_path: &std::path::Path) -> Result<String, String> {
    let repo = open(repo_path)?;
    let kept = abort(&repo).map_err(gerr)?;
    Ok(format!(
        "aborted — restored to the pre-operation state{}",
        kept_note(&kept)
    ))
}

pub fn cmd_status(repo_path: &std::path::Path) -> Result<String, String> {
    Ok(status_text(status(&open(repo_path)?).map_err(gerr)?))
}

pub fn cmd_log(repo_path: &std::path::Path, range: Option<&str>) -> Result<String, String> {
    log(&open(repo_path)?, range, 50).map_err(gerr)
}

pub fn cmd_show(repo_path: &std::path::Path, commit: &str) -> Result<String, String> {
    let repo = open(repo_path)?;
    let oid = resolve_s(&repo, commit)?;
    show(&repo, oid).map_err(gerr)
}

pub fn cmd_blame(
    repo_path: &std::path::Path,
    path: Option<&str>,
    lines: Option<(usize, usize)>,
    since: Option<&str>,
    worktree: bool,
    group_by_commit: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let since = since.map(|s| resolve_s(&repo, s)).transpose()?;
    if group_by_commit && !worktree {
        return Err("git_blame: group_by applies to worktree mode only".to_string());
    }
    // Accept an absolute path (what grep/occur hand back) or one already
    // relative to the workdir; blame_file wants it workdir-relative.
    let rel = path
        .map(|path| -> Result<std::path::PathBuf, String> {
            let p = std::path::Path::new(path);
            if p.is_absolute() {
                let wd = repo
                    .workdir()
                    .ok_or_else(|| "bare repo has no working tree to blame".to_string())?;
                Ok(p.strip_prefix(wd)
                    .map_err(|_| format!("path is outside the repo: {path}"))?
                    .to_path_buf())
            } else {
                Ok(p.to_path_buf())
            }
        })
        .transpose()?;
    if worktree {
        blame_worktree(&repo, rel.as_deref(), since, group_by_commit).map_err(gerr)
    } else {
        let rel = rel.ok_or_else(|| {
            "git_blame: pass `path` (blaming committed lines is per-file; only \
             worktree mode can sweep the whole tree)"
                .to_string()
        })?;
        blame(&repo, &rel, lines, since).map_err(gerr)
    }
}

pub fn cmd_move(
    repo_path: &std::path::Path,
    from: &str,
    to: &str,
    paths: &[String],
    hunks: &[HunkSel],
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let from = resolve_s(&repo, from)?;
    let to = resolve_s(&repo, to)?;
    move_changes(&repo, from, to, paths, hunks).map_err(gerr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use git2::Signature;
    use std::path::Path;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mime-seq-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Commit a tree built from `files`, with explicit parents, updating no ref.
    fn commit(repo: &Repository, parents: &[Oid], files: &[(&str, &str)], msg: &str) -> Oid {
        let mut tb = repo.treebuilder(None).unwrap();
        for (name, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            tb.insert(name, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let sig = Signature::now("test", "test@example.invalid").unwrap();
        let pc: Vec<_> = parents
            .iter()
            .map(|o| repo.find_commit(*o).unwrap())
            .collect();
        let pr: Vec<&git2::Commit> = pc.iter().collect();
        repo.commit(None, &sig, &sig, msg, &tree, &pr).unwrap()
    }

    /// Point `name` (and HEAD + worktree) at `tip`.
    fn on_branch(repo: &Repository, name: &str, tip: Oid) {
        repo.branch(name, &repo.find_commit(tip).unwrap(), true)
            .unwrap();
        repo.set_head(&format!("refs/heads/{name}")).unwrap();
        repo.reset(&repo.find_object(tip, None).unwrap(), ResetType::Hard, None)
            .unwrap();
    }

    fn read(repo: &Repository, f: &str) -> String {
        std::fs::read_to_string(repo.workdir().unwrap().join(f)).unwrap()
    }

    fn step(commit: Oid, action: Action, message: Option<&str>) -> Step {
        Step {
            commit,
            action,
            message: message.map(str::to_string),
            message_edits: Vec::new(),
            split_into: Vec::new(),
        }
    }

    /// The de-risking gate: a cherry-pick conflict must produce diff3 markers
    /// the conflict scanner parses, base section and all.
    #[test]
    fn cherrypick_conflict_writes_diff3_markers_that_conflict_rs_parses() {
        let dir = tmp("cp");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("f.txt", "base line\n")], "base");
        let ours = commit(&repo, &[base], &[("f.txt", "our line\n")], "ours");
        let theirs = commit(&repo, &[base], &[("f.txt", "their line\n")], "theirs");

        let ours_c = repo.find_commit(ours).unwrap();
        let theirs_c = repo.find_commit(theirs).unwrap();
        let mut index = repo.cherrypick_commit(&theirs_c, &ours_c, 0, None).unwrap();
        assert!(index.has_conflicts());

        repo.checkout_index(Some(&mut index), Some(&mut diff3_checkout()))
            .unwrap();
        let text = read(&repo, "f.txt");
        let mut b = Buffer::from_string("f.txt", &text);
        let hunks = crate::conflict::scan(&mut b);
        assert_eq!(hunks.len(), 1, "worktree:\n{text}");
        assert!(hunks[0].base.is_some(), "diff3 base present:\n{text}");
    }

    #[test]
    fn clean_rebase_applies_changes_onto_new_base() {
        let dir = tmp("clean");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "a"), "2\n");
        assert_eq!(read(&repo, "b"), "1\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent(0).unwrap().id(), m1, "rebased onto m1");
        assert!(!state_path(&repo).exists(), "state cleared on finish");
    }

    #[test]
    fn reword_swaps_the_message() {
        let dir = tmp("reword");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "original");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Reword, Some("reworded"))],
            },
        )
        .unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "reworded");
    }

    #[test]
    fn drop_omits_a_commit() {
        let dir = tmp("drop");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let f2 = commit(
            &repo,
            &[f1],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "add c",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f2);

        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None), step(f2, Action::Drop, None)],
            },
        )
        .unwrap();
        assert_eq!(read(&repo, "b"), "1\n");
        assert!(
            !repo.workdir().unwrap().join("c").exists(),
            "dropped commit's file is absent"
        );
    }

    #[test]
    fn conflict_stops_then_continue_lands_the_resolution() {
        let dir = tmp("conflict");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "10\n")], "ours change a");
        let m1 = commit(&repo, &[base], &[("a", "20\n")], "their change a");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap();
        assert_eq!(
            out,
            Outcome::Conflict {
                step: 0,
                files: vec!["a".to_string()]
            }
        );
        // The worktree carries parseable markers.
        let mut b = Buffer::from_string("a", read(&repo, "a"));
        assert_eq!(crate::conflict::scan(&mut b).len(), 1);
        assert!(status(&repo).unwrap().is_some());

        // Resolve and resume.
        std::fs::write(repo.workdir().unwrap().join("a"), "resolved\n").unwrap();
        let out = continue_op(&repo, false).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "a"), "resolved\n");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn skip_omits_the_conflicting_step() {
        let dir = tmp("skip");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "10\n")], "ours a");
        let f2 = commit(&repo, &[f1], &[("a", "10\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "20\n")], "their a");
        on_branch(&repo, "topic", f2);

        // f1 conflicts on a; f2 (adds b) applies cleanly after skipping f1.
        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None), step(f2, Action::Pick, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Conflict { step: 0, .. }));

        let out = skip(&repo).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "a"), "20\n", "skipped f1, kept their a");
        assert_eq!(read(&repo, "b"), "1\n", "f2 still applied");
    }

    #[test]
    fn abort_restores_the_pre_op_state() {
        let dir = tmp("abort");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "10\n")], "ours");
        let m1 = commit(&repo, &[base], &[("a", "20\n")], "theirs");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Conflict { .. }));

        abort(&repo).unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.id(), f1, "branch restored to its original tip");
        assert_eq!(read(&repo, "a"), "10\n", "worktree restored");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn squash_melds_two_commits_into_one() {
        let dir = tmp("squash");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let f2 = commit(
            &repo,
            &[f1],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "add c",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f2);

        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None), step(f2, Action::Squash, None)],
            },
        )
        .unwrap();
        // One commit on m1 carrying both changes plus the new base.
        assert_eq!(read(&repo, "a"), "2\n");
        assert_eq!(read(&repo, "b"), "1\n");
        assert_eq!(read(&repo, "c"), "1\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent(0).unwrap().id(), m1, "squashed onto m1");
        let msg = tip.message().unwrap();
        assert!(
            msg.contains("add b") && msg.contains("add c"),
            "melded message: {msg}"
        );
    }

    #[test]
    fn fixup_keeps_the_first_message() {
        let dir = tmp("fixup");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "keep me");
        let f2 = commit(
            &repo,
            &[f1],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "discard me",
        );
        on_branch(&repo, "topic", f2);

        start(
            &repo,
            Plan {
                onto: base,
                steps: vec![step(f1, Action::Pick, None), step(f2, Action::Fixup, None)],
            },
        )
        .unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "keep me");
        assert_eq!(read(&repo, "c"), "1\n", "fixup's changes still applied");
    }

    #[test]
    fn cherry_pick_appends_a_commit_onto_head() {
        let dir = tmp("cherry");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let c1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "main work");
        let x = commit(&repo, &[base], &[("a", "1\n"), ("x", "1\n")], "add x");
        on_branch(&repo, "main", c1);

        let out = cherry_pick(&repo, vec![x]).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "b"), "1\n", "existing work preserved");
        assert_eq!(read(&repo, "x"), "1\n", "picked change applied");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent(0).unwrap().id(), c1, "appended on the tip");
    }

    #[test]
    fn revert_undoes_a_commit_on_top() {
        let dir = tmp("revert");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let c1 = commit(&repo, &[base], &[("a", "2\n")], "bump a");
        on_branch(&repo, "main", c1);

        let out = revert(&repo, vec![c1]).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "a"), "1\n", "change undone");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent(0).unwrap().id(), c1, "revert commit on top");
        assert!(tip.message().unwrap().contains("Revert \"bump a\""));
    }

    #[test]
    fn cmd_layer_drives_a_rebase_by_revspec_and_reports() {
        let dir = tmp("cmd");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        // Default plan (onto..HEAD) via the path-facing wrapper, onto by oid string.
        let out = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap();
        assert!(out.starts_with("done"), "{out}");
        // `add b` and `change a` are distinct patches, so cherry-pick detection
        // drops nothing — a plain rebase onto a diverged base is unaffected.
        assert!(!out.contains("dropped"), "{out}");
        assert_eq!(
            cmd_status(&dir).unwrap(),
            "no sequencer operation in progress"
        );

        let log = cmd_log(&dir, None).unwrap();
        assert!(log.contains("add b") && log.contains("change a"), "{log}");
        let show = cmd_show(&dir, "HEAD").unwrap();
        assert!(
            show.contains("Changed files") && show.contains(" b"),
            "{show}"
        );
        // The full unified diff follows the file summary: b's added line shows.
        assert!(
            show.contains("Diff:") && show.contains("+1"),
            "show carries the unified diff: {show}"
        );
    }

    #[test]
    fn three_arg_onto_transplants_only_from_head() {
        // The stacked-branch driver: `lower` was AMENDED (new patch-id, so
        // cherry-pick detection cannot drop the stale copy), and topic must
        // transplant only its own commits — from..HEAD — onto the rewrite.
        let dir = tmp("rebase-three-arg");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("base", "0\n")], "base");
        let l1 = commit(
            &repo,
            &[base],
            &[("base", "0\n"), ("lib", "v1\n")],
            "add lib v1",
        );
        let t1 = commit(
            &repo,
            &[l1],
            &[("base", "0\n"), ("lib", "v1\n"), ("t1", "1\n")],
            "t1",
        );
        let t2 = commit(
            &repo,
            &[t1],
            &[
                ("base", "0\n"),
                ("lib", "v1\n"),
                ("t1", "1\n"),
                ("t2", "1\n"),
            ],
            "t2",
        );
        on_branch(&repo, "topic", t2);
        // The rewritten lower branch: same file, different content — a
        // different patch, so only `from` can keep it out of the replay.
        let l1p = commit(
            &repo,
            &[base],
            &[("base", "0\n"), ("lib", "v2\n")],
            "lib v2 (amended)",
        );

        let pre = cmd_rebase(
            &dir,
            &l1p.to_string(),
            Some(&l1.to_string()),
            None,
            None,
            true,
            false,
        )
        .unwrap();
        assert!(pre.contains("t1") && pre.contains("t2"), "{pre}");
        assert!(
            !pre.contains("add lib v1"),
            "old base commit excluded: {pre}"
        );

        let out = cmd_rebase(
            &dir,
            &l1p.to_string(),
            Some(&l1.to_string()),
            None,
            None,
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(read(&repo, "lib"), "v2\n", "the amended base won");
        assert_eq!(read(&repo, "t1"), "1\n");
        assert_eq!(read(&repo, "t2"), "1\n");
        let log = cmd_log(&dir, None).unwrap();
        assert!(log.contains("lib v2 (amended)"), "{log}");
        assert!(!log.contains("add lib v1"), "stale base commit gone: {log}");
    }

    #[test]
    fn three_arg_onto_refuses_an_explicit_plan() {
        let dir = tmp("rebase-from-plan");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        on_branch(&repo, "topic", f1);
        let err = cmd_rebase(
            &dir,
            &base.to_string(),
            Some(&base.to_string()),
            Some(vec![(
                f1.to_string(),
                "pick".to_string(),
                None,
                Vec::new(),
                Vec::new(),
            )]),
            None,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("explicit plan"), "{err}");
    }

    #[test]
    fn plan_less_rebase_drops_commits_already_in_the_rewritten_base() {
        // The dogfooding scenario: branch `topic` sits on a base whose top two
        // commits get REORDERED (rewritten to new oids, identical patch-ids).
        // Re-rebasing topic onto the rewritten base must drop topic's stale copies
        // of those commits (already present by patch-id) rather than replay them.
        let dir = tmp("rebase-cherry-drop");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("base", "0\n")], "base");
        let a1 = commit(&repo, &[base], &[("base", "0\n"), ("x", "1\n")], "add x");
        let a2 = commit(
            &repo,
            &[a1],
            &[("base", "0\n"), ("x", "1\n"), ("y", "1\n")],
            "add y",
        );
        let t = commit(
            &repo,
            &[a2],
            &[("base", "0\n"), ("x", "1\n"), ("y", "1\n"), ("z", "1\n")],
            "add z",
        );
        on_branch(&repo, "topic", t);

        // Rewrite the base: add-y then add-x — new oids, same patch-ids. `a1p` is
        // the rewritten tip we rebase onto; the merge-base with topic is `base`,
        // BELOW the rewrite, so onto..HEAD still holds the pre-rewrite add-x/add-y.
        let a2p = commit(&repo, &[base], &[("base", "0\n"), ("y", "1\n")], "add y");
        let a1p = commit(
            &repo,
            &[a2p],
            &[("base", "0\n"), ("x", "1\n"), ("y", "1\n")],
            "add x",
        );

        // rehearse, default: the two already-applied commits are reported dropped.
        let pre = cmd_rebase(&dir, &a1p.to_string(), None, None, None, true, false).unwrap();
        assert!(pre.contains("dropped 2 commit(s)"), "{pre}");
        assert!(pre.contains("add x") && pre.contains("add y"), "{pre}");

        // rehearse, reapply_cherry_picks: keeps them — no drop note.
        let keep = cmd_rebase(&dir, &a1p.to_string(), None, None, None, true, true).unwrap();
        assert!(!keep.contains("dropped"), "{keep}");

        // apply, default: topic ends as base→add y→add x→add z — no duplicates.
        let out = cmd_rebase(&dir, &a1p.to_string(), None, None, None, false, false).unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert!(out.contains("dropped 2 commit(s)"), "{out}");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.summary().unwrap(), "add z");
        assert_eq!(tip.parent(0).unwrap().id(), a1p);
        assert_eq!(
            commits_since(&repo, base).unwrap().len(),
            3,
            "no duplicates"
        );
    }

    #[test]
    fn cherry_pick_detection_never_drops_empty_or_merge_commits() {
        // Empty commits all hash to the constant empty-diff patch-id, and a merge
        // has no single patch — so neither may be matched as "already applied".
        // Otherwise one intentional empty commit upstream would drop an unrelated
        // empty commit (or an `ours` merge) on the branch.
        let dir = tmp("cherry-empty-merge");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        // Upstream (onto): a real commit then an intentional empty one.
        let u_real = commit(&repo, &[base], &[("a", "1\n"), ("u", "1\n")], "add u");
        let u_empty = commit(
            &repo,
            &[u_real],
            &[("a", "1\n"), ("u", "1\n")],
            "upstream empty",
        );
        // Branch: an intentional empty commit, a real commit, and an `ours` merge
        // (tree equals its first parent, so its first-parent diff is empty).
        let h_empty = commit(&repo, &[base], &[("a", "1\n")], "keep me (empty)");
        let t_real = commit(&repo, &[h_empty], &[("a", "1\n"), ("t", "1\n")], "add t");
        let side = commit(&repo, &[base], &[("a", "1\n"), ("s", "1\n")], "add s");
        let merge = commit(
            &repo,
            &[t_real, side],
            &[("a", "1\n"), ("t", "1\n")],
            "merge -s ours side",
        );
        on_branch(&repo, "topic", merge);

        let (kept, dropped) = partition_cherry_picks(&repo, u_empty, u_empty).unwrap();
        assert!(dropped.is_empty(), "nothing is a cherry-pick: {dropped:?}");
        assert!(kept.contains(&h_empty), "empty branch commit kept");
        assert!(kept.contains(&merge), "ours merge kept");
        assert!(kept.contains(&t_real), "real commit kept");
    }

    #[test]
    fn cmd_blame_attributes_each_line_to_its_last_commit() {
        let dir = tmp("blame");
        let repo = Repository::init(&dir).unwrap();
        // c1 introduces both lines; c2 rewrites only line 2.
        let c1 = commit(&repo, &[], &[("f.txt", "one\ntwo\n")], "add one and two");
        let c2 = commit(&repo, &[c1], &[("f.txt", "one\nTWO\n")], "change two");
        on_branch(&repo, "main", c2);

        let out = cmd_blame(&dir, Some("f.txt"), None, None, false, false).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // Line 1 is still c1's; line 2 now belongs to c2. Hunks are one line each.
        assert!(
            lines[0].starts_with("  L1\t") && lines[0].ends_with("add one and two"),
            "L1 -> c1: {out}"
        );
        assert!(
            lines[1].starts_with("  L2\t") && lines[1].ends_with("change two"),
            "L2 -> c2: {out}"
        );

        // A `lines` window blames only that span.
        let just2 = cmd_blame(&dir, Some("f.txt"), Some((2, 2)), None, false, false).unwrap();
        assert!(
            just2.contains("change two") && !just2.contains("add one and two"),
            "windowed blame: {just2}"
        );

        // An absolute path (what grep/occur hand back) resolves the same way.
        let abs = dir.join("f.txt");
        let via_abs =
            cmd_blame(&dir, Some(abs.to_str().unwrap()), None, None, false, false).unwrap();
        assert_eq!(via_abs, out, "absolute path blames identically");
    }

    #[test]
    fn blame_since_scopes_to_recent_commits() {
        let dir = tmp("blame-since");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("f", "a\n")], "base");
        let c1 = commit(&repo, &[base], &[("f", "a\nB\n")], "add B");
        let c2 = commit(&repo, &[c1], &[("f", "a\nB\nC\n")], "add C");
        on_branch(&repo, "main", c2);

        // Unscoped: line 1 ("a") is still base's.
        let full = cmd_blame(&dir, Some("f"), None, None, false, false).unwrap();
        assert!(full.contains(&short(base)), "base owns line 1: {full}");
        // Scoped to c1..: base predates the boundary, so its line collapses to c1
        // and base's oid no longer appears; c2 still owns its own line.
        let scoped = cmd_blame(&dir, Some("f"), None, Some(&c1.to_string()), false, false).unwrap();
        assert!(
            !scoped.contains(&short(base)),
            "base hidden by since: {scoped}"
        );
        assert!(
            scoped.contains(&short(c2)),
            "c2 still owns its line: {scoped}"
        );
    }

    #[test]
    fn blame_worktree_maps_a_change_to_its_owning_commit() {
        let dir = tmp("blame-wt");
        let repo = Repository::init(&dir).unwrap();
        let _base = commit(&repo, &[], &[("f", "a\nb\nc\n")], "base");
        let c1 = commit(&repo, &[_base], &[("f", "a\nB\nc\n")], "edit line 2");
        on_branch(&repo, "main", c1);

        // Re-edit line 2 (which c1 last set) WITHOUT committing.
        std::fs::write(repo.workdir().unwrap().join("f"), b"a\nB2\nc\n").unwrap();
        let out = cmd_blame(&dir, Some("f"), None, None, true, false).unwrap();
        // The uncommitted hunk at new line 2 is owned by c1.
        assert!(
            out.contains(&short(c1)) && out.contains("edit line 2"),
            "worktree hunk maps to c1: {out}"
        );
        assert!(out.contains("L2"), "{out}");
    }

    /// base sets f's five lines; c1 owns line 2, c2 owns line 4.
    fn absorb_fixture(tag: &str) -> (std::path::PathBuf, Repository, Oid, Oid, Oid) {
        let dir = tmp(tag);
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("f", "l1\nl2\nl3\nl4\nl5\n")], "base");
        let c1 = commit(&repo, &[base], &[("f", "l1\nc1\nl3\nl4\nl5\n")], "edit two");
        let c2 = commit(&repo, &[c1], &[("f", "l1\nc1\nl3\nc2\nl5\n")], "edit four");
        on_branch(&repo, "main", c2);
        (dir, repo, base, c1, c2)
    }

    #[test]
    fn absorb_folds_each_hunk_into_its_owning_commit() {
        let (dir, repo, base, _c1, _c2) = absorb_fixture("absorb");
        // Two uncommitted hunks: line 2 (c1's) and line 4 (c2's).
        std::fs::write(repo.workdir().unwrap().join("f"), b"l1\nw1\nl3\nw2\nl5\n").unwrap();

        let out = cmd_absorb(&dir, None, false).unwrap();
        assert!(out.contains("2 hunk(s) → 2 commit(s)"), "{out}");
        // History re-sliced, not grown; each commit carries its fix and keeps
        // its message; nothing is left uncommitted.
        let branch = commits_since(&repo, base).unwrap();
        assert_eq!(branch.len(), 2, "{out}");
        let (n1, n2) = (branch[0], branch[1]);
        assert!(at_commit(&repo, n1, "f").contains("w1"));
        assert!(!at_commit(&repo, n1, "f").contains("w2"));
        assert!(at_commit(&repo, n2, "f").contains("w2"));
        assert_eq!(repo.find_commit(n1).unwrap().summary(), Some("edit two"));
        assert_eq!(repo.find_commit(n2).unwrap().summary(), Some("edit four"));
        assert_eq!(read(&repo, "f"), "l1\nw1\nl3\nw2\nl5\n");
        assert!(!is_dirty(&repo).unwrap(), "everything folded: {out}");
    }

    #[test]
    fn absorb_leaves_ambiguous_hunks_in_the_worktree() {
        // Wider fixture so the split hunk and the clear hunk stay separate:
        // c1 owns line 2, c2 owns line 6.
        let dir = tmp("absorb-ambig");
        let repo = Repository::init(&dir).unwrap();
        let all = "l1\nl2\nl3\nl4\nl5\nl6\nl7\n";
        let base = commit(&repo, &[], &[("f", all)], "base");
        let c1 = commit(
            &repo,
            &[base],
            &[("f", "l1\nc1\nl3\nl4\nl5\nl6\nl7\n")],
            "edit two",
        );
        let c2 = commit(
            &repo,
            &[c1],
            &[("f", "l1\nc1\nl3\nl4\nl5\nc2\nl7\n")],
            "edit six",
        );
        on_branch(&repo, "main", c2);
        // One hunk spans lines owned by two commits (line 2: c1, line 3:
        // base) — split ownership; the line-6 hunk is clearly c2's.
        std::fs::write(
            repo.workdir().unwrap().join("f"),
            b"l1\nx\ny\nl4\nl5\nw6\nl7\n",
        )
        .unwrap();

        let out = cmd_absorb(&dir, None, false).unwrap();
        assert!(out.contains("left in the worktree"), "{out}");
        assert!(out.contains("split ownership"), "{out}");
        // The clear hunk folded; the ambiguous one is back, uncommitted.
        assert_eq!(
            read(&repo, "f"),
            "l1\nx\ny\nl4\nl5\nw6\nl7\n",
            "worktree restored"
        );
        assert!(is_dirty(&repo).unwrap());
        let branch = commits_since(&repo, base).unwrap();
        let tip_file = at_commit(&repo, branch[1], "f");
        assert!(
            tip_file.contains("w6") && !tip_file.contains("x\ny"),
            "only the clear hunk folded: {tip_file}"
        );
    }

    #[test]
    fn absorb_rehearse_mutates_nothing() {
        let (dir, repo, _base, _c1, _c2) = absorb_fixture("absorb-dry");
        std::fs::write(repo.workdir().unwrap().join("f"), b"l1\nw1\nl3\nw2\nl5\n").unwrap();
        let head_before = repo.head().unwrap().target().unwrap();

        let out = cmd_absorb(&dir, None, true).unwrap();
        assert!(out.contains("2 hunk(s) → 2 commit(s)"), "{out}");
        assert_eq!(repo.head().unwrap().target().unwrap(), head_before);
        assert_eq!(read(&repo, "f"), "l1\nw1\nl3\nw2\nl5\n");
    }

    #[test]
    fn absorb_with_nothing_attributable_refuses() {
        let (dir, repo, _base, _c1, _c2) = absorb_fixture("absorb-none");
        // The only dirty hunk has split ownership — nothing to fold.
        std::fs::write(repo.workdir().unwrap().join("f"), b"l1\nx\ny\nc2\nl5\n").unwrap();
        let err = cmd_absorb(&dir, None, false).unwrap_err();
        assert!(err.contains("nothing to fold"), "{err}");
        assert!(err.contains("split ownership"), "{err}");
        assert_eq!(read(&repo, "f"), "l1\nx\ny\nc2\nl5\n", "untouched");
    }

    #[test]
    fn exec_over_visits_every_commit_and_restores_head() {
        let (dir, repo, base, _c1, c2) = absorb_fixture("exec-over");
        // Log each visited commit's file content — proves the worktree really
        // holds each commit while the command runs.
        let log = dir.join("visits.log");
        let cmd = format!(
            "head -c 20 f >> {}; echo . >> {}",
            log.display(),
            log.display()
        );
        let range = format!("{base}..HEAD");
        let out = exec_over(&repo, &range, &cmd).unwrap();
        assert!(out.contains("all passed"), "{out}");
        assert!(
            out.contains("edit two") && out.contains("edit four"),
            "{out}"
        );
        let visits = std::fs::read_to_string(&log).unwrap();
        assert_eq!(visits.matches('.').count(), 2, "{visits}");
        // HEAD is back on the branch, at the same tip.
        let head = repo.head().unwrap();
        assert!(head.is_branch(), "restored to the branch");
        assert_eq!(head.target(), Some(c2));

        // A failing command stops at the offending commit ("c2" is not in the
        // file until the second commit), names it, and still restores HEAD.
        let err = exec_over(&repo, &range, "grep -q c2 f").unwrap_err();
        let msg = err.message();
        assert!(msg.contains("failed at"), "{msg}");
        assert!(
            msg.contains(&short(_c1)),
            "names the first failing commit: {msg}"
        );
        assert!(repo.head().unwrap().is_branch());

        // Dirty worktree refuses.
        std::fs::write(repo.workdir().unwrap().join("f"), b"dirty\n").unwrap();
        let err = exec_over(&repo, &range, "true").unwrap_err();
        assert!(err.message().contains("uncommitted"), "{err}");
    }

    #[test]
    fn exec_over_refuses_when_an_untracked_file_collides_with_the_range() {
        let dir = tmp("exec-over-untracked");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("f", "1\n")], "base");
        let c1 = commit(
            &repo,
            &[base],
            &[("f", "1\n"), ("gen", "old\n")],
            "track gen",
        );
        let c2 = commit(&repo, &[c1], &[("f", "2\n")], "drop gen");
        on_branch(&repo, "main", c2);
        // `gen` is untracked at HEAD, but checking out c1 would overwrite it
        // and the restore to HEAD would then delete it.
        std::fs::write(dir.join("gen"), b"precious\n").unwrap();
        let err = exec_over(&repo, &format!("{base}..HEAD"), "true").unwrap_err();
        assert!(err.message().contains("untracked file gen"), "{err}");
        assert_eq!(read(&repo, "gen"), "precious\n");
        // With the collision gone, the same range runs.
        std::fs::remove_file(dir.join("gen")).unwrap();
        let out = exec_over(&repo, &format!("{base}..HEAD"), "true").unwrap();
        assert!(out.contains("all passed"), "{out}");
    }

    #[test]
    fn rebase_autostashes_dirty_files_the_plan_does_not_touch() {
        let dir = tmp("autostash");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("notes", "n\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n"), ("notes", "n\n")], "f1");
        let m1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n"), ("notes", "n\n")],
            "m1",
        );
        on_branch(&repo, "topic", f1);
        // An unrelated uncommitted edit: `notes` is touched by NO step and
        // does not differ between onto and HEAD.
        std::fs::write(dir.join("notes"), "scratch\n").unwrap();

        // m1 adds b; rebase f1 onto m1. `notes` is untouched.
        let out = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(
            read(&repo, "notes"),
            "scratch\n",
            "autostashed edit restored after the rebase"
        );
        assert_eq!(read(&repo, "a"), "2\n", "the rebase itself landed");
        assert!(
            repo.refname_to_id(&autostash_ref("topic")).is_err(),
            "the autostash ref is cleaned up after the restore"
        );
    }

    #[test]
    fn rebase_refuses_dirty_files_the_plan_rewrites() {
        let dir = tmp("autostash-clash");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        let m1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "m1");
        on_branch(&repo, "topic", f1);
        std::fs::write(dir.join("a"), "uncommitted\n").unwrap();
        let err = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap_err();
        assert!(err.contains("uncommitted changes"), "{err}");
        assert!(err.contains('a'), "names the clashing path: {err}");
        assert_eq!(read(&repo, "a"), "uncommitted\n", "nothing destroyed");
    }

    #[test]
    fn autostash_survives_a_conflict_pause_and_an_abort() {
        let dir = tmp("autostash-conflict");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("notes", "n\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n"), ("notes", "n\n")], "f1");
        let m1 = commit(&repo, &[base], &[("a", "3\n"), ("notes", "n\n")], "m1");
        on_branch(&repo, "topic", f1);
        std::fs::write(dir.join("notes"), "scratch\n").unwrap();

        // f1 conflicts with m1 on `a`; `notes` rides the autostash.
        let out = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap();
        assert!(out.contains("conflict"), "{out}");
        assert!(out.contains("autostashed"), "the pause says where: {out}");
        assert_ne!(
            read(&repo, "notes"),
            "scratch\n",
            "parked during the operation"
        );
        // Abort hands the parked change back.
        cmd_abort(&dir).unwrap();
        assert_eq!(read(&repo, "notes"), "scratch\n", "restored on abort");
        assert_eq!(repo.head().unwrap().target(), Some(f1));
        assert!(repo.refname_to_id(&autostash_ref("topic")).is_err());
    }

    #[test]
    fn autostash_keeps_edits_made_during_a_conflict_pause() {
        let dir = tmp("autostash-pause-edit");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("notes", "n\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n"), ("notes", "n\n")], "f1");
        let m1 = commit(&repo, &[base], &[("a", "3\n"), ("notes", "n\n")], "m1");
        on_branch(&repo, "topic", f1);
        std::fs::write(dir.join("notes"), "scratch\n").unwrap();

        // f1 conflicts with m1 on `a`; `notes` rides the autostash.
        let out = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap();
        assert!(out.contains("conflict"), "{out}");
        // During the pause the user writes `notes` again — the later edit
        // must win over the parked bytes.
        std::fs::write(dir.join("notes"), "newer\n").unwrap();
        std::fs::write(dir.join("a"), "3\n").unwrap();
        let out = cmd_continue(&dir, false).unwrap();
        assert!(out.contains("done"), "{out}");
        assert_eq!(read(&repo, "notes"), "newer\n", "pause-time edit kept");
        assert!(
            out.contains("edited during the operation"),
            "the result says the stash was not applied: {out}"
        );
        // The parked bytes stay recoverable on the ref.
        let stash = repo.refname_to_id(&autostash_ref("topic")).unwrap();
        let tree = repo.find_commit(stash).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new("notes")).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), b"scratch\n");
    }

    #[test]
    #[cfg(unix)]
    fn autostash_restores_a_mode_only_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("autostash-mode");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("run.sh", "s\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n"), ("run.sh", "s\n")], "f1");
        let m1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n"), ("run.sh", "s\n")],
            "m1",
        );
        on_branch(&repo, "topic", f1);
        // The only dirty change is chmod +x — same bytes, different mode.
        let abs = dir.join("run.sh");
        let mut perm = std::fs::metadata(&abs).unwrap().permissions();
        perm.set_mode(perm.mode() | 0o111);
        std::fs::set_permissions(&abs, perm).unwrap();
        let out = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap();
        assert!(out.starts_with("done"), "{out}");
        let mode = std::fs::metadata(&abs).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "the exec bit came back");
        assert!(
            repo.refname_to_id(&autostash_ref("topic")).is_err(),
            "a clean restore deletes the ref"
        );
    }

    #[test]
    fn edit_pause_amend_keeps_autostash_paths_out_of_the_commit() {
        let dir = tmp("autostash-edit-amend");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("notes", "n\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n"), ("notes", "n\n")], "f1");
        on_branch(&repo, "topic", f1);
        std::fs::write(dir.join("notes"), "scratch\n").unwrap();
        let out = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![step(f1, Action::Edit, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Paused { .. }));
        // During the edit pause the user edits a plan file AND the parked path.
        std::fs::write(dir.join("a"), "3\n").unwrap();
        std::fs::write(dir.join("notes"), "newer\n").unwrap();
        let out = continue_op(&repo, false).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        // The amended commit carries `a` but NOT the parked path's edit...
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let t = tip.tree().unwrap();
        let a = repo
            .find_blob(t.get_path(Path::new("a")).unwrap().id())
            .unwrap();
        assert_eq!(a.content(), b"3\n");
        let n = repo
            .find_blob(t.get_path(Path::new("notes")).unwrap().id())
            .unwrap();
        assert_eq!(n.content(), b"n\n", "parked path stays out of the amend");
        // ...the worktree keeps the pause-time edit, the stash stays parked.
        assert_eq!(read(&repo, "notes"), "newer\n");
        assert!(repo.refname_to_id(&autostash_ref("topic")).is_ok());
    }

    #[test]
    fn autostash_refuses_staged_changes() {
        let dir = tmp("autostash-staged");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("notes", "n\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n"), ("notes", "n\n")], "f1");
        let m1 = commit(&repo, &[base], &[("a", "3\n"), ("notes", "n\n")], "m1");
        on_branch(&repo, "topic", f1);
        // Stage a change to a path no step touches — the autostash cannot
        // carry index state, so this refuses instead of flattening it.
        std::fs::write(dir.join("notes"), "staged\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("notes")).unwrap();
        index.write().unwrap();
        let err = cmd_rebase(&dir, &m1.to_string(), None, None, None, false, false).unwrap_err();
        assert!(err.contains("staged changes"), "{err}");
        assert!(err.contains("notes"), "{err}");
        // Nothing moved: branch tip and the staged bytes are untouched.
        assert_eq!(repo.head().unwrap().target(), Some(f1));
        assert_eq!(read(&repo, "notes"), "staged\n");
    }

    #[test]
    fn range_diff_pairs_commits_and_reports_drift() {
        let dir = tmp("range-diff");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let c1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        let c2 = commit(&repo, &[c1], &[("a", "2\n"), ("b", "1\n")], "add b");
        let c3 = commit(
            &repo,
            &[c2],
            &[("a", "2\n"), ("b", "1\n"), ("c", "1\n")],
            "add c",
        );
        on_branch(&repo, "main", c3);
        let before = c3;

        // Rewrite: reword c1 (patch identical, message drifts) and drop the
        // tip commit ("add c") by moving the branch to its parent.
        cmd_reword(
            &dir,
            &c1.to_string(),
            Some("change a, better\n"),
            &[],
            false,
        )
        .unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let parent = tip.parent(0).unwrap().id();
        repo.reset(
            &repo.find_object(parent, None).unwrap(),
            ResetType::Hard,
            None,
        )
        .unwrap(); // drop the tip commit ("add c")

        let out = cmd_range_diff(&dir, &before.to_string(), "HEAD").unwrap();
        assert!(out.contains("3 commit(s) before → 2 after"), "{out}");
        assert!(out.contains("(dropped) add c"), "{out}");
        assert!(
            out.contains("! ") && out.contains("change a, better — message changed"),
            "the reworded pair reports message drift: {out}"
        );
        assert!(out.contains("= ") && out.contains("add b"), "{out}");
        assert!(out.contains("final trees DIFFER"), "{out}");

        // A pure message rewrite: trees identical bottom line.
        let before2 = repo.head().unwrap().target().unwrap();
        cmd_reword(&dir, "HEAD", Some("add b, renamed\n"), &[], false).unwrap();
        let out = cmd_range_diff(&dir, &before2.to_string(), "HEAD").unwrap();
        assert!(
            out.contains("final trees identical"),
            "a message-only rewrite re-slices history: {out}"
        );
    }

    #[test]
    fn discard_drops_selected_hunks_with_a_parachute() {
        let (dir, repo, _base, _c1, _c2) = absorb_fixture("discard");
        // Two separate dirty hunks: line 2 and line 4.
        std::fs::write(repo.workdir().unwrap().join("f"), b"l1\nw1\nl3\nw2\nl5\n").unwrap();

        // Rehearse lists, touches nothing.
        let sel = [HunkSel {
            path: "f".into(),
            lo: 2,
            hi: 2,
        }];
        let out = cmd_discard(&dir, &[], &sel, true).unwrap();
        assert!(
            out.contains("would discard") && out.contains("f L2"),
            "{out}"
        );
        assert_eq!(read(&repo, "f"), "l1\nw1\nl3\nw2\nl5\n");

        // Discard just the line-2 hunk: it resets to HEAD content ("c1"),
        // the line-4 hunk stays, and the parachute holds the full pre-state.
        let out = cmd_discard(&dir, &[], &sel, false).unwrap();
        assert!(out.contains("discarded 1"), "{out}");
        assert_eq!(read(&repo, "f"), "l1\nc1\nl3\nw2\nl5\n");
        let backup = repo
            .refname_to_id("refs/mime-backup/main-worktree")
            .unwrap();
        let tree = repo.find_commit(backup).unwrap().tree().unwrap();
        let e = tree.get_path(Path::new("f")).unwrap();
        assert_eq!(
            repo.find_blob(e.id()).unwrap().content(),
            b"l1\nw1\nl3\nw2\nl5\n",
            "the pre-discard bytes are on the backup ref"
        );

        // No selection / an empty selection are loud errors.
        let err = cmd_discard(&dir, &[], &[], false).unwrap_err();
        assert!(err.contains("select what to drop"), "{err}");
        // Whole-file discard drops the remaining hunk.
        cmd_discard(&dir, &["f".to_string()], &[], false).unwrap();
        assert_eq!(read(&repo, "f"), "l1\nc1\nl3\nc2\nl5\n");
        assert!(!is_dirty(&repo).unwrap());
    }

    #[test]
    fn reword_changes_one_message_and_replays_descendants() {
        let dir = tmp("reword-one");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let c1 = commit(&repo, &[base], &[("a", "2\n")], "midle commit");
        let c2 = commit(&repo, &[c1], &[("a", "3\n")], "tip");
        on_branch(&repo, "main", c2);

        // Rehearse shows the new message and moves nothing.
        let out = cmd_reword(
            &dir,
            &c1.to_string(),
            None,
            &[MsgEditSpec {
                find: Some("midle".into()),
                replace: Some("middle".into()),
                append: None,
            }],
            true,
        )
        .unwrap();
        assert!(out.contains("middle commit"), "{out}");
        assert_eq!(repo.head().unwrap().target(), Some(c2));

        let out = cmd_reword(
            &dir,
            &c1.to_string(),
            None,
            &[MsgEditSpec {
                find: Some("midle".into()),
                replace: Some("middle".into()),
                append: None,
            }],
            false,
        )
        .unwrap();
        assert!(out.contains("byte-identical"), "{out}");
        let branch = commits_since(&repo, base).unwrap();
        let (m1, m2) = (
            repo.find_commit(branch[0]).unwrap(),
            repo.find_commit(branch[1]).unwrap(),
        );
        assert_eq!(m1.summary(), Some("middle commit"));
        assert_eq!(m2.summary(), Some("tip"), "descendant message untouched");
        assert_eq!(m1.tree_id(), repo.find_commit(c1).unwrap().tree_id());
        assert_eq!(m2.tree_id(), repo.find_commit(c2).unwrap().tree_id());
        assert_eq!(
            repo.refname_to_id(&backup_slot("main", 0)).unwrap(),
            c2,
            "pre-op tip on the backup ring"
        );

        // A wholesale message on the tip.
        let tip = repo.head().unwrap().target().unwrap();
        cmd_reword(
            &dir,
            &tip.to_string(),
            Some("new tip message\n"),
            &[],
            false,
        )
        .unwrap();
        let now = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(now.summary(), Some("new tip message"));

        // Unchanged message / off-branch commit are loud errors.
        let err = cmd_reword(&dir, "HEAD", Some("new tip message\n"), &[], false).unwrap_err();
        assert!(err.contains("unchanged"), "{err}");
        let side = commit(&repo, &[base], &[("z", "9\n")], "side");
        let err = cmd_reword(&dir, &side.to_string(), Some("x\n"), &[], false).unwrap_err();
        assert!(err.contains("not on the current branch"), "{err}");
    }

    #[test]
    fn msg_rewrite_edits_every_message_and_keeps_trees() {
        let dir = tmp("msgrange");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let c1 = commit(
            &repo,
            &[base],
            &[("a", "2\n")],
            "one old_name\n\nCo-Authored-By: Bot <bot@invalid>\n",
        );
        let c2 = commit(
            &repo,
            &[c1],
            &[("a", "3\n")],
            "two\n\nold_name twice: old_name.\n",
        );
        on_branch(&repo, "main", c2);
        let old_trees: Vec<Oid> = [c1, c2]
            .iter()
            .map(|o| repo.find_commit(*o).unwrap().tree_id())
            .collect();

        // Rehearse first: counts visible, nothing moved.
        let specs = vec![
            MsgEditSpec {
                find: Some("old_name".into()),
                replace: Some("new_name".into()),
                append: None,
            },
            MsgEditSpec {
                find: Some("Co-Authored-By: Bot <bot@invalid>\n".into()),
                replace: None,
                append: None,
            },
        ];
        let out = cmd_msg_rewrite(&dir, "HEAD~2..HEAD", &specs, true).unwrap();
        assert!(out.contains("rehearse"), "{out}");
        assert_eq!(repo.head().unwrap().target(), Some(c2), "nothing moved");

        let out = cmd_msg_rewrite(&dir, "HEAD~2..HEAD", &specs, false).unwrap();
        assert!(out.contains("byte-identical"), "{out}");
        let branch = commits_since(&repo, base).unwrap();
        assert_eq!(branch.len(), 2);
        let m1 = repo.find_commit(branch[0]).unwrap();
        let m2 = repo.find_commit(branch[1]).unwrap();
        assert_eq!(m1.message().unwrap(), "one new_name\n\n");
        assert_eq!(m2.message().unwrap(), "two\n\nnew_name twice: new_name.\n");
        // Trees byte-identical; the pre-op tip is on the backup ring.
        assert_eq!(m1.tree_id(), old_trees[0]);
        assert_eq!(m2.tree_id(), old_trees[1]);
        assert_eq!(
            repo.refname_to_id(&backup_slot("main", 0)).unwrap(),
            c2,
            "pre-op tip stamped"
        );

        // A find that matches NOWHERE in the range errors, nothing changes.
        let miss = vec![MsgEditSpec {
            find: Some("absent-token".into()),
            replace: Some("x".into()),
            append: None,
        }];
        let err = cmd_msg_rewrite(&dir, "HEAD~2..HEAD", &miss, false).unwrap_err();
        assert!(err.contains("matches no commit message"), "{err}");
        assert_eq!(repo.head().unwrap().target(), Some(branch[1]));

        // The range must end at HEAD.
        let err = cmd_msg_rewrite(&dir, "HEAD~2..HEAD~1", &specs, false).unwrap_err();
        assert!(err.contains("end at HEAD"), "{err}");
    }

    #[test]
    fn exec_over_is_gated_behind_an_env_opt_in() {
        // No test sets MIME_EXEC, so the gate must hold here.
        let (dir, _repo, _base, _c1, _c2) = absorb_fixture("exec-gate");
        let err = cmd_exec_over(&dir, "HEAD~1..HEAD", "true").unwrap_err();
        assert!(err.contains("MIME_EXEC"), "{err}");
    }

    #[test]
    fn grouped_worktree_blame_buckets_hunks_by_commit() {
        let (dir, repo, _base, c1, c2) = absorb_fixture("blame-group");
        std::fs::write(repo.workdir().unwrap().join("f"), b"l1\nw1\nl3\nw2\nl5\n").unwrap();

        // Whole-tree sweep (no path), grouped under the owning commits.
        let out = cmd_blame(&dir, None, None, None, true, true).unwrap();
        assert!(out.contains(&format!("{} edit two", short(c1))), "{out}");
        assert!(out.contains(&format!("{} edit four", short(c2))), "{out}");
        assert!(out.contains("f L2"), "{out}");
        assert!(out.contains("f L4"), "{out}");
        // group_by needs worktree mode; committed blame needs a path.
        let err = cmd_blame(&dir, Some("f"), None, None, false, true).unwrap_err();
        assert!(err.contains("worktree"), "{err}");
        let err = cmd_blame(&dir, None, None, None, false, false).unwrap_err();
        assert!(err.contains("path"), "{err}");
    }

    #[test]
    fn move_relocates_a_file_forward_and_replays_the_tail() {
        let dir = tmp("move-fwd");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let f = commit(
            &repo,
            &[base],
            &[("x", "0\n"), ("a", "1\n"), ("b", "1\n")],
            "F: a and b",
        );
        let t = commit(
            &repo,
            &[f],
            &[("x", "0\n"), ("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "T: c",
        );
        let tail = commit(
            &repo,
            &[t],
            &[
                ("x", "0\n"),
                ("a", "1\n"),
                ("b", "1\n"),
                ("c", "1\n"),
                ("d", "1\n"),
            ],
            "tail: d",
        );
        on_branch(&repo, "main", tail);

        // Move b's change from F forward into its child T.
        let out = cmd_move(
            &dir,
            &f.to_string(),
            &t.to_string(),
            &["b".to_string()],
            &[],
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");

        // Final tree is unchanged: every file still present.
        for p in ["a", "b", "c", "d"] {
            assert_eq!(read(&repo, p), "1\n", "{p} survives the move");
        }
        // T' now introduces b; F' no longer does; the tail (d) was replayed.
        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // tail'
        let t_prime = tip.parent(0).unwrap();
        let f_prime = t_prime.parent(0).unwrap();
        assert_eq!(f_prime.message().unwrap(), "F: a and b");
        assert_eq!(t_prime.message().unwrap(), "T: c");
        assert_eq!(tip.message().unwrap(), "tail: d");
        assert!(f_prime.tree().unwrap().get_path(Path::new("a")).is_ok());
        assert!(
            f_prime.tree().unwrap().get_path(Path::new("b")).is_err(),
            "b moved out of F'"
        );
        assert!(
            t_prime.tree().unwrap().get_path(Path::new("b")).is_ok(),
            "b moved into T'"
        );
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn move_relocates_a_file_backward() {
        let dir = tmp("move-back");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let f = commit(&repo, &[base], &[("x", "0\n"), ("a", "1\n")], "F: a");
        let t = commit(
            &repo,
            &[f],
            &[("x", "0\n"), ("a", "1\n"), ("b", "1\n"), ("d", "1\n")],
            "T: b and d",
        );
        on_branch(&repo, "main", t);

        // Move b's change from T back into its parent F.
        let out = cmd_move(
            &dir,
            &t.to_string(),
            &f.to_string(),
            &["b".to_string()],
            &[],
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");

        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // T'
        let f_prime = tip.parent(0).unwrap();
        assert!(
            f_prime.tree().unwrap().get_path(Path::new("b")).is_ok(),
            "b moved into F'"
        );
        // T' still introduces d, no longer b.
        assert!(tip.tree().unwrap().get_path(Path::new("d")).is_ok());
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn move_relocates_a_single_hunk() {
        let dir = tmp("move-hunk");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(
            &repo,
            &[],
            &[
                ("f", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n"),
                ("g", "0\n"),
            ],
            "base",
        );
        // F edits f's top and bottom (two hunks); T edits g.
        let f = commit(
            &repo,
            &[base],
            &[
                ("f", "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nY\n"),
                ("g", "0\n"),
            ],
            "F: edit f",
        );
        let t = commit(
            &repo,
            &[f],
            &[
                ("f", "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nY\n"),
                ("g", "G\n"),
            ],
            "T: edit g",
        );
        on_branch(&repo, "main", t);

        // Move only f's TOP hunk (new line 1) from F forward into T.
        let sel = vec![HunkSel {
            path: "f".into(),
            lo: 1,
            hi: 1,
        }];
        let out = cmd_move(&dir, &f.to_string(), &t.to_string(), &[], &sel).unwrap();
        assert!(out.starts_with("done"), "{out}");

        // Final content unchanged.
        assert_eq!(read(&repo, "f"), "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nY\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // T'
        let f_prime = tip.parent(0).unwrap();
        // F' has only the bottom hunk (line 1 still "1"); T' adds the top hunk.
        let e = f_prime.tree().unwrap().get_path(Path::new("f")).unwrap();
        assert_eq!(
            repo.find_blob(e.id()).unwrap().content(),
            b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nY\n"
        );
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn move_rejects_a_change_present_in_both_commits() {
        let dir = tmp("move-both");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "0\n")], "base");
        let f = commit(&repo, &[base], &[("a", "1\n")], "F: a=1");
        let t = commit(&repo, &[f], &[("a", "2\n")], "T: a=2");
        on_branch(&repo, "main", t);

        // `a` is changed by BOTH F and T — the move is ambiguous.
        let err = cmd_move(
            &dir,
            &f.to_string(),
            &t.to_string(),
            &["a".to_string()],
            &[],
        )
        .unwrap_err();
        assert!(err.contains("changed by both commits"), "{err}");
        // Non-adjacent commits are rejected too.
        let err2 = cmd_move(
            &dir,
            &base.to_string(),
            &t.to_string(),
            &["a".to_string()],
            &[],
        )
        .unwrap_err();
        assert!(err2.contains("must be adjacent"), "{err2}");
    }

    /// A branch base←c1(add a)←c2(add b)←c3(fix a), on `main`.
    fn fixup_fixture(dir: &std::path::Path) -> (Repository, Oid, Oid, Oid, Oid) {
        let repo = Repository::init(dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let c1 = commit(&repo, &[base], &[("x", "0\n"), ("a", "1\n")], "add a");
        let c2 = commit(
            &repo,
            &[c1],
            &[("x", "0\n"), ("a", "1\n"), ("b", "1\n")],
            "add b",
        );
        let c3 = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            "fix a",
        );
        on_branch(&repo, "main", c3);
        (repo, base, c1, c2, c3)
    }

    #[test]
    fn autosquash_markers_respect_the_from_bound() {
        // Three-arg transplant: from..HEAD replays onto `onto`; the stale
        // commit below `from` must not come along, and the fold still fires.
        let dir = tmp("autosq-from");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a.txt", "a\n")], "base");
        let stale = commit(
            &repo,
            &[base],
            &[("a.txt", "a\n"), ("s.txt", "s\n")],
            "stale",
        );
        let c1 = commit(
            &repo,
            &[stale],
            &[("a.txt", "a\n"), ("s.txt", "s\n"), ("b.txt", "one\n")],
            "add b",
        );
        let c2 = commit(
            &repo,
            &[c1],
            &[("a.txt", "a\n"), ("s.txt", "s\n"), ("b.txt", "one\ntwo\n")],
            "fixup! add b",
        );
        on_branch(&repo, "topic", c2);

        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            Some(&stale.to_string()),
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary().unwrap(), "add b");
        assert_eq!(head.parent(0).unwrap().summary().unwrap(), "base");
        assert_eq!(read(&repo, "b.txt"), "one\ntwo\n");
        assert!(
            !repo.workdir().unwrap().join("s.txt").exists(),
            "the below-from commit must not be replayed"
        );
    }

    #[test]
    fn autosquash_sparse_plan_folds_without_listing_every_commit() {
        let dir = tmp("autosquash");
        let (repo, base, c1, _c2, c3) = fixup_fixture(&dir);

        // Only the RELATIONSHIP: fold c3 into c1; c2 is auto-picked untouched.
        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Directives(vec![(
                c3.to_string(),
                c1.to_string(),
                "fixup".to_string(),
            )])),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");

        // a carries c3's fix, b survives; c3's message is dropped (fixup).
        assert_eq!(read(&repo, "a"), "2\n");
        assert_eq!(read(&repo, "b"), "1\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // c2'
        let c1_prime = tip.parent(0).unwrap();
        assert_eq!(tip.message().unwrap(), "add b");
        assert_eq!(c1_prime.message().unwrap(), "add a");
        assert_eq!(c1_prime.parent(0).unwrap().id(), base);
        let e = c1_prime.tree().unwrap().get_path(Path::new("a")).unwrap();
        assert_eq!(repo.find_blob(e.id()).unwrap().content(), b"2\n");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn autosquash_rejects_chained_directives() {
        let dir = tmp("autosquash-chain");
        let (repo, base, c1, c2, c3) = fixup_fixture(&dir);

        // Chaining (fold c3 into c2, and c2 into c1) would strand a fold beside
        // the wrong commit — reject rather than silently mis-fold.
        let err = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Directives(vec![
                (c3.to_string(), c2.to_string(), "fixup".to_string()),
                (c2.to_string(), c1.to_string(), "fixup".to_string()),
            ])),
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("itself being folded"), "{err}");
        assert!(!state_path(&repo).exists());
    }

    /// A branch base←c1(add a)←c2(add b) ready for marker commits on top.
    fn marker_fixture(dir: &std::path::Path) -> (Repository, Oid, Oid, Oid) {
        let repo = Repository::init(dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let c1 = commit(&repo, &[base], &[("x", "0\n"), ("a", "1\n")], "add a");
        let c2 = commit(
            &repo,
            &[c1],
            &[("x", "0\n"), ("a", "1\n"), ("b", "1\n")],
            "add b",
        );
        (repo, base, c1, c2)
    }

    #[test]
    fn autosquash_markers_derive_the_plan_from_subjects() {
        let dir = tmp("asq-markers");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        let f = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            "fixup! add a",
        );
        on_branch(&repo, "main", f);

        // No directives: the plan derives from the fixup! subject.
        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(read(&repo, "a"), "2\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // c2'
        assert_eq!(tip.message().unwrap(), "add b");
        let c1p = tip.parent(0).unwrap();
        // The marker message is gone (fixup keeps the target's message)...
        assert_eq!(c1p.message().unwrap(), "add a");
        // ...and its change landed inside the target commit.
        let e = c1p.tree().unwrap().get_path(Path::new("a")).unwrap();
        assert_eq!(repo.find_blob(e.id()).unwrap().content(), b"2\n");
        assert_eq!(c1p.parent(0).unwrap().id(), base);
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn autosquash_markers_stack_in_commit_order() {
        let dir = tmp("asq-marker-order");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        // Two fixups of one target: f2's diff (a: 2→3) only applies AFTER
        // f1's (a: 1→2) — a reversed stacking would conflict, not just
        // mis-order messages.
        let f1 = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            "fixup! add a",
        );
        let f2 = commit(
            &repo,
            &[f1],
            &[("x", "0\n"), ("a", "3\n"), ("b", "1\n")],
            "fixup! add a",
        );
        on_branch(&repo, "main", f2);

        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(read(&repo, "a"), "3\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "add b");
        assert_eq!(tip.parent(0).unwrap().message().unwrap(), "add a");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn autosquash_markers_flatten_a_chained_fixup() {
        let dir = tmp("asq-marker-chain");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        let f1 = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            "fixup! add a",
        );
        // A fixup OF the fixup (git commit --fixup=<f1>): both prefixes in the
        // subject. It flattens to c1, sitting after f1 — no chain rejection.
        let f2 = commit(
            &repo,
            &[f1],
            &[("x", "0\n"), ("a", "3\n"), ("b", "1\n")],
            "fixup! fixup! add a",
        );
        on_branch(&repo, "main", f2);

        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(read(&repo, "a"), "3\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "add b");
        assert_eq!(tip.parent(0).unwrap().message().unwrap(), "add a");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn autosquash_markers_chain_beats_a_duplicate_subject() {
        let dir = tmp("asq-marker-dup");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        let f1 = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            "fixup! add a",
        );
        // A LATER plain commit reusing the target's subject, sitting between
        // the chain and its root.
        let dup = commit(
            &repo,
            &[f1],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n"), ("c", "1\n")],
            "add a",
        );
        // The chained marker's remainder ("fixup! add a") names f1's FULL
        // subject — it must fold through f1 into c1, not into the nearer
        // duplicate. (If it folded into `dup`, c1' would keep a=2.)
        let f2 = commit(
            &repo,
            &[dup],
            &[("x", "0\n"), ("a", "3\n"), ("b", "1\n"), ("c", "1\n")],
            "fixup! fixup! add a",
        );
        on_branch(&repo, "main", f2);

        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(read(&repo, "a"), "3\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // dup'
        assert_eq!(tip.message().unwrap(), "add a");
        let c2p = tip.parent(0).unwrap();
        assert_eq!(c2p.message().unwrap(), "add b");
        // Both fixes landed in c1, upstream of the duplicate.
        let c1p = c2p.parent(0).unwrap();
        assert_eq!(c1p.message().unwrap(), "add a");
        let e = c1p.tree().unwrap().get_path(Path::new("a")).unwrap();
        assert_eq!(repo.find_blob(e.id()).unwrap().content(), b"3\n");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn autosquash_marker_prefix_tier_skips_marker_subjects() {
        let dir = tmp("asq-marker-prefix");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let ca = commit(&repo, &[base], &[("x", "0\n"), ("t", "1\n")], "fix typo");
        let cb = commit(
            &repo,
            &[ca],
            &[("x", "0\n"), ("t", "1\n"), ("b", "1\n")],
            "add b",
        );
        let fb = commit(
            &repo,
            &[cb],
            &[("x", "0\n"), ("t", "1\n"), ("b", "2\n")],
            "fixup! add b",
        );
        // Truncated remainder "fix": must prefix-match the PLAIN "fix typo",
        // not the nearer marker subject "fixup! add b".
        let ft = commit(
            &repo,
            &[fb],
            &[("x", "0\n"), ("t", "2\n"), ("b", "2\n")],
            "fixup! fix",
        );
        on_branch(&repo, "main", ft);

        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // cb'
        assert_eq!(tip.message().unwrap(), "add b");
        let cap = tip.parent(0).unwrap(); // ca'
        assert_eq!(cap.message().unwrap(), "fix typo");
        // The typo fix landed in "fix typo", not inside "add b"'s fold.
        let e = cap.tree().unwrap().get_path(Path::new("t")).unwrap();
        assert_eq!(repo.find_blob(e.id()).unwrap().content(), b"2\n");
        assert!(tip.tree().unwrap().get_path(Path::new("b")).is_ok());
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn planless_replay_notes_unfolded_marker_commits() {
        let dir = tmp("asq-note");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        let f = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            "fixup! add a",
        );
        on_branch(&repo, "main", f);

        // A bare replay leaves the marker unfolded — the result must say so
        // and name the argument that folds it (rehearse and real run alike).
        let pre = cmd_rebase(&dir, &base.to_string(), None, None, None, true, false).unwrap();
        assert!(
            pre.contains("1 fixup!/squash! commit(s) replayed as-is"),
            "{pre}"
        );
        assert!(pre.contains("autosquash:true"), "{pre}");
        let out = cmd_rebase(&dir, &base.to_string(), None, None, None, false, false).unwrap();
        assert!(out.contains("autosquash:true"), "{out}");
        // The autosquash run itself must NOT carry the note.
        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert!(!out.contains("replayed as-is"), "{out}");
    }

    #[test]
    fn autosquash_rejects_an_empty_directive_list() {
        let dir = tmp("asq-empty");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        on_branch(&repo, "main", c2);

        // An empty list must not degrade to a bare pick-all.
        let err = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Directives(vec![])),
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("nothing to fold"), "{err}");
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), c2);
    }

    #[test]
    fn autosquash_markers_squash_melds_and_sha_form_resolves() {
        let dir = tmp("asq-marker-sha");
        let (repo, base, c1, c2) = marker_fixture(&dir);
        // squash! with a revspec remainder instead of a subject copy.
        let s = commit(
            &repo,
            &[c2],
            &[("x", "0\n"), ("a", "2\n"), ("b", "1\n")],
            &format!("squash! {}", &c1.to_string()[..10]),
        );
        on_branch(&repo, "main", s);

        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap();
        assert!(out.starts_with("done"), "{out}");
        assert_eq!(read(&repo, "a"), "2\n");
        let c1p = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent(0)
            .unwrap();
        // squash melds the messages (marker subject kept verbatim — reword
        // after if it matters).
        let msg = c1p.message().unwrap().to_string();
        assert!(msg.starts_with("add a"), "{msg}");
        assert!(msg.contains("squash!"), "{msg}");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn autosquash_markers_reject_orphans_amend_and_empty() {
        let dir = tmp("asq-marker-err");
        let (repo, base, _c1, c2) = marker_fixture(&dir);
        on_branch(&repo, "main", c2);
        // Move the already-checked-out branch to `tip` (on_branch can't force-
        // update the current HEAD's branch).
        let advance = |tip: Oid| {
            repo.reference("refs/heads/main", tip, true, "test")
                .unwrap();
            repo.reset(&repo.find_object(tip, None).unwrap(), ResetType::Hard, None)
                .unwrap();
        };

        // No markers at all: an error, not a silent full replay.
        let err = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("no fixup!/squash! commits"), "{err}");

        // A marker with no matching target names the orphan instead of leaving
        // it in place silently (git's behavior would read as "folded" here).
        let o = commit(
            &repo,
            &[c2],
            &[("x", "1\n"), ("a", "1\n"), ("b", "1\n")],
            "fixup! no such subject",
        );
        advance(o);
        let err = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("no target found"), "{err}");
        assert!(err.contains("no such subject"), "{err}");

        // amend! (message-replacing fold) is rejected, not half-applied.
        let a = commit(
            &repo,
            &[c2],
            &[("x", "2\n"), ("a", "1\n"), ("b", "1\n")],
            "amend! add a",
        );
        advance(a);
        let err = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            None,
            Some(Autosquash::Markers),
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("amend!"), "{err}");
        // Nothing was applied by any of the failures.
        assert!(!state_path(&repo).exists());
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), a);
    }

    #[test]
    fn fixup_folds_a_commit_into_its_target() {
        let dir = tmp("fixup-fold");
        let (repo, base, c1, _c2, c3) = fixup_fixture(&dir);

        // One call: fold c3 into c1, inferring the base and picking the rest.
        let out = cmd_fixup(&dir, &c1.to_string(), &c3.to_string(), false).unwrap();
        assert!(out.starts_with("done"), "{out}");
        // A fold re-slices history without changing what the branch builds —
        // the result says so instead of leaving a rev-parse comparison to do.
        assert!(
            out.contains("tree identical to the pre-op tip"),
            "tree-identity note rides in the result: {out}"
        );
        assert_eq!(read(&repo, "a"), "2\n");
        assert_eq!(read(&repo, "b"), "1\n");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let c1_prime = tip.parent(0).unwrap();
        assert_eq!(c1_prime.message().unwrap(), "add a");
        let e = c1_prime.tree().unwrap().get_path(Path::new("a")).unwrap();
        assert_eq!(repo.find_blob(e.id()).unwrap().content(), b"2\n");

        // A source/target not on one line of history errors cleanly.
        let side = commit(&repo, &[base], &[("z", "1\n")], "side branch");
        let err = cmd_fixup(&dir, &side.to_string(), &c3.to_string(), true).unwrap_err();
        assert!(err.contains("same line of history"), "{err}");
    }

    /// The blob `path` holds in `commit`'s tree.
    fn at_commit(repo: &Repository, commit: Oid, path: &str) -> String {
        let tree = repo.find_commit(commit).unwrap().tree().unwrap();
        let e = tree.get_path(std::path::Path::new(path)).unwrap();
        String::from_utf8(repo.find_blob(e.id()).unwrap().content().to_vec()).unwrap()
    }

    #[test]
    fn fixup_worktree_folds_a_path_and_hands_back_the_rest() {
        let dir = tmp("fixup-wt");
        let (repo, _base, _c1, _c2, c3) = fixup_fixture(&dir);
        // Uncommitted: a fix to `a` (belongs in c3, "fix a") and an unrelated
        // edit to `b` that must survive as uncommitted work.
        std::fs::write(dir.join("a"), "3\n").unwrap();
        std::fs::write(dir.join("b"), "keep me\n").unwrap();

        let out =
            cmd_fixup_worktree(&dir, &c3.to_string(), &["a".to_string()], &[], false).unwrap();
        assert!(out.contains("back in the worktree"), "{out}");

        // History: the tip amends c3 (same message), now carrying the fold;
        // `b` in history is untouched.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary().unwrap(), "fix a");
        assert_eq!(at_commit(&repo, head.id(), "a"), "3\n");
        assert_eq!(at_commit(&repo, head.id(), "b"), "1\n");
        // Worktree: byte-identical to before — `b` still holds the edit.
        assert_eq!(read(&repo, "a"), "3\n");
        assert_eq!(read(&repo, "b"), "keep me\n");
        // The worktree parachute ref exists and holds the full pre-op state.
        let wt = repo
            .refname_to_id("refs/mime-backup/main-worktree")
            .unwrap();
        assert_eq!(at_commit(&repo, wt, "b"), "keep me\n");
    }

    #[test]
    fn fixup_worktree_folds_one_hunk_of_a_file_keeping_the_other() {
        let dir = tmp("fixup-wt-hunk");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let body = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let c1 = commit(&repo, &[base], &[("x", "0\n"), ("f", body)], "add f");
        on_branch(&repo, "main", c1);
        // Two edits far enough apart to be separate hunks; fold only line 1.
        std::fs::write(dir.join("f"), "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nL9\nl10\n").unwrap();

        cmd_fixup_worktree(
            &dir,
            &c1.to_string(),
            &[],
            &[HunkSel {
                path: "f".to_string(),
                lo: 1,
                hi: 1,
            }],
            false,
        )
        .unwrap();

        let head = repo.head().unwrap().peel_to_commit().unwrap();
        // History took the L1 hunk only; the L9 edit stays uncommitted.
        assert_eq!(
            at_commit(&repo, head.id(), "f"),
            "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n"
        );
        assert_eq!(
            read(&repo, "f"),
            "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nL9\nl10\n"
        );
    }

    #[test]
    fn fixup_worktree_empty_selection_folds_everything() {
        let dir = tmp("fixup-wt-all");
        let (repo, _base, _c1, _c2, c3) = fixup_fixture(&dir);
        std::fs::write(dir.join("a"), "3\n").unwrap();
        std::fs::write(dir.join("b"), "2\n").unwrap();

        cmd_fixup_worktree(&dir, &c3.to_string(), &[], &[], false).unwrap();

        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(at_commit(&repo, head.id(), "a"), "3\n");
        assert_eq!(at_commit(&repo, head.id(), "b"), "2\n");
        // Everything folded → nothing uncommitted remains.
        assert!(
            !is_dirty(&repo).unwrap(),
            "worktree clean after a full fold"
        );
    }

    #[test]
    fn fixup_worktree_conflict_aborts_whole_and_restores() {
        let dir = tmp("fixup-wt-conflict");
        let (repo, _base, c1, _c2, c3) = fixup_fixture(&dir);
        // The worktree edit to `a` is based on c3's content ("2\n"); folding it
        // into c1 (where `a` is "1\n") cannot apply — the fold conflicts.
        std::fs::write(dir.join("a"), "conflict me\n").unwrap();
        std::fs::write(dir.join("b"), "keep me\n").unwrap();

        let err =
            cmd_fixup_worktree(&dir, &c1.to_string(), &["a".to_string()], &[], false).unwrap_err();
        assert!(
            err.contains("nothing changed") && err.contains("target"),
            "the error explains the abort and the fix: {err}"
        );
        // Branch untouched; the whole uncommitted state is back, byte-exact.
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), c3);
        assert_eq!(read(&repo, "a"), "conflict me\n");
        assert_eq!(read(&repo, "b"), "keep me\n");
        // No half-done operation is left behind.
        assert!(status(&repo).unwrap().is_none(), "no op in progress");
    }

    #[test]
    fn fixup_worktree_rehearse_mutates_nothing() {
        let dir = tmp("fixup-wt-rehearse");
        let (repo, _base, _c1, _c2, c3) = fixup_fixture(&dir);
        std::fs::write(dir.join("a"), "3\n").unwrap();

        let out = cmd_fixup_worktree(&dir, &c3.to_string(), &["a".to_string()], &[], true).unwrap();
        assert!(
            out.contains("fix a"),
            "preview lists the fold target: {out}"
        );

        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), c3);
        assert_eq!(read(&repo, "a"), "3\n", "worktree untouched");
        assert!(
            repo.refname_to_id("refs/mime-backup/main-worktree")
                .is_err(),
            "no backup ref from a rehearsal"
        );
    }

    #[test]
    fn state_round_trips_message_edits_and_split_parts() {
        // Guards the persisted-vs-input key unification: save_state/load_state and
        // the MCP input must agree, or a resume after a conflict loses edits.
        let dir = tmp("state-rt");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        on_branch(&repo, "main", base);

        let st = State {
            branch: "refs/heads/main".to_string(),
            orig: base,
            onto: base,
            current: base,
            autostash: vec!["parked.txt".to_string()],
            next: 1,
            steps: vec![
                Step {
                    commit: base,
                    action: Action::Reword,
                    message: Some("hi".to_string()),
                    message_edits: vec![
                        MsgEdit::Replace {
                            find: "a".into(),
                            with: "b".into(),
                        },
                        MsgEdit::Append {
                            text: "Sign".into(),
                        },
                    ],
                    split_into: Vec::new(),
                },
                Step {
                    commit: base,
                    action: Action::Split,
                    message: None,
                    message_edits: Vec::new(),
                    split_into: vec![
                        SplitPart {
                            message: "p1".into(),
                            paths: vec!["a".into()],
                            hunks: Vec::new(),
                            rest: false,
                        },
                        SplitPart {
                            message: "p2".into(),
                            paths: Vec::new(),
                            hunks: vec![HunkSel {
                                path: "a".into(),
                                lo: 1,
                                hi: 2,
                            }],
                            rest: false,
                        },
                    ],
                },
            ],
            mode: Mode::Pick,
            editing: false,
        };
        save_state(&repo, &st).unwrap();
        let back = load_state(&repo).unwrap();

        match &back.steps[0].message_edits[0] {
            MsgEdit::Replace { find, with } => {
                assert_eq!(find, "a");
                assert_eq!(with, "b"); // the `replace` key survived the round trip
            }
            other => panic!("expected Replace, got {other:?}"),
        }
        assert!(
            matches!(&back.steps[0].message_edits[1], MsgEdit::Append { text } if text == "Sign")
        );
        let parts = &back.steps[1].split_into;
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].paths, vec!["a".to_string()]);
        assert_eq!(parts[1].hunks[0].path, "a");
        assert_eq!((parts[1].hunks[0].lo, parts[1].hunks[0].hi), (1, 2));
    }

    #[test]
    fn squash_as_first_applied_step_is_rejected() {
        let dir = tmp("squash-first");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        on_branch(&repo, "topic", f1);

        let err = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![step(f1, Action::Squash, None)],
            },
        )
        .unwrap_err();
        assert!(err.message().contains("first applied step"));
        assert!(!state_path(&repo).exists(), "rejected before any mutation");
    }

    /// A conflicting plan, stopped at the conflict, for the continue/guard tests.
    fn conflict_repo(dir: &std::path::Path) -> (Repository, Oid) {
        let repo = Repository::init(dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("b", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "10\n"), ("b", "1\n")], "ours a");
        let m1 = commit(&repo, &[base], &[("a", "20\n"), ("b", "1\n")], "their a");
        on_branch(&repo, "topic", f1);
        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Conflict { .. }));
        (repo, f1)
    }

    #[test]
    fn continue_refuses_while_markers_remain() {
        let dir = tmp("markers");
        let (repo, _) = conflict_repo(&dir);
        // The worktree still carries the diff3 markers; continue must refuse to
        // bake them into history rather than silently committing them.
        let err = continue_op(&repo, false).unwrap_err();
        assert!(
            err.message().contains("unresolved conflict markers"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn continue_refuses_a_stray_opener_but_force_overrides() {
        // A partial resolution that deletes the lower markers but leaves the
        // `<<<<<<<` opener must NOT slip through (a structural parser would miss
        // it); `force` is the escape hatch when the marker line is intentional.
        let dir = tmp("stray-opener");
        let (repo, _) = conflict_repo(&dir);
        std::fs::write(dir.join("a"), "resolved\n<<<<<<< leftover opener\n").unwrap();
        assert!(
            continue_op(&repo, false).is_err(),
            "stray opener must block"
        );
        // The failed continue committed/advanced nothing, so a forced retry works.
        let out = continue_op(&repo, true).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "a"), "resolved\n<<<<<<< leftover opener\n");
    }

    #[test]
    fn abort_recovers_when_the_branch_was_deleted_mid_op() {
        let dir = tmp("abort-deleted");
        let (repo, f1) = conflict_repo(&dir);
        // Another process deletes the branch while paused at the conflict.
        repo.find_reference("refs/heads/topic")
            .unwrap()
            .delete()
            .unwrap();
        // abort must not wedge: it recreates the branch from the backup and
        // clears the in-progress state.
        abort(&repo).unwrap();
        assert_eq!(repo.refname_to_id("refs/heads/topic").unwrap(), f1);
        assert!(!state_path(&repo).exists());
        assert!(!repo.head_detached().unwrap());
    }

    #[test]
    fn continue_commits_only_the_conflicted_paths() {
        let dir = tmp("outofset");
        let (repo, _) = conflict_repo(&dir);
        // Resolve the conflicted file AND scribble on an unrelated tracked file.
        std::fs::write(dir.join("a"), "resolved\n").unwrap();
        std::fs::write(dir.join("b"), "also changed\n").unwrap();
        assert!(matches!(
            continue_op(&repo, false).unwrap(),
            Outcome::Done { .. }
        ));
        // Only the conflicted path is committed; the unrelated edit is NOT folded
        // into the cherry-picked commit (it stays uncommitted in the worktree).
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let entry = tip.tree().unwrap().get_path(Path::new("b")).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), b"1\n", "out-of-set edit not committed");
    }

    #[test]
    fn continue_resolves_a_modify_delete_conflict_by_deletion() {
        let dir = tmp("moddel");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n"), ("f", "x\n")], "base");
        let f1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("f", "edited\n")],
            "modify f",
        );
        let m1 = commit(&repo, &[base], &[("a", "1\n")], "delete f");
        on_branch(&repo, "topic", f1);
        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Conflict { .. }));
        // Resolve by keeping the deletion — add_path on a missing file would error.
        let _ = std::fs::remove_file(dir.join("f"));
        assert!(matches!(
            continue_op(&repo, false).unwrap(),
            Outcome::Done { .. }
        ));
        assert!(!dir.join("f").exists(), "deletion landed");
    }

    #[test]
    fn start_refuses_on_a_dirty_worktree() {
        let dir = tmp("dirty");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        on_branch(&repo, "topic", f1);
        std::fs::write(dir.join("a"), "uncommitted\n").unwrap();
        let err = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap_err();
        assert!(
            err.message().contains("uncommitted changes"),
            "{}",
            err.message()
        );
        assert!(!state_path(&repo).exists());
        assert_eq!(
            read(&repo, "a"),
            "uncommitted\n",
            "dirty edit not destroyed"
        );
    }

    #[test]
    fn abort_preserves_a_concurrent_branch_advance() {
        let dir = tmp("abort-concurrent");
        let (repo, _) = conflict_repo(&dir);
        // While paused at the conflict, another writer advances the branch.
        let extra = {
            let tip = repo.refname_to_id("refs/heads/topic").unwrap();
            commit(
                &repo,
                &[tip],
                &[("a", "10\n"), ("c", "new\n")],
                "concurrent",
            )
        };
        repo.reference("refs/heads/topic", extra, true, "concurrent advance")
            .unwrap();
        // Aborting must NOT roll the branch back to the pre-op tip and lose it.
        abort(&repo).unwrap();
        assert_eq!(
            repo.refname_to_id("refs/heads/topic").unwrap(),
            extra,
            "abort kept the concurrent commit"
        );
    }

    #[test]
    fn start_refuses_when_an_untracked_file_would_be_overwritten() {
        let dir = tmp("untracked");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        // `onto` introduces file `u`; the worktree has an untracked `u`.
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        let m1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("u", "from onto\n")],
            "adds u",
        );
        on_branch(&repo, "topic", f1);
        std::fs::write(dir.join("u"), "my untracked work\n").unwrap();
        let err = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap_err();
        assert!(
            err.message().contains("would be overwritten"),
            "{}",
            err.message()
        );
        assert_eq!(
            read(&repo, "u"),
            "my untracked work\n",
            "untracked file untouched"
        );
    }

    #[test]
    fn start_refuses_when_an_untracked_dir_file_would_be_overwritten() {
        let dir = tmp("untracked-dir");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        // `onto` introduces `d/u`; the worktree has a fully-untracked
        // directory `d` containing `u`. Statuses without recursion reports
        // only `d/`, which matches no tree path — the guard must still see
        // the file inside.
        let sub = {
            let blob = repo.blob(b"from onto\n").unwrap();
            let mut tb = repo.treebuilder(None).unwrap();
            tb.insert("u", blob, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let m1 = {
            let a = repo.blob(b"1\n").unwrap();
            let mut tb = repo.treebuilder(None).unwrap();
            tb.insert("a", a, 0o100644).unwrap();
            tb.insert("d", sub, 0o040000).unwrap();
            let tree = repo.find_tree(tb.write().unwrap()).unwrap();
            let sig = Signature::now("test", "test@example.invalid").unwrap();
            let parent = repo.find_commit(base).unwrap();
            repo.commit(None, &sig, &sig, "adds d/u", &tree, &[&parent])
                .unwrap()
        };
        on_branch(&repo, "topic", f1);
        std::fs::create_dir(dir.join("d")).unwrap();
        std::fs::write(dir.join("d").join("u"), "my untracked work\n").unwrap();
        let err = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap_err();
        assert!(
            err.message()
                .contains("untracked file d/u would be overwritten"),
            "{}",
            err.message()
        );
        assert_eq!(
            read(&repo, "d/u"),
            "my untracked work\n",
            "untracked file untouched"
        );
    }

    #[test]
    fn start_refuses_when_an_untracked_dir_collides_with_a_file_in_onto() {
        let dir = tmp("untracked-dirblob");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        // `onto` has a FILE `d`; the worktree has a fully-untracked directory
        // `d/` with `d/u` inside. bypath cannot descend through the blob, so
        // only the ancestor check can catch this — the hard reset would
        // otherwise delete the directory to write the file.
        let m1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("d", "a file\n")],
            "adds file d",
        );
        on_branch(&repo, "topic", f1);
        std::fs::create_dir(dir.join("d")).unwrap();
        std::fs::write(dir.join("d").join("u"), "my untracked work\n").unwrap();
        let err = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap_err();
        assert!(
            err.message()
                .contains("untracked file d/u would be deleted"),
            "{}",
            err.message()
        );
        assert_eq!(
            read(&repo, "d/u"),
            "my untracked work\n",
            "untracked file untouched"
        );
    }

    #[test]
    fn start_refuses_on_detached_head() {
        let dir = tmp("detached");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "2\n")], "f1");
        on_branch(&repo, "topic", f1);
        repo.set_head_detached(f1).unwrap();
        let err = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap_err();
        assert!(err.message().contains("detached"), "{}", err.message());
    }

    #[test]
    fn begin_stamps_a_recovery_backup_ref() {
        let dir = tmp("backup");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
        )
        .unwrap();
        // The pre-op tip is recoverable even after a clean finish moved the branch.
        assert_eq!(repo.refname_to_id("refs/mime-backup/topic/0").unwrap(), f1);
        assert_ne!(repo.refname_to_id("refs/heads/topic").unwrap(), f1);
    }

    #[test]
    fn backup_ring_keeps_earlier_ops_and_migrates_a_flat_ref() {
        let dir = tmp("backup-ring");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let t1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        on_branch(&repo, "topic", t1);
        // A pre-ring flat backup (an older mime stamped it) must fold into the
        // ring instead of blocking the /0 ref with a D/F conflict.
        repo.reference("refs/mime-backup/topic", base, true, "legacy")
            .unwrap();

        // Three ops in a row: reword, reword, reword — each pre-tip is kept.
        let mut tips = vec![t1];
        for n in 1..=3 {
            let head = repo.head().unwrap().peel_to_commit().unwrap().id();
            let msg = format!("reword {n}");
            start(
                &repo,
                Plan {
                    onto: base,
                    steps: vec![step(head, Action::Reword, Some(&msg))],
                },
            )
            .unwrap();
            tips.push(repo.head().unwrap().peel_to_commit().unwrap().id());
        }
        let slot = |n: usize| {
            repo.refname_to_id(&format!("refs/mime-backup/topic/{n}"))
                .unwrap()
        };
        // /0 = the last op's pre-tip, /1 and /2 the two before.
        assert_eq!(slot(0), tips[2]);
        assert_eq!(slot(1), tips[1]);
        assert_eq!(slot(2), tips[0]);
        // The legacy flat ref was consumed (rotated to /1 by the first op,
        // then aged out) and no longer exists.
        assert!(repo.refname_to_id("refs/mime-backup/topic").is_err());
    }

    #[test]
    fn rehearse_previews_without_mutating() {
        let dir = tmp("rehearse");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);
        let before = repo.refname_to_id("refs/heads/topic").unwrap();

        let plan = Plan {
            onto: m1,
            steps: vec![step(f1, Action::Pick, None)],
        };
        let preview = rehearse(&repo, &plan, Mode::Pick).unwrap();
        assert_eq!(preview.commits.len(), 1);
        assert!(preview.conflicts.is_empty());
        // Nothing mutated: branch tip, attached HEAD, no state file.
        assert_eq!(repo.refname_to_id("refs/heads/topic").unwrap(), before);
        assert!(!repo.head_detached().unwrap());
        assert!(!state_path(&repo).exists());
        // The previewed tree matches what a real run then produces.
        start(&repo, plan).unwrap();
        assert_eq!(
            repo.head().unwrap().peel_to_tree().unwrap().id(),
            preview.final_tree
        );
    }

    #[test]
    fn rehearse_folds_a_fixup_in_the_preview() {
        let dir = tmp("rehearse-fold");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("x", "0\n")], "base");
        let c1 = commit(&repo, &[base], &[("x", "0\n"), ("a", "1\n")], "add a");
        let c2 = commit(&repo, &[c1], &[("x", "0\n"), ("a", "2\n")], "tweak a");
        on_branch(&repo, "topic", c2);

        let plan = Plan {
            onto: base,
            steps: vec![step(c1, Action::Pick, None), step(c2, Action::Fixup, None)],
        };
        let preview = rehearse(&repo, &plan, Mode::Pick).unwrap();
        // The fixup folds into the pick: ONE previewed commit, not two, carrying
        // the target's message (matching a real apply, checked below).
        assert_eq!(preview.commits.len(), 1, "fixup folded in preview");
        assert_eq!(preview.commits[0].1, "add a");

        // A real apply of the same plan produces the identical single commit.
        start(&repo, plan).unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "add a");
        assert_eq!(tip.parent(0).unwrap().id(), base, "one commit above base");
        assert_eq!(tip.tree_id(), preview.final_tree);
    }

    #[test]
    fn rehearse_reports_a_conflict_without_mutating() {
        let dir = tmp("rehearse-conflict");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "10\n")], "ours");
        let m1 = commit(&repo, &[base], &[("a", "20\n")], "theirs");
        on_branch(&repo, "topic", f1);
        let preview = rehearse(
            &repo,
            &Plan {
                onto: m1,
                steps: vec![step(f1, Action::Pick, None)],
            },
            Mode::Pick,
        )
        .unwrap();
        assert_eq!(preview.conflicts.len(), 1);
        assert_eq!(preview.conflicts[0].step, 0);
        assert_eq!(preview.conflicts[0].files, vec!["a".to_string()]);
        assert!(preview.commits.is_empty());
        assert!(!repo.head_detached().unwrap(), "no mutation");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn rehearse_reports_every_conflicting_step_with_its_reshaper() {
        let dir = tmp("rehearse-multi-conflict");
        let repo = Repository::init(&dir).unwrap();
        // Two files, each rewritten by a later commit; two fixups that target
        // the ORIGINAL commits conflict independently — one rehearsal must
        // name both, each with the commit that last reshaped its lines.
        let base = commit(&repo, &[], &[("a", "1\n"), ("b", "1\n")], "base");
        let add = commit(
            &repo,
            &[base],
            &[("a", "a1\n"), ("b", "b1\n")],
            "add a and b",
        );
        let ra = commit(&repo, &[add], &[("a", "a2\n"), ("b", "b1\n")], "reshape a");
        let rb = commit(&repo, &[ra], &[("a", "a2\n"), ("b", "b2\n")], "reshape b");
        // Fixups built against the TIP's content — they cannot apply at `add`.
        let fa = commit(&repo, &[rb], &[("a", "a3\n"), ("b", "b2\n")], "fixup a");
        let fb = commit(&repo, &[fa], &[("a", "a3\n"), ("b", "b3\n")], "fixup b");
        on_branch(&repo, "main", fb);

        let steps = autosquash_steps(
            &repo,
            base,
            &[(fa, add, Action::Fixup), (fb, add, Action::Fixup)],
        )
        .unwrap();
        let preview = rehearse(&repo, &Plan { onto: base, steps }, Mode::Pick).unwrap();

        assert_eq!(preview.conflicts.len(), 2, "{:?}", preview.conflicts);
        // autosquash inserts the LAST directive right after the target, so
        // fb replays first (conflicting in b), then fa (in a).
        let files: Vec<&str> = preview
            .conflicts
            .iter()
            .flat_map(|c| c.files.iter().map(String::as_str))
            .collect();
        assert_eq!(files, vec!["b", "a"], "both fixups reported in one pass");
        // The why names the reshaping commits — the targets the fixups
        // should have used.
        assert!(
            preview.conflicts[0].why[0].contains("reshape b"),
            "{:?}",
            preview.conflicts[0].why
        );
        assert!(
            preview.conflicts[1].why[0].contains("reshape a"),
            "{:?}",
            preview.conflicts[1].why
        );
        // Nothing moved: rehearsal only, branch tip intact.
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), fb);
    }

    #[test]
    fn edit_pauses_then_amends_the_worktree() {
        let dir = tmp("edit");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Edit, None)],
            },
        )
        .unwrap();
        assert!(
            matches!(out, Outcome::Paused { step: 0, .. }),
            "paused at the edit step"
        );
        assert_eq!(
            read(&repo, "b"),
            "1\n",
            "the commit is applied at the pause"
        );

        // The agent edits the worktree, then continues.
        std::fs::write(repo.workdir().unwrap().join("b"), b"EDITED\n").unwrap();
        std::fs::write(repo.workdir().unwrap().join("c"), b"new\n").unwrap();
        let out = continue_op(&repo, false).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));

        assert_eq!(read(&repo, "b"), "EDITED\n", "edit folded into the commit");
        assert_eq!(read(&repo, "c"), "new\n", "new file folded in");
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "add b", "message preserved");
        assert_eq!(tip.parent(0).unwrap().id(), m1, "still rebased onto m1");
        assert!(!state_path(&repo).exists(), "state cleared on finish");
    }

    #[test]
    fn edit_skip_leaves_the_commit_unchanged() {
        let dir = tmp("edit-skip");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Edit, None)],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Paused { .. }));

        // A stray edit, then skip — the edit must be discarded, the commit kept.
        std::fs::write(repo.workdir().unwrap().join("b"), b"STRAY\n").unwrap();
        let out = skip(&repo).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(read(&repo, "b"), "1\n", "stray edit discarded");
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn edit_step_that_conflicts_pauses_after_resolution() {
        let dir = tmp("edit-conflict");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "f1\n")], "ours");
        let m1 = commit(&repo, &[base], &[("a", "m1\n")], "theirs");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![step(f1, Action::Edit, None)],
            },
        )
        .unwrap();
        assert!(
            matches!(out, Outcome::Conflict { step: 0, .. }),
            "edit step conflicts on apply"
        );

        // Resolve the conflict, continue → lands the commit, then PAUSES for the edit.
        std::fs::write(repo.workdir().unwrap().join("a"), b"resolved\n").unwrap();
        let out = continue_op(&repo, false).unwrap();
        assert!(
            matches!(out, Outcome::Paused { .. }),
            "paused after resolving the conflict"
        );
        assert_eq!(read(&repo, "a"), "resolved\n");

        // Now amend the resolved commit and finish.
        std::fs::write(repo.workdir().unwrap().join("a"), b"final\n").unwrap();
        let out = continue_op(&repo, false).unwrap();
        assert!(matches!(out, Outcome::Done { .. }));
        assert_eq!(
            read(&repo, "a"),
            "final\n",
            "amend applied on the resolution"
        );
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn reword_with_message_edits_preserves_the_rest() {
        let dir = tmp("msgedit");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n")],
            "Subject line\n\nBody paragraph.\n\nSigned-off-by: T <t@e.invalid>\n",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let s = Step {
            commit: f1,
            action: Action::Reword,
            message: None,
            message_edits: vec![MsgEdit::Replace {
                find: "Subject line".to_string(),
                with: "New subject".to_string(),
            }],
            split_into: Vec::new(),
        };
        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![s],
            },
        )
        .unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            tip.message().unwrap(),
            "New subject\n\nBody paragraph.\n\nSigned-off-by: T <t@e.invalid>\n",
            "only the subject changed; body + sign-off preserved"
        );

        // A find that occurs several times is replaced EVERYWHERE.
        let msg = apply_msg_edits(
            "add old_name\n\nold_name feeds the meter; see old_name docs.\n".to_string(),
            &[MsgEdit::Replace {
                find: "old_name".to_string(),
                with: "new_name".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(
            msg, "add new_name\n\nnew_name feeds the meter; see new_name docs.\n",
            "every occurrence replaced"
        );
        // Replacement text containing the find must not re-match (no loop,
        // no double replacement).
        let msg = apply_msg_edits(
            "x x\n".to_string(),
            &[MsgEdit::Replace {
                find: "x".to_string(),
                with: "xx".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(msg, "xx xx\n");
        assert!(
            apply_msg_edits(
                "nothing here\n".to_string(),
                &[MsgEdit::Replace {
                    find: "absent".to_string(),
                    with: "y".to_string(),
                }],
            )
            .is_err(),
            "an absent find stays a loud error"
        );
    }

    #[test]
    fn message_edit_append_adds_a_trailing_line() {
        let dir = tmp("msgappend");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "Add b\n");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let s = Step {
            commit: f1,
            action: Action::Reword,
            message: None,
            message_edits: vec![MsgEdit::Append {
                text: "Acked-by: Z <z@e.invalid>".to_string(),
            }],
            split_into: Vec::new(),
        };
        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![s],
            },
        )
        .unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "Add b\nAcked-by: Z <z@e.invalid>\n");
    }

    #[test]
    fn message_edit_missing_find_fails_before_mutating() {
        let dir = tmp("msgmiss");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "Add b\n");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);
        let before = repo.head().unwrap().peel_to_commit().unwrap().id();

        let s = Step {
            commit: f1,
            action: Action::Reword,
            message: None,
            message_edits: vec![MsgEdit::Replace {
                find: "not present".to_string(),
                with: "x".to_string(),
            }],
            split_into: Vec::new(),
        };
        assert!(
            start(
                &repo,
                Plan {
                    onto: m1,
                    steps: vec![s]
                }
            )
            .is_err()
        );
        assert!(!state_path(&repo).exists(), "no op left dangling");
        assert!(!repo.head_detached().unwrap(), "branch untouched");
        assert_eq!(
            repo.head().unwrap().peel_to_commit().unwrap().id(),
            before,
            "no mutation on a pre-validated message-edit error"
        );
    }

    #[test]
    fn reword_message_edits_target_the_provided_message() {
        let dir = tmp("msg-provided");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "orig\n");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        // message_edits apply to the PROVIDED message, not the commit's original;
        // pre-validation must use the same base or it falsely rejects this plan.
        let s = Step {
            commit: f1,
            action: Action::Reword,
            message: Some("brand new subject".to_string()),
            message_edits: vec![MsgEdit::Replace {
                find: "new".to_string(),
                with: "NEW".to_string(),
            }],
            split_into: Vec::new(),
        };
        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![s],
            },
        )
        .unwrap();
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.message().unwrap(), "brand NEW subject");
    }

    #[test]
    fn message_edits_rejected_on_a_pick_step() {
        let dir = tmp("msg-on-pick");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);
        let before = repo.head().unwrap().peel_to_commit().unwrap().id();

        // pick doesn't build a message from a base, so message_edits would be
        // silently dropped — reject them before mutating instead.
        let s = Step {
            commit: f1,
            action: Action::Pick,
            message: None,
            message_edits: vec![MsgEdit::Append {
                text: "Note".to_string(),
            }],
            split_into: Vec::new(),
        };
        assert!(
            start(
                &repo,
                Plan {
                    onto: m1,
                    steps: vec![s]
                }
            )
            .is_err()
        );
        assert!(!state_path(&repo).exists(), "no op left dangling");
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), before);
    }

    #[test]
    fn chained_message_edits_validate_as_a_sequence() {
        let dir = tmp("msg-chain");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");

        // Create-then-match: edit 2's anchor is produced by edit 1 — must SUCCEED
        // (validating each find against the static base would falsely reject it).
        let f_ok = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n")],
            "Fix the bar widget",
        );
        on_branch(&repo, "topic", f_ok);
        let s = Step {
            commit: f_ok,
            action: Action::Reword,
            message: None,
            message_edits: vec![
                MsgEdit::Replace {
                    find: "bar".to_string(),
                    with: "baz".to_string(),
                },
                MsgEdit::Replace {
                    find: "baz widget".to_string(),
                    with: "baz gadget".to_string(),
                },
            ],
            split_into: Vec::new(),
        };
        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![s],
            },
        )
        .unwrap();
        assert_eq!(
            repo.head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .message()
                .unwrap(),
            "Fix the baz gadget"
        );

        // Remove-then-match: edit 1 deletes edit 2's anchor — must fail BEFORE
        // mutating (the old per-find check passed this, then died mid-land).
        let f_bad = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("c", "1\n")],
            "Remove the old flag and the old code",
        );
        on_branch(&repo, "topic2", f_bad);
        let before = repo.head().unwrap().peel_to_commit().unwrap().id();
        let s2 = Step {
            commit: f_bad,
            action: Action::Reword,
            message: None,
            message_edits: vec![
                MsgEdit::Replace {
                    find: "the old flag and ".to_string(),
                    with: String::new(),
                },
                MsgEdit::Replace {
                    find: "old flag".to_string(),
                    with: "X".to_string(),
                },
            ],
            split_into: Vec::new(),
        };
        assert!(
            start(
                &repo,
                Plan {
                    onto: m1,
                    steps: vec![s2]
                }
            )
            .is_err()
        );
        assert!(!state_path(&repo).exists(), "fails before mutation");
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), before);
    }

    #[test]
    fn msg_edit_spec_requires_exactly_find_or_append() {
        let both = MsgEditSpec {
            find: Some("x".into()),
            replace: None,
            append: Some("y".into()),
        };
        let neither = MsgEditSpec {
            find: None,
            replace: Some("y".into()),
            append: None,
        };
        assert!(MsgEdit::from_spec(&both).is_err(), "both find + append");
        assert!(
            MsgEdit::from_spec(&neither).is_err(),
            "neither find nor append"
        );
        let ok = MsgEditSpec {
            find: Some("x".into()),
            replace: Some("y".into()),
            append: None,
        };
        assert!(matches!(
            MsgEdit::from_spec(&ok),
            Ok(MsgEdit::Replace { .. })
        ));
    }

    fn split_part(message: &str, paths: &[&str], rest: bool) -> SplitPart {
        SplitPart {
            message: message.to_string(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            hunks: Vec::new(),
            rest,
        }
    }

    /// A part that claims hunks of one file by new-side line span [lo, hi].
    fn hunk_part(message: &str, path: &str, lo: u32, hi: u32) -> SplitPart {
        SplitPart {
            message: message.to_string(),
            paths: Vec::new(),
            hunks: vec![HunkSel {
                path: path.to_string(),
                lo,
                hi,
            }],
            rest: false,
        }
    }

    fn split_step(commit: Oid, parts: Vec<SplitPart>) -> Step {
        Step {
            commit,
            action: Action::Split,
            message: None,
            message_edits: Vec::new(),
            split_into: parts,
        }
    }

    #[test]
    fn split_partitions_a_commit_by_path() {
        let dir = tmp("split");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "add b and c",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let out = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![split_step(
                    f1,
                    vec![
                        split_part("add b", &["b"], false),
                        split_part("add c", &["c"], false),
                    ],
                )],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Done { .. }));

        // Final state = the whole original change, rebased onto m1.
        assert_eq!(read(&repo, "a"), "2\n");
        assert_eq!(read(&repo, "b"), "1\n");
        assert_eq!(read(&repo, "c"), "1\n");

        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let first = tip.parent(0).unwrap();
        assert_eq!(tip.message().unwrap(), "add c");
        assert_eq!(first.message().unwrap(), "add b");
        assert_eq!(first.parent(0).unwrap().id(), m1, "chain rooted on m1");
        // The first commit has b but not yet c; the second adds c.
        assert!(first.tree().unwrap().get_path(Path::new("b")).is_ok());
        assert!(first.tree().unwrap().get_path(Path::new("c")).is_err());
        assert!(tip.tree().unwrap().get_path(Path::new("c")).is_ok());
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn split_partitions_a_commit_by_hunk() {
        let dir = tmp("split-hunk");
        let repo = Repository::init(&dir).unwrap();
        // A 12-line file so edits at line 1 and line 12 form two SEPARATE diff
        // hunks (their 3-line context windows don't touch).
        let base = commit(
            &repo,
            &[],
            &[("f", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n")],
            "base",
        );
        let c1 = commit(
            &repo,
            &[base],
            &[("f", "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nY\n")],
            "edit top and bottom",
        );
        on_branch(&repo, "topic", c1);

        let out = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![split_step(
                    c1,
                    vec![
                        hunk_part("top", "f", 1, 1),     // the new-line-1 hunk
                        split_part("bottom", &[], true), // catch-all: the rest
                    ],
                )],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Done { .. }));

        // Reassembled worktree equals the original commit.
        assert_eq!(read(&repo, "f"), "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nY\n");

        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let first = tip.parent(0).unwrap();
        assert_eq!(first.message().unwrap(), "top");
        assert_eq!(tip.message().unwrap(), "bottom");
        assert_eq!(first.parent(0).unwrap().id(), base, "chain rooted on base");
        // The first commit carries ONLY the top hunk; the bottom line is still base's.
        let e = first.tree().unwrap().get_path(Path::new("f")).unwrap();
        assert_eq!(
            repo.find_blob(e.id()).unwrap().content(),
            b"X\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n"
        );
        assert!(!state_path(&repo).exists());
    }

    #[test]
    fn split_rejects_a_hunk_selector_that_matches_nothing() {
        let dir = tmp("split-hunk-bad");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("f", "a\nb\nc\n")], "base");
        let c1 = commit(&repo, &[base], &[("f", "A\nb\nc\n")], "edit line 1");
        on_branch(&repo, "topic", c1);

        // Line 99 overlaps no hunk — begin() rejects before any mutation.
        let err = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![split_step(
                    c1,
                    vec![
                        hunk_part("nope", "f", 99, 99),
                        split_part("rest", &[], true),
                    ],
                )],
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("no hunk of f overlaps"), "{err}");
        assert!(
            !state_path(&repo).exists(),
            "a rejected start leaves no operation state"
        );
    }

    #[test]
    fn split_by_hunk_does_not_leak_an_empty_added_file() {
        let dir = tmp("split-hunk-add");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("other", "x\n")], "base");
        // c1 modifies `other` AND adds a new file (one add-hunk).
        let c1 = commit(
            &repo,
            &[base],
            &[("other", "X\n"), ("new", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n")],
            "edit other + add new",
        );
        on_branch(&repo, "topic", c1);

        // part0 takes `other`; part1 takes the added file by hunk. `new` must NOT
        // appear (even empty) in part0 — the delta-level gate drops the add there.
        let out = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![split_step(
                    c1,
                    vec![
                        split_part("other only", &["other"], false),
                        hunk_part("add new", "new", 1, 10),
                    ],
                )],
            },
        )
        .unwrap();
        assert!(matches!(out, Outcome::Done { .. }));

        let tip = repo.head().unwrap().peel_to_commit().unwrap(); // part1
        let first = tip.parent(0).unwrap(); // part0
        assert_eq!(first.message().unwrap(), "other only");
        assert!(
            first.tree().unwrap().get_path(Path::new("new")).is_err(),
            "the added file must not leak into the earlier part"
        );
        assert!(first.tree().unwrap().get_path(Path::new("other")).is_ok());
        assert!(tip.tree().unwrap().get_path(Path::new("new")).is_ok());
    }

    #[test]
    fn split_by_hunk_rejects_an_empty_non_catch_all_part() {
        let dir = tmp("split-hunk-empty");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("f", "a\nb\n")], "base");
        let c1 = commit(&repo, &[base], &[("f", "A\nb\n")], "edit");
        on_branch(&repo, "topic", c1);

        // A hunk part forces the hunk-aware validator; a second part with neither
        // paths nor hunks and rest=false must be rejected (not silently promoted).
        let err = start(
            &repo,
            Plan {
                onto: base,
                steps: vec![split_step(
                    c1,
                    vec![hunk_part("top", "f", 1, 1), split_part("empty", &[], false)],
                )],
            },
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("needs at least one path or hunk"),
            "{err}"
        );
    }

    #[test]
    fn split_catch_all_collects_the_rest() {
        let dir = tmp("split-rest");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "add b and c",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![split_step(
                    f1,
                    vec![
                        split_part("just b", &["b"], false),
                        split_part("the rest", &[], true),
                    ],
                )],
            },
        )
        .unwrap();

        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let first = tip.parent(0).unwrap();
        assert_eq!(first.message().unwrap(), "just b");
        assert_eq!(tip.message().unwrap(), "the rest");
        assert!(first.tree().unwrap().get_path(Path::new("c")).is_err());
        assert!(tip.tree().unwrap().get_path(Path::new("c")).is_ok());
    }

    #[test]
    fn split_rejects_an_unassigned_path_before_mutating() {
        let dir = tmp("split-bad");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "add b and c",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);
        let before = repo.head().unwrap().peel_to_commit().unwrap().id();

        // Only b is assigned and there is no catch-all → c is unassigned.
        let err = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![split_step(f1, vec![split_part("just b", &["b"], false)])],
            },
        );
        assert!(err.is_err());
        assert!(!state_path(&repo).exists(), "no op left dangling");
        assert_eq!(
            repo.head().unwrap().peel_to_commit().unwrap().id(),
            before,
            "no mutation on a pre-validated split error"
        );
    }

    #[test]
    fn split_rejects_a_path_the_commit_does_not_change() {
        let dir = tmp("split-untouched");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "add b");
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let err = start(
            &repo,
            Plan {
                onto: m1,
                steps: vec![split_step(
                    f1,
                    vec![
                        split_part("nope", &["a"], false),
                        split_part("rest", &[], true),
                    ],
                )],
            },
        );
        assert!(err.is_err(), "the commit does not change a");
    }

    #[test]
    fn split_rehearse_previews_the_parts_without_mutating() {
        let dir = tmp("split-rehearse");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(
            &repo,
            &[base],
            &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")],
            "add b and c",
        );
        let m1 = commit(&repo, &[base], &[("a", "2\n")], "change a");
        on_branch(&repo, "topic", f1);

        let preview = rehearse(
            &repo,
            &Plan {
                onto: m1,
                steps: vec![split_step(
                    f1,
                    vec![
                        split_part("add b", &["b"], false),
                        split_part("add c", &["c"], false),
                    ],
                )],
            },
            Mode::Pick,
        )
        .unwrap();
        let msgs: Vec<&str> = preview.commits.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(msgs, vec!["add b", "add c"], "both parts previewed");
        assert!(preview.conflicts.is_empty());
        assert!(!repo.head_detached().unwrap(), "no mutation");
        assert!(!state_path(&repo).exists());
    }
}
