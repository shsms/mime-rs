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

/// The recovery ref a `begin` stamps with the pre-op tip of `branch` (a full
/// refname). Lives under `refs/mime-backup/` so it stays out of `git branch` yet
/// is trivially recoverable (`git reset --hard <ref>`).
fn backup_ref(branch: &str) -> String {
    format!(
        "refs/mime-backup/{}",
        branch.strip_prefix("refs/heads/").unwrap_or(branch)
    )
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
    /// Replace the first occurrence of `find` with `with` (with = "" deletes).
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
/// typo'd anchor fails loudly rather than silently doing nothing).
fn apply_msg_edits(mut msg: String, edits: &[MsgEdit]) -> Result<String, Error> {
    for e in edits {
        match e {
            MsgEdit::Replace { find, with } => match msg.find(find.as_str()) {
                Some(pos) => msg.replace_range(pos..pos + find.len(), with),
                None => {
                    return Err(estr(&format!(
                        "message edit: text not found in the commit message: {find:?}"
                    )));
                }
            },
            MsgEdit::Append { text } => {
                if !msg.is_empty() && !msg.ends_with('\n') {
                    msg.push('\n');
                }
                msg.push_str(text);
                if !text.ends_with('\n') {
                    msg.push('\n');
                }
            }
        }
    }
    Ok(msg)
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
/// moving HEAD/refs or touching the worktree. `conflict` is set (and the commit
/// list truncated) at the first step a real run would stop on.
#[derive(Debug)]
pub struct Preview {
    pub commits: Vec<(Oid, String)>,
    pub final_tree: Oid,
    pub conflict: Option<(usize, Vec<String>)>,
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
                    MsgEdit::Replace { find, with } => json!({"find": find, "with": with}),
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
                                        with: e["with"].as_str().unwrap_or("").to_string(),
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
    // Like `git rebase`, refuse to start over uncommitted work the hard reset
    // below would silently destroy.
    if is_dirty(repo)? {
        return Err(estr(
            "the working tree has uncommitted changes — commit or stash them first",
        ));
    }
    // is_dirty ignores untracked files, but the hard reset to `onto` would still
    // clobber an untracked file colliding with a path in onto's tree — git rebase
    // refuses that, so we do too.
    let onto_tree = repo.find_commit(plan.onto)?.tree()?;
    let mut uopts = git2::StatusOptions::new();
    uopts.include_untracked(true).include_ignored(false);
    for e in repo.statuses(Some(&mut uopts))?.iter() {
        if e.status().contains(git2::Status::WT_NEW)
            && let Some(p) = e.path()
            && onto_tree.get_path(Path::new(p)).is_ok()
        {
            return Err(estr(&format!(
                "untracked file {p} would be overwritten by the checkout — move or remove it first"
            )));
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
    // only recoverable via the reflog) — even after a clean finish.
    repo.reference(
        &backup_ref(&branch),
        orig,
        true,
        "mime sequencer: pre-op backup",
    )?;

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
    };
    save_state(repo, &st)?;
    drive(repo, st)
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
            return Ok(Preview {
                commits,
                final_tree: current_commit.tree_id(),
                conflict: Some((i, conflict_paths(&index))),
            });
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
        conflict: None,
    })
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
    let new = if step.action == Action::Split {
        land_split(repo, &st, &step, &tree)?
    } else {
        land_step(repo, &st, &step, &tree)?
    };
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
        hard_reset(repo, st.current)?;
        let _ = repo.cleanup_state();
        st.editing = false;
        save_state(repo, &st)?;
        return drive(repo, st);
    }
    if st.next >= st.steps.len() {
        return Err(estr("nothing to skip"));
    }
    // Discard the in-progress merge residue, back to the last good tip.
    hard_reset(repo, st.current)?;
    let _ = repo.cleanup_state();
    st.next += 1;
    save_state(repo, &st)?;
    drive(repo, st)
}

/// Abort: drop the replay and put HEAD back on the branch at its CURRENT tip.
/// We never move the branch ref during an op (only `finish` does), so resetting
/// to the branch's present tip — not the recorded `orig` — preserves any commits
/// another process added while we were paused, instead of clobbering them.
pub fn abort(repo: &Repository) -> Result<(), Error> {
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
    hard_reset(repo, tip)?;
    let _ = repo.cleanup_state();
    let _ = std::fs::remove_file(state_path(repo));
    Ok(())
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

/// Paths that differ between the `base` and `target` trees (a commit's net change).
fn changed_paths(
    repo: &Repository,
    base: &git2::Tree,
    target: &git2::Tree,
) -> Result<Vec<String>, Error> {
    let diff = repo.diff_tree_to_tree(Some(base), Some(target), None)?;
    let mut paths = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                paths.push(p.to_string_lossy().into_owned());
            }
            true
        },
        None,
        None,
        None,
    )?;
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
            if !touched.iter().any(|t| t == p) {
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
        let prev_tree = repo.find_commit(current)?.tree()?;
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
        if p.paths.is_empty() && p.hunks.is_empty() {
            if rest_idx.is_some() {
                return Err(estr(
                    "split: at most one part may be the catch-all (the one with no paths or hunks)",
                ));
            }
            rest_idx = Some(i);
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
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
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

    let mut current = base;
    let mut made = Vec::new();
    for k in 0..parts.len() {
        let cur_path = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let cur_path_delta = cur_path.clone();
        let whole = &plan.whole;
        let per_hunk = &plan.per_hunk;
        let mut opts = git2::ApplyOptions::new();
        opts.delta_callback(move |d: Option<git2::DiffDelta>| {
            let path = d
                .and_then(|d| d.new_file().path().or_else(|| d.old_file().path()))
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            *cur_path_delta.borrow_mut() = path.clone();
            // Whole-file: apply iff a part in 0..=k owns it. Hunk-split file: let
            // the hunk callback decide, so return true here.
            whole.get(&path).map(|&owner| owner <= k).unwrap_or(true)
        });
        opts.hunk_callback(move |h: Option<git2::DiffHunk>| {
            let Some(h) = h else { return false };
            let path = cur_path.borrow().clone();
            // A whole-file-owned file's hunks aren't in per_hunk; delta_callback
            // already gated it, so apply them all (None → true).
            per_hunk
                .get(&(path, h.new_start(), h.new_lines()))
                .map(|&owner| owner <= k)
                .unwrap_or(true)
        });
        let mut index = repo.apply_to_tree(&base_tree, &diff, Some(&mut opts))?;
        let tree = repo.find_tree(index.write_tree_to(repo)?)?;
        // Safety invariant: the final part must reassemble the original commit
        // exactly — a mismatch means the partition dropped or duplicated a change.
        if k == parts.len() - 1 && tree.id() != target.id() {
            return Err(estr(
                "split: internal error — the parts do not reassemble the original commit's tree",
            ));
        }
        let parent = repo.find_commit(current)?;
        let new = repo.commit(
            None,
            &pick.author(),
            &pick.committer(),
            &parts[k].message,
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
        .and_then(|p| p.to_str())
        .map(str::to_string)
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
            return Err(estr(&format!("move: `from` does not change {p}")));
        }
        whole.insert(p.clone());
    }
    let mut hunk_keys = std::collections::HashSet::new();
    for sel in hunks {
        if whole.contains(&sel.path) {
            return Err(estr(&format!(
                "move: {} is named both by path and by hunk",
                sel.path
            )));
        }
        let Some((_, hs)) = files.iter().find(|(f, _)| f == &sel.path) else {
            return Err(estr(&format!("move: `from` does not change {}", sel.path)));
        };
        if hs.is_empty() {
            return Err(estr(&format!(
                "move: {} has no text hunks (a rename/mode/binary change — move it by path)",
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
                "move: no hunk of {} overlaps lines {}-{}",
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
    let sel = resolve_move_selection(&from_diff, paths, hunks)?;
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
    Ok(format!("{}{note}", outcome_text(&start(repo, plan)?)))
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
        let new = if step.action == Action::Split {
            land_split(repo, &st, &step, &tree)?
        } else {
            land_step(repo, &st, &step, &tree)?
        };
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
    finish(repo, &st)?;
    Ok(Outcome::Done { head: st.current })
}

/// Land the rebased history: move the branch ref to the new tip, reattach
/// HEAD, clean the worktree, drop the state.
fn finish(repo: &Repository, st: &State) -> Result<(), Error> {
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
    hard_reset(repo, st.current)?;
    let _ = repo.cleanup_state();
    let _ = std::fs::remove_file(state_path(repo));
    Ok(())
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
        let p = d
            .new_file()
            .path()
            .or_else(|| d.old_file().path())
            .and_then(|p| p.to_str())
            .unwrap_or("?");
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
) -> Result<String, Error> {
    let mut opts = BlameOptions::new();
    if let Some((lo, hi)) = range {
        opts.min_line(lo.max(1));
        opts.max_line(hi.max(lo.max(1)));
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

// ---- text rendering for the tool layer ------------------------------------

/// The agent-facing summary of an [`Outcome`].
pub fn outcome_text(out: &Outcome) -> String {
    match out {
        Outcome::Done { head } => format!("done — new tip {}", short(*head)),
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
    match &preview.conflict {
        Some((step, files)) => out.push_str(&format!(
            "  stops at step {} on a conflict: {}\n",
            step + 1,
            files.join(", ")
        )),
        None => {
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
        }
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
                "\n  pre-op tip backed up at {} (the next op on this branch overwrites it)",
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
    Ok(format!(
        "{}{note}",
        outcome_text(&start(&repo, plan).map_err(gerr)?)
    ))
}

pub fn cmd_rebase(
    repo_path: &std::path::Path,
    onto: &str,
    plan: Option<Vec<PlanItem>>,
    autosquash: Option<Vec<(String, String, String)>>,
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let onto_oid = resolve_s(&repo, onto)?;
    let steps = if let Some(directives) = autosquash {
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
        autosquash_steps(&repo, onto_oid, &resolved).map_err(gerr)?
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
            None => commits_since(&repo, onto_oid)
                .map_err(gerr)?
                .into_iter()
                .map(pick_step)
                .collect(),
        }
    };
    let plan = Plan {
        onto: onto_oid,
        steps,
    };
    if rehearse_only {
        return Ok(preview_text(
            &repo,
            &rehearse(&repo, &plan, Mode::Pick).map_err(gerr)?,
        ));
    }
    let note = backup_note(&repo);
    Ok(format!(
        "{}{note}",
        outcome_text(&start(&repo, plan).map_err(gerr)?)
    ))
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
    Ok(outcome_text(
        &continue_op(&open(repo_path)?, force).map_err(gerr)?,
    ))
}

pub fn cmd_skip(repo_path: &std::path::Path) -> Result<String, String> {
    Ok(outcome_text(&skip(&open(repo_path)?).map_err(gerr)?))
}

pub fn cmd_abort(repo_path: &std::path::Path) -> Result<String, String> {
    abort(&open(repo_path)?).map_err(gerr)?;
    Ok("aborted — restored to the pre-operation state".to_string())
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
    path: &str,
    lines: Option<(usize, usize)>,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    // Accept an absolute path (what grep/occur hand back) or one already
    // relative to the workdir; blame_file wants it workdir-relative.
    let p = std::path::Path::new(path);
    let rel = if p.is_absolute() {
        let wd = repo
            .workdir()
            .ok_or_else(|| "bare repo has no working tree to blame".to_string())?;
        p.strip_prefix(wd)
            .map_err(|_| format!("path is outside the repo: {path}"))?
            .to_path_buf()
    } else {
        p.to_path_buf()
    };
    blame(&repo, &rel, lines).map_err(gerr)
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
        let out = cmd_rebase(&dir, &m1.to_string(), None, None, false).unwrap();
        assert!(out.starts_with("done"), "{out}");
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
    fn cmd_blame_attributes_each_line_to_its_last_commit() {
        let dir = tmp("blame");
        let repo = Repository::init(&dir).unwrap();
        // c1 introduces both lines; c2 rewrites only line 2.
        let c1 = commit(&repo, &[], &[("f.txt", "one\ntwo\n")], "add one and two");
        let c2 = commit(&repo, &[c1], &[("f.txt", "one\nTWO\n")], "change two");
        on_branch(&repo, "main", c2);

        let out = cmd_blame(&dir, "f.txt", None).unwrap();
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
        let just2 = cmd_blame(&dir, "f.txt", Some((2, 2))).unwrap();
        assert!(
            just2.contains("change two") && !just2.contains("add one and two"),
            "windowed blame: {just2}"
        );

        // An absolute path (what grep/occur hand back) resolves the same way.
        let abs = dir.join("f.txt");
        let via_abs = cmd_blame(&dir, abs.to_str().unwrap(), None).unwrap();
        assert_eq!(via_abs, out, "absolute path blames identically");
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
    fn autosquash_sparse_plan_folds_without_listing_every_commit() {
        let dir = tmp("autosquash");
        let (repo, base, c1, _c2, c3) = fixup_fixture(&dir);

        // Only the RELATIONSHIP: fold c3 into c1; c2 is auto-picked untouched.
        let out = cmd_rebase(
            &dir,
            &base.to_string(),
            None,
            Some(vec![(c3.to_string(), c1.to_string(), "fixup".to_string())]),
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
    fn fixup_folds_a_commit_into_its_target() {
        let dir = tmp("fixup-fold");
        let (repo, base, c1, _c2, c3) = fixup_fixture(&dir);

        // One call: fold c3 into c1, inferring the base and picking the rest.
        let out = cmd_fixup(&dir, &c1.to_string(), &c3.to_string(), false).unwrap();
        assert!(out.starts_with("done"), "{out}");
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
        assert_eq!(repo.refname_to_id("refs/mime-backup/topic").unwrap(), f1);
        assert_ne!(repo.refname_to_id("refs/heads/topic").unwrap(), f1);
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
        assert!(preview.conflict.is_none());
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
        assert_eq!(preview.conflict, Some((0, vec!["a".to_string()])));
        assert!(preview.commits.is_empty());
        assert!(!repo.head_detached().unwrap(), "no mutation");
        assert!(!state_path(&repo).exists());
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
        assert!(preview.conflict.is_none());
        assert!(!repo.head_detached().unwrap(), "no mutation");
        assert!(!state_path(&repo).exists());
    }
}
