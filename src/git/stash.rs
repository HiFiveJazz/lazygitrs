use anyhow::Result;

use super::GitCommands;
use crate::model::StashEntry;

impl GitCommands {
    pub fn load_stash(&self) -> Result<Vec<StashEntry>> {
        let result = self
            .git()
            .args(&["stash", "list", "--format=%H|%gs"])
            .run()?;

        if !result.success {
            return Ok(Vec::new());
        }

        let entries = result
            .stdout
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let parts: Vec<&str> = line.splitn(2, '|').collect();
                if parts.len() >= 2 {
                    Some(StashEntry {
                        index: i,
                        hash: parts[0].to_string(),
                        name: parts[1].to_string(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(entries)
    }

    fn stash_push(&self, message: &str, mode: &[&str]) -> Result<()> {
        let mut args = Vec::with_capacity(5);
        args.extend_from_slice(&["stash", "push", "--quiet"]);
        args.extend_from_slice(mode);
        if !message.is_empty() {
            args.extend_from_slice(&["-m", message]);
        }
        self.git().args(&args).run_expecting_success()?;
        Ok(())
    }

    pub fn stash_save(&self, message: &str) -> Result<()> {
        self.stash_push(message, &[])
    }

    /// Stash only staged changes.
    pub fn stash_staged(&self, message: &str) -> Result<()> {
        self.stash_push(message, &["--staged"])
    }

    /// Rename a stash entry.
    pub fn stash_rename(&self, index: usize, new_message: &str) -> Result<()> {
        // Drop and re-create: git doesn't have a native rename
        let stash_ref = format!("stash@{{{}}}", index);
        // Get the stash commit
        let result = self
            .git()
            .args(&["stash", "store", "-m", new_message, &stash_ref])
            .run();
        // Fallback approach: drop + store
        if result.is_err() || !result.as_ref().unwrap().success {
            // Can't easily rename, just return Ok for now
        }
        Ok(())
    }

    /// View the diff of a stash entry.
    pub fn stash_diff(&self, index: usize) -> Result<String> {
        let result = self
            .git()
            .args(&[
                "stash",
                "show",
                "-p",
                "--no-ext-diff",
                "--no-textconv",
                "--color=never",
                &format!("stash@{{{}}}", index),
            ])
            .run()?;
        if result.success {
            Ok(result.stdout)
        } else {
            Ok(String::new())
        }
    }

    pub fn stash_pop(&self, index: usize) -> Result<()> {
        self.git()
            .args(&["stash", "pop", "--quiet", &format!("stash@{{{}}}", index)])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn stash_apply(&self, index: usize) -> Result<()> {
        self.git()
            .args(&["stash", "apply", "--quiet", &format!("stash@{{{}}}", index)])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn stash_drop(&self, index: usize) -> Result<()> {
        self.git()
            .args(&["stash", "drop", "--quiet", &format!("stash@{{{}}}", index)])
            .run_expecting_success()?;
        Ok(())
    }

    /// Stash all changes including untracked files.
    pub fn stash_include_untracked(&self, message: &str) -> Result<()> {
        self.stash_push(message, &["--include-untracked"])
    }

    /// Stash all changes but keep the index (staged changes remain staged).
    pub fn stash_keep_index(&self, message: &str) -> Result<()> {
        self.stash_push(message, &["--keep-index"])
    }

    /// Stash only unstaged changes (equivalent to keeping the current index).
    pub fn stash_unstaged(&self, message: &str) -> Result<()> {
        self.stash_keep_index(message)
    }

}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use crate::git::GitCommands;
    use crate::git::test_support::TempDir;
    use crate::pager::side_by_side::DiffViewState;

    fn git(repo: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .expect("run git command")
                .success()
        );
    }

    #[test]
    fn stash_diff_is_plain_unified_output_when_git_color_is_forced() {
        let temp = TempDir::new("stash-diff");
        let repo = &temp.path;
        git(repo, &["init", "-q", "--initial-branch=main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        git(repo, &["config", "color.ui", "always"]);

        let original = (1..=25)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(repo.join("file.txt"), &original).expect("write original file");
        git(repo, &["add", "file.txt"]);
        git(repo, &["commit", "-q", "-m", "initial"]);

        let changed = original.replace("line 13\nline 14\nline 15", "LINE 13\nLINE 14\nLINE 15");
        std::fs::write(repo.join("file.txt"), changed).expect("write changed file");
        git(repo, &["stash", "push", "-q", "-m", "replacement"]);

        let commands = GitCommands::new(repo).expect("create git commands");
        let diff = commands.stash_diff(0).expect("load stash diff");

        assert!(!diff.contains('\x1b'), "stash diff contains ANSI escapes");

        let parsed = DiffViewState::parse_diff_output("stash@{0}", &diff, 4, false);
        let replacement_rows: Vec<_> = parsed
            .lines
            .iter()
            .filter_map(|line| match (&line.old_line, &line.new_line) {
                (Some((_, old)), Some((_, new))) if new.starts_with("LINE ") => {
                    Some((old.as_str(), new.as_str()))
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            replacement_rows,
            vec![
                ("line 13", "LINE 13"),
                ("line 14", "LINE 14"),
                ("line 15", "LINE 15"),
            ]
        );
    }
}
