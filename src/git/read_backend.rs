use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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

#[derive(Clone, Debug)]
pub(crate) struct RemoteBranchRead {
    pub name: String,
    pub remote_name: String,
    pub hash: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteRead {
    pub name: String,
    pub urls: Vec<String>,
    pub branches: Vec<RemoteBranchRead>,
}

#[derive(Clone, Debug)]
pub(crate) struct TagRead {
    pub name: String,
    pub hash: String,
    pub message: String,
    pub creation_time: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct ReflogRead {
    pub hash: String,
    pub name: String,
    pub author_name: String,
    pub author_email: String,
    pub unix_timestamp: i64,
    pub parents: Vec<String>,
}

/// Paths discovered while opening a repository with gix.
pub(crate) struct DiscoveredRepository {
    pub read_repo: ReadRepository,
    pub worktree_path: PathBuf,
    pub repo_path: PathBuf,
}

/// Read-only gix state with a short idle lifetime.
///
/// Mutating/network operations intentionally remain on the system Git CLI.
/// During a burst of model reads we keep one gix repository alive so HEAD,
/// refs, branches, tags, remotes, and reflog queries can reuse repository/ODB
/// setup and a small decoded-object cache. After a short idle window the
/// repository is cold-reloaded, releasing pack/MIDX mappings instead of
/// charging them to lazygitrs' idle RSS for the lifetime of the application.
pub struct ReadRepository {
    inner: Arc<ReadRepositoryInner>,
}

const GIX_BURST_IDLE: Duration = Duration::from_millis(75);
const GIX_BURST_OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

struct ReadRepositoryInner {
    path: PathBuf,
    state: Mutex<ReadRepositoryState>,
    wake: Condvar,
    shutdown: AtomicBool,
}

struct ReadRepositoryState {
    repo: Option<gix::Repository>,
    last_used: Option<Instant>,
    generation: u64,
}

struct ReadRepositoryHandle<'a> {
    state: MutexGuard<'a, ReadRepositoryState>,
    wake: &'a Condvar,
}

impl Deref for ReadRepositoryHandle<'_> {
    type Target = gix::Repository;

    fn deref(&self) -> &Self::Target {
        self.state
            .repo
            .as_ref()
            .expect("gix burst repository missing while leased")
    }
}

impl Drop for ReadRepositoryHandle<'_> {
    fn drop(&mut self) {
        self.state.last_used = Some(Instant::now());
        self.state.generation = self.state.generation.wrapping_add(1);
        self.wake.notify_one();
    }
}

impl Drop for ReadRepository {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.wake.notify_one();
    }
}

impl ReadRepository {
    fn configure_burst_repo(repo: &mut gix::Repository) {
        // A small cache is enough to reuse recently decoded commits while a
        // refresh is active without recreating the old 32 MiB steady-state
        // cache. The freelist is disabled so large temporary buffers are not
        // retained when the burst is cold-reloaded after inactivity.
        repo.object_cache_size(Some(GIX_BURST_OBJECT_CACHE_BYTES));
        repo.set_freelist(None);
    }

    fn from_repo(mut repo: gix::Repository) -> Self {
        let path = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.common_dir().to_path_buf());
        let path = path.canonicalize().unwrap_or(path);
        Self::configure_burst_repo(&mut repo);

        let inner = Arc::new(ReadRepositoryInner {
            path,
            state: Mutex::new(ReadRepositoryState {
                repo: Some(repo),
                last_used: Some(Instant::now()),
                generation: 0,
            }),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        Self::spawn_idle_reaper(&inner);
        Self { inner }
    }

    fn spawn_idle_reaper(inner: &Arc<ReadRepositoryInner>) {
        let weak = Arc::downgrade(inner);
        std::thread::Builder::new()
            .name("lazygitrs-gix-idle".to_string())
            .spawn(move || {
                loop {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let mut state = inner
                        .state
                        .lock()
                        .expect("gix burst repository lock poisoned");

                    while state.last_used.is_none() && !inner.shutdown.load(Ordering::Acquire) {
                        state = inner
                            .wake
                            .wait(state)
                            .expect("gix burst repository lock poisoned");
                    }
                    if inner.shutdown.load(Ordering::Acquire) {
                        return;
                    }

                    let generation = state.generation;
                    let last_used = state.last_used.unwrap_or_else(Instant::now);
                    let remaining = GIX_BURST_IDLE.saturating_sub(last_used.elapsed());
                    let (mut state, timeout) = inner
                        .wake
                        .wait_timeout(state, remaining)
                        .expect("gix burst repository lock poisoned");

                    if inner.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if !timeout.timed_out() {
                        continue;
                    }

                    let idle_long_enough = state
                        .last_used
                        .is_some_and(|last_used| last_used.elapsed() >= GIX_BURST_IDLE);
                    if state.generation == generation && idle_long_enough {
                        // `reload()` keeps the lightweight repository handle and
                        // open options available while dropping caches and ODB
                        // system resources (including pack/MIDX mappings). If
                        // reopening fails, discard the handle and let the next
                        // query recreate it from the stored path.
                        let reload_ok = if let Some(repo) = state.repo.as_mut() {
                            if repo.reload().is_ok() {
                                Self::configure_burst_repo(repo);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if !reload_ok {
                            state.repo.take();
                        }
                        state.last_used = None;
                    }
                }
            })
            .expect("failed to spawn gix idle reaper");
    }

    fn open_for_read(&self) -> Result<ReadRepositoryHandle<'_>> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("gix burst repository lock poisoned");

        if state.repo.is_none() {
            let mut repo = gix::open(&self.inner.path).with_context(|| {
                format!("failed to open {} with gix", self.inner.path.display())
            })?;
            Self::configure_burst_repo(&mut repo);
            state.repo = Some(repo);
            state.last_used = Some(Instant::now());
        }

        Ok(ReadRepositoryHandle {
            state,
            wake: &self.inner.wake,
        })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path)
            .with_context(|| format!("failed to open {} with gix", path.display()))?;
        Ok(Self::from_repo(repo))
    }

    /// Refresh configuration/refs after a Git CLI mutation while returning the
    /// repository to its cold, low-memory state.
    pub fn reload(&self) -> Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("gix burst repository lock poisoned");
        let result: Result<()> = if let Some(repo) = state.repo.as_mut() {
            repo.reload()
                .map(|repo| Self::configure_burst_repo(repo))
                .map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        if result.is_err() {
            state.repo.take();
        }
        state.last_used = None;
        state.generation = state.generation.wrapping_add(1);
        self.inner.wake.notify_one();
        result
    }

    /// Discover and open a repository in one pass so startup doesn't need two
    /// `git rev-parse` subprocesses before creating the burst gix handle.
    pub fn discover(path: &Path) -> Result<DiscoveredRepository> {
        let repo = gix::discover(path)
            .with_context(|| format!("failed to discover {} with gix", path.display()))?;

        let fallback = path.canonicalize()?;
        let worktree_path = repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| fallback.clone())
            .canonicalize()
            .unwrap_or_else(|_| fallback.clone());

        // Linked worktrees may expose the common dir with lexical `..`
        // components. Normalize it before deriving the main repository path.
        let common_dir = repo
            .common_dir()
            .canonicalize()
            .unwrap_or_else(|_| repo.common_dir().to_path_buf());

        let repo_path = if repo.workdir().is_none() {
            // For a bare repository, the common dir is the repository itself.
            common_dir
        } else if matches!(repo.kind(), gix::repository::Kind::Submodule) {
            worktree_path.clone()
        } else {
            common_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| worktree_path.clone())
        };

        Ok(DiscoveredRepository {
            read_repo: Self::from_repo(repo),
            worktree_path,
            repo_path,
        })
    }

    pub fn head_state(&self) -> Result<HeadState> {
        let repo = self.open_for_read()?;
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
        let repo = self.open_for_read()?;
        let id = hash
            .parse::<gix::ObjectId>()
            .with_context(|| format!("invalid commit id {hash}"))?;
        let commit = repo
            .find_commit(id)
            .with_context(|| format!("failed to find commit {hash}"))?;
        Ok(commit.parent_ids().next().map(|id| id.to_string()))
    }

    pub fn ref_snapshot(&self) -> Result<RefSnapshot> {
        let repo = self.open_for_read()?;
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

    pub fn unpushed_commit_hashes(&self) -> Result<Vec<String>> {
        let repo = self.open_for_read()?;
        let mut head_ref = match repo.head_ref()? {
            Some(reference) => reference,
            None => return Ok(Vec::new()),
        };
        let head_id = head_ref.peel_to_commit()?.id;
        let Some(upstream_name) = head_ref
            .remote_tracking_ref_name(gix::remote::Direction::Fetch)
            .transpose()?
        else {
            return Ok(Vec::new());
        };
        let Some(mut upstream_ref) = repo.try_find_reference(upstream_name.as_ref())? else {
            return Ok(Vec::new());
        };
        let upstream_id = upstream_ref.peel_to_commit()?.id;

        let walk = repo.rev_walk([head_id]).with_hidden([upstream_id]).all()?;
        let mut hashes = Vec::new();
        for info in walk {
            hashes.push(info?.id.to_string());
        }
        Ok(hashes)
    }

    pub fn remotes(&self) -> Result<Vec<RemoteRead>> {
        let repo = self.open_for_read()?;
        let names: Vec<String> = repo
            .remote_names()
            .into_iter()
            .map(|name| name.to_string())
            .collect();
        let known: HashSet<&str> = names.iter().map(String::as_str).collect();
        let mut branches_by_remote: HashMap<String, Vec<RemoteBranchRead>> = HashMap::new();

        for reference in repo.references()?.remote_branches()?.peeled()? {
            let mut reference = reference?;
            let short = reference.name().shorten().to_string();
            let Some((remote_name, branch_name)) = names
                .iter()
                .filter_map(|name| {
                    short
                        .strip_prefix(name)
                        .and_then(|rest| rest.strip_prefix('/'))
                        .map(|branch| (name.as_str(), branch))
                })
                .max_by_key(|(name, _)| name.len())
            else {
                continue;
            };
            if !known.contains(remote_name) || branch_name == "HEAD" || branch_name.is_empty() {
                continue;
            }
            let commit = match reference.peel_to_commit() {
                Ok(commit) => commit,
                Err(_) => continue,
            };
            branches_by_remote
                .entry(remote_name.to_string())
                .or_default()
                .push(RemoteBranchRead {
                    name: branch_name.to_string(),
                    remote_name: remote_name.to_string(),
                    hash: commit.short_id()?.to_string(),
                });
        }

        let mut remotes = Vec::new();
        for name in names {
            let remote = repo.find_remote(name.as_str())?;
            let mut urls = Vec::new();
            for direction in [gix::remote::Direction::Fetch, gix::remote::Direction::Push] {
                for url in remote.urls(direction) {
                    let value = url.to_bstring().to_string();
                    if !urls.contains(&value) {
                        urls.push(value);
                    }
                }
            }
            let mut branches = branches_by_remote.remove(&name).unwrap_or_default();
            branches.sort_by(|a, b| a.name.cmp(&b.name));
            remotes.push(RemoteRead {
                name,
                urls,
                branches,
            });
        }
        Ok(remotes)
    }

    pub fn origin_url(&self) -> Result<Option<String>> {
        let repo = self.open_for_read()?;
        let Some(remote) = repo.try_find_remote("origin") else {
            return Ok(None);
        };
        let remote = remote?;
        Ok(remote
            .url(gix::remote::Direction::Fetch)
            .map(|url| url.to_bstring().to_string()))
    }

    pub fn tags(&self) -> Result<Vec<TagRead>> {
        let repo = self.open_for_read()?;

        // Collect names first so the packed-refs iterator can release its
        // buffer before we follow/peel individual refs. This also lets us
        // handle the unusual-but-valid case of a symbolic ref under tags/.
        let tag_refs = {
            let platform = repo.references()?;
            let mut refs = Vec::new();
            for reference in platform.tags()? {
                let reference = reference?;
                refs.push((
                    reference.name().to_string(),
                    reference.name().shorten().to_string(),
                ));
            }
            refs
        };

        let mut tags = Vec::new();
        for (full_name, name) in tag_refs {
            let Some(mut reference) = repo.try_find_reference(full_name.as_str())? else {
                continue;
            };
            while let Some(next) = reference.follow() {
                reference = next?;
            }
            let Some(original_id) = reference.try_id().map(|id| id.detach()) else {
                continue;
            };
            let commit = match reference.peel_to_commit() {
                Ok(commit) => commit,
                Err(_) => continue,
            };

            let (message, creation_time) = match repo.find_tag(original_id) {
                Ok(tag) => {
                    let creation_time = tag
                        .tagger()?
                        .and_then(|tagger| tagger.time().ok())
                        .map(|time| time.seconds)
                        .unwrap_or_else(|| commit.time().map(|t| t.seconds).unwrap_or(0));
                    let decoded = tag.decode()?;
                    let message = decoded.message.to_string();
                    let subject = message.lines().next().unwrap_or_default().to_string();
                    (subject, creation_time)
                }
                Err(_) => (
                    commit.message()?.summary().to_string(),
                    commit.time()?.seconds,
                ),
            };

            tags.push(TagRead {
                name,
                hash: commit.short_id()?.to_string(),
                message,
                creation_time,
            });
        }

        tags.sort_by(|a, b| b.creation_time.cmp(&a.creation_time));
        Ok(tags)
    }

    pub fn reflog(&self, limit: usize) -> Result<Vec<ReflogRead>> {
        let repo = self.open_for_read()?;
        let head = repo.head()?;

        let mut log_iter = head.log_iter();
        let Some(iter) = log_iter.all()? else {
            return Ok(Vec::new());
        };
        // Reflog iteration is oldest-first and yields borrowed lines. Own them
        // before reversing so no entry can outlive the iterator's read buffer.
        let mut lines = iter
            .filter_map(Result::ok)
            .map(|line| line.to_owned())
            .collect::<Vec<_>>();
        lines.reverse();

        let mut out = Vec::new();
        for line in lines.into_iter().take(limit) {
            let id = line.new_oid;
            if id.is_null() {
                continue;
            }
            let commit = match repo.find_commit(id) {
                Ok(commit) => commit,
                Err(_) => continue,
            };
            let author = commit.author()?;
            let unix_timestamp = author.time()?.seconds;
            out.push(ReflogRead {
                hash: id.to_string(),
                name: line.message.to_string(),
                author_name: author.name.to_string(),
                author_email: author.email.to_string(),
                unix_timestamp,
                parents: commit.parent_ids().map(|id| id.to_string()).collect(),
            });
        }
        Ok(out)
    }
}
