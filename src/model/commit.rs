use std::fmt;

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub name: String,
    pub status: CommitStatus,
    pub tags: Vec<String>,
    /// Branch/ref decorations (e.g. "HEAD -> main", "origin/main").
    pub refs: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub unix_timestamp: i64,
    pub parents: Vec<String>,
    pub divergence: Divergence,
}

impl Commit {
    pub fn short_hash(&self) -> &str {
        if self.hash.len() >= 8 {
            &self.hash[..8]
        } else {
            &self.hash
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitStatus {
    #[default]
    Pushed,
    Unpushed,
    Reflog,
}

impl fmt::Display for CommitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pushed => write!(f, "pushed"),
            Self::Unpushed => write!(f, "unpushed"),
            Self::Reflog => write!(f, "reflog"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Divergence {
    #[default]
    None,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommitStat {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}
