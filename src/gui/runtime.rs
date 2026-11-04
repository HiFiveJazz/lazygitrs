//! Main terminal event loop.
//!
//! Kept as a direct `Gui` method so the hot loop is unchanged; this module
//! merely separates runtime orchestration from the state/model definition.

use super::*;

impl Gui {
    pub(super) fn main_loop(
        &mut self,
        terminal: &mut Term,
        input: &InputReader,
        keyboard_enhanced: &mut bool,
    ) -> Result<()> {
        loop {
            // Drain any model parts that have arrived from the background load.
            if let Some(rx) = &self.initial_load_rx {
                let mut got_files = false;
                let mut got_commits = false;
                let mut got_rebase_in_progress = false;
                let mut diff_source_changed = false;
                while let Ok(part) = rx.try_recv() {
                    diff_source_changed |=
                        streamed_model_part_affects_diff(self.context_mgr.active(), &part);
                    let mut model = self.model.lock().unwrap();
                    match part {
                        ModelPart::Files(v) => {
                            model.set_files(v);
                            got_files = true;
                        }
                        ModelPart::FileStats(v) => {
                            let was_empty = model.files.is_empty();
                            let file_set_grew = model.apply_file_stats(v);
                            if file_set_grew {
                                got_files = true;
                            }
                            // On startup with only untracked changes, the first
                            // Files phase is empty. When untracked discovery then
                            // supplies the first visible row, the preview source
                            // really did change from Files:none to that file. If
                            // tracked rows were already visible, appended ?? rows
                            // do not invalidate the selected tracked diff.
                            if was_empty
                                && !model.files.is_empty()
                                && self.context_mgr.active() == ContextId::Files
                            {
                                diff_source_changed = true;
                            }
                        }
                        ModelPart::Branches(v) => model.branches = v,
                        ModelPart::Commits(v) => {
                            got_commits = true;
                            // Stream already applies the active filter when one is set
                            // (`load_model_streaming(commit_filter)`), so always take it.
                            self.commit_history_complete = v.len() < DEFAULT_COMMIT_LIMIT;
                            model.set_commits(v);
                        }
                        ModelPart::UnpushedCommitHashes(hashes) => {
                            GitCommands::apply_unpushed_status(&mut model.commits, &hashes);
                            model.commits_revision = model.commits_revision.wrapping_add(1);
                        }
                        ModelPart::Stash(v) => model.stash_entries = v,
                        ModelPart::Remotes(v) => {
                            model.remotes = v;
                            if !self.remote_branches_name.is_empty()
                                && let Some(remote) = model
                                    .remotes
                                    .iter()
                                    .find(|remote| remote.name == self.remote_branches_name)
                            {
                                model.sub_remote_branches = remote.branches.clone();
                            }
                        }
                        ModelPart::Tags(v) => model.tags = v,
                        ModelPart::Worktrees(v) => model.worktrees = v,
                        ModelPart::Submodules(v) => model.submodules = v,
                        ModelPart::Reflog(v) => model.reflog_commits = v,
                        ModelPart::DiffStats { added, deleted } => {
                            model.total_additions = added;
                            model.total_deletions = deleted;
                        }
                        ModelPart::RepoStatus {
                            is_rebasing,
                            is_merging,
                            is_cherry_picking,
                            is_bisecting,
                            rebase_onto_hash,
                        } => {
                            model.is_rebasing = is_rebasing;
                            model.is_merging = is_merging;
                            model.is_cherry_picking = is_cherry_picking;
                            model.is_bisecting = is_bisecting;
                            model.rebase_onto_hash = rebase_onto_hash;
                            if is_rebasing {
                                got_rebase_in_progress = true;
                            }
                        }
                        ModelPart::Head { hash, branch_name } => {
                            model.head_hash = hash;
                            model.head_branch_name = branch_name;
                        }
                        ModelPart::RepoUrl(url) => model.repo_url = url,
                        ModelPart::Contributors(c) => model.contributors = c,
                    }
                    self.initial_load_received += 1;
                }
                if got_commits {
                    if let Err(err) = self.select_startup_head() {
                        self.show_error("Could not select HEAD", err);
                    }
                }
                // Enter the InProgress rebase view as soon as we know a rebase
                // is on disk — don't wait for a future `refresh()` tick (focus
                // event / auto-refresh interval), which is what made the view
                // pop in ~0.8s after the default screen appeared on startup.
                if got_rebase_in_progress
                    && !self.rebase_mode.active
                    && !self.rebase_mode.in_progress_dismissed
                {
                    self.sync_rebase_progress_view();
                }
                // Rebuild file tree if files arrived this frame. Any new
                // authoritative Files snapshot also invalidates path-keyed
                // working-tree previews: staging can change a diff without
                // changing its filename.
                if got_files {
                    self.invalidate_files_diff_previews();
                    if self.show_file_tree {
                        let model = self.model.lock().unwrap();
                        self.file_tree_nodes = build_file_tree(&model.files, &self.collapsed_dirs);
                        self.context_mgr.files_list_len_override = Some(self.file_tree_nodes.len());
                    }
                }
                // Only invalidate the preview when the model data that backs the
                // active diff actually changed. FileStats, DiffStats, branch/tag
                // metadata, contributor data, etc. cannot change the selected diff
                // and used to restart the exact same Git diff as they streamed in.
                if diff_source_changed {
                    self.needs_diff_refresh = true;
                }
                // All parts received — done loading.
                if self.initial_load_received >= self.initial_load_expected {
                    self.initial_load_rx = None;

                    let was_refresh = self.refresh_in_progress;
                    let was_lightweight_refresh = was_refresh
                        && matches!(
                            self.initial_load_expected,
                            PERIODIC_MODEL_PART_COUNT | PUSH_MODEL_PART_COUNT
                        );

                    self.refresh_in_progress = false;
                    self.checkout_refresh_active = false;

                    // Leave needs_refresh alone: if another mutation arrived
                    // mid-refresh it will re-queue on the next frame. The preview
                    // was already invalidated when its actual backing model part
                    // arrived; stream completion alone does not change the diff.
                    self.needs_files_refresh = false;
                    self.last_refresh_at = Instant::now();

                    // Periodic and push refreshes only stream model parts whose
                    // effects are fully applied above. Avoid selection-dependent
                    // Git reloads that the old synchronous push path never needed.
                    if !was_lightweight_refresh
                        && (was_refresh
                            || self.commit_path_filter.is_some()
                            || !self.commit_branch_filter.is_empty()
                            || !self.commit_author_filter.is_empty())
                    {
                        if let Err(err) = self.after_model_refresh() {
                            self.show_error("Refresh failed", err);
                        }
                    }
                }
            }

            // Apply initial compare-mode data before scheduling a selected-file diff.
            self.receive_compare_load_results();

            // Request diff loading on background thread if selection changed
            self.maybe_request_diff();

            // Check for completed background diff/metadata results
            self.receive_diff_results();
            self.receive_commit_stat_prefetch_results();
            self.receive_index_mutation_results();

            // Warm caches with neighbor data while idle. Shortstats are tiny but
            // otherwise visibly lag the rest of the commit-details panel.
            self.maybe_prefetch_diffs();
            self.maybe_prefetch_commit_stats();

            // Queue full details for only the commit where navigation has settled.
            self.maybe_request_commit_details();

            // Check for AI commit message generation results
            self.receive_deferred_notifications();
            self.receive_ai_commit_results();

            // Check for completed incremental commit page loads
            self.receive_commit_page_results();
            self.receive_filter_paths();
            self.maybe_request_more_commits();

            // Check for completed background remote operations
            self.receive_remote_op_results();

            // Check for completed auto-fetch and kick off a new one if due
            self.receive_auto_fetch_results();
            self.maybe_start_auto_fetch();

            // Check for completed background menu item operations
            controller::custom_commands::receive_command_result(self);
            self.receive_menu_async_results();
            self.receive_ui_jobs();

            // Advance spinner animation
            self.spinner_frame = self.spinner_frame.wrapping_add(1);

            if self
                .action_toast
                .as_ref()
                .is_some_and(|(_, shown_at)| shown_at.elapsed() >= ACTION_TOAST_DURATION)
            {
                self.action_toast = None;
            }

            // Render
            let theme = self.active_theme();
            crate::pager::image_preview::flush_cleanup(terminal.backend_mut());
            // The commit editor uses the real terminal cursor so it can mirror
            // Vim's shape: a thin bar in Insert and a block in Normal/Visual.
            // Frame::set_cursor_position (in views.rs) owns visibility/position;
            // this only selects the terminal cursor shape.
            if let PopupState::CommitInput { mode, .. } = &self.popup {
                let cursor_style = match mode {
                    popup::CommitInputMode::Insert => cursor::SetCursorStyle::SteadyBar,
                    popup::CommitInputMode::Normal
                    | popup::CommitInputMode::Visual
                    | popup::CommitInputMode::VisualLine => cursor::SetCursorStyle::SteadyBlock,
                };
                execute!(terminal.backend_mut(), cursor_style)?;
            }
            self.diff_view.image_preview_hidden = self.popup != PopupState::None
                || (self.show_command_log
                    && self
                        .diff_view
                        .image_preview
                        .as_ref()
                        .is_some_and(|p| !p.uses_placeholders()))
                || self.ai_commit_generation_active()
                || self.remote_op_label.is_some()
                || self.background_mutation_label.is_some()
                || self.action_toast.is_some()
                || self.rebase_mode.active
                || self.patch_building.active;
            self.context_mgr.tick_selection_animations();
            self.diff_view.tick_smooth_scroll();
            self.tick_patch_preview_animation();
            self.tick_email_preview_animation();
            self.tick_commit_input_scroll_animation();
            self.tick_patch_file_picker_animation();
            terminal.draw(|frame| {
                if self.rebase_mode.active {
                    presentation::rebase_mode::render(frame, &mut self.rebase_mode, &theme);
                    // Render popup overlay on top of rebase mode
                    if self.popup != PopupState::None {
                        views::render_popup(
                            frame,
                            &self.popup,
                            frame.area(),
                            self.spinner_frame,
                            &theme,
                            false,
                            !self
                                .config
                                .user_config
                                .git
                                .commit
                                .generate_command
                                .trim()
                                .is_empty(),
                        );
                    } else if self.ai_commit_generation_active() {
                        views::render_loading_overlay(
                            frame,
                            frame.area(),
                            self.spinner_frame,
                            &theme,
                            "AI Commit",
                            "Generating commit message...",
                            Some(("Esc esc", "cancel")),
                        );
                    } else if let Some(label) = self
                        .remote_op_label
                        .as_deref()
                        .or(self.background_mutation_label.as_deref())
                    {
                        views::render_loading_overlay(
                            frame,
                            frame.area(),
                            self.spinner_frame,
                            &theme,
                            label,
                            "",
                            None,
                        );
                    } else if let Some((label, _)) = self.action_toast.as_ref() {
                        views::render_success_overlay(frame, frame.area(), &theme, label);
                    }
                } else if self.diff_mode.active {
                    let diff_loading_show = self.diff_loading
                        && self
                            .diff_loading_since
                            .map(|t| t.elapsed() >= std::time::Duration::from_millis(50))
                            .unwrap_or(false);
                    presentation::diff_mode::render(
                        frame,
                        &mut self.diff_mode,
                        &mut self.diff_view,
                        &theme,
                        self.diff_loading,
                        diff_loading_show,
                        &mut self.commit_list_cache,
                        self.layout.side_panel_ratio,
                        self.screen_mode,
                        &self.config.user_config.keybinding,
                    );
                    // Render popup overlay on top of diff mode (for ? help, errors, etc.)
                    if self.popup != PopupState::None {
                        views::render_popup(
                            frame,
                            &self.popup,
                            frame.area(),
                            self.spinner_frame,
                            &theme,
                            false,
                            !self
                                .config
                                .user_config
                                .git
                                .commit
                                .generate_command
                                .trim()
                                .is_empty(),
                        );
                    } else if self.ai_commit_generation_active() {
                        views::render_loading_overlay(
                            frame,
                            frame.area(),
                            self.spinner_frame,
                            &theme,
                            "AI Commit",
                            "Generating commit message...",
                            Some(("Esc esc", "cancel")),
                        );
                    } else if let Some(label) = self
                        .remote_op_label
                        .as_deref()
                        .or(self.background_mutation_label.as_deref())
                    {
                        views::render_loading_overlay(
                            frame,
                            frame.area(),
                            self.spinner_frame,
                            &theme,
                            label,
                            "",
                            None,
                        );
                    } else if let Some((label, _)) = self.action_toast.as_ref() {
                        views::render_success_overlay(frame, frame.area(), &theme, label);
                    }
                } else {
                    let model = self.model.lock().unwrap();
                    let search_state = if self.search_active || !self.search_query.is_empty() {
                        Some((
                            self.search_query.as_str(),
                            self.search_matches.len(),
                            self.search_match_idx,
                        ))
                    } else {
                        None
                    };
                    let cmd_log = self.command_log.lock().unwrap();
                    let mut active_commit_filters: Vec<String> = self
                        .commit_branch_filter
                        .iter()
                        .map(|branch| format!("branch: {branch}"))
                        .collect();
                    if let Some(path) = self.commit_path_filter.as_deref() {
                        active_commit_filters.push(format!("path: {path}"));
                    }
                    if !self.commit_author_filter.is_empty() {
                        active_commit_filters
                            .push(format!("author: {}", self.commit_author_filter.join(", ")));
                    }
                    views::render(
                        frame,
                        &model,
                        &mut self.context_mgr,
                        &self.layout,
                        &self.popup,
                        &self.config,
                        &theme,
                        &mut self.diff_view,
                        &mut self.commit_list_cache,
                        self.screen_mode,
                        self.show_file_tree,
                        &self.file_tree_nodes,
                        &self.collapsed_dirs,
                        self.diff_focused,
                        search_state,
                        self.search_textarea.as_ref(),
                        &cmd_log,
                        self.show_command_log,
                        &active_commit_filters,
                        self.show_commit_file_tree,
                        &self.commit_file_tree_nodes,
                        &self.commit_files_collapsed_dirs,
                        &self.commit_files_hash,
                        &self.commit_files_message,
                        &self.branch_commits_name,
                        &self.remote_branches_name,
                        self.sub_commits_parent_context,
                        self.spinner_frame,
                        self.remote_op_label.as_deref(),
                        self.remote_op_success_at
                            .map(|t| t.elapsed() < std::time::Duration::from_secs(5))
                            .unwrap_or(false),
                        &self.cherry_pick_clipboard,
                        self.range_select_anchor,
                        self.diff_loading,
                        // Only show "Loading diff..." text after a short delay to avoid jitter on fast loads
                        self.diff_loading
                            && self
                                .diff_loading_since
                                .map(|t| t.elapsed() >= std::time::Duration::from_millis(50))
                                .unwrap_or(false),
                        &self.commit_stats_cache,
                        &self.commit_messages_cache,
                        &mut self.commit_details_scroll,
                        &mut self.commit_details_scroll_hash,
                        self.show_commit_details,
                        false,
                        !self
                            .config
                            .user_config
                            .git
                            .commit
                            .generate_command
                            .trim()
                            .is_empty(),
                    );
                    if self.popup == PopupState::None {
                        if self.ai_commit_generation_active() {
                            views::render_loading_overlay(
                                frame,
                                frame.area(),
                                self.spinner_frame,
                                &theme,
                                "AI Commit",
                                "Generating commit message...",
                                Some(("Esc esc", "cancel")),
                            );
                        } else if let Some(label) = self
                            .remote_op_label
                            .as_deref()
                            .or(self.background_mutation_label.as_deref())
                        {
                            views::render_loading_overlay(
                                frame,
                                frame.area(),
                                self.spinner_frame,
                                &theme,
                                label,
                                "",
                                None,
                            );
                        } else if let Some((label, _)) = self.action_toast.as_ref() {
                            views::render_success_overlay(frame, frame.area(), &theme, label);
                        }
                    }
                }

                // Action confirmations normally yield to popups. Email is a
                // long-lived compose/review surface, though, so confirmations
                // such as a successful dry run must be visible above it.
                if matches!(self.popup, PopupState::EmailPatch { .. }) {
                    if let Some((label, _)) = self.action_toast.as_ref() {
                        views::render_success_overlay(frame, frame.area(), &theme, label);
                    }
                }
            })?;

            // One batch per frame. Reassembly lives on the reader thread so a
            // split ESC [ A cannot leak as Char('A') → amend between frames.
            // Keep the frame budget tight while anything animated/async is up.
            let events = input.wait_batch(self.input_wait_timeout());
            self.handle_event_batch(events);

            // Terminal editors: leave alt screen, run hx/nvim, restore TUI.
            if let Some(launch) = self.pending_editor_launch.take() {
                self.run_suspended_editor(terminal, input, keyboard_enhanced, launch)?;
            }

            if self.should_quit {
                break;
            }

            if self.periodic_refresh_due() && !self.start_background_periodic_refresh() {
                self.needs_refresh = true;
            }

            // Kick off a non-blocking full refresh (same streaming path as
            // initial load). Avoids freezing the UI for ~1s on commit/reword.
            if self.needs_refresh
                && !(self.pending_checkout_by_name.is_some() && self.remote_op_label.is_some())
                && !self.refresh_in_progress
                && self.initial_load_rx.is_none()
            {
                self.start_background_refresh();
            } else if self.needs_files_refresh
                && !(self.pending_checkout_by_name.is_some() && self.remote_op_label.is_some())
                && !self.refresh_in_progress
                && !self.files_refresh_in_progress
            {
                // Status-only async refresh — Space spam stays responsive.
                self.start_files_refresh_async();
            }

            // Apply completed light files refresh without blocking input.
            self.receive_files_refresh();

            // Run allocator collection only after this frame has started any
            // refresh work that became due, so we never trim immediately before
            // kicking off another allocation-heavy background operation.
            self.maybe_collect_idle_memory();

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }
}
