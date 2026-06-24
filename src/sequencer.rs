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
//! process restarts; the branch ref only moves at `finish`. `abort` is a
//! `reset --hard` back to the pre-op snapshot.
//!
//! Network and arbitrary-code channels are unused by construction: no
//! remotes/transports, no hooks or filters; repos are confined to `MIME_ROOTS`
//! at the tool boundary (see todo.org for the security checklist).

use git2::{
    CherrypickOptions, Index, Oid, Repository, ResetType, RevertOptions, build::CheckoutBuilder,
};
use serde_json::json;
use std::path::Path;

type Error = git2::Error;

fn estr(msg: &str) -> Error {
    Error::from_str(msg)
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
    let data = std::fs::read(state_path(repo))
        .map_err(|_| estr("no sequencer operation in progress"))?;
    let v: serde_json::Value =
        serde_json::from_slice(&data).map_err(|e| estr(&format!("corrupt sequencer state: {e}")))?;
    let oid = |k: &str| -> Result<Oid, Error> {
        Oid::from_str(v[k].as_str().ok_or_else(|| estr("corrupt sequencer state"))?)
    };
    let steps = v["steps"]
        .as_array()
        .ok_or_else(|| estr("corrupt sequencer state"))?
        .iter()
        .map(|s| {
            Ok(Step {
                commit: Oid::from_str(
                    s["commit"].as_str().ok_or_else(|| estr("corrupt step"))?,
                )?,
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
    let head = repo.head()?;
    let branch = head
        .name()
        .ok_or_else(|| estr("HEAD has no name (detached) — cannot rebase"))?
        .to_string();
    let orig = head.peel_to_commit()?.id();

    // Detach onto the base and clean the worktree to it.
    repo.set_head_detached(plan.onto)?;
    repo.reset(&repo.find_object(plan.onto, None)?, ResetType::Hard, None)?;

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

/// Resume after the agent resolved a conflict in the worktree: stage the
/// resolved paths, commit the stopped step, then continue the plan.
pub fn continue_op(repo: &Repository) -> Result<Outcome, Error> {
    let mut st = load_state(repo)?;
    let step = st
        .steps
        .get(st.next)
        .ok_or_else(|| estr("nothing to continue"))?
        .clone();

    let mut index = repo.index()?;
    for path in conflict_paths(&index) {
        index.add_path(Path::new(&path))?;
    }
    if index.has_conflicts() {
        return Err(estr(
            "unresolved conflict markers remain — resolve them, then continue",
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

/// Abort: restore HEAD/refs/worktree to the pre-op snapshot, drop the state.
pub fn abort(repo: &Repository) -> Result<(), Error> {
    let st = load_state(repo)?;
    repo.set_head(&st.branch)?;
    repo.reset(&repo.find_object(st.orig, None)?, ResetType::Hard, None)?;
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

/// Commit the merged `tree` for `step` and move HEAD onto it. Pick/reword add a
/// new commit on `current` (reword swaps the message); squash/fixup REPLACE
/// `current` with a commit on `current`'s parent, melding the message (squash
/// concatenates, fixup keeps `current`'s) — so the step folds into the
/// preceding one. Authorship follows git: the picked commit's for pick/reword,
/// the kept (earlier) commit's for squash/fixup.
fn land_step(repo: &Repository, st: &State, step: &Step, tree: &git2::Tree) -> Result<Oid, Error> {
    let current = repo.find_commit(st.current)?;
    let pick = repo.find_commit(step.commit)?;
    let pick_msg = || pick.message().unwrap_or("").to_string();
    // Commit detached (not via "HEAD"): squash/fixup parent on `current`'s
    // parent, which the "HEAD" path would reject (first-parent != HEAD). Move
    // HEAD afterwards; the worktree already matches the merged tree.
    if st.mode == Mode::Revert {
        // A revert always appends on `current` with a generated message; the
        // forward-action vocabulary (reword/squash/fixup) does not apply.
        let summary = pick.summary().unwrap_or("commit");
        let msg = format!("Revert \"{summary}\"\n\nThis reverts commit {}.\n", pick.id());
        let new = repo.commit(None, &pick.committer(), &pick.committer(), &msg, tree, &[&current])?;
        repo.set_head_detached(new)?;
        return Ok(new);
    }
    let new = match step.action {
        Action::Pick => {
            repo.commit(None, &pick.author(), &pick.committer(), &pick_msg(), tree, &[&current])?
        }
        Action::Reword => {
            let msg = step.message.clone().unwrap_or_else(pick_msg);
            repo.commit(None, &pick.author(), &pick.committer(), &msg, tree, &[&current])?
        }
        Action::Squash | Action::Fixup => {
            if st.current == st.onto {
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
            repo.commit(None, &current.author(), &current.committer(), &msg, tree, &[&parent])?
        }
        Action::Drop => unreachable!("drop is handled before land_step"),
    };
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
        save_state(repo, &st)?;
    }
    finish(repo, &st)?;
    Ok(Outcome::Done { head: st.current })
}

/// Land the rebased history: move the branch ref to the new tip, reattach
/// HEAD, clean the worktree, drop the state.
fn finish(repo: &Repository, st: &State) -> Result<(), Error> {
    repo.reference(&st.branch, st.current, true, "rebase (mime sequencer)")?;
    repo.set_head(&st.branch)?;
    repo.reset(&repo.find_object(st.current, None)?, ResetType::Hard, None)?;
    let _ = repo.cleanup_state();
    let _ = std::fs::remove_file(state_path(repo));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use git2::Signature;

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
        let pc: Vec<_> = parents.iter().map(|o| repo.find_commit(*o).unwrap()).collect();
        let pr: Vec<&git2::Commit> = pc.iter().collect();
        repo.commit(None, &sig, &sig, msg, &tree, &pr).unwrap()
    }

    /// Point `name` (and HEAD + worktree) at `tip`.
    fn on_branch(repo: &Repository, name: &str, tip: Oid) {
        repo.branch(name, &repo.find_commit(tip).unwrap(), true).unwrap();
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
        let f2 = commit(&repo, &[f1], &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")], "add c");
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
        let mut b = Buffer::from_string("a", &read(&repo, "a"));
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
        let f2 = commit(&repo, &[f1], &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")], "add c");
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
        assert!(msg.contains("add b") && msg.contains("add c"), "melded message: {msg}");
    }

    #[test]
    fn fixup_keeps_the_first_message() {
        let dir = tmp("fixup");
        let repo = Repository::init(&dir).unwrap();
        let base = commit(&repo, &[], &[("a", "1\n")], "base");
        let f1 = commit(&repo, &[base], &[("a", "1\n"), ("b", "1\n")], "keep me");
        let f2 = commit(&repo, &[f1], &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")], "discard me");
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
}
