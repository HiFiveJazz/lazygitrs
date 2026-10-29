use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};

use super::HeadState;

/// A stable snapshot of the refs that can affect the global `--all` history.
///
/// Pseudo refs such as FETCH_HEAD are intentionally excluded: they are not part
/// of `git log --all` and changing them must not force an expensive history
/// rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefSnapshot {
    pub head: Option<String>,
    pub refs: Vec<(String, String)>,
}

/// Persistent gix repository state used for hot read-only queries.
///
/// Mutating/network operations intentionally remain on the system Git CLI. The
/// repository handle is kept alive so its object cache survives across UI
/// actions. gix metadata reads are short, so a mutex is preferable to creating
/// a fresh thread-local handle (and fresh cache) for every query.
pub struct ReadRepository {
    repo: Mutex<gix::Repository>,
}

impl ReadRepository {
    pub fn open(path: &Path) -> Result<Self> {
        let mut repo = gix::open(path)
            .with_context(|| format!("failed to open {} with gix", path.display()))?;
        // Recent commit objects are repeatedly touched by commit/amend/refresh
        // paths. The cache grows lazily up to this cap.
        repo.object_cache_size_if_unset(32 * 1024 * 1024);
        Ok(Self {
            repo: Mutex::new(repo),
        })
    }

    pub fn head_state(&self) -> Result<HeadState> {
        let repo = self.repo.lock().expect("gix read repository poisoned");
        let head = repo.head()?;
        let hash = head
            .id()
            .map(|id| id.to_string())
            .ok_or_else(|| anyhow::anyhow!("HEAD does not point to a commit"))?;
        let branch_name = head
            .referent_name()
            .map(|name| name.shorten().to_string())
            .unwrap_or_default();

        Ok(HeadState { hash, branch_name })
    }

    pub fn first_parent_hash(&self, hash: &str) -> Result<Option<String>> {
        let repo = self.repo.lock().expect("gix read repository poisoned");
        let id = hash
            .parse::<gix::ObjectId>()
            .with_context(|| format!("invalid commit id {hash}"))?;
        let commit = repo
            .find_commit(id)
            .with_context(|| format!("failed to find commit {hash}"))?;
        Ok(commit.parent_ids().next().map(|id| id.to_string()))
    }

    pub fn ref_snapshot(&self) -> Result<RefSnapshot> {
        let repo = self.repo.lock().expect("gix read repository poisoned");
        let head = repo.head()?.id().map(|id| id.to_string());
        let mut refs = Vec::new();

        for reference in repo.references()?.all()? {
            let reference = reference?;
            let name = reference.name().to_string();

            // `git log --all` considers every ref under `refs/`, including
            // custom namespaces. Keep direct and symbolic targets so decoration
            // changes are detected as well as changes to reachable commits.
            let target = reference.target();
            let target = if let Some(id) = target.try_id() {
                format!("id:{id}")
            } else if let Some(name) = target.try_name() {
                format!("sym:{name}")
            } else {
                continue;
            };
            refs.push((name, target));
        }

        refs.sort_unstable();
        Ok(RefSnapshot { head, refs })
    }
}
