use std::collections::HashMap;

use anyhow::{Result, anyhow};

use super::GitCommands;
use crate::model::{Remote, RemoteBranch};
use crate::os::cmd::{CmdBuilder, CmdResult};

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubRemoteRepo {
    remote_name: String,
    repo: String,
    owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullStrategy {
    Rebase,
    Merge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    UpToDate {
        upstream: String,
    },
    AheadOnly {
        upstream: String,
        ahead: usize,
    },
    FastForwarded {
        upstream: String,
        behind: usize,
    },
    Diverged {
        upstream: String,
        ahead: usize,
        behind: usize,
    },
    FastForwardOnlyBlocked {
        upstream: String,
        ahead: usize,
        behind: usize,
    },
    NeedsCleanWorktree {
        upstream: String,
        ahead: usize,
        behind: usize,
    },
    Integrated {
        upstream: String,
        strategy: PullStrategy,
        conflicted: bool,
    },
    NoUpstream {
        branch: String,
    },
}

impl GitCommands {
    pub fn load_remotes(&self) -> Result<Vec<Remote>> {
        if let Some(repo) = &self.read_repo
            && let Ok(remotes) = repo.remotes()
        {
            return Ok(remotes
                .into_iter()
                .map(|remote| Remote {
                    name: remote.name,
                    urls: remote.urls,
                    branches: remote
                        .branches
                        .into_iter()
                        .map(|branch| RemoteBranch {
                            name: branch.name,
                            remote_name: branch.remote_name,
                            hash: branch.hash,
                        })
                        .collect(),
                })
                .collect());
        }

        self.load_remotes_cli()
    }

    fn load_remotes_cli(&self) -> Result<Vec<Remote>> {
        let result = self.git().args(&["remote", "-v"]).run()?;

        if !result.success {
            return Ok(Vec::new());
        }

        let mut remotes: Vec<Remote> = Vec::new();
        for line in result.stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }

            let name = parts[0].to_string();
            let url = parts[1].to_string();

            if let Some(existing) = remotes.iter_mut().find(|r| r.name == name) {
                if !existing.urls.contains(&url) {
                    existing.urls.push(url);
                }
            } else {
                remotes.push(Remote {
                    name,
                    urls: vec![url],
                    branches: Vec::new(),
                });
            }
        }

        for remote in &mut remotes {
            remote.branches = self.load_remote_branches(&remote.name)?;
        }

        Ok(remotes)
    }

    fn load_remote_branches(&self, remote_name: &str) -> Result<Vec<RemoteBranch>> {
        let format = "%(refname:short)|%(objectname:short)";
        let result = self
            .git()
            .args(&[
                "for-each-ref",
                &format!("--format={}", format),
                &format!("refs/remotes/{}/", remote_name),
            ])
            .run()?;

        if !result.success {
            return Ok(Vec::new());
        }

        let branches = result
            .stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(2, '|').collect();
                if parts.len() >= 2 {
                    let full_name = parts[0];
                    let branch_name = full_name
                        .strip_prefix(&format!("{}/", remote_name))
                        .unwrap_or(full_name);
                    // Filter out HEAD (explicit or symref that resolves to just the remote name)
                    if branch_name == "HEAD" || branch_name == remote_name {
                        return None;
                    }
                    Some(RemoteBranch {
                        name: branch_name.to_string(),
                        remote_name: remote_name.to_string(),
                        hash: parts[1].to_string(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(branches)
    }

    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.git()
            .args(&["remote", "add", name, url])
            .run_expecting_success()?;
        if let Some(repo) = &self.read_repo {
            let _ = repo.reload();
        }
        Ok(())
    }

    pub fn delete_remote(&self, name: &str) -> Result<()> {
        self.git()
            .args(&["remote", "remove", name])
            .run_expecting_success()?;
        if let Some(repo) = &self.read_repo {
            let _ = repo.reload();
        }
        Ok(())
    }

    pub fn rename_remote(&self, old_name: &str, new_name: &str) -> Result<()> {
        self.git()
            .args(&["remote", "rename", old_name, new_name])
            .run_expecting_success()?;
        if let Some(repo) = &self.read_repo {
            let _ = repo.reload();
        }
        Ok(())
    }

    pub fn update_remote_url(&self, name: &str, url: &str) -> Result<()> {
        self.git()
            .args(&["remote", "set-url", name, url])
            .run_expecting_success()?;
        if let Some(repo) = &self.read_repo {
            let _ = repo.reload();
        }
        Ok(())
    }

    pub fn fetch(&self, remote: &str) -> Result<()> {
        // --no-write-fetch-head: still updates remote-tracking refs, but avoids
        // racing a concurrent `git pull` on .git/FETCH_HEAD (can surface as a
        // spurious "divergent branches" / merge failure on the first p).
        self.git()
            .args(&["fetch", "--no-write-fetch-head", remote])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn fetch_all(&self) -> Result<()> {
        self.git()
            .args(&["fetch", "--all", "--no-write-fetch-head"])
            .run_expecting_success()?;
        Ok(())
    }

    /// Non-interactive fetch for the periodic auto-fetch loop. Suppresses any
    /// terminal/credential prompts so a missing SSH passphrase or stored
    /// credential can't hang the TUI.
    pub fn fetch_all_background(&self) -> Result<bool> {
        // Git's fetch progress output is not a reliable signal that refs moved.
        // Compare the exact ref set instead so a no-op auto-fetch does zero model
        // refresh work. If gix cannot snapshot refs, conservatively report a
        // change and keep the existing refresh behavior.
        let before = self.ref_snapshot().ok();

        self.git()
            .args(&[
                "fetch",
                "--all",
                "--no-write-fetch-head",
                "--quiet",
                "--no-show-forced-updates",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .run_expecting_success()?;

        let Some(before) = before else {
            return Ok(true);
        };
        Ok(self
            .ref_snapshot()
            .map(|after| after != before)
            .unwrap_or(true))
    }

    /// Fetch the current upstream, classify the relationship with HEAD, and
    /// integrate only when the choice is unambiguous (or already configured).
    ///
    /// This intentionally avoids opaque `git pull` failures such as Git asking
    /// the user to choose merge vs rebase. The UI can present that decision as
    /// a first-class menu instead.
    pub fn pull(&self) -> Result<PullOutcome> {
        let upstream = match self.current_upstream_ref() {
            Ok(upstream) => upstream,
            Err(_) => {
                return Ok(PullOutcome::NoUpstream {
                    branch: self.current_branch_name().unwrap_or_default(),
                });
            }
        };

        // Fetch first, but never rely on FETCH_HEAD for integration. Using the
        // configured upstream ref keeps the workflow deterministic and lets us
        // inspect ahead/behind counts before mutating the worktree.
        self.git()
            .args(&[
                "fetch",
                "--quiet",
                "--no-write-fetch-head",
                "--no-show-forced-updates",
            ])
            .run_expecting_success()?;

        let (ahead, behind) = self.diff_refs_ahead_behind("HEAD", &upstream)?;
        if behind == 0 {
            return Ok(if ahead == 0 {
                PullOutcome::UpToDate { upstream }
            } else {
                PullOutcome::AheadOnly { upstream, ahead }
            });
        }

        if self.has_tracked_worktree_changes()? {
            return Ok(PullOutcome::NeedsCleanWorktree {
                upstream,
                ahead,
                behind,
            });
        }

        if ahead == 0 {
            self.fast_forward_upstream(&upstream)?;
            return Ok(PullOutcome::FastForwarded { upstream, behind });
        }

        if self.configured_pull_ff_only()? {
            return Ok(PullOutcome::FastForwardOnlyBlocked {
                upstream,
                ahead,
                behind,
            });
        }

        if let Some(strategy) = self.configured_pull_strategy()? {
            let conflicted = self.integrate_upstream(&upstream, strategy)?;
            return Ok(PullOutcome::Integrated {
                upstream,
                strategy,
                conflicted,
            });
        }

        Ok(PullOutcome::Diverged {
            upstream,
            ahead,
            behind,
        })
    }

    /// Integrate an already-fetched upstream. Returns true when Git stopped on
    /// conflicts but deliberately left merge/rebase state for the conflict UI.
    pub fn integrate_upstream(&self, upstream: &str, strategy: PullStrategy) -> Result<bool> {
        match strategy {
            PullStrategy::Rebase => {
                let result = self.git().args(&["rebase", upstream]).run()?;
                // A conflict intentionally leaves an in-progress rebase for the
                // normal rebase UI to handle. Treat that as a successful start
                // rather than replacing useful conflict state with a raw error.
                if result.success {
                    Ok(false)
                } else if self.is_rebase_in_progress() {
                    Ok(true)
                } else {
                    anyhow::bail!(
                        "Command failed (exit {}): git rebase {}\n\n{}{}",
                        result.exit_code.unwrap_or(-1),
                        upstream,
                        result.stdout,
                        result.stderr
                    )
                }
            }
            PullStrategy::Merge => {
                let result = self
                    .git()
                    .args(&["merge", "--no-stat", upstream])
                    .run()?;
                let merge_in_progress = self
                    .git()
                    .args(&["rev-parse", "-q", "--verify", "MERGE_HEAD"])
                    .run()
                    .map(|probe| probe.success)
                    .unwrap_or(false);
                if result.success {
                    Ok(false)
                } else if merge_in_progress {
                    Ok(true)
                } else {
                    anyhow::bail!(
                        "Command failed (exit {}): git merge --no-stat {}\n\n{}{}",
                        result.exit_code.unwrap_or(-1),
                        upstream,
                        result.stdout,
                        result.stderr
                    )
                }
            }
        }
    }

    pub fn remember_pull_strategy(&self, strategy: PullStrategy) -> Result<()> {
        let value = match strategy {
            PullStrategy::Rebase => "true",
            PullStrategy::Merge => "false",
        };
        self.git()
            .args(&["config", "--local", "pull.rebase", value])
            .run_expecting_success()?;
        Ok(())
    }

    fn current_upstream_ref(&self) -> Result<String> {
        let result = self
            .git()
            .args(&[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{u}",
            ])
            .run()?;
        let upstream = result.stdout.trim();
        if result.success && !upstream.is_empty() {
            Ok(upstream.to_string())
        } else {
            anyhow::bail!(
                "The current branch has no upstream. Push it with 'set upstream' first, then pull."
            )
        }
    }

    fn has_tracked_worktree_changes(&self) -> Result<bool> {
        let result = self
            .git()
            .args(&["status", "--porcelain=v1", "-uno"])
            .run_expecting_success()?;
        Ok(!result.stdout.trim().is_empty())
    }

    fn fast_forward_upstream(&self, upstream: &str) -> Result<()> {
        self.git()
            .args(&["merge", "--ff-only", "--no-stat", upstream])
            .run_expecting_success()?;
        Ok(())
    }

    fn configured_pull_ff_only(&self) -> Result<bool> {
        let result = self.git().args(&["config", "--get", "pull.ff"]).run()?;
        Ok(result.success && result.stdout.trim().eq_ignore_ascii_case("only"))
    }

    fn configured_pull_strategy(&self) -> Result<Option<PullStrategy>> {
        let branch = self.current_branch_name().unwrap_or_default();
        if !branch.is_empty()
            && let Some(strategy) = self.configured_rebase_value(&format!("branch.{branch}.rebase"))?
        {
            return Ok(Some(strategy));
        }
        self.configured_rebase_value("pull.rebase")
    }

    fn configured_rebase_value(&self, key: &str) -> Result<Option<PullStrategy>> {
        let result = self.git().args(&["config", "--get", key]).run()?;
        if !result.success {
            return Ok(None);
        }
        let value = result.stdout.trim().to_ascii_lowercase();
        Ok(match value.as_str() {
            "true" | "yes" | "on" | "1" => Some(PullStrategy::Rebase),
            "false" | "no" | "off" | "0" => Some(PullStrategy::Merge),
            // Values such as `merges` and `interactive` carry semantics beyond
            // a plain rebase. Let the user choose instead of silently changing
            // their history shape.
            _ => None,
        })
    }

    pub fn push(&self, force: bool) -> Result<()> {
        let mut cmd = self.git();
        // Successful push output is captured and discarded by the TUI. Ask Git
        // not to spend time formatting progress/ref summaries we never render.
        cmd = cmd.args(&["push", "--quiet"]);
        if force {
            cmd = cmd.arg("--force-with-lease");
        }
        cmd.run_expecting_success()?;
        Ok(())
    }

    pub fn checkout_remote_branch(&self, remote: &str, branch: &str) -> Result<()> {
        // Creates a local branch tracking the remote branch and checks it out.
        self.git()
            .args(&["checkout", "-b", branch, &format!("{}/{}", remote, branch)])
            .run_expecting_success()?;
        if let Some(repo) = &self.read_repo {
            let _ = repo.reload();
        }
        Ok(())
    }

    pub fn delete_remote_branch(&self, remote: &str, branch: &str) -> Result<()> {
        self.git()
            .args(&["push", "--quiet", remote, "--delete", branch])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn merge_remote_branch(&self, remote: &str, branch: &str, args: &str) -> Result<()> {
        let ref_name = format!("{}/{}", remote, branch);
        let mut cmd = self.git();
        cmd = cmd.arg("merge").arg(&ref_name);
        if !args.is_empty() {
            for arg in args.split_whitespace() {
                cmd = cmd.arg(arg);
            }
        }
        cmd.run_expecting_success()?;
        Ok(())
    }

    pub fn rebase_remote_branch(&self, remote: &str, branch: &str) -> Result<()> {
        self.git()
            .args(&["rebase", &format!("{}/{}", remote, branch)])
            .run_expecting_success()?;
        Ok(())
    }

    pub fn push_with_upstream(&self, remote: &str, branch: &str) -> Result<()> {
        self.git()
            .args(&["push", "--quiet", "-u", remote, branch])
            .run_expecting_success()?;
        if let Some(repo) = &self.read_repo {
            let _ = repo.reload();
        }
        Ok(())
    }

    /// Build a provider-aware web URL for a commit from the origin remote URL.
    ///
    /// `services` mirrors lazygit's services map (`gitDomain: provider:webDomain`)
    /// so self-hosted GitLab/Gitea/etc. instances can override auto-detection.
    pub fn get_commit_url(&self, hash: &str, services: &HashMap<String, String>) -> Result<String> {
        let remote_url = self.origin_remote_url()?;
        commit_web_url(&remote_url, hash, services)
            .ok_or_else(|| anyhow!("unsupported remote URL: {remote_url}"))
    }

    /// Get a browser-friendly URL for the origin remote repository.
    pub fn get_repo_url(&self) -> Result<String> {
        self.get_repo_url_with_services(&HashMap::new())
    }

    /// Get a browser-friendly repository URL, honoring self-hosted service
    /// mappings from the user's lazygit-compatible `services` config.
    pub fn get_repo_url_with_services(&self, services: &HashMap<String, String>) -> Result<String> {
        let remote_url = self.origin_remote_url()?;
        repo_web_url(&remote_url, services)
            .ok_or_else(|| anyhow!("unsupported remote URL: {remote_url}"))
    }

    fn origin_remote_url(&self) -> Result<String> {
        if let Some(repo) = &self.read_repo
            && let Ok(Some(remote_url)) = repo.origin_url()
        {
            return Ok(remote_url);
        }

        let result = self
            .git()
            .args(&["remote", "get-url", "origin"])
            .run_expecting_success()?;
        Ok(result.stdout.trim().to_string())
    }

    /// Build a PR creation URL for a branch (GitHub compare URL).
    pub fn get_pr_create_url(&self, branch: &str) -> Result<String> {
        let base = self.get_repo_url()?;
        Ok(format!("{}/compare/{}?expand=1", base, branch))
    }

    /// Get the PR URL for a branch using `gh pr view`.
    pub fn get_pr_url(&self, branch: &str) -> Result<String> {
        if let Some(url) = self.gh_pr_view_url(branch, None)? {
            return Ok(url);
        }

        let preferred_remote = self.branch_upstream_remote(branch);
        let repos = order_github_remote_repos(
            self.load_github_remote_repos()?,
            preferred_remote.as_deref(),
        );
        let head_selectors = github_pr_head_selectors(branch, &repos);

        for repo in &repos {
            if let Some(url) = self.gh_pr_view_url(branch, Some(&repo.repo))? {
                return Ok(url);
            }

            for head in &head_selectors {
                if let Some(url) = self.gh_pr_list_url(&repo.repo, head)? {
                    return Ok(url);
                }
            }
        }

        anyhow::bail!("No PR found for branch '{}'", branch)
    }

    fn gh_pr_view_url(&self, branch: &str, repo: Option<&str>) -> Result<Option<String>> {
        let mut cmd = CmdBuilder::new("gh")
            .args(&["pr", "view", branch, "--json", "url", "-q", ".url"])
            .cwd_path(self.repo_path());
        if let Some(repo) = repo {
            cmd = cmd.args(&["--repo", repo]);
        }

        Ok(gh_url_from_result(&cmd.run()?))
    }

    fn gh_pr_list_url(&self, repo: &str, head: &str) -> Result<Option<String>> {
        let result = CmdBuilder::new("gh")
            .args(&[
                "pr", "list", "--repo", repo, "--head", head, "--json", "url", "--limit", "1",
                "-q", ".[0].url",
            ])
            .cwd_path(self.repo_path())
            .run()?;

        Ok(gh_url_from_result(&result))
    }

    fn load_github_remote_repos(&self) -> Result<Vec<GithubRemoteRepo>> {
        let mut repos = Vec::new();
        for remote in self.load_remotes()? {
            for url in remote.urls {
                let Some(repo) = github_repo_slug_from_remote_url(&url) else {
                    continue;
                };
                let Some(owner) = repo.split('/').next().map(str::to_string) else {
                    continue;
                };
                if repos
                    .iter()
                    .any(|existing: &GithubRemoteRepo| existing.repo == repo)
                {
                    continue;
                }
                repos.push(GithubRemoteRepo {
                    remote_name: remote.name.clone(),
                    repo,
                    owner,
                });
            }
        }
        Ok(repos)
    }

    fn branch_upstream_remote(&self, branch: &str) -> Option<String> {
        let ref_name = format!("refs/heads/{}", branch);
        let result = self
            .git()
            .args(&["for-each-ref", "--format=%(upstream:short)", &ref_name])
            .run()
            .ok()?;
        if !result.success {
            return None;
        }

        result
            .stdout
            .trim()
            .split_once('/')
            .map(|(remote, _)| remote.to_string())
            .filter(|remote| !remote.is_empty())
    }
}

impl GitCommands {
    /// Get the origin repo HTTPS URL, returning an empty string on failure.
    pub fn load_repo_url(&self) -> String {
        self.get_repo_url().unwrap_or_default()
    }

    /// Load top contributors using `git shortlog -sn` capped to recent commits.
    ///
    /// Contributor data is informational and relatively expensive to rebuild on
    /// large histories. Cache the exact Git result by HEAD and, importantly,
    /// checkout refreshes no longer request it at all. Keeping Git as the source
    /// here preserves shortlog/mailmap semantics while still removing the hot-path
    /// recomputation that showed up in the checkout trace.
    pub fn load_contributors(&self, max_commits: usize, top_n: usize) -> Vec<(String, usize)> {
        let head = self.read_head_state().ok().map(|head| head.hash);
        if let Some(head) = head.as_ref() {
            let key = (head.clone(), max_commits, top_n);
            if let Some(cached) = self
                .contributor_cache
                .lock()
                .expect("contributor cache poisoned")
                .get(&key)
                .cloned()
            {
                return cached;
            }
        }

        let contributors = self.load_contributors_cli(max_commits, top_n);
        if let Some(head) = head {
            let mut cache = self
                .contributor_cache
                .lock()
                .expect("contributor cache poisoned");
            // This cache is informational and keyed by HEAD. Keep it tiny so
            // long branch-hopping sessions cannot retain one contributor list
            // for every historical head visited.
            if cache.len() >= 16 {
                cache.clear();
            }
            cache.insert((head, max_commits, top_n), contributors.clone());
        }
        contributors
    }

    fn load_contributors_cli(&self, max_commits: usize, top_n: usize) -> Vec<(String, usize)> {
        let max_arg = format!("--max-count={}", max_commits);
        let result = self
            .git()
            .args(&["shortlog", "-sn", "-e", "HEAD", &max_arg])
            .env("GIT_PAGER", "cat")
            .run();
        let Ok(result) = result else {
            return Vec::new();
        };
        if !result.success {
            return Vec::new();
        }

        let mut out: Vec<(String, usize)> = Vec::new();
        for line in result.stdout.lines() {
            let trimmed = line.trim_start();
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let count_str = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            let Ok(count) = count_str.parse::<usize>() else {
                continue;
            };
            let name = match rest.rfind('<') {
                Some(i) => rest[..i].trim().to_string(),
                None => rest.to_string(),
            };
            if name.is_empty() {
                continue;
            }
            out.push((name, count));
            if out.len() >= top_n {
                break;
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebProvider {
    GithubLike,
    Gitlab,
    Bitbucket,
    Cgit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteLocation {
    host: String,
    web_authority: String,
    web_scheme: String,
    path: String,
}

/// Turn common Git transport URLs into enough information to construct a web URL.
/// SSH/git transports intentionally switch to HTTPS; HTTP(S) remotes keep their
/// original scheme so local/self-hosted HTTP instances continue to work.
fn parse_remote_location(url: &str) -> Option<RemoteLocation> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }

    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let path = path.trim_start_matches('/').to_string();
        if authority.is_empty() || path.is_empty() {
            return None;
        }

        let (host, web_authority) = match scheme {
            "http" | "https" => {
                let host = authority_host(authority).to_string();
                (host, authority.to_string())
            }
            "ssh" | "git" | "git+ssh" => {
                let host = authority_host(authority).to_string();
                (host.clone(), host)
            }
            _ => return None,
        };
        if host.is_empty() {
            return None;
        }

        let web_scheme = if matches!(scheme, "http" | "https") {
            scheme.to_string()
        } else {
            "https".to_string()
        };

        return Some(RemoteLocation {
            host,
            web_authority,
            web_scheme,
            path,
        });
    }

    // SCP-style SSH URL: git@host:owner/repo.git (also supports host:path).
    let (authority, path) = url.split_once(':')?;
    if authority.contains('/') || path.is_empty() {
        return None;
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if host.is_empty() {
        return None;
    }

    Some(RemoteLocation {
        host: host.to_string(),
        web_authority: host.to_string(),
        web_scheme: "https".to_string(),
        path: path.trim_start_matches('/').to_string(),
    })
}

fn authority_host(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .split_once(']')
            .map(|(host, _)| host.trim_start_matches('['))
            .unwrap_or(authority);
    }
    authority
        .split_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority)
}

fn auto_provider(host: &str) -> WebProvider {
    let host = host.to_ascii_lowercase();
    if host == "git.kernel.org" {
        WebProvider::Cgit
    } else if host == "gitlab.com" || host.contains("gitlab") {
        WebProvider::Gitlab
    } else if host == "bitbucket.org" {
        WebProvider::Bitbucket
    } else {
        // GitHub, Gitea, Forgejo, Codeberg, SourceHut and many lightweight
        // self-hosted forges use /commit/<oid>. Unknown hosts get this common
        // convention, while `services` can override providers that differ.
        WebProvider::GithubLike
    }
}

fn configured_service(
    host: &str,
    services: &HashMap<String, String>,
) -> Option<(WebProvider, String)> {
    let value = services.get(host)?;
    let (provider, web_domain) = value.split_once(':')?;
    let provider = match provider.trim().to_ascii_lowercase().as_str() {
        "github" | "gitea" | "codeberg" | "forgejo" | "gogs" => WebProvider::GithubLike,
        "gitlab" => WebProvider::Gitlab,
        "bitbucket" => WebProvider::Bitbucket,
        "cgit" => WebProvider::Cgit,
        _ => return None,
    };
    let web_domain = web_domain.trim().trim_end_matches('/');
    if web_domain.is_empty() {
        return None;
    }
    Some((provider, web_domain.to_string()))
}

fn web_root(location: &RemoteLocation, configured_domain: Option<&str>) -> String {
    if let Some(domain) = configured_domain {
        if domain.starts_with("http://") || domain.starts_with("https://") {
            return domain.trim_end_matches('/').to_string();
        }
        return format!("https://{}", domain.trim_end_matches('/'));
    }
    format!("{}://{}", location.web_scheme, location.web_authority)
}

fn repo_web_url(url: &str, services: &HashMap<String, String>) -> Option<String> {
    let location = parse_remote_location(url)?;
    let configured = configured_service(&location.host, services);
    let provider = configured
        .as_ref()
        .map(|(provider, _)| *provider)
        .unwrap_or_else(|| auto_provider(&location.host));
    let root = web_root(
        &location,
        configured.as_ref().map(|(_, domain)| domain.as_str()),
    );

    let mut path = location.path.trim_matches('/').to_string();
    if provider == WebProvider::Cgit {
        // kernel.org's cgit web path includes the .git suffix even though many
        // clone URLs/tools normalize it away.
        if location.host == "git.kernel.org" && !path.ends_with(".git") {
            path.push_str(".git");
        }
    } else if path.ends_with(".git") {
        path.truncate(path.len() - 4);
    }

    Some(format!("{}/{}", root.trim_end_matches('/'), path))
}

fn commit_web_url(url: &str, hash: &str, services: &HashMap<String, String>) -> Option<String> {
    let location = parse_remote_location(url)?;
    let provider = configured_service(&location.host, services)
        .map(|(provider, _)| provider)
        .unwrap_or_else(|| auto_provider(&location.host));
    let base = repo_web_url(url, services)?;

    Some(match provider {
        WebProvider::GithubLike => format!("{base}/commit/{hash}"),
        WebProvider::Gitlab => format!("{base}/-/commit/{hash}"),
        WebProvider::Bitbucket => format!("{base}/commits/{hash}"),
        WebProvider::Cgit => format!("{base}/commit/?id={hash}"),
    })
}

fn gh_url_from_result(result: &CmdResult) -> Option<String> {
    let url = result.stdout.trim();
    if result.success && !url.is_empty() && url != "null" {
        Some(url.to_string())
    } else {
        None
    }
}

#[cfg(test)]
fn github_remote_repos_from_remote_v_output(output: &str) -> Vec<GithubRemoteRepo> {
    let mut repos = Vec::new();

    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let Some(remote_name) = parts.next() else {
            continue;
        };
        let Some(url) = parts.next() else {
            continue;
        };
        let Some(repo) = github_repo_slug_from_remote_url(url) else {
            continue;
        };
        let Some(owner) = repo.split('/').next().map(|owner| owner.to_string()) else {
            continue;
        };

        if repos
            .iter()
            .any(|existing: &GithubRemoteRepo| existing.repo == repo)
        {
            continue;
        }

        repos.push(GithubRemoteRepo {
            remote_name: remote_name.to_string(),
            repo,
            owner,
        });
    }

    repos
}

fn github_repo_slug_from_remote_url(url: &str) -> Option<String> {
    let path = github_path_from_remote_url(url)?;
    let mut parts = path.trim_matches('/').split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches(".git");

    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    Some(format!("{}/{}", owner, repo))
}

fn github_path_from_remote_url(url: &str) -> Option<&str> {
    let url = url.trim().trim_end_matches('/');

    if let Some(path) = url.strip_prefix("git@github.com:") {
        return Some(path);
    }

    for scheme in ["https://", "http://", "ssh://"] {
        let Some(rest) = url.strip_prefix(scheme) else {
            continue;
        };
        let Some((authority, path)) = rest.split_once('/') else {
            continue;
        };
        let host = authority.rsplit('@').next().unwrap_or(authority);
        if host == "github.com" {
            return Some(path);
        }
    }

    None
}

fn order_github_remote_repos(
    mut repos: Vec<GithubRemoteRepo>,
    preferred_remote: Option<&str>,
) -> Vec<GithubRemoteRepo> {
    let mut ordered = Vec::new();

    if let Some(preferred_remote) = preferred_remote {
        move_repos_for_remote(&mut repos, &mut ordered, preferred_remote);
    }
    move_repos_for_remote(&mut repos, &mut ordered, "upstream");
    move_repos_for_remote(&mut repos, &mut ordered, "origin");
    ordered.extend(repos);

    ordered
}

fn move_repos_for_remote(
    repos: &mut Vec<GithubRemoteRepo>,
    ordered: &mut Vec<GithubRemoteRepo>,
    remote_name: &str,
) {
    let mut i = 0;
    while i < repos.len() {
        if repos[i].remote_name == remote_name {
            ordered.push(repos.remove(i));
        } else {
            i += 1;
        }
    }
}

fn github_pr_head_selectors(branch: &str, repos: &[GithubRemoteRepo]) -> Vec<String> {
    let mut selectors = Vec::from([branch.to_string()]);

    for repo in repos {
        let selector = format!("{}:{}", repo.owner, branch);
        if !selectors.contains(&selector) {
            selectors.push(selector);
        }
    }

    selectors
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
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
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn configure_identity(repo: &Path) {
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
    }

    #[test]
    fn guided_pull_classifies_fast_forward_ahead_and_divergence() {
        let temp = TempDir::new("guided-pull");
        let remote = temp.path.join("remote.git");
        let seed = temp.path.join("seed");
        let local = temp.path.join("local");
        let peer = temp.path.join("peer");
        std::fs::create_dir_all(&seed).expect("create seed");

        let remote_arg = remote.to_string_lossy().to_string();
        let local_arg = local.to_string_lossy().to_string();
        let peer_arg = peer.to_string_lossy().to_string();

        git(
            &temp.path,
            &["init", "-q", "--bare", "--initial-branch=main", &remote_arg],
        );
        git(&seed, &["init", "-q", "--initial-branch=main"]);
        configure_identity(&seed);
        std::fs::write(seed.join("file"), "base\n").expect("write seed");
        git(&seed, &["add", "file"]);
        git(&seed, &["commit", "-q", "-m", "base"]);
        git(&seed, &["remote", "add", "origin", &remote_arg]);
        git(&seed, &["push", "-q", "-u", "origin", "main"]);

        git(&temp.path, &["clone", "-q", &remote_arg, &local_arg]);
        git(&temp.path, &["clone", "-q", &remote_arg, &peer_arg]);
        configure_identity(&local);
        configure_identity(&peer);

        let commands = GitCommands::new(&local).expect("open local clone");
        assert!(matches!(
            commands.pull().expect("up-to-date pull"),
            PullOutcome::UpToDate { .. }
        ));

        std::fs::write(peer.join("remote-only"), "remote\n").expect("write peer change");
        git(&peer, &["add", "remote-only"]);
        git(&peer, &["commit", "-q", "-m", "remote"]);
        git(&peer, &["push", "-q"]);

        std::fs::write(local.join("file"), "dirty\n").expect("dirty tracked file");
        assert!(matches!(
            commands.pull().expect("dirty pull preflight"),
            PullOutcome::NeedsCleanWorktree { behind: 1, .. }
        ));
        git(&local, &["restore", "file"]);

        assert!(matches!(
            commands.pull().expect("fast-forward pull"),
            PullOutcome::FastForwarded { behind: 1, .. }
        ));

        std::fs::write(local.join("local-only"), "local\n").expect("write local change");
        git(&local, &["add", "local-only"]);
        git(&local, &["commit", "-q", "-m", "local"]);
        assert!(matches!(
            commands.pull().expect("ahead-only pull"),
            PullOutcome::AheadOnly { ahead: 1, .. }
        ));

        std::fs::write(peer.join("remote-two"), "remote two\n").expect("write peer two");
        git(&peer, &["add", "remote-two"]);
        git(&peer, &["commit", "-q", "-m", "remote two"]);
        git(&peer, &["push", "-q"]);

        // Override any global pull.rebase preference so this test exercises the
        // UI-decision path deterministically. Unknown values are intentionally
        // treated as "ask" by the guided pull implementation.
        git(&local, &["config", "pull.rebase", "ask"]);
        git(&local, &["config", "branch.main.rebase", "ask"]);
        git(&local, &["config", "pull.ff", "false"]);
        assert!(matches!(
            commands.pull().expect("diverged pull"),
            PullOutcome::Diverged {
                ahead: 1,
                behind: 1,
                ..
            }
        ));
    }

    #[test]
    fn builds_browser_urls_for_common_git_transports_and_forges() {
        let services = HashMap::new();

        assert_eq!(
            repo_web_url("git@github.com:owner/repo.git", &services),
            Some("https://github.com/owner/repo".to_string())
        );
        assert_eq!(
            commit_web_url(
                "ssh://git@gitlab.com/group/project.git",
                "deadbeef",
                &services,
            ),
            Some("https://gitlab.com/group/project/-/commit/deadbeef".to_string())
        );
        assert_eq!(
            commit_web_url(
                "git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
                "4feafef66a0f7b071e8aea653ca441b94769296f",
                &services,
            ),
            Some(
                "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id=4feafef66a0f7b071e8aea653ca441b94769296f"
                    .to_string()
            )
        );
        assert_eq!(
            commit_web_url(
                "https://bitbucket.org/workspace/repo.git",
                "deadbeef",
                &services,
            ),
            Some("https://bitbucket.org/workspace/repo/commits/deadbeef".to_string())
        );
    }

    #[test]
    fn services_override_provider_and_web_domain_for_self_hosted_forges() {
        let mut services = HashMap::new();
        services.insert(
            "git.internal.example".to_string(),
            "gitlab:code.example.com".to_string(),
        );

        assert_eq!(
            commit_web_url(
                "ssh://git@git.internal.example/team/project.git",
                "cafebabe",
                &services,
            ),
            Some("https://code.example.com/team/project/-/commit/cafebabe".to_string())
        );
    }

    #[test]
    fn parses_github_repo_slugs_from_common_remote_urls() {
        assert_eq!(
            github_repo_slug_from_remote_url("https://github.com/Blankeos/zed.git"),
            Some("Blankeos/zed".to_string())
        );
        assert_eq!(
            github_repo_slug_from_remote_url("git@github.com:zed-industries/zed.git"),
            Some("zed-industries/zed".to_string())
        );
        assert_eq!(
            github_repo_slug_from_remote_url("ssh://git@github.com/zed-industries/zed.git"),
            Some("zed-industries/zed".to_string())
        );
        assert_eq!(
            github_repo_slug_from_remote_url("https://token@github.com/owner/repo.git"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            github_repo_slug_from_remote_url("https://gitlab.com/owner/repo.git"),
            None
        );
    }

    #[test]
    fn loads_unique_github_remote_repos_from_remote_v_output() {
        let output = "\
origin\thttps://github.com/Blankeos/zed.git (fetch)
origin\thttps://github.com/Blankeos/zed.git (push)
upstream\thttps://github.com/zed-industries/zed.git (fetch)
upstream\thttps://github.com/zed-industries/zed.git (push)
gitlab\thttps://gitlab.com/example/zed.git (fetch)
";

        let repos = github_remote_repos_from_remote_v_output(output);

        assert_eq!(
            repos,
            vec![
                GithubRemoteRepo {
                    remote_name: "origin".to_string(),
                    repo: "Blankeos/zed".to_string(),
                    owner: "Blankeos".to_string(),
                },
                GithubRemoteRepo {
                    remote_name: "upstream".to_string(),
                    repo: "zed-industries/zed".to_string(),
                    owner: "zed-industries".to_string(),
                },
            ]
        );
    }

    #[test]
    fn orders_repos_by_preferred_remote_then_common_fallbacks() {
        let repos = vec![
            GithubRemoteRepo {
                remote_name: "origin".to_string(),
                repo: "me/project".to_string(),
                owner: "me".to_string(),
            },
            GithubRemoteRepo {
                remote_name: "mirror".to_string(),
                repo: "mirror/project".to_string(),
                owner: "mirror".to_string(),
            },
            GithubRemoteRepo {
                remote_name: "upstream".to_string(),
                repo: "org/project".to_string(),
                owner: "org".to_string(),
            },
        ];

        let ordered = order_github_remote_repos(repos, Some("origin"));
        let names: Vec<_> = ordered
            .iter()
            .map(|repo| repo.remote_name.as_str())
            .collect();

        assert_eq!(names, vec!["origin", "upstream", "mirror"]);
    }

    #[test]
    fn builds_head_selectors_from_remote_owners() {
        let repos = vec![
            GithubRemoteRepo {
                remote_name: "upstream".to_string(),
                repo: "zed-industries/zed".to_string(),
                owner: "zed-industries".to_string(),
            },
            GithubRemoteRepo {
                remote_name: "origin".to_string(),
                repo: "Blankeos/zed".to_string(),
                owner: "Blankeos".to_string(),
            },
        ];

        assert_eq!(
            github_pr_head_selectors("feature/foo", &repos),
            vec![
                "feature/foo".to_string(),
                "zed-industries:feature/foo".to_string(),
                "Blankeos:feature/foo".to_string(),
            ]
        );
    }
}
