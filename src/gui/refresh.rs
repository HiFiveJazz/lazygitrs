//! Model refresh orchestration and refresh-specific cache invalidation.
//!
//! This module deliberately preserves the existing streaming/targeted refresh
//! implementation; it only gives that concurrency-sensitive code one home.

use super::*;

impl Gui {
    pub(crate) fn sync_rebase_progress_view(&mut self) -> bool {
        let was_active_in_progress =
            self.rebase_mode.active && self.rebase_mode.phase == RebasePhase::InProgress;
        let previous_current_hash = if was_active_in_progress {
            self.rebase_mode
                .entries
                .iter()
                .find(|entry| entry.status == EntryStatus::Current)
                .map(|entry| entry.hash.clone())
        } else {
            None
        };
        let previous_selected_hash = if was_active_in_progress {
            self.rebase_mode
                .entries
                .get(self.rebase_mode.selected)
                .map(|entry| entry.hash.clone())
        } else {
            None
        };
        let previous_scroll = self.rebase_mode.scroll;

        let Some(mut progress) = self.git.parse_rebase_progress() else {
            return false;
        };
        self.git.hydrate_progress(&mut progress);
        self.rebase_mode.enter_in_progress(&progress);

        let current_hash = self
            .rebase_mode
            .entries
            .iter()
            .find(|entry| entry.status == EntryStatus::Current)
            .map(|entry| entry.hash.clone());

        if was_active_in_progress
            && previous_current_hash.is_some()
            && previous_current_hash == current_hash
        {
            if let Some(selected_hash) = previous_selected_hash {
                if let Some(selected) = self
                    .rebase_mode
                    .entries
                    .iter()
                    .position(|entry| entry.hash == selected_hash)
                {
                    self.rebase_mode.selected = selected;
                    let list_len = self.rebase_mode.entries.len() + 1;
                    let max_scroll = list_len.saturating_sub(self.rebase_mode.visible_height);
                    self.rebase_mode.scroll = previous_scroll.min(max_scroll);
                    self.rebase_mode
                        .ensure_visible(self.rebase_mode.visible_height);
                }
            }
        }

        true
    }

    /// Reserve the shared streaming-refresh lane and install its receiver.
    /// Individual refresh kinds only need to describe their expected parts and
    /// any cache invalidation specific to that operation.
    pub(super) fn begin_streaming_refresh(
        &mut self,
        expected_parts: usize,
        clear_pending_refresh: bool,
    ) -> Option<mpsc::Sender<ModelPart>> {
        if self.refresh_in_progress || self.initial_load_rx.is_some() {
            return None;
        }

        let (tx, rx) = mpsc::channel();
        self.initial_load_rx = Some(rx);
        self.initial_load_received = 0;
        self.initial_load_expected = expected_parts;
        self.refresh_in_progress = true;
        self.checkout_refresh_active = false;
        if clear_pending_refresh {
            self.needs_refresh = false;
        }
        Some(tx)
    }

    /// Kick off a full model reload on a background thread (same streaming
    /// path as initial load). UI stays responsive; panels fill as parts arrive.
    pub(super) fn start_background_refresh(&mut self) {
        let Some(tx) = self.begin_streaming_refresh(MODEL_PART_COUNT, true) else {
            return;
        };
        self.reset_commit_pagination();
        self.diff_preview_cache.retain_immutable();
        let git = Arc::clone(&self.git);
        let commit_filter = self.commit_filter_for_load();
        std::thread::spawn(move || {
            git.load_model_streaming(&tx, commit_filter, false);
        });
    }

    pub(super) fn start_background_periodic_refresh(&mut self) -> bool {
        let Some(tx) = self.begin_streaming_refresh(PERIODIC_MODEL_PART_COUNT, false) else {
            return false;
        };

        let git = Arc::clone(&self.git);

        std::thread::spawn(move || {
            git.load_model_periodic_refresh_streaming(&tx);
        });

        true
    }

    /// Kick off the minimal refresh required after a successful push.
    ///
    /// A push cannot modify the local worktree or local commit topology, so the
    /// only model state that needs to catch up is branch tracking metadata,
    /// remote-tracking refs, and pushed/unpushed commit decorations.
    pub(super) fn start_background_push_refresh(&mut self) -> bool {
        let Some(tx) = self.begin_streaming_refresh(PUSH_MODEL_PART_COUNT, true) else {
            return false;
        };

        let git = Arc::clone(&self.git);
        std::thread::spawn(move || {
            git.load_model_after_push_streaming(&tx);
        });

        true
    }

    pub(super) fn start_background_fetch_refresh(&mut self) -> bool {
        let Some(tx) = self.begin_streaming_refresh(FETCH_MODEL_PART_COUNT, true) else {
            return false;
        };

        self.reset_commit_pagination();

        let previous_tag_remote_state = {
            let model = self.model.lock().unwrap();
            model
                .tags
                .iter()
                .map(|tag| (tag.name.clone(), tag.on_remote))
                .collect::<std::collections::HashMap<_, _>>()
        };

        let git = Arc::clone(&self.git);
        let commit_filter = self.commit_filter_for_load();

        std::thread::spawn(move || {
            git.load_model_after_fetch_streaming(&tx, commit_filter, previous_tag_remote_state);
        });

        true
    }

    pub(super) fn commit_filter_for_load(&self) -> Option<crate::git::commit::CommitFilter> {
        let has = self.commit_path_filter.is_some()
            || !self.commit_author_filter.is_empty()
            || !self.commit_branch_filter.is_empty();
        has.then(|| crate::git::commit::CommitFilter {
            branches: self.commit_branch_filter.clone(),
            path: self.commit_path_filter.clone(),
            authors: self.commit_author_filter.clone(),
        })
    }

    #[cfg(test)]
    pub(super) fn refresh(&mut self) -> Result<()> {
        self.reset_commit_pagination();
        self.diff_preview_cache.retain_immutable();
        let new_model = self.git.load_model()?;
        {
            let mut model = self.model.lock().unwrap();
            model.replace_keeping_file_order(new_model);
        }
        self.after_model_refresh()
    }

    /// Re-apply selection-dependent views after the model was reloaded
    /// (blocking `refresh` or background streaming refresh).
    pub(super) fn after_model_refresh(&mut self) -> Result<()> {
        // Commits arrive already filtered via load_model_streaming(commit_filter)
        // or reload_filtered_commits_async — don't re-fetch here.
        self.subview_refresh_generation = self.subview_refresh_generation.wrapping_add(1);
        let generation = self.subview_refresh_generation;

        let is_rebasing = {
            let mut model = self.model.lock().unwrap();
            self.commit_history_complete = model.commits.len() < DEFAULT_COMMIT_LIMIT;

            if self.show_file_tree {
                self.file_tree_nodes = build_file_tree(&model.files, &self.collapsed_dirs);
                self.context_mgr.files_list_len_override = Some(self.file_tree_nodes.len());
            } else {
                self.file_tree_nodes.clear();
                self.context_mgr.files_list_len_override = None;
            }

            // Remote-branch rows are already present in the refreshed model;
            // copying the selected remote's list is cheap and purely in-memory.
            if !self.remote_branches_name.is_empty()
                && (self.context_mgr.active() == ContextId::RemoteBranches
                    || ((self.context_mgr.active() == ContextId::BranchCommits
                        || self.context_mgr.active() == ContextId::BranchCommitFiles)
                        && self.sub_commits_parent_context == ContextId::RemoteBranches))
                && let Some(remote) = model
                    .remotes
                    .iter()
                    .find(|r| r.name == self.remote_branches_name)
            {
                model.sub_remote_branches = remote.branches.clone();
            }

            model.is_rebasing
        };

        // Refreshing the whole model used to synchronously call git again for
        // these selection-dependent views. On large repositories that created
        // a visible hitch *after* the background refresh had supposedly ended.
        if (self.context_mgr.active() == ContextId::BranchCommits
            || self.context_mgr.active() == ContextId::BranchCommitFiles)
            && !self.branch_commits_name.is_empty()
        {
            let branch = self.branch_commits_name.clone();
            let git = Arc::clone(&self.git);
            let tx = self.ui_job_tx.clone();
            std::thread::spawn(move || {
                let result = git.load_commits_for_branch(&branch, 300);
                let _ = tx.send(Box::new(move |gui: &mut Gui| {
                    if gui.subview_refresh_generation != generation
                        || gui.branch_commits_name != branch
                        || !matches!(
                            gui.context_mgr.active(),
                            ContextId::BranchCommits | ContextId::BranchCommitFiles
                        )
                    {
                        return;
                    }
                    match result {
                        Ok(commits) => {
                            gui.model.lock().unwrap().set_sub_commits(commits);
                            gui.needs_diff_refresh = true;
                        }
                        Err(err) => gui.show_error("Branch commits refresh failed", err),
                    }
                }) as UiJob);
            });
        }

        if matches!(
            self.context_mgr.active(),
            ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles
        ) && !self.commit_files_hash.is_empty()
        {
            let hash = self.commit_files_hash.clone();
            let git = Arc::clone(&self.git);
            let tx = self.ui_job_tx.clone();
            std::thread::spawn(move || {
                let result = git.commit_files(&hash);
                let _ = tx.send(Box::new(move |gui: &mut Gui| {
                    if gui.subview_refresh_generation != generation
                        || gui.commit_files_hash != hash
                        || !matches!(
                            gui.context_mgr.active(),
                            ContextId::CommitFiles
                                | ContextId::StashFiles
                                | ContextId::BranchCommitFiles
                        )
                    {
                        return;
                    }
                    match result {
                        Ok(commit_files) => {
                            {
                                gui.model.lock().unwrap().commit_files = commit_files;
                            }
                            if gui.show_commit_file_tree {
                                let model = gui.model.lock().unwrap();
                                gui.commit_file_tree_nodes =
                                    crate::model::file_tree::build_commit_file_tree(
                                        &model.commit_files,
                                        &gui.commit_files_collapsed_dirs,
                                    );
                                gui.context_mgr.commit_files_list_len_override =
                                    Some(gui.commit_file_tree_nodes.len());
                            }
                            gui.needs_diff_refresh = true;
                        }
                        Err(err) => gui.show_error("Commit files refresh failed", err),
                    }
                }) as UiJob);
            });
        }

        // Keep rebase-mode state synchronized with the refreshed repository
        // status. This is intentionally immediate even though selection-dependent
        // subviews above are refreshed asynchronously: finishing/aborting a rebase
        // must update the modal state and deferred notification queue in this tick.
        if is_rebasing {
            let should_open = !self.rebase_mode.active && !self.rebase_mode.in_progress_dismissed;
            let should_resync =
                self.rebase_mode.active && self.rebase_mode.phase == RebasePhase::InProgress;
            if should_open || should_resync {
                self.sync_rebase_progress_view();
            }
        }
        if !is_rebasing
            && self.rebase_mode.active
            && self.rebase_mode.phase == RebasePhase::InProgress
        {
            let branch = self.rebase_mode.branch_name.clone();
            let count = self.rebase_mode.total_count;
            self.rebase_mode.exit();
            self.show_notification(PopupState::Message {
                title: "Rebase ended".to_string(),
                message: format!(
                    "Rebase of '{}' is no longer in progress ({} commit{}). It may have completed or been aborted.",
                    branch,
                    count,
                    if count == 1 { "" } else { "s" },
                ),
                kind: MessageKind::Info,
            });
        }
        if !is_rebasing && self.rebase_mode.in_progress_dismissed {
            self.rebase_mode.in_progress_dismissed = false;
        }

        self.last_refresh_at = Instant::now();
        Ok(())
    }

    /// Fast refresh after creating exactly one new commit at HEAD.
    ///
    /// Returns false whenever the current model cannot be updated safely;
    /// callers then fall back to the normal full history refresh.
    pub(super) fn try_fast_append_refresh(&mut self) -> bool {
        if self.refresh_in_progress
            || self.initial_load_rx.is_some()
            || self.commit_filter_for_load().is_some()
        {
            return false;
        }

        let (old_head, old_first, head_branch, operation_in_progress) = {
            let model = self.model.lock().unwrap();

            (
                model.head_hash.clone(),
                model.commits.first().map(|commit| commit.hash.clone()),
                model.head_branch_name.clone(),
                model.is_rebasing
                    || model.is_merging
                    || model.is_cherry_picking
                    || model.is_bisecting,
            )
        };

        // Finishing a merge/rebase/cherry-pick can have wider history effects.
        if operation_in_progress {
            return false;
        }

        let new_commit = match self.git.load_head_commit() {
            Ok(Some(commit)) => commit,
            _ => return false,
        };

        let new_hash = new_commit.hash.clone();

        if new_hash == old_head {
            return false;
        }

        if old_head.is_empty() {
            // Unborn repository: only fast-path when the loaded list was empty.
            if old_first.is_some() {
                return false;
            }
        } else {
            // Active path/author/branch filters can hide HEAD from the first row.
            // Only prepend when the old HEAD itself was already visible first.
            if old_first.as_deref() != Some(old_head.as_str()) {
                return false;
            }

            // A normal commit must be exactly one new first-parent child.
            let parent = match self.git.first_parent_hash(&new_hash) {
                Ok(Some(parent)) => parent,
                _ => return false,
            };

            if parent != old_head {
                return false;
            }
        }

        // This is the only substantial command on the fast path (~100 ms on
        // the Linux kernel tree); a full history reload is deliberately absent.
        let files = match self.git.refresh_files_status_only() {
            Ok(files) => files,
            Err(_) => return false,
        };

        let (added, deleted) = self.git.worktree_diff_totals(&files).unwrap_or((0, 0));

        // Refresh branch tracking metadata as well as the tip hash. This is
        // cheap, and keeps the current branch's ↑ahead/↓behind indicator
        // correct immediately after committing.
        let branches = match self.git.load_branches() {
            Ok(branches) => branches,
            Err(_) => return false,
        };

        {
            let mut model = self.model.lock().unwrap();

            // Re-check the state after running git commands so a concurrent
            // refresh cannot make the append decision stale.
            if model.head_hash != old_head {
                return false;
            }

            if !old_head.is_empty()
                && model.commits.first().map(|commit| commit.hash.as_str())
                    != Some(old_head.as_str())
            {
                return false;
            }

            // The old HEAD remains in the list, but the local HEAD/branch
            // decoration moved to the new commit. Keep unrelated decorations
            // (for example origin/main when it still points at the old tip).
            if let Some(old_commit) = model.commits.first_mut() {
                if head_branch.is_empty() {
                    old_commit.refs.retain(|reference| reference != "HEAD");
                } else {
                    let head_ref = format!("HEAD -> {head_branch}");
                    old_commit
                        .refs
                        .retain(|reference| reference != &head_ref && reference != &head_branch);
                }
            }

            model.prepend_commit(new_commit);
            model.head_hash = new_hash;
            model.branches = branches;

            model.set_files(files);
            model.total_additions = added;
            model.total_deletions = deleted;
        }

        // `set_files()` above may have removed the last working-tree file.
        // Keep the cached tree and its list-length override in lockstep with
        // the model before the next frame renders it.
        self.rebuild_file_tree_from_model();

        // Invalidate any in-flight pagination request based on the old list.
        // We intentionally do NOT truncate the list: if it was 300 rows it is
        // now 301, so the next page naturally starts at skip=301.
        self.reset_commit_pagination();

        // Commit diffs are immutable, so existing cached historical previews
        // remain valid. The selected working-tree diff is not.
        self.diff_preview_cache.retain_immutable();
        self.needs_diff_refresh = true;

        true
    }

    pub(crate) fn request_append_refresh(&mut self) {
        if !self.try_fast_append_refresh() {
            self.request_history_refresh();
        }
    }

    /// Fast refresh after rewriting only the current HEAD commit (amend/reword).
    /// Falls back unless the old HEAD was the first visible global-history row
    /// and the rewrite preserved its complete parent list.
    pub(super) fn try_fast_head_rewrite_refresh(&mut self) -> bool {
        if self.refresh_in_progress
            || self.initial_load_rx.is_some()
            || self.commit_filter_for_load().is_some()
        {
            return false;
        }

        let (old_head, old_first, old_parents, operation_in_progress) = {
            let model = self.model.lock().unwrap();
            let first = model.commits.first();
            (
                model.head_hash.clone(),
                first.map(|commit| commit.hash.clone()),
                first
                    .map(|commit| commit.parents.clone())
                    .unwrap_or_default(),
                model.is_rebasing
                    || model.is_merging
                    || model.is_cherry_picking
                    || model.is_bisecting,
            )
        };

        if operation_in_progress
            || old_head.is_empty()
            || old_first.as_deref() != Some(old_head.as_str())
        {
            return false;
        }

        let new_commit = match self.git.load_head_commit() {
            Ok(Some(commit)) => commit,
            _ => return false,
        };
        let new_hash = new_commit.hash.clone();
        if new_hash == old_head || new_commit.parents != old_parents {
            return false;
        }

        let files = match self.git.refresh_files_status_only() {
            Ok(files) => files,
            Err(_) => return false,
        };
        let (added, deleted) = self.git.worktree_diff_totals(&files).unwrap_or((0, 0));
        let branches = match self.git.load_branches() {
            Ok(branches) => branches,
            Err(_) => return false,
        };

        {
            let mut model = self.model.lock().unwrap();
            if model.head_hash != old_head
                || model.commits.first().map(|commit| commit.hash.as_str())
                    != Some(old_head.as_str())
            {
                return false;
            }

            if !model.replace_head_commit(new_commit) {
                return false;
            }
            model.head_hash = new_hash;
            model.branches = branches;
            model.set_files(files);
            model.total_additions = added;
            model.total_deletions = deleted;
        }

        // Amend/rewrite can also consume the final staged working-tree file,
        // so refresh the cached file tree before the next render.
        self.rebuild_file_tree_from_model();
        self.reset_commit_pagination();
        self.diff_preview_cache.retain_immutable();
        self.needs_diff_refresh = true;
        true
    }

    pub(crate) fn request_head_rewrite_refresh(&mut self) {
        if !self.try_fast_head_rewrite_refresh() {
            self.request_history_refresh();
        }
    }

    pub(super) fn start_background_history_refresh(&mut self) -> bool {
        let Some(tx) = self.begin_streaming_refresh(HISTORY_MODEL_PART_COUNT, true) else {
            return false;
        };

        self.reset_commit_pagination();
        self.diff_preview_cache.retain_immutable();

        let git = Arc::clone(&self.git);
        let commit_filter = self.commit_filter_for_load();

        std::thread::spawn(move || {
            git.load_model_after_history_change_streaming(&tx, commit_filter);
        });

        true
    }

    pub(crate) fn request_history_refresh(&mut self) {
        if !self.start_background_history_refresh() {
            self.needs_refresh = true;
        }
    }

    /// Prepare for a checkout by invalidating HEAD-relative work that belongs
    /// to the old worktree state.
    ///
    /// We deliberately keep non-file parts of a generic model refresh alive
    /// until checkout succeeds; the GitCommands generation stops only the stale
    /// status/diff chain. A separate status-only files refresh is disconnected
    /// immediately because its result would be obsolete as soon as checkout
    /// starts.
    pub(crate) fn prepare_for_checkout(&mut self) {
        let had_refresh = (self.refresh_in_progress && self.initial_load_rx.is_some())
            || self.files_refresh_in_progress
            || self.files_refresh_rx.is_some();
        self.checkout_invalidated_refresh |= had_refresh;

        self.cancel_checkout_refresh_if_running();
        self.git.cancel_worktree_reads();

        if self.files_refresh_rx.is_some() || self.files_refresh_in_progress {
            self.files_refresh_rx = None;
            self.files_refresh_in_progress = false;
            self.needs_files_refresh = false;
        }
    }

    /// A failed checkout leaves the old HEAD in place. If we invalidated a
    /// refresh while attempting it, discard the now-incomplete stream and queue
    /// a clean refresh against the still-current HEAD.
    pub(super) fn recover_refresh_after_failed_checkout(&mut self) {
        if !self.checkout_invalidated_refresh {
            return;
        }

        self.initial_load_rx = None;
        self.initial_load_received = 0;
        self.refresh_in_progress = false;
        self.checkout_refresh_active = false;
        self.files_refresh_rx = None;
        self.files_refresh_in_progress = false;
        self.needs_files_refresh = false;
        self.checkout_invalidated_refresh = false;
        self.needs_refresh = true;
    }

    /// Drop a still-running checkout refresh before starting another checkout.
    ///
    /// The worker threads observe the disconnected receiver at their next
    /// result boundary and stop doing follow-up work. Other refresh kinds are
    /// left alone; only checkout refreshes are safe and useful to supersede
    /// this aggressively.
    pub(crate) fn cancel_checkout_refresh_if_running(&mut self) {
        if self.refresh_in_progress
            && self.checkout_refresh_active
            && self.initial_load_rx.is_some()
        {
            self.initial_load_rx = None;
            self.initial_load_received = 0;
            self.refresh_in_progress = false;
            self.checkout_refresh_active = false;
        }
    }

    /// Kick off the model refresh required after a successful checkout.
    pub(super) fn start_background_checkout_refresh(
        &mut self,
        confirmed_head: Option<HeadState>,
        reload_branches: bool,
    ) -> bool {
        let Some(tx) = self.begin_streaming_refresh(CHECKOUT_MODEL_PART_COUNT, true) else {
            return false;
        };
        self.checkout_refresh_active = true;

        self.reset_commit_pagination();
        self.diff_preview_cache.retain_immutable();

        let commit_filter = self.commit_filter_for_load();
        let (commits, branches, worktrees, reflog, stashes) = {
            let model = self.model.lock().unwrap();
            (
                model.commits.clone(),
                model.branches.clone(),
                model.worktrees.clone(),
                model.reflog_commits.clone(),
                model.stash_entries.clone(),
            )
        };

        let git = Arc::clone(&self.git);

        std::thread::spawn(move || {
            git.load_model_after_checkout_streaming(
                &tx,
                commits,
                branches,
                worktrees,
                reflog,
                stashes,
                confirmed_head,
                commit_filter,
                reload_branches,
            );
        });

        true
    }

    /// Kick off the model refresh required after a successful pull.
    ///
    /// Reuses the normal streaming ModelPart path, but avoids reloading state
    /// that pull cannot affect and avoids expensive per-file diff-stat scans.
    pub(super) fn start_background_pull_refresh(&mut self) -> bool {
        let Some(tx) = self.begin_streaming_refresh(PULL_MODEL_PART_COUNT, true) else {
            return false;
        };

        self.reset_commit_pagination();
        self.diff_preview_cache.retain_immutable();

        let previous_tag_remote_state = {
            let model = self.model.lock().unwrap();
            model
                .tags
                .iter()
                .map(|tag| (tag.name.clone(), tag.on_remote))
                .collect::<std::collections::HashMap<_, _>>()
        };

        let git = Arc::clone(&self.git);
        let commit_filter = self.commit_filter_for_load();

        std::thread::spawn(move || {
            git.load_model_after_pull_streaming(&tx, commit_filter, previous_tag_remote_state);
        });

        true
    }

    /// Rebuild the file tree from the current in-memory model (no git).
    pub(crate) fn rebuild_file_tree_from_model(&mut self) {
        let model = self.model.lock().unwrap();
        if self.show_file_tree {
            self.file_tree_nodes = build_file_tree(&model.files, &self.collapsed_dirs);
            self.context_mgr.files_list_len_override = Some(self.file_tree_nodes.len());
        } else {
            self.file_tree_nodes.clear();
            self.context_mgr.files_list_len_override = None;
        }
    }
}
