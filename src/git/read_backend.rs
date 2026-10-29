use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};

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
pub(crate) struct BranchRead {
    pub name: String,
    pub hash: String,
    pub commit_time: i64,
    pub ahead: usize,
    pub behind: usize,
    pub upstream: Option<String>,
    pub head: bool,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusRead {
    /// Current repository-relative path.
    pub path: String,
    /// Source path for a staged rename/copy.
    pub source_path: Option<String>,
    /// Porcelain-v1 compatible index/HEAD status column.
    pub index_status: char,
    /// Porcelain-v1 compatible worktree/index status column.
    pub worktree_status: char,
    pub tracked: bool,
}

/// Paths discovered while opening a repository with gix.
pub(crate) struct DiscoveredRepository {
    pub read_repo: ReadRepository,
    pub worktree_path: PathBuf,
    pub repo_path: PathBuf,
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
    fn from_repo(mut repo: gix::Repository) -> Self {
        // Recent commit objects are repeatedly touched by commit/amend/refresh
        // paths. The cache grows lazily up to this cap.
        repo.object_cache_size_if_unset(32 * 1024 * 1024);
        Self {
            repo: Mutex::new(repo),
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path)
            .with_context(|| format!("failed to open {} with gix", path.display()))?;
        Ok(Self::from_repo(repo))
    }

    /// Refresh configuration after a Git CLI mutation such as changing remotes
    /// or upstream tracking. These operations are rare, so dropping gix's
    /// caches here is preferable to serving stale config-derived metadata.
    pub fn reload(&self) -> Result<()> {
        let mut repo = self.repo.lock().expect("gix read repository poisoned");
        repo.reload()?;
        repo.object_cache_size_if_unset(32 * 1024 * 1024);
        Ok(())
    }

    /// Discover and open a repository in one pass so startup doesn't need two
    /// `git rev-parse` subprocesses before creating the persistent gix handle.
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

    pub fn branches(&self) -> Result<Vec<BranchRead>> {
        let repo = self.repo.lock().expect("gix read repository poisoned");
        let head_name = repo
            .head()?
            .referent_name()
            .map(|name| name.shorten().to_string());
        let mut branches = Vec::new();

        for reference in repo.references()?.local_branches()?.peeled()? {
            let mut reference = reference?;
            let name = reference.name().shorten().to_string();
            let commit = reference.peel_to_commit()?;
            let local_id = commit.id;
            let commit_time = commit.time()?.seconds;

            let mut upstream = None;
            let mut ahead = 0usize;
            let mut behind = 0usize;
            if let Some(upstream_name) = reference
                .remote_tracking_ref_name(gix::remote::Direction::Fetch)
                .transpose()?
            {
                upstream = Some(upstream_name.shorten().to_string());
                if let Some(mut upstream_ref) = repo.try_find_reference(upstream_name.as_ref())? {
                    if let Ok(upstream_commit) = upstream_ref.peel_to_commit() {
                        let upstream_id = upstream_commit.id;
                        ahead = rev_walk_count(&repo, local_id, Some(upstream_id))?;
                        behind = rev_walk_count(&repo, upstream_id, Some(local_id))?;
                    }
                }
            }

            branches.push(BranchRead {
                name: name.clone(),
                hash: commit.short_id()?.to_string(),
                commit_time,
                ahead,
                behind,
                upstream,
                head: head_name.as_deref() == Some(name.as_str()),
            });
        }

        branches.sort_by(|a, b| b.commit_time.cmp(&a.commit_time));
        if let Some(index) = branches.iter().position(|branch| branch.head) {
            let head = branches.remove(index);
            branches.insert(0, head);
        }
        Ok(branches)
    }

    pub fn unpushed_commit_hashes(&self) -> Result<Vec<String>> {
        let repo = self.repo.lock().expect("gix read repository poisoned");
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
        let repo = self.repo.lock().expect("gix read repository poisoned");
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
        let repo = self.repo.lock().expect("gix read repository poisoned");
        let Some(remote) = repo.try_find_remote("origin") else {
            return Ok(None);
        };
        let remote = remote?;
        Ok(remote
            .url(gix::remote::Direction::Fetch)
            .map(|url| url.to_bstring().to_string()))
    }

    pub fn tags(&self) -> Result<Vec<TagRead>> {
        let repo = self.repo.lock().expect("gix read repository poisoned");

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

    /// Return a porcelain-v1-compatible status snapshot using gix.
    ///
    /// Individual untracked files are requested to preserve the existing
    /// `git status --porcelain -uall` behavior.  Complex conflict states and
    /// worktree-side rewrite detection deliberately return an error so the
    /// caller can fall back to Git's porcelain parser rather than risk
    /// changing staging/discard semantics.
    pub fn status_untracked_all(&self) -> Result<Vec<StatusRead>> {
        use gix::bstr::ByteSlice;

        let repo = self.repo.lock().expect("gix read repository poisoned");
        let platform = repo
            .status(gix::progress::Discard)?
            .untracked_files(gix::status::UntrackedFiles::Files);
        let mut iter = platform.into_iter(None::<gix::bstr::BString>)?;

        let mut tracked: HashMap<String, StatusRead> = HashMap::new();
        let mut untracked = Vec::new();
        let mut untracked_paths = HashSet::new();
        let mut intent_to_add_paths = HashSet::new();

        for item in iter.by_ref() {
            match item? {
                gix::status::Item::TreeIndex(change) => {
                    use gix::diff::index::Change;

                    let (path, source_path, code) = match change {
                        Change::Addition { location, .. } => {
                            (location.as_ref().to_str_lossy().into_owned(), None, 'A')
                        }
                        Change::Deletion { location, .. } => {
                            (location.as_ref().to_str_lossy().into_owned(), None, 'D')
                        }
                        Change::Modification {
                            location,
                            previous_entry_mode,
                            entry_mode,
                            ..
                        } => (
                            location.as_ref().to_str_lossy().into_owned(),
                            None,
                            if previous_entry_mode == entry_mode {
                                'M'
                            } else {
                                'T'
                            },
                        ),
                        Change::Rewrite {
                            source_location,
                            location,
                            copy,
                            ..
                        } => (
                            location.as_ref().to_str_lossy().into_owned(),
                            Some(source_location.as_ref().to_str_lossy().into_owned()),
                            if copy { 'C' } else { 'R' },
                        ),
                    };

                    if untracked_paths.contains(&path) {
                        return Err(anyhow!(
                            "gix tracked/untracked collision for {path} requires porcelain fallback"
                        ));
                    }
                    let effective_code = if code == 'A' && intent_to_add_paths.contains(&path) {
                        ' '
                    } else {
                        code
                    };
                    let status = tracked.entry(path.clone()).or_insert_with(|| StatusRead {
                        path,
                        source_path: None,
                        index_status: ' ',
                        worktree_status: ' ',
                        tracked: true,
                    });
                    if effective_code != ' ' {
                        if status.index_status != ' ' && status.index_status != effective_code {
                            return Err(anyhow!(
                                "gix produced multiple staged status codes for {}",
                                status.path
                            ));
                        }
                        status.index_status = effective_code;
                        status.source_path = source_path;
                    }
                }
                gix::status::Item::IndexWorktree(change) => {
                    use gix::status::index_worktree::Item;
                    use gix::status::plumbing::index_as_worktree::{
                        Change as WorktreeChange, EntryStatus,
                    };

                    match change {
                        Item::Modification {
                            rela_path, status, ..
                        } => {
                            let path = rela_path.to_str_lossy().into_owned();
                            let intent_to_add = matches!(&status, EntryStatus::IntentToAdd);
                            let code = match status {
                                EntryStatus::Conflict { .. } => {
                                    return Err(anyhow!(
                                        "gix conflict status for {path} requires porcelain fallback"
                                    ));
                                }
                                EntryStatus::Change(WorktreeChange::Removed) => 'D',
                                EntryStatus::Change(WorktreeChange::Type { .. }) => 'T',
                                EntryStatus::Change(
                                    WorktreeChange::Modification { .. }
                                    | WorktreeChange::SubmoduleModification(_),
                                ) => 'M',
                                EntryStatus::NeedsUpdate(_) => continue,
                                // Git porcelain intentionally suppresses the
                                // staged-add column for intent-to-add entries.
                                EntryStatus::IntentToAdd => 'A',
                            };

                            let entry = tracked.entry(path.clone()).or_insert_with(|| StatusRead {
                                path: path.clone(),
                                source_path: None,
                                index_status: ' ',
                                worktree_status: ' ',
                                tracked: true,
                            });
                            if intent_to_add {
                                intent_to_add_paths.insert(path.clone());
                                entry.index_status = ' ';
                            }
                            entry.worktree_status = code;
                        }
                        Item::DirectoryContents { entry, .. } => {
                            if entry.status != gix::dir::entry::Status::Untracked {
                                continue;
                            }
                            let path = entry.rela_path.to_str_lossy().into_owned();
                            // A staged deletion followed by a new untracked
                            // file at the same path is represented by two
                            // porcelain records. Keep that rare case exact by
                            // falling back instead of merging the records.
                            if tracked.contains_key(&path) {
                                return Err(anyhow!(
                                    "gix tracked/untracked collision for {path} requires porcelain fallback"
                                ));
                            }
                            untracked_paths.insert(path.clone());
                            untracked.push(StatusRead {
                                path,
                                source_path: None,
                                index_status: '?',
                                worktree_status: '?',
                                tracked: false,
                            });
                        }
                        Item::Rewrite { .. } => {
                            return Err(anyhow!(
                                "gix worktree rewrite status requires porcelain fallback"
                            ));
                        }
                    }
                }
            }
        }

        // Like `git status`, opportunistically refresh cached index stat data.
        // gix documents these changes as safe bookkeeping that avoids paying
        // the same work on subsequent status calls. A failure to persist them
        // does not invalidate the status snapshot we already produced.
        if let Some(mut outcome) = iter.into_outcome()
            && let Some(write_result) = outcome.write_changes()
        {
            let _ = write_result;
        }

        let mut out: Vec<_> = tracked.into_values().collect();
        out.extend(untracked);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    pub fn reflog(&self, limit: usize) -> Result<Vec<ReflogRead>> {
        let repo = self.repo.lock().expect("gix read repository poisoned");
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

fn rev_walk_count(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    hidden: Option<gix::ObjectId>,
) -> Result<usize> {
    let platform = repo.rev_walk([tip]);
    let walk = match hidden {
        Some(hidden) => platform.with_hidden([hidden]).all()?,
        None => platform.all()?,
    };
    let mut count = 0usize;
    for item in walk {
        item?;
        count += 1;
    }
    Ok(count)
}
