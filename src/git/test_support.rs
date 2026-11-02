//! Shared filesystem fixtures for git integration tests.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct TempDir {
    pub(crate) path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(prefix: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lazygitrs-{prefix}-{unique}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
