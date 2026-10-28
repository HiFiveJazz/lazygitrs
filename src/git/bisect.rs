use anyhow::Result;

use super::GitCommands;

impl GitCommands {
    /// Start a bisect session.
    pub fn bisect_start(&self) -> Result<()> {
        self.git()
            .args(&["bisect", "start"])
            .run_expecting_success()?;
        Ok(())
    }

    /// Mark the current (or given) commit as good.
    pub fn bisect_good(&self, hash: &str) -> Result<String> {
        let result = if hash.is_empty() {
            self.git()
                .args(&["bisect", "good"])
                .run_expecting_success()?
        } else {
            self.git()
                .args(&["bisect", "good", hash])
                .run_expecting_success()?
        };
        Ok(result.stdout)
    }

    /// Mark the current (or given) commit as bad.
    pub fn bisect_bad(&self, hash: &str) -> Result<String> {
        let result = if hash.is_empty() {
            self.git()
                .args(&["bisect", "bad"])
                .run_expecting_success()?
        } else {
            self.git()
                .args(&["bisect", "bad", hash])
                .run_expecting_success()?
        };
        Ok(result.stdout)
    }

    /// Skip the current commit during bisect.
    pub fn bisect_skip(&self, hash: &str) -> Result<String> {
        let result = if hash.is_empty() {
            self.git()
                .args(&["bisect", "skip"])
                .run_expecting_success()?
        } else {
            self.git()
                .args(&["bisect", "skip", hash])
                .run_expecting_success()?
        };
        Ok(result.stdout)
    }

    /// End the bisect session.
    pub fn bisect_reset(&self) -> Result<()> {
        self.git()
            .args(&["bisect", "reset"])
            .run_expecting_success()?;
        Ok(())
    }

    /// Check if currently in a bisect session.
    pub fn is_bisecting(&self) -> bool {
        self.repo_path().join(".git/BISECT_LOG").exists()
    }
}
