use std::path::Path;

use anyhow::{Result, bail};

use super::GitCommands;
use crate::os::cmd::CmdBuilder;
use crate::model::Worktree;

impl GitCommands {
    pub fn load_worktrees(&self) -> Result<Vec<Worktree>> {
        let result = self
            .git()
            .args(&["worktree", "list", "--porcelain"])
            .run()?;
        if !result.success {
            return Ok(Vec::new());
        }

        let mut worktrees = Vec::new();
        let mut path = String::new();
        let mut branch = String::new();
        let mut hash = String::new();
        let mut is_bare = false;

        for line in result.stdout.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                if !path.is_empty() && !is_bare {
                    worktrees.push(Worktree {
                        path: path.clone(),
                        branch: branch.clone(),
                        hash: hash.clone(),
                        is_current: false,
                        is_main: worktrees.is_empty(),
                    });
                }
                path = p.to_string();
                branch.clear();
                hash.clear();
                is_bare = false;
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                hash = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
            } else if line == "bare" {
                is_bare = true;
            } else if line == "detached" {
                branch = "(detached)".to_string();
            }
        }

        // Push the last one
        if !path.is_empty() && !is_bare {
            worktrees.push(Worktree {
                path: path.clone(),
                branch: branch.clone(),
                hash: hash.clone(),
                is_current: false,
                is_main: worktrees.is_empty(),
            });
        }

        // Mark the current worktree
        let repo_path = self.repo_path().to_string_lossy().to_string();
        for wt in &mut worktrees {
            if wt.path == repo_path {
                wt.is_current = true;
            }
        }

        Ok(worktrees)
    }

    /// Return the linked worktree currently checking out `branch`, if any.
    ///
    /// The current worktree is intentionally excluded: callers already reject
    /// deletion of the current branch separately.
    pub fn linked_worktree_for_branch(&self, branch: &str) -> Result<Option<Worktree>> {
        Ok(self
            .load_worktrees()?
            .into_iter()
            .find(|worktree| !worktree.is_current && worktree.branch == branch))
    }

    /// Refuse destructive worktree removal when the linked worktree contains
    /// tracked or untracked changes.  This deliberately does not use
    /// `git worktree remove --force`; preserving Git's safety boundary is more
    /// important than making branch deletion succeed automatically.
    pub fn ensure_worktree_clean_for_removal(&self, path: &str) -> Result<()> {
        let result = CmdBuilder::git_no_optional_locks()
            .cwd_path(Path::new(path))
            .args(&["status", "--porcelain", "--untracked-files=normal"])
            .run_expecting_success()?;

        if !result.stdout.trim().is_empty() {
            bail!(
                "Worktree '{}' has local changes. Open that worktree and commit, stash, or discard them before removing it. Lazygitrs will not force-remove a dirty worktree.",
                path
            );
        }
        Ok(())
    }

    /// Remove a disposable linked worktree before deleting its branch. If its
    /// directory has already disappeared, prune the stale administrative entry
    /// instead. Existing worktrees are never removed with `--force`.
    pub fn remove_worktree_for_branch_deletion(&self, path: &str) -> Result<()> {
        if !Path::new(path).exists() {
            self.git()
                .args(&["worktree", "prune"])
                .run_expecting_success()?;
            return Ok(());
        }

        self.ensure_worktree_clean_for_removal(path)?;
        self.remove_worktree(path, false)
    }

    pub fn create_worktree(&self, path: &str, branch: &str) -> Result<()> {
        self.git()
            .args(&["worktree", "add", path, branch])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn create_worktree_new_branch(&self, path: &str, new_branch: &str) -> Result<()> {
        self.git()
            .args(&["worktree", "add", "-b", new_branch, path])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn remove_worktree(&self, path: &str, force: bool) -> Result<()> {
        let mut cmd = self.git();
        cmd = cmd.args(&["worktree", "remove", path]);
        if force {
            cmd = cmd.arg("--force");
        }
        cmd.run_expecting_success()?;
        Ok(())
    }
}
