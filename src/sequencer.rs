//! Git sequencer — drives rebase / cherry-pick / revert as a sequence of
//! pick → 3-way-merge → commit steps, entirely in-process via `git2`
//! (libgit2). See todo.org "Rebase / cherry-pick driver".
//!
//! The discriminating capability the whole feature rests on is a 3-way merge
//! that writes conflict markers into the worktree, so a conflicted step routes
//! straight into the existing conflict vocabulary ([`crate::conflict`]) — no
//! new resolution surface. This module establishes that capability first; the
//! state machine (`.git/mime-sequencer`) and the `git_*` MCP tools build on it.
//!
//! Network and arbitrary-code channels are unused by construction: we never
//! touch remotes/transports, never run hooks or filters, and confine repos to
//! `MIME_ROOTS` at the tool boundary (see todo.org for the security checklist).

#[cfg(test)]
mod tests {
    use crate::buffer::Buffer;
    use git2::{Oid, Repository, Signature};

    /// Commit a single-file tree (built directly, no index/worktree round-trip)
    /// with explicit parents, updating no ref — so a test can lay out an
    /// arbitrary commit graph without HEAD getting in the way.
    fn mk_commit(repo: &Repository, file: &str, contents: &str, parents: &[Oid]) -> Oid {
        let blob = repo.blob(contents.as_bytes()).unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert(file, blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let sig = Signature::now("test", "test@example.invalid").unwrap();
        let parent_commits: Vec<_> = parents.iter().map(|o| repo.find_commit(*o).unwrap()).collect();
        let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();
        repo.commit(None, &sig, &sig, "msg", &tree, &parent_refs).unwrap()
    }

    /// The de-risking gate from todo.org's first implementation step: a git2
    /// cherry-pick whose step conflicts must produce diff3 conflict markers in
    /// the worktree that `crate::conflict`'s scanner parses unchanged. If this
    /// holds, the rest of the sequencer is plumbing.
    #[test]
    fn cherrypick_conflict_writes_diff3_markers_that_conflict_rs_parses() {
        let dir = std::env::temp_dir().join(format!("mime-seq-cp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repository::init(&dir).unwrap();

        // A criss-cross: both `ours` and `theirs` change base's one line, so
        // cherry-picking `theirs` onto `ours` conflicts on that line.
        let base = mk_commit(&repo, "f.txt", "base line\n", &[]);
        let ours = mk_commit(&repo, "f.txt", "our line\n", &[base]);
        let theirs = mk_commit(&repo, "f.txt", "their line\n", &[base]);

        let ours_c = repo.find_commit(ours).unwrap();
        let theirs_c = repo.find_commit(theirs).unwrap();
        let mut index = repo.cherrypick_commit(&theirs_c, &ours_c, 0, None).unwrap();
        assert!(index.has_conflicts(), "the step must conflict");

        // Write the conflicted index to the worktree with diff3 markers.
        let mut co = git2::build::CheckoutBuilder::new();
        co.force().conflict_style_diff3(true);
        repo.checkout_index(Some(&mut index), Some(&mut co)).unwrap();

        let text = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        let mut b = Buffer::from_string("f.txt", &text);
        let hunks = crate::conflict::scan(&mut b);

        assert_eq!(hunks.len(), 1, "one hunk; worktree was:\n{text}");
        let h = &hunks[0];
        assert!(h.base.is_some(), "diff3 base section present:\n{text}");
        assert_eq!(b.substring(h.ours.0, h.ours.1), "our line\n");
        assert_eq!(b.substring(h.theirs.0, h.theirs.1), "their line\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
