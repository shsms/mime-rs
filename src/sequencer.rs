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

use git2::{
    CherrypickOptions, Delta, Index, Oid, Repository, ResetType, RevertOptions, Sort,
    build::CheckoutBuilder,
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
    Reword,
    Squash,
    Fixup,
    Drop,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword => "reword",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
        }
    }
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "pick" => Action::Pick,
            "reword" => Action::Reword,
            "squash" => Action::Squash,
            "fixup" => Action::Fixup,
            "drop" => Action::Drop,
            _ => return None,
        })
    }
}

/// One planned step: a commit to replay, how, and (for reword) a new message.
#[derive(Clone, Debug)]
pub struct Step {
    pub commit: Oid,
    pub action: Action,
    pub message: Option<String>,
}

/// A rebase plan: replay `steps` (in order) onto `onto`.
#[derive(Clone, Debug)]
pub struct Plan {
    pub onto: Oid,
    pub steps: Vec<Step>,
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
    Done { head: Oid },
    Conflict { step: usize, files: Vec<String> },
}

/// A snapshot of an in-progress operation, for `git_status`.
#[derive(Debug)]
pub struct Status {
    pub next: usize,
    pub total: usize,
    pub current: Oid,
    pub conflicts: Vec<String>,
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
}

fn state_path(repo: &Repository) -> std::path::PathBuf {
    repo.path().join("mime-sequencer.json")
}

fn save_state(repo: &Repository, st: &State) -> Result<(), Error> {
    let steps: Vec<_> = st
        .steps
        .iter()
        .map(|s| {
            json!({"commit": s.commit.to_string(), "action": s.action.as_str(), "message": s.message})
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

/// Whether `bytes` has a conflict-marker opener (`<<<<<<<`, ≥7 `<`) at the start
/// of any line — `git diff --check`'s signal. Byte-level so it works on non-UTF-8
/// files and on conflicts the diff3 hunk parser can't structure.
fn has_conflict_markers(bytes: &[u8]) -> bool {
    bytes
        .split(|&b| b == b'\n')
        .any(|line| line.starts_with(b"<<<<<<<"))
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
    }
}

fn begin(repo: &Repository, plan: Plan, mode: Mode) -> Result<Outcome, Error> {
    if let Some(first) = plan.steps.iter().find(|s| s.action != Action::Drop)
        && matches!(first.action, Action::Squash | Action::Fixup)
    {
        return Err(estr("the first applied step cannot be squash/fixup"));
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
        let new = make_commit(repo, mode, plan.onto, current, step, &tree)?;
        let summary = repo.find_commit(new)?.summary().unwrap_or("").to_string();
        commits.push((new, summary));
        current = new;
    }
    Ok(Preview {
        commits,
        final_tree: repo.find_commit(current)?.tree_id(),
        conflict: None,
    })
}

/// Resume after the agent resolved a conflict in the worktree: stage the
/// resolved paths, commit the stopped step, then continue the plan.
pub fn continue_op(repo: &Repository) -> Result<Outcome, Error> {
    let mut st = load_state(repo)?;
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
            // Refuse to commit a file that still carries conflict markers. A
            // byte-level opener check (git diff --check's signal) is encoding-
            // agnostic and catches malformed runs a hunk parser would miss; the
            // earlier add_path-then-has_conflicts check was dead (add_path
            // clears the conflict, so the guard never fired).
            if has_conflict_markers(&std::fs::read(&full).unwrap_or_default()) {
                return Err(estr(&format!(
                    "unresolved conflict markers remain in {path} — resolve them, then continue"
                )));
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
    let new = land_step(repo, &st, &step, &tree)?;
    repo.cleanup_state()?;
    st.current = new;
    st.next += 1;
    save_state(repo, &st)?;
    drive(repo, st)
}

/// Skip the stopped step: discard its (conflicted) merge, then continue the
/// plan as if that commit had been dropped.
pub fn skip(repo: &Repository) -> Result<Outcome, Error> {
    let mut st = load_state(repo)?;
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
    let tip = repo
        .refname_to_id(&st.branch)
        .map_err(|_| estr(&format!("{} no longer exists — cannot restore", st.branch)))?;
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
        Action::Reword => {
            let msg = step.message.clone().unwrap_or_else(pick_msg);
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
        let new = land_step(repo, &st, &step, &tree)?;
        repo.cleanup_state()?;
        st.current = new;
        st.next += 1;
        // Persist after each landed step: a crash mid-run otherwise leaves HEAD
        // ahead of a stale next=0/current=onto state that would re-apply commits.
        save_state(repo, &st)?;
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
            format!(
                "operation in progress: step {}/{}, tip {}{conflicts}",
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

/// `git_rebase`: replay `onto..HEAD` (or an explicit `plan` of
/// `(commit, action, message)`) onto `onto`. With `rehearse`, only preview the
/// result (commit list + whether the tree is unchanged) — no changes applied.
pub fn cmd_rebase(
    repo_path: &std::path::Path,
    onto: &str,
    plan: Option<Vec<(String, String, Option<String>)>>,
    rehearse_only: bool,
) -> Result<String, String> {
    let repo = open(repo_path)?;
    let onto_oid = resolve_s(&repo, onto)?;
    let steps = match plan {
        Some(items) => items
            .iter()
            .map(|(commit, action, message)| {
                Ok(Step {
                    commit: resolve_s(&repo, commit)?,
                    action: Action::parse(action)
                        .ok_or_else(|| format!("unknown action \"{action}\""))?,
                    message: message.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        None => commits_since(&repo, onto_oid)
            .map_err(gerr)?
            .into_iter()
            .map(pick_step)
            .collect(),
    };
    let plan = Plan {
        onto: onto_oid,
        steps,
    };
    if rehearse_only {
        Ok(preview_text(
            &repo,
            &rehearse(&repo, &plan, Mode::Pick).map_err(gerr)?,
        ))
    } else {
        Ok(outcome_text(&start(&repo, plan).map_err(gerr)?))
    }
}

fn resolve_all(repo: &Repository, specs: &[String]) -> Result<Vec<Oid>, String> {
    specs.iter().map(|s| resolve_s(repo, s)).collect()
}

pub fn cmd_cherry_pick(repo_path: &std::path::Path, commits: &[String]) -> Result<String, String> {
    let repo = open(repo_path)?;
    let oids = resolve_all(&repo, commits)?;
    Ok(outcome_text(&cherry_pick(&repo, oids).map_err(gerr)?))
}

pub fn cmd_revert(repo_path: &std::path::Path, commits: &[String]) -> Result<String, String> {
    let repo = open(repo_path)?;
    let oids = resolve_all(&repo, commits)?;
    Ok(outcome_text(&revert(&repo, oids).map_err(gerr)?))
}

pub fn cmd_continue(repo_path: &std::path::Path) -> Result<String, String> {
    Ok(outcome_text(&continue_op(&open(repo_path)?).map_err(gerr)?))
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
        let out = continue_op(&repo).unwrap();
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
        let out = cmd_rebase(&dir, &m1.to_string(), None, false).unwrap();
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
        let err = continue_op(&repo).unwrap_err();
        assert!(
            err.message().contains("unresolved conflict markers"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn continue_commits_only_the_conflicted_paths() {
        let dir = tmp("outofset");
        let (repo, _) = conflict_repo(&dir);
        // Resolve the conflicted file AND scribble on an unrelated tracked file.
        std::fs::write(dir.join("a"), "resolved\n").unwrap();
        std::fs::write(dir.join("b"), "also changed\n").unwrap();
        assert!(matches!(continue_op(&repo).unwrap(), Outcome::Done { .. }));
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
        assert!(matches!(continue_op(&repo).unwrap(), Outcome::Done { .. }));
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
}
