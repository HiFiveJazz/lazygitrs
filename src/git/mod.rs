pub mod ai_commit;
pub mod bisect;
pub mod branch;
pub mod commit;
pub mod diff;
pub(crate) mod diff_paths;
pub mod email;
pub mod file;
pub mod loader;
mod read_backend;
pub mod rebase;
pub mod remote;
pub mod staging;
pub mod stash;
pub mod status;
pub mod submodule;
pub mod tag;
pub mod worktree;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use anyhow::Result;

use crate::model::{self, Model};
use crate::os::cmd::CmdBuilder;

use read_backend::{ReadRepository, RefSnapshot};

/// A single piece of model data loaded from git. Each variant arrives
/// independently so the UI can display whichever data is ready first.
pub enum ModelPart {
    Files(Vec<model::File>),
    /// Second-phase Files data. On startup this completes the early tracked-only
    /// list with untracked rows; on full loads it also carries per-file line/hunk
    /// counts after the status-only list is already interactive.
    FileStats(Vec<model::File>),
    Branches(Vec<model::Branch>),
    Commits(Vec<model::Commit>),
    /// Commit hashes currently ahead of the configured upstream. Commit rows are
    /// streamed first; this follow-up only decorates them as pushed/unpushed.
    UnpushedCommitHashes(Vec<String>),
    Stash(Vec<model::StashEntry>),
    Remotes(Vec<model::Remote>),
    Tags(Vec<model::Tag>),
    Worktrees(Vec<model::Worktree>),
    Submodules(Vec<submodule::Submodule>),
    Reflog(Vec<model::Commit>),
    DiffStats {
        added: usize,
        deleted: usize,
    },
    RepoStatus {
        is_rebasing: bool,
        is_merging: bool,
        is_cherry_picking: bool,
        is_bisecting: bool,
        rebase_onto_hash: String,
    },
    /// Current HEAD hash + branch name. Must be refreshed with the rest of the
    /// model so the commits graph filled-circle indicator tracks the tip.
    Head {
        hash: String,
        branch_name: String,
    },
    RepoUrl(String),
    Contributors(Vec<(String, usize)>),
}

/// Number of commits to load in the normal commits panel.
///
/// This mirrors lazygit's first-load guardrail: large repositories can have
/// tens of thousands of commits, but the sidebar only needs a recent window.
pub const DEFAULT_COMMIT_LIMIT: usize = 300;

/// Total number of `ModelPart` variants that `load_model_streaming` sends.
pub const MODEL_PART_COUNT: usize = 16;

/// Confirmed HEAD identity, read after a successful checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadState {
    pub hash: String,
    /// Empty when HEAD is detached.
    pub branch_name: String,
}

struct RepoPaths {
    worktree_path: PathBuf,
    repo_path: PathBuf,
}

#[inline]
fn trim_process_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        // SAFETY: no pointers are passed. This is only used when a checkout
        // refresh has been abandoned and its temporary allocations are dead.
        libc::malloc_trim(0);
    }
}

/// Facade for all git operations. Mirrors lazygit's GitCommand.
pub struct GitCommands {
    repo_path: PathBuf,
    repo_name: String,
    read_repo: Option<ReadRepository>,
    contributor_cache: Mutex<HashMap<(String, usize, usize), Vec<(String, usize)>>>,
    /// Generation for HEAD-relative worktree reads. Checkout bumps this before
    /// mutating the worktree so an older refresh can stop before launching
    /// follow-up diff scans against state that is already obsolete.
    worktree_read_generation: AtomicU64,
}

impl GitCommands {
    pub fn new(repo_path: &Path) -> Result<Self> {
        // Prefer gix discovery so startup doesn't need to spawn Git just to
        // validate the repository and resolve its worktree/common directory.
        // Keep the existing CLI path resolver as a compatibility fallback.
        let (paths, read_repo) = match ReadRepository::discover(repo_path) {
            Ok(discovered) => (
                RepoPaths {
                    worktree_path: discovered.worktree_path,
                    repo_path: discovered.repo_path,
                },
                Some(discovered.read_repo),
            ),
            Err(_) => {
                let paths = Self::resolve_repo_paths(repo_path)?;
                let read_repo = ReadRepository::open(&paths.worktree_path).ok();
                (paths, read_repo)
            }
        };
        let repo_name = paths
            .repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        Ok(Self {
            repo_path: paths.worktree_path,
            repo_name,
            read_repo,
            contributor_cache: Mutex::new(HashMap::new()),
            worktree_read_generation: AtomicU64::new(0),
        })
    }

    /// Invalidate in-flight HEAD-relative worktree reads before a checkout.
    ///
    /// We cannot cheaply interrupt a Git subprocess that is already running, but
    /// generation checks between phases prevent a stale status result from
    /// cascading into numstat/hunk/shortstat scans after checkout has started.
    pub fn cancel_worktree_reads(&self) {
        self.worktree_read_generation.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn worktree_read_generation(&self) -> u64 {
        self.worktree_read_generation.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn worktree_read_is_current(&self, generation: u64) -> bool {
        self.worktree_read_generation.load(Ordering::Relaxed) == generation
    }

    fn resolve_repo_paths(path: &Path) -> Result<RepoPaths> {
        let fallback = path.canonicalize()?;
        let result = CmdBuilder::git()
            .cwd_path(path)
            .args(&[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
                "--is-bare-repository",
                "--show-superproject-working-tree",
            ])
            .run()?;

        if !result.success {
            return Ok(RepoPaths {
                worktree_path: fallback.clone(),
                repo_path: fallback,
            });
        }

        let lines: Vec<&str> = result.stdout_trimmed().lines().collect();
        if lines.len() < 4 || lines[0].is_empty() {
            return Ok(RepoPaths {
                worktree_path: fallback.clone(),
                repo_path: fallback,
            });
        }

        let worktree_path = PathBuf::from(lines[0]).canonicalize()?;
        let repo_git_dir_path = PathBuf::from(lines[2]);
        let is_submodule = lines.get(4).is_some_and(|line| !line.is_empty());
        let repo_path = if is_submodule {
            worktree_path.clone()
        } else {
            repo_git_dir_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| worktree_path.clone())
        };

        Ok(RepoPaths {
            worktree_path,
            repo_path,
        })
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Snapshot refs that can affect the global commit model. This is an
    /// in-process gix read and is used to avoid unnecessary post-network
    /// history walks when fetch/pull changed no relevant refs.
    pub(crate) fn ref_snapshot(&self) -> Result<RefSnapshot> {
        if let Some(repo) = &self.read_repo
            && let Ok(snapshot) = repo.ref_snapshot()
        {
            return Ok(snapshot);
        }

        // Compatibility fallback. This is still far cheaper than rebuilding
        // the global topological history and is only used if gix cannot read
        // this repository.
        let head = self
            .git()
            .args(&["rev-parse", "-q", "--verify", "HEAD"])
            .run()?;
        let head = head
            .success
            .then(|| head.stdout_trimmed().to_string())
            .filter(|hash| !hash.is_empty());
        let mut refs = self
            .git()
            .args(&[
                "for-each-ref",
                "--format=%(refname)|%(objectname)|%(symref)",
                "refs/",
            ])
            .run_expecting_success()?
            .stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '|');
                let name = parts.next()?;
                let hash = parts.next()?;
                let symref = parts.next().unwrap_or_default();
                let target = if symref.is_empty() {
                    format!("id:{hash}")
                } else {
                    format!("sym:{symref}")
                };
                Some((name.to_string(), target))
            })
            .collect::<Vec<_>>();
        refs.sort_unstable();

        Ok(RefSnapshot { head, refs })
    }

    /// Read HEAD through the burst-reused gix repository, falling back to Git
    /// for compatibility with repository layouts gix cannot handle yet.
    pub(crate) fn read_head_state(&self) -> Result<HeadState> {
        if let Some(repo) = &self.read_repo
            && let Ok(head) = repo.head_state()
        {
            return Ok(head);
        }

        let result = self
            .git()
            .args(&["rev-parse", "HEAD", "--symbolic-full-name", "HEAD"])
            .run_expecting_success()?;
        let mut lines = result.stdout.lines();
        let hash = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("Git did not return a HEAD hash"))?;
        let reference = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("Git did not return a HEAD ref"))?;
        let branch_name = if reference == "HEAD" {
            ""
        } else {
            reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| anyhow::anyhow!("Unexpected HEAD ref: {reference}"))?
        };
        Ok(HeadState {
            hash: hash.to_string(),
            branch_name: branch_name.to_string(),
        })
    }

    /// Read a commit's first parent through gix with a Git CLI fallback.
    pub(crate) fn read_first_parent_hash(&self, hash: &str) -> Result<Option<String>> {
        if let Some(repo) = &self.read_repo
            && let Ok(parent) = repo.first_parent_hash(hash)
        {
            return Ok(parent);
        }

        let rev = format!("{hash}^1");
        let result = self.git().args(&["rev-parse", &rev]).run()?;
        if !result.success {
            return Ok(None);
        }
        let parent = result.stdout_trimmed().to_string();
        Ok((!parent.is_empty()).then_some(parent))
    }

    fn git(&self) -> CmdBuilder {
        CmdBuilder::git_no_optional_locks().cwd_path(&self.repo_path)
    }

    /// Public access to the git command builder.
    pub fn git_cmd(&self) -> CmdBuilder {
        CmdBuilder::git().cwd_path(&self.repo_path)
    }

    /// Stream commit rows before computing their pushed/unpushed decoration.
    ///
    /// On large repositories the upstream-only revision walk can take hundreds
    /// of milliseconds. It must not sit in front of the main commit-log query,
    /// because the UI can render useful commit rows immediately and apply the
    /// status decoration afterward. Sending both parts from the same worker
    /// preserves their order on the channel.
    fn stream_commits_then_unpushed(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        commit_filter: Option<crate::git::commit::CommitFilter>,
    ) {
        let tx = tx.clone();
        let git = Arc::clone(self);
        std::thread::spawn(move || {
            let commits = match commit_filter {
                Some(filter) => {
                    git.load_filtered_commits_page(&filter, DEFAULT_COMMIT_LIMIT, 0)
                }
                None => git.load_commits_page(DEFAULT_COMMIT_LIMIT, 0),
            };

            let Ok(commits) = commits else {
                return;
            };
            if tx.send(ModelPart::Commits(commits)).is_err() {
                return;
            }

            let unpushed = git.unpushed_commit_hashes().unwrap_or_default();
            let _ = tx.send(ModelPart::UnpushedCommitHashes(unpushed));
        });
    }

    /// Load all model data from the repository.
    ///
    /// Git commands are run in parallel using scoped threads since they are
    /// all independent reads against the same repo.
    pub fn load_model(&self) -> Result<Model> {
        let mut model = Model::default();

        model.repo_name = self.repo_name();
        model.head_hash = self.head_hash().unwrap_or_default();
        model.head_branch_name = self.current_branch_name().unwrap_or_default();

        // Run all independent git loads in parallel.
        std::thread::scope(|s| {
            let h_files = s.spawn(|| self.load_files());
            let h_branches = s.spawn(|| self.load_branches());
            let h_commits = s.spawn(|| self.load_commits(DEFAULT_COMMIT_LIMIT));
            let h_stash = s.spawn(|| self.load_stash());
            let h_remotes = s.spawn(|| self.load_remotes());
            let h_tags = s.spawn(|| self.load_tags());
            let h_worktrees = s.spawn(|| self.load_worktrees());
            let h_submodules = s.spawn(|| self.load_submodules());
            let h_reflog = s.spawn(|| self.load_reflog(100));
            let h_shortstat = s.spawn(|| self.diff_shortstat());
            let h_status = s.spawn(|| self.repo_status());
            let h_repo_url = s.spawn(|| self.load_repo_url());
            let h_contribs = s.spawn(|| self.load_contributors(500, 10));

            model.files = h_files.join().unwrap()?;
            model.branches = h_branches.join().unwrap()?;
            model.set_commits(h_commits.join().unwrap()?);
            model.stash_entries = h_stash.join().unwrap()?;
            model.remotes = h_remotes.join().unwrap()?;
            model.tags = h_tags.join().unwrap()?;
            model.worktrees = h_worktrees.join().unwrap().unwrap_or_default();
            model.submodules = h_submodules.join().unwrap().unwrap_or_default();
            model.reflog_commits = h_reflog.join().unwrap().unwrap_or_default();

            if let Ok((added, deleted)) = h_shortstat.join().unwrap() {
                model.total_additions = added;
                model.total_deletions = deleted;
            }

            if let Ok(status) = h_status.join().unwrap() {
                model.is_rebasing = status.is_rebasing;
                model.is_merging = status.is_merging;
                model.is_cherry_picking = status.is_cherry_picking;
                model.is_bisecting = status.is_bisecting;
                model.rebase_onto_hash = status.rebase_onto_hash;
            }

            model.repo_url = h_repo_url.join().unwrap_or_default();
            model.contributors = h_contribs.join().unwrap_or_default();

            Ok(model)
        })
    }

    /// Stream working-tree files first, then compute the aggregate shortstat on
    /// the same worker.
    ///
    /// File-status discovery and `git diff --shortstat` both walk a large portion
    /// of the worktree/index. Running them concurrently caused measurable I/O
    /// contention on large repositories (notably the Linux kernel tree).
    ///
    /// Status-only Files are always sent first. On initial startup,
    /// `publish_tracked_early` lets tracked changes reach the UI before the slower
    /// untracked scan completes. `include_file_stats` controls whether the second
    /// FileStats phase also enriches rows with per-file numstat/hunk counts.
    fn stream_files_then_diff_stats(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        include_file_stats: bool,
        trim_on_disconnect: bool,
        publish_tracked_early: bool,
    ) {
        // Capture before spawning: if checkout starts before this worker gets
        // scheduled, it must still recognize itself as belonging to the old HEAD.
        let read_generation = self.worktree_read_generation();
        let tx = tx.clone();
        let git = Arc::clone(self);
        std::thread::spawn(move || {
            let is_current = || git.worktree_read_is_current(read_generation);
            let trim_if_abandoned = || {
                if trim_on_disconnect {
                    trim_process_heap();
                }
            };

            let (mut files, published_tracked_early) = if publish_tracked_early {
                // Start the expensive untracked walk first, then obtain tracked
                // status on this worker. As soon as tracked status is ready we can
                // paint useful Files rows without waiting for repo-wide `??`
                // discovery to finish. The second phase is carried in FileStats so
                // the stream still emits the same number of model parts.
                let untracked_git = Arc::clone(&git);
                let untracked = std::thread::spawn(move || {
                    untracked_git.load_untracked_files_status_only_git()
                });

                match git.load_tracked_files_status_only_git() {
                    Ok(tracked) => {
                        if !is_current() {
                            let _ = untracked.join();
                            return;
                        }
                        if tx.send(ModelPart::Files(tracked.clone())).is_err() {
                            trim_if_abandoned();
                            return;
                        }

                        let files = match untracked.join() {
                            Ok(Ok(untracked)) => {
                                GitCommands::merge_file_status_parts(tracked, untracked)
                            }
                            _ => {
                                if !is_current() {
                                    return;
                                }
                                match git.load_files_status_only_git_uall() {
                                    Ok(files) => files,
                                    Err(_) => {
                                        trim_if_abandoned();
                                        return;
                                    }
                                }
                            }
                        };
                        if !is_current() {
                            return;
                        }
                        (files, true)
                    }
                    Err(_) => {
                        let _ = untracked.join();
                        if !is_current() {
                            return;
                        }
                        let files = match git.load_files_status_only_git_uall() {
                            Ok(files) => files,
                            Err(_) => {
                                trim_if_abandoned();
                                return;
                            }
                        };
                        (files, false)
                    }
                }
            } else {
                match git.load_files_status_only() {
                    Ok(files) if is_current() => (files, false),
                    Ok(_) => return,
                    Err(_) => {
                        trim_if_abandoned();
                        return;
                    }
                }
            };

            if !is_current() {
                return;
            }
            let has_tracked_changes = files.iter().any(|file| file.tracked);

            // Normal refreshes publish the complete status in one Files part.
            // Startup may already have published tracked rows above; in that case
            // FileStats carries the complete set so apply_file_stats can append
            // newly discovered untracked rows without disturbing the visible
            // tracked selection.
            if !published_tracked_early
                && tx.send(ModelPart::Files(files.clone())).is_err()
            {
                trim_if_abandoned();
                return;
            }

            let mut diff_totals = None;
            if include_file_stats && has_tracked_changes {
                let Some(totals) =
                    git.populate_file_diff_stats_if_current(&mut files, read_generation)
                else {
                    return;
                };
                diff_totals = totals;
                if tx.send(ModelPart::FileStats(files)).is_err() {
                    trim_if_abandoned();
                    return;
                }
            } else {
                let completion = if published_tracked_early {
                    files
                } else {
                    Vec::new()
                };
                if tx.send(ModelPart::FileStats(completion)).is_err() {
                    trim_if_abandoned();
                    return;
                }
            }

            // shortstat intentionally omits untracked files. A clean tree
            // (or one containing only untracked files) therefore has an
            // exact 0/0 answer without another full-tree Git process.
            if !has_tracked_changes {
                if tx
                    .send(ModelPart::DiffStats {
                        added: 0,
                        deleted: 0,
                    })
                    .is_err()
                {
                    trim_if_abandoned();
                }
                return;
            }

            if !is_current() {
                return;
            }

            // A full file-stat load already ran `git diff --numstat`; sum those
            // exact results rather than launching another full-tree
            // `git diff HEAD --shortstat`. Status-only refresh paths still use
            // shortstat because they intentionally skipped numstat enrichment.
            if let Some((added, deleted)) = diff_totals {
                if tx.send(ModelPart::DiffStats { added, deleted }).is_err() {
                    trim_if_abandoned();
                }
            } else if let Ok((added, deleted)) = git.diff_shortstat()
                && tx.send(ModelPart::DiffStats { added, deleted }).is_err()
            {
                trim_if_abandoned();
            }
        });
    }

    /// Load model data by spawning one thread per data type. Each thread
    /// sends its result through `tx` as soon as it finishes, so the UI can
    /// waterfall-display whichever data arrives first.
    ///
    /// The caller may set `model.repo_name` (and optionally an initial
    /// `head_hash`) synchronously for the first paint; subsequent refreshes
    /// receive an updated `ModelPart::Head`.
    /// Stream model parts in parallel. When `commit_filter` is set (e.g. `-f`),
    /// the Commits part loads the filtered page immediately — no unfiltered log
    /// first, matching lazygit's startup path filter.
    pub fn load_model_streaming(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        commit_filter: Option<crate::git::commit::CommitFilter>,
        publish_tracked_early: bool,
    ) {
        macro_rules! spawn_part {
            ($tx:expr, $self:expr, $variant:ident, $expr:expr) => {{
                let tx = $tx.clone();
                let git = Arc::clone($self);
                std::thread::spawn(move || {
                    if let Ok(data) = $expr(&git) {
                        let _ = tx.send(ModelPart::$variant(data));
                    }
                });
            }};
        }

        // Preserve full-refresh per-file stats, but avoid racing the aggregate
        // shortstat against the same status/diff work.
        self.stream_files_then_diff_stats(tx, true, false, publish_tracked_early);
        spawn_part!(tx, self, Branches, |g: &GitCommands| g.load_branches());
        self.stream_commits_then_unpushed(tx, commit_filter);
        spawn_part!(tx, self, Stash, |g: &GitCommands| g.load_stash());
        spawn_part!(tx, self, Remotes, |g: &GitCommands| g.load_remotes());
        spawn_part!(tx, self, Tags, |g: &GitCommands| g.load_tags());
        spawn_part!(tx, self, Worktrees, |g: &GitCommands| g
            .load_worktrees()
            .or_else(|_| Ok::<_, anyhow::Error>(Vec::new())));
        spawn_part!(tx, self, Submodules, |g: &GitCommands| g
            .load_submodules()
            .or_else(|_| Ok::<_, anyhow::Error>(Vec::new())));
        spawn_part!(tx, self, Reflog, |g: &GitCommands| g
            .load_reflog(100)
            .or_else(|_| Ok::<_, anyhow::Error>(Vec::new())));

        // Head, RepoUrl, Contributors, RepoStatus have different shapes —
        // spawn them directly. DiffStats is serialized behind Files above.
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                let hash = git.head_hash().unwrap_or_default();
                let branch_name = git.current_branch_name().unwrap_or_default();
                let _ = tx.send(ModelPart::Head { hash, branch_name });
            });
        }
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                let _ = tx.send(ModelPart::RepoUrl(git.load_repo_url()));
            });
        }
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                let _ = tx.send(ModelPart::Contributors(git.load_contributors(500, 10)));
            });
        }
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                if let Ok(status) = git.repo_status() {
                    let _ = tx.send(ModelPart::RepoStatus {
                        is_rebasing: status.is_rebasing,
                        is_merging: status.is_merging,
                        is_cherry_picking: status.is_cherry_picking,
                        is_bisecting: status.is_bisecting,
                        rebase_onto_hash: status.rebase_onto_hash,
                    });
                }
            });
        }
    }

    pub fn load_model_periodic_refresh_streaming(self: &Arc<Self>, tx: &mpsc::Sender<ModelPart>) {
        // refreshInterval tracks working-tree file state.
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                if let Ok(files) = git.load_files_status_only() {
                    let _ = tx.send(ModelPart::Files(files));
                }
            });
        }

        // Submodule state can also change independently of lazygitrs.
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                if let Ok(submodules) = git.load_submodules() {
                    let _ = tx.send(ModelPart::Submodules(submodules));
                }
            });
        }
    }

    /// Refresh only state that can change as a result of a successful push.
    ///
    /// Keep the three independent reads off the UI thread and run them in
    /// parallel. In particular, do not rebuild the current-branch commit list:
    /// push does not change local commit topology or worktree contents.
    pub fn load_model_after_push_streaming(self: &Arc<Self>, tx: &mpsc::Sender<ModelPart>) {
        let tx_branches = tx.clone();
        let git = Arc::clone(self);
        std::thread::spawn(move || {
            if let Ok(branches) = git.load_branches() {
                let _ = tx_branches.send(ModelPart::Branches(branches));
            }
        });

        let tx_remotes = tx.clone();
        let git = Arc::clone(self);
        std::thread::spawn(move || {
            if let Ok(remotes) = git.load_remotes() {
                let _ = tx_remotes.send(ModelPart::Remotes(remotes));
            }
        });

        let tx_unpushed = tx.clone();
        let git = Arc::clone(self);
        std::thread::spawn(move || {
            if let Ok(unpushed) = git.unpushed_commit_hashes() {
                let _ = tx_unpushed.send(ModelPart::UnpushedCommitHashes(unpushed));
            }
        });
    }

    pub fn load_model_after_fetch_streaming(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        commit_filter: Option<crate::git::commit::CommitFilter>,
        previous_tag_remote_state: std::collections::HashMap<String, bool>,
    ) {
        macro_rules! spawn_part {
            ($tx:expr, $self:expr, $variant:ident, $expr:expr) => {{
                let tx = $tx.clone();
                let git = Arc::clone($self);
                std::thread::spawn(move || {
                    if let Ok(data) = $expr(&git) {
                        let _ = tx.send(ModelPart::$variant(data));
                    }
                });
            }};
        }

        // Fetch can change upstream divergence / remote-facing branch state.
        spawn_part!(tx, self, Branches, |g: &GitCommands| g.load_branches());

        // Reload the current-branch page so fetched remote decorations and pushed/
        // unpushed status are reflected without a global history traversal.
        self.stream_commits_then_unpushed(tx, commit_filter);

        // Remote-tracking branches can change.
        spawn_part!(tx, self, Remotes, |g: &GitCommands| g.load_remotes());

        // Refresh local tags without re-querying the remotes we just fetched.
        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                if let Ok(tags) = git.load_tags_after_remote_update(&previous_tag_remote_state) {
                    let _ = tx.send(ModelPart::Tags(tags));
                }
            });
        }
    }

    /// Refresh model state that can be affected by a successful pull.
    ///
    /// Unlike a full model refresh:
    /// - stash and repo URL are unchanged and are not reloaded
    /// - files use status-only loading, avoiding expensive numstat/hunk scans
    ///
    /// Other potentially pull-affected state is still refreshed.
    pub fn load_model_after_pull_streaming(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        commit_filter: Option<crate::git::commit::CommitFilter>,
        previous_tag_remote_state: std::collections::HashMap<String, bool>,
    ) {
        macro_rules! spawn_part {
            ($tx:expr, $self:expr, $variant:ident, $expr:expr) => {{
                let tx = $tx.clone();
                let git = Arc::clone($self);
                std::thread::spawn(move || {
                    if let Ok(data) = $expr(&git) {
                        let _ = tx.send(ModelPart::$variant(data));
                    }
                });
            }};
        }

        // Pull can change HEAD-relative file status, but we don't need the
        // expensive per-file numstat/hunk scans immediately. The selected diff
        // is refreshed separately by the GUI. Serialize shortstat behind status
        // so the two full-tree scans do not compete with each other.
        self.stream_files_then_diff_stats(tx, false, false, false);

        spawn_part!(tx, self, Branches, |g: &GitCommands| g.load_branches());

        self.stream_commits_then_unpushed(tx, commit_filter);

        spawn_part!(tx, self, Remotes, |g: &GitCommands| g.load_remotes());

        // A pull may auto-follow tags. Refresh local refs immediately and merge
        // the remote-state information we already know without another network
        // round-trip.
        spawn_part!(tx, self, Tags, move |g: &GitCommands| {
            g.load_tags_after_remote_update(&previous_tag_remote_state)
        });

        // Pull can move the current worktree's HEAD.
        spawn_part!(tx, self, Worktrees, |g: &GitCommands| {
            g.load_worktrees()
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
        });

        // A changed superproject HEAD can change gitlink state.
        spawn_part!(tx, self, Submodules, |g: &GitCommands| {
            g.load_submodules()
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
        });

        // Pull moving HEAD changes reflog history.
        spawn_part!(tx, self, Reflog, |g: &GitCommands| {
            g.load_reflog(100)
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
        });

        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                if let Ok(head) = git.head_state() {
                    let _ = tx.send(ModelPart::Head {
                        hash: head.hash,
                        branch_name: head.branch_name,
                    });
                }
            });
        }

        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                let _ = tx.send(ModelPart::Contributors(git.load_contributors(500, 10)));
            });
        }

        {
            let tx = tx.clone();
            let git = Arc::clone(self);
            std::thread::spawn(move || {
                if let Ok(status) = git.repo_status() {
                    let _ = tx.send(ModelPart::RepoStatus {
                        is_rebasing: status.is_rebasing,
                        is_merging: status.is_merging,
                        is_cherry_picking: status.is_cherry_picking,
                        is_bisecting: status.is_bisecting,
                        rebase_onto_hash: status.rebase_onto_hash,
                    });
                }
            });
        }
    }

    /// Refresh model state that can be affected by a successful checkout.
    ///
    /// Checkout can change HEAD-relative files, branch state, commit decorations,
    /// worktree state, submodule state, reflog, diff stats, and repository status.
    /// Contributors are intentionally not recomputed on every checkout; the
    /// informational list is refreshed by full/history-changing model refreshes.
    /// Checkout cannot change stash, remotes, tags, or the repo URL.
    pub fn load_model_after_checkout_streaming(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        mut commits: Vec<crate::model::Commit>,
        mut branches: Vec<crate::model::Branch>,
        mut worktrees: Vec<crate::model::Worktree>,
        mut reflog: Vec<crate::model::Commit>,
        confirmed_head: Option<HeadState>,
        commit_filter: Option<crate::git::commit::CommitFilter>,
    ) {
        // Keep the worktree scan on its own worker: on very large repositories
        // (notably Linux) status dominates checkout refresh latency. If this
        // refresh is superseded by another checkout, the dropped receiver makes
        // the worker stop before shortstat and trim any allocator residue.
        self.stream_files_then_diff_stats(tx, false, true, false);

        // Everything else is intentionally serialized on one worker. These
        // reads are individually cheap compared with status, and avoiding a
        // fresh fan-out of short-lived threads on every checkout prevents rapid
        // branch toggling from stacking workers and glibc thread arenas.
        let tx = tx.clone();
        let git = Arc::clone(self);
        std::thread::spawn(move || {
            macro_rules! send_or_abandon {
                ($part:expr) => {
                    if tx.send($part).is_err() {
                        trim_process_heap();
                        return;
                    }
                };
            }

            // The checkout worker already read HEAD before reporting success to
            // the UI. Reuse that exact state instead of resolving HEAD again;
            // retain the gix read as a compatibility fallback for older/failed
            // metadata paths.
            let head = confirmed_head.or_else(|| git.head_state().ok());

            if let Some(head) = &head {
                // Checkout of an existing local branch changes only which cached
                // branch carries the HEAD marker. Branch tips/upstreams do not
                // move. Only fall back to `for-each-ref` when checkout created a
                // local branch we did not previously know about.
                let cached_branch_is_complete = head.branch_name.is_empty()
                    || branches.iter().any(|branch| branch.name == head.branch_name);
                if cached_branch_is_complete {
                    for branch in &mut branches {
                        branch.head = branch.name == head.branch_name;
                    }
                    crate::model::sort_branches_for_display(&mut branches);
                } else if let Ok(fresh) = git.load_branches() {
                    branches = fresh;
                }
            } else if let Ok(fresh) = git.load_branches() {
                branches = fresh;
            }
            send_or_abandon!(ModelPart::Branches(branches));

            // The main commits panel is HEAD-relative, so checkout changes which
            // history belongs in the panel even though the repository's global
            // commit graph itself did not change. Reload only the current page;
            // this is much cheaper than the old `git log --all --topo-order` walk.
            let refreshed_commits = match commit_filter {
                Some(filter) => {
                    git.load_filtered_commits_page(&filter, DEFAULT_COMMIT_LIMIT, 0)
                }
                None => git.load_commits_page(DEFAULT_COMMIT_LIMIT, 0),
            };
            if let Ok(fresh) = refreshed_commits {
                commits = fresh;
            }

            if let Some(head) = &head {
                // Ensure the decoration is immediately correct even if a custom
                // pretty format or older Git omitted the symbolic HEAD decoration.
                GitCommands::apply_checkout_head_state(&mut commits, head);
                send_or_abandon!(ModelPart::Head {
                    hash: head.hash.clone(),
                    branch_name: head.branch_name.clone(),
                });
            }

            send_or_abandon!(ModelPart::Commits(commits));
            let unpushed = git.unpushed_commit_hashes().unwrap_or_default();
            send_or_abandon!(ModelPart::UnpushedCommitHashes(unpushed));

            // `git checkout` cannot add/remove worktrees. Update only the current
            // cached entry in memory; if the cache is unexpectedly missing that
            // entry, retain the old Git-backed fallback for correctness.
            let mut updated_current_worktree = false;
            if let Some(head) = &head {
                if let Some(worktree) = worktrees.iter_mut().find(|worktree| worktree.is_current) {
                    worktree.hash.clone_from(&head.hash);
                    worktree.branch = if head.branch_name.is_empty() {
                        "(detached)".to_string()
                    } else {
                        head.branch_name.clone()
                    };
                    updated_current_worktree = true;
                }
            }
            if !updated_current_worktree {
                worktrees = git.load_worktrees().unwrap_or_default();
            }
            send_or_abandon!(ModelPart::Worktrees(worktrees));

            if let Ok(submodules) = git
                .load_submodules()
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
            {
                send_or_abandon!(ModelPart::Submodules(submodules));
            }

            // Checkout appends one HEAD reflog record. Reuse the cached tail and
            // read only the newest entry rather than rebuilding the full 100-row
            // model. `load_reflog(1)` is gix-backed when available. If checkout
            // happened unusually early and the startup reflog has not arrived
            // yet, retain the full-load fallback so older entries are not lost.
            if reflog.is_empty() {
                reflog = git.load_reflog(100).unwrap_or_default();
            } else if let Ok(mut latest) = git.load_reflog(1) {
                if let Some(latest) = latest.pop() {
                    let already_first = reflog.first().is_some_and(|first| {
                        first.hash == latest.hash
                            && first.name == latest.name
                            && first.unix_timestamp == latest.unix_timestamp
                    });
                    if !already_first {
                        reflog.insert(0, latest);
                    }
                    reflog.truncate(100);
                }
            }
            send_or_abandon!(ModelPart::Reflog(reflog));

            if let Ok(status) = git.repo_status() {
                send_or_abandon!(ModelPart::RepoStatus {
                    is_rebasing: status.is_rebasing,
                    is_merging: status.is_merging,
                    is_cherry_picking: status.is_cherry_picking,
                    is_bisecting: status.is_bisecting,
                    rebase_onto_hash: status.rebase_onto_hash,
                });
            }
        });
    }

    pub fn load_model_after_history_change_streaming(
        self: &Arc<Self>,
        tx: &mpsc::Sender<ModelPart>,
        commit_filter: Option<crate::git::commit::CommitFilter>,
    ) {
        macro_rules! spawn_part {
            ($tx:expr, $self:expr, $variant:ident, $expr:expr) => {{
                let tx = $tx.clone();
                let git = Arc::clone($self);
                std::thread::spawn(move || {
                    if let Ok(data) = $expr(&git) {
                        let _ = tx.send(ModelPart::$variant(data));
                    }
                });
            }};
        }

        // Local history changes can affect working-tree status, but the selected
        // diff is refreshed separately by the GUI, so avoid per-file diff stats.
        // Serialize shortstat behind status to avoid two competing worktree scans.
        self.stream_files_then_diff_stats(tx, false, false, false);

        spawn_part!(tx, self, Branches, |g: &GitCommands| g.load_branches());

        // Unlike checkout, commits/rewords/amends/rebases actually change the
        // commit graph, so reload the current commit page.
        self.stream_commits_then_unpushed(tx, commit_filter);

        spawn_part!(tx, self, Worktrees, |g: &GitCommands| {
            g.load_worktrees()
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
        });

        spawn_part!(tx, self, Submodules, |g: &GitCommands| {
            g.load_submodules()
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
        });

        spawn_part!(tx, self, Reflog, |g: &GitCommands| {
            g.load_reflog(100)
                .or_else(|_| Ok::<_, anyhow::Error>(Vec::new()))
        });

        {
            let tx = tx.clone();
            let git = Arc::clone(self);

            std::thread::spawn(move || {
                if let Ok(head) = git.head_state() {
                    let _ = tx.send(ModelPart::Head {
                        hash: head.hash,
                        branch_name: head.branch_name,
                    });
                }
            });
        }

        {
            let tx = tx.clone();
            let git = Arc::clone(self);

            std::thread::spawn(move || {
                let _ = tx.send(ModelPart::Contributors(git.load_contributors(500, 10)));
            });
        }

        {
            let tx = tx.clone();
            let git = Arc::clone(self);

            std::thread::spawn(move || {
                if let Ok(status) = git.repo_status() {
                    let _ = tx.send(ModelPart::RepoStatus {
                        is_rebasing: status.is_rebasing,
                        is_merging: status.is_merging,
                        is_cherry_picking: status.is_cherry_picking,
                        is_bisecting: status.is_bisecting,
                        rebase_onto_hash: status.rebase_onto_hash,
                    });
                }
            });
        }
    }

    /// Status-only file refresh (skips numstat/hunk subprocesses).
    pub fn refresh_files_status_only(&self) -> Result<Vec<crate::model::File>> {
        self.load_files_status_only()
    }

    /// Get the current branch name. Usually this is an in-process gix read;
    /// for an unborn repository HEAD has no object id yet, so fall back to the
    /// symbolic ref name instead of treating the repository as detached.
    pub fn current_branch_name(&self) -> Result<String> {
        match self.read_head_state() {
            Ok(head) => Ok(head.branch_name),
            Err(read_err) => {
                let symbolic = self
                    .git()
                    .args(&["symbolic-ref", "--quiet", "--short", "HEAD"])
                    .run()?;
                if symbolic.success && !symbolic.stdout_trimmed().is_empty() {
                    Ok(symbolic.stdout_trimmed().to_string())
                } else {
                    Err(read_err)
                }
            }
        }
    }

    /// Read the full HEAD hash and symbolic ref in-process through gix.
    pub fn head_state(&self) -> Result<HeadState> {
        self.read_head_state()
    }

    /// Resolve a revision to a verified commit, peeling annotated tags.
    /// Unlike `resolve_ref`, this rejects trees and option-like input.
    fn resolve_commit_ref(&self, reference: &str) -> Result<String> {
        let result = self
            .git()
            .args(&[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ])
            .run_expecting_success()?;
        Ok(result.stdout_trimmed().to_string())
    }

    /// Get the name of the previously checked-out branch (`@{-1}`), if any.
    pub fn previous_branch_name(&self) -> Option<String> {
        let result = self
            .git()
            .args(&["rev-parse", "--abbrev-ref", "@{-1}"])
            .run()
            .ok()?;
        if !result.success {
            return None;
        }
        let name = result.stdout_trimmed().to_string();
        if name.is_empty() || name == "HEAD" || name == "@{-1}" {
            None
        } else {
            Some(name)
        }
    }

    /// Get the repo name (last component of path).
    pub fn repo_name(&self) -> String {
        self.repo_name.clone()
    }

    /// Check if the working directory is a valid git repo.
    pub fn is_valid_repo(path: &Path) -> bool {
        if gix::discover(path).is_ok() {
            return true;
        }

        // Preserve compatibility for repositories gix cannot discover even
        // though the installed Git can (unusual layouts/configurations).
        CmdBuilder::git()
            .cwd_path(path)
            .args(&["rev-parse", "--git-dir"])
            .run()
            .is_ok_and(|result| result.success)
    }

    /// Get the HEAD commit hash without spawning Git.
    pub fn head_hash(&self) -> Result<String> {
        let hash = self.read_head_state()?.hash;
        if hash.is_empty() {
            anyhow::bail!("HEAD does not point to a commit");
        }
        Ok(hash)
    }

    /// Resolve a ref (branch name, tag, hash) to a full commit hash.
    pub fn resolve_ref(&self, refspec: &str) -> Result<String> {
        let result = self
            .git()
            .args(&["rev-parse", refspec])
            .run_expecting_success()?;
        Ok(result.stdout_trimmed().to_string())
    }

    /// Get the subject line of a commit. Test-only helper for rebase assertions.
    #[cfg(test)]
    pub fn commit_subject(&self, hash: &str) -> Result<String> {
        let result = self
            .git()
            .args(&["log", "-1", "--format=%s", hash])
            .run_expecting_success()?;
        Ok(result.stdout_trimmed().to_string())
    }

}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
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

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn run_test_git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn try_init_test_repo(path: &Path, extra_args: &[&str]) -> bool {
        let mut args = vec!["init", "--quiet", "--initial-branch=main"];
        args.extend_from_slice(extra_args);
        Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn git3_repository_formats_are_supported_when_available() {
        for (name, init_args, expected_hash_len) in [
            ("sha256", vec!["--object-format=sha256"], 64usize),
            ("reftable", vec!["--ref-format=reftable"], 40usize),
            (
                "sha256-reftable",
                vec!["--object-format=sha256", "--ref-format=reftable"],
                64usize,
            ),
        ] {
            let temp = TempDir::new(&format!("git3-{name}"));
            if !try_init_test_repo(temp.path(), &init_args) {
                // Older Git versions may not know one or both format options.
                // Keep the suite usable there while exercising these paths on
                // current Git and, eventually, Git 3.x.
                continue;
            }

            run_test_git(temp.path(), &["commit", "--allow-empty", "-m", "root"]);
            let expected_head = run_test_git(temp.path(), &["rev-parse", "HEAD"]);
            let expected_short_head = run_test_git(temp.path(), &["rev-parse", "--short", "HEAD"]);
            assert_eq!(expected_head.len(), expected_hash_len, "{name}");

            let git = GitCommands::new(temp.path()).expect("open Git-3-style repository");
            let head = git.head_state().expect("read HEAD");
            assert_eq!(head.hash, expected_head, "{name}");
            assert_eq!(head.branch_name, "main", "{name}");

            let commits = git.load_commits(10).expect("load commits");
            assert_eq!(commits.first().map(|commit| commit.hash.as_str()), Some(head.hash.as_str()), "{name}");

            let branches = git.load_branches().expect("load branches");
            assert!(
                branches.iter().any(|branch| {
                    branch.name == "main"
                        && branch.head
                        && branch.hash == expected_short_head
                }),
                "{name}: main branch should be loaded from the repository format"
            );
        }
    }

    #[test]
    fn head_state_reads_branch_names_and_detached_annotated_tags() {
        let temp = TempDir::new("head-state");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let hash = run(&["rev-parse", "HEAD"]);
        run(&["checkout", "-b", "feature/nested-name"]);
        let git = GitCommands::new(temp.path()).unwrap();
        let head = git.head_state().unwrap();
        assert_eq!(head.hash, hash);
        assert_eq!(head.branch_name, "feature/nested-name");
        run(&["tag", "-a", "release", "-m", "release"]);
        run(&["checkout", "release"]);
        let head = git.head_state().unwrap();
        assert_eq!(head.hash, hash);
        assert!(head.branch_name.is_empty());
    }

    #[test]
    fn head_state_rejects_unborn_head_but_branch_name_is_available() {
        let temp = TempDir::new("unborn-head-state");
        run_test_git(temp.path(), &["init", "-b", "main"]);
        let git = GitCommands::new(temp.path()).unwrap();
        assert!(git.head_state().is_err());
        assert_eq!(git.current_branch_name().unwrap(), "main");
    }

    #[test]
    fn default_commit_history_follows_checked_out_head() {
        let temp = TempDir::new("commit-history-head-relative");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        run(&["checkout", "-b", "feature"]);
        run(&["commit", "--allow-empty", "-m", "feature only"]);
        let feature_tip = run(&["rev-parse", "HEAD"]);
        run(&["checkout", "main"]);
        run(&["commit", "--allow-empty", "-m", "main only"]);
        let main_tip = run(&["rev-parse", "HEAD"]);

        let git = GitCommands::new(temp.path()).unwrap();
        let main_commits = git.load_commits_page(50, 0).unwrap();
        assert_eq!(main_commits.first().map(|c| c.hash.as_str()), Some(main_tip.as_str()));
        assert!(!main_commits.iter().any(|commit| commit.hash == feature_tip));

        run(&["checkout", "feature"]);
        let feature_commits = git.load_commits_page(50, 0).unwrap();
        assert_eq!(
            feature_commits.first().map(|c| c.hash.as_str()),
            Some(feature_tip.as_str())
        );
        assert!(!feature_commits.iter().any(|commit| commit.hash == main_tip));
    }

    #[test]
    fn default_commit_history_excludes_tag_roots_but_keeps_tag_navigation() {
        let temp = TempDir::new("commit-history-tag-roots");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let root = run(&["rev-parse", "HEAD"]);
        run(&["tag", "reachable-tag"]);

        run(&["checkout", "-b", "archive"]);
        run(&["commit", "--allow-empty", "-m", "tag only"]);
        let tag_only = run(&["rev-parse", "HEAD"]);
        run(&["tag", "tag-only"]);
        run(&["checkout", "main"]);
        run(&["branch", "-D", "archive"]);

        let git = GitCommands::new(temp.path()).unwrap();
        let commits = git.load_commits_page(50, 0).unwrap();
        assert!(commits.iter().any(|commit| commit.hash == root));
        assert!(!commits.iter().any(|commit| commit.hash == tag_only));
        assert!(
            commits
                .iter()
                .find(|commit| commit.hash == root)
                .unwrap()
                .tags
                .iter()
                .any(|tag| tag == "reachable-tag")
        );

        let tagged = git.load_commits_for_branch("tag-only", 10).unwrap();
        assert_eq!(tagged[0].hash, tag_only);

        run(&["checkout", "--detach", &tag_only]);
        let git = GitCommands::new(temp.path()).unwrap();
        let detached = git.load_commits_page(50, 0).unwrap();
        assert!(detached.iter().any(|commit| commit.hash == tag_only));
    }

    #[test]
    fn load_compare_commits_linear_history_and_commit_revisions() {
        use crate::model::commit::Divergence;

        let temp = TempDir::new("compare-commits-linear");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let root = run(&["rev-parse", "HEAD"]);
        run(&["tag", "-a", "base", "-m", "base"]);
        run(&["checkout", "-b", "feature"]);
        run(&["commit", "--allow-empty", "-m", "one | pipe"]);
        let first = run(&["rev-parse", "HEAD"]);
        run(&["commit", "--allow-empty", "-m", "two"]);
        let tip = run(&["rev-parse", "HEAD"]);
        run(&["tag", "-a", "tip", "-m", "tip"]);
        let git = GitCommands::new(temp.path()).unwrap();

        for (a, b, side) in [
            ("feature", "main", Divergence::Left),
            ("base", "feature", Divergence::Right),
            (root.as_str(), tip.as_str(), Divergence::Right),
            ("HEAD~2", "tip", Divergence::Right),
        ] {
            let commits = git.load_compare_commits(a, b, 0, 0).unwrap();
            assert_eq!(commits.len(), 2);
            assert_eq!(commits[0].hash, tip);
            assert_eq!(commits[1].hash, first);
            assert!(commits.iter().all(|commit| commit.divergence == side));
            assert_eq!(commits[1].name, "one | pipe");
            assert_eq!(commits[1].author_name, "Test");
            assert_eq!(commits[1].author_email, "test@example.com");
            assert!(commits[1].unix_timestamp > 0);
            assert_eq!(commits[1].parents, [root.clone()]);
            assert_eq!(commits[0].tags, ["tip"]);
            assert!(commits[0].refs.iter().any(|r| r.contains("feature")));
        }
        assert!(
            git.load_compare_commits("feature", "tip", 10, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            git.load_compare_commits("main", "main", 10, 0)
                .unwrap()
                .is_empty()
        );
        let commits = git.load_compare_commits("HEAD~1", "HEAD", 10, 0).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, tip);
        assert_eq!(commits[0].divergence, Divergence::Right);
    }

    #[test]
    fn load_compare_commits_diverged_history_and_paging() {
        use crate::model::commit::Divergence;

        let temp = TempDir::new("compare-commits-diverged");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let root = run(&["rev-parse", "HEAD"]);
        run(&["checkout", "-b", "feature"]);
        run(&["commit", "--allow-empty", "-m", "feature one"]);
        let feature_one = run(&["rev-parse", "HEAD"]);
        run(&["commit", "--allow-empty", "-m", "feature two"]);
        let feature_two = run(&["rev-parse", "HEAD"]);
        run(&["checkout", "main"]);
        run(&["commit", "--allow-empty", "-m", "main one"]);
        let main_one = run(&["rev-parse", "HEAD"]);
        let git = GitCommands::new(temp.path()).unwrap();
        let commits = git.load_compare_commits("feature", "main", 0, 0).unwrap();
        assert_eq!(commits.len(), 3);
        assert!(!commits.iter().any(|commit| commit.hash == root));
        for (hash, side) in [
            (&feature_one, Divergence::Left),
            (&feature_two, Divergence::Left),
            (&main_one, Divergence::Right),
        ] {
            assert_eq!(
                commits.iter().find(|c| &c.hash == hash).unwrap().divergence,
                side
            );
        }
        let position = |hash: &str| commits.iter().position(|c| c.hash == hash).unwrap();
        assert!(position(&feature_two) < position(&feature_one));

        for skip in 0..commits.len() {
            let page = git
                .load_compare_commits("feature", "main", 1, skip)
                .unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].hash, commits[skip].hash);
            assert_eq!(page[0].divergence, commits[skip].divergence);
        }
        assert!(
            git.load_compare_commits("feature", "main", 2, 3)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            git.load_compare_commits("feature", "main", 0, 1)
                .unwrap()
                .len(),
            2
        );
        let reverse = git.load_compare_commits("main", "feature", 0, 0).unwrap();
        for commit in reverse {
            let original = commits.iter().find(|c| c.hash == commit.hash).unwrap();
            assert_ne!(original.divergence, commit.divergence);
        }
    }

    #[test]
    fn load_compare_commits_rejects_invalid_and_non_commit_refs() {
        let temp = TempDir::new("compare-commits-invalid");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let tree = run(&["rev-parse", "HEAD^{tree}"]);
        run(&["tag", "-a", "tree-tag", &tree, "-m", "tree tag"]);
        let git = GitCommands::new(temp.path()).unwrap();
        for reference in [
            "missing-ref",
            "HEAD^{tree}",
            tree.as_str(),
            "tree-tag",
            "--all",
            "HEAD..HEAD",
            "",
        ] {
            assert!(
                git.load_compare_commits(reference, "main", 10, 0).is_err(),
                "left: {reference}"
            );
            assert!(
                git.load_compare_commits("main", reference, 10, 0).is_err(),
                "right: {reference}"
            );
        }
    }

    #[test]
    fn diff_refs_ahead_behind_counts_commit_ancestry_not_file_changes() {
        let temp = TempDir::new("compare-ancestry");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(temp.path())
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "tag.gpgsign=false",
                ])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "root"]);
        let root = run(&["rev-parse", "HEAD"]);
        run(&["tag", "-a", "base", "-m", "base"]);
        run(&["checkout", "-b", "feature"]);
        run(&["commit", "--allow-empty", "-m", "feature one"]);
        run(&["commit", "--allow-empty", "-m", "feature two"]);
        let tip = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/remotes/origin/feature", &tip]);

        let git = GitCommands::new(temp.path()).unwrap();
        assert_eq!(
            git.diff_refs_ahead_behind("feature", "main").unwrap(),
            (2, 0)
        );
        assert_eq!(
            git.diff_refs_ahead_behind("base", "feature").unwrap(),
            (0, 2)
        );
        assert_eq!(git.diff_refs_ahead_behind(&tip, &root).unwrap(), (2, 0));
        assert_eq!(
            git.diff_refs_ahead_behind("HEAD~1", "HEAD").unwrap(),
            (0, 1)
        );
        assert_eq!(
            git.diff_refs_ahead_behind("origin/feature", "feature")
                .unwrap(),
            (0, 0)
        );
        assert!(git.diff_refs_files("base", "feature").unwrap().is_empty());

        run(&["checkout", "main"]);
        run(&["commit", "--allow-empty", "-m", "main one"]);
        assert_eq!(
            git.diff_refs_ahead_behind("feature", "main").unwrap(),
            (2, 1)
        );
        assert_eq!(
            git.diff_refs_ahead_behind("main", "feature").unwrap(),
            (1, 2)
        );
        assert!(git.diff_refs_ahead_behind("missing-ref", "main").is_err());
        assert!(git.diff_refs_ahead_behind("HEAD^{tree}", "main").is_err());
        assert!(git.diff_refs_files("HEAD^{tree}", "main").is_ok());
    }

    #[test]
    fn new_resolves_repo_path_to_worktree_root_from_subdirectory() {
        let temp = TempDir::new("repo-root");
        let repo = temp.path().join("my-repo");
        let subdir = repo.join("nested").join("folder");
        std::fs::create_dir_all(&subdir).expect("create nested repo dir");

        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .arg(&repo)
            .status()
            .expect("run git init");
        assert!(status.success());

        let git = GitCommands::new(&subdir).expect("create git commands");

        assert_eq!(git.repo_path(), repo.canonicalize().unwrap());
        assert_eq!(git.repo_name(), "my-repo");
    }

    #[test]
    fn repo_name_comes_from_main_repo_for_linked_worktree() {
        let temp = TempDir::new("linked-worktree");
        let repo = temp.path().join("main-repo");
        let linked = temp.path().join("linked-checkout");
        std::fs::create_dir_all(&repo).expect("create repo dir");

        assert_success(
            Command::new("git")
                .args(["init", "--initial-branch=main"])
                .arg(&repo)
                .status(),
        );
        assert_success(
            Command::new("git")
                .args(["config", "user.email", "test@example.com"])
                .current_dir(&repo)
                .status(),
        );
        assert_success(
            Command::new("git")
                .args(["config", "user.name", "Test"])
                .current_dir(&repo)
                .status(),
        );
        std::fs::write(repo.join("file"), "content").expect("write file");
        assert_success(
            Command::new("git")
                .args(["add", "file"])
                .current_dir(&repo)
                .status(),
        );
        assert_success(
            Command::new("git")
                .args(["commit", "-m", "init"])
                .current_dir(&repo)
                .status(),
        );
        assert_success(
            Command::new("git")
                .arg("worktree")
                .arg("add")
                .arg(&linked)
                .arg("-b")
                .arg("linked")
                .current_dir(&repo)
                .status(),
        );

        let subdir = linked.join("nested");
        std::fs::create_dir_all(&subdir).expect("create linked subdir");
        let git = GitCommands::new(&subdir).expect("create git commands");

        assert_eq!(git.repo_path(), linked.canonicalize().unwrap());
        assert_eq!(git.repo_name(), "main-repo");
    }

    #[test]
    fn linked_worktree_for_branch_finds_other_worktree() {
        let temp = TempDir::new("branch-linked-worktree");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        std::fs::create_dir_all(&repo).expect("create repo");
        let run = |args: &[&str]| run_test_git(&repo, args);

        run(&["init", "-b", "main"]);
        std::fs::write(repo.join("file"), "content").expect("write file");
        run(&["add", "file"]);
        run(&["commit", "-m", "init"]);
        run(&[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("utf8 linked path"),
        ]);

        let git = GitCommands::new(&repo).expect("create git commands");
        let worktree = git
            .linked_worktree_for_branch("linked")
            .expect("load linked worktree")
            .expect("linked branch worktree");
        assert_eq!(
            Path::new(&worktree.path).canonicalize().unwrap(),
            linked.canonicalize().unwrap()
        );
        assert!(!worktree.is_current);
        assert!(!worktree.is_main);
    }

    #[test]
    fn branch_delete_requires_force_for_unmerged_branch() {
        let temp = TempDir::new("branch-delete-force-check");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        let run = |args: &[&str]| run_test_git(&repo, args);

        run(&["init", "-b", "main"]);
        run(&["commit", "--allow-empty", "-m", "base"]);
        run(&["checkout", "-b", "feature"]);
        run(&["commit", "--allow-empty", "-m", "feature"]);
        run(&["checkout", "main"]);

        let git = GitCommands::new(&repo).expect("create git commands");
        assert!(git.branch_delete_requires_force("feature").unwrap());

        run(&["merge", "--ff-only", "feature"]);
        assert!(!git.branch_delete_requires_force("feature").unwrap());
    }

    #[test]
    fn branch_delete_safety_uses_upstream_when_available() {
        let temp = TempDir::new("branch-delete-upstream-check");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        let run = |args: &[&str]| run_test_git(&repo, args);

        run(&["init", "-b", "main"]);
        run(&["remote", "add", "origin", "https://example.invalid/repo"]);
        run(&["commit", "--allow-empty", "-m", "base"]);
        run(&["checkout", "-b", "feature"]);
        run(&["commit", "--allow-empty", "-m", "feature"]);
        let feature = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/remotes/origin/feature", &feature]);
        run(&["branch", "--set-upstream-to=origin/feature", "feature"]);
        run(&["checkout", "main"]);

        let git = GitCommands::new(&repo).expect("create git commands");
        assert!(!git.branch_delete_requires_force("feature").unwrap());
    }

    #[test]
    fn branch_worktree_removal_refuses_dirty_worktree() {
        let temp = TempDir::new("dirty-branch-worktree");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        std::fs::create_dir_all(&repo).expect("create repo");
        let run = |args: &[&str]| run_test_git(&repo, args);

        run(&["init", "-b", "main"]);
        std::fs::write(repo.join("file"), "content").expect("write file");
        run(&["add", "file"]);
        run(&["commit", "-m", "init"]);
        run(&[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("utf8 linked path"),
        ]);
        std::fs::write(linked.join("file"), "dirty").expect("dirty linked worktree");

        let git = GitCommands::new(&repo).expect("create git commands");
        let err = git
            .remove_worktree_for_branch_deletion(linked.to_str().unwrap())
            .expect_err("dirty worktree must be preserved");
        assert!(err.to_string().contains("local changes"));
        assert!(linked.exists());
        assert!(git
            .linked_worktree_for_branch("linked")
            .unwrap()
            .is_some());
    }

    #[test]
    fn branch_worktree_removal_prunes_missing_worktree_metadata() {
        let temp = TempDir::new("stale-branch-worktree");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        std::fs::create_dir_all(&repo).expect("create repo");
        let run = |args: &[&str]| run_test_git(&repo, args);

        run(&["init", "-b", "main"]);
        std::fs::write(repo.join("file"), "content").expect("write file");
        run(&["add", "file"]);
        run(&["commit", "-m", "init"]);
        run(&[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("utf8 linked path"),
        ]);
        std::fs::remove_dir_all(&linked).expect("remove linked worktree directory");

        let git = GitCommands::new(&repo).expect("create git commands");
        git.remove_worktree_for_branch_deletion(linked.to_str().unwrap())
            .expect("prune stale worktree metadata");
        assert!(git
            .linked_worktree_for_branch("linked")
            .unwrap()
            .is_none());
    }

    #[test]
    fn load_files_is_fast_on_unborn_repo_with_many_untracked() {
        let temp = TempDir::new("many-untracked");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert_success(
            Command::new("git")
                .args(["init", "--initial-branch=main"])
                .arg(&repo)
                .status(),
        );

        // node_modules-style tree: many untracked files, no commits yet.
        let nm = repo.join("node_modules").join("pkg");
        std::fs::create_dir_all(&nm).expect("mkdir nm");
        for i in 0..500 {
            std::fs::write(
                nm.join(format!("f{i}.js")),
                format!("console.log({i});\n").repeat(20),
            )
            .expect("write");
        }
        std::fs::write(repo.join("README.md"), "hi\n").expect("write readme");

        let git = GitCommands::new(&repo).expect("git");
        let start = std::time::Instant::now();
        let files = git.load_files().expect("load_files");
        let elapsed = start.elapsed();

        assert!(
            files.len() >= 501,
            "expected all untracked files, got {}",
            files.len()
        );
        assert!(
            files.iter().all(|f| !f.tracked && f.additions == 0),
            "untracked files should not pay for line counts (lazygit parity)"
        );
        assert!(
            elapsed.as_secs() < 5,
            "load_files too slow on large untracked tree: {elapsed:?}"
        );
    }

    #[test]
    fn file_stream_publishes_status_before_diff_enrichment() {
        let temp = TempDir::new("stream-files-first");
        let run = |args: &[&str]| run_test_git(temp.path(), args);
        run(&["init", "-b", "main"]);
        std::fs::write(temp.path().join("tracked.txt"), "one\n").expect("write tracked");
        run(&["add", "tracked.txt"]);
        run(&["commit", "-m", "initial"]);
        std::fs::write(temp.path().join("tracked.txt"), "one\ntwo\n").expect("modify tracked");
        std::fs::write(temp.path().join("untracked.txt"), "new\n").expect("write untracked");

        let git = Arc::new(GitCommands::new(temp.path()).expect("git"));
        let (tx, rx) = mpsc::channel();
        git.stream_files_then_diff_stats(&tx, true, false, true);
        drop(tx);

        let parts: Vec<_> = rx.into_iter().collect();
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], ModelPart::Files(_)));
        assert!(matches!(&parts[1], ModelPart::FileStats(_)));
        assert!(matches!(&parts[2], ModelPart::DiffStats { .. }));

        let ModelPart::Files(status_files) = &parts[0] else {
            unreachable!();
        };
        assert_eq!(status_files.len(), 1);
        assert_eq!(status_files[0].name, "tracked.txt");
        assert!(status_files[0].tracked);
        assert_eq!(status_files[0].additions, 0);
        assert_eq!(status_files[0].hunk_count, 0);

        let ModelPart::FileStats(enriched_files) = &parts[1] else {
            unreachable!();
        };
        assert_eq!(enriched_files.len(), 2);
        let tracked = enriched_files
            .iter()
            .find(|file| file.name == "tracked.txt")
            .unwrap();
        assert_eq!(tracked.additions, 1);
        assert_eq!(tracked.deletions, 0);
        assert_eq!(tracked.hunk_count, 1);
        assert!(enriched_files.iter().any(|file| {
            file.name == "untracked.txt" && !file.tracked && file.status == model::FileStatus::Untracked
        }));
    }

    fn assert_success(status: std::io::Result<std::process::ExitStatus>) {
        assert!(status.expect("run git command").success());
    }
}
