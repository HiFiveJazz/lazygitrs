//! Keyboard, paste, undo/redo, and search interaction.
//!
//! Keeping key routing together makes the input precedence easy to audit while
//! preserving direct inherent-method calls in the event loop.

use super::*;

impl Gui {
    pub(super) fn try_handle_layout_key(&mut self, mut key: KeyEvent) -> bool {
        if self.try_handle_panel_resize_key(key) {
            return true;
        }
        // Terminals may report shifted punctuation with or without SHIFT.
        if matches!(key.code, KeyCode::Char('+') | KeyCode::Char('_')) {
            key.modifiers.remove(KeyModifiers::SHIFT);
        }
        let keybindings = &self.config.user_config.keybinding;
        if matches_key(key, &keybindings.universal.next_screen_mode) {
            self.next_screen_mode();
            return true;
        }
        if matches_key(key, &keybindings.universal.prev_screen_mode) {
            self.prev_screen_mode();
            return true;
        }
        false
    }

    /// Recognize a configurable one- or two-character top motion. Keeping
    /// the pending prefix in `Gui` lets this survive separate terminal events.
    fn try_handle_goto_top(&mut self, key: KeyEvent) -> bool {
        let binding = self.config.user_config.keybinding.universal.goto_top.clone();
        let mut chars = binding.chars();
        let first = chars.next();
        let second = chars.next();
        let is_pair = first.is_some() && second.is_some() && chars.next().is_none();
        if is_pair {
            let first = first.unwrap().to_string();
            let second = second.unwrap().to_string();
            if self.pending_goto_top && matches_key(key, &second) {
                self.pending_goto_top = false;
                if self.diff_focused {
                    self.diff_view.scroll_offset = 0;
                } else {
                    self.context_mgr.set_selection(0);
                }
                return true;
            }
            self.pending_goto_top = false;
            if matches_key(key, &first) {
                self.pending_goto_top = true;
                return true;
            }
            return false;
        }
        self.pending_goto_top = false;
        if matches_key(key, &binding) {
            if self.diff_focused {
                self.diff_view.scroll_offset = 0;
            } else {
                self.context_mgr.set_selection(0);
            }
            return true;
        }
        false
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        self.pending_startup_head = false;
        if self.popup != PopupState::None || self.search_active || self.shell_command_job.is_some() {
            self.pending_goto_top = false;
        }
        if self.shell_command_job.is_some() {
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                controller::custom_commands::cancel_running_command(self);
            }
            return Ok(());
        }
        if has_command_modifier(key.modifiers) && matches!(key.code, KeyCode::Char(_)) {
            return Ok(());
        }
        // Shell input/output own Escape as well as text. An armed AI cancel
        // gesture must not intercept dismissal of an unrelated modal.
        if matches!(
            self.popup,
            PopupState::ShellCommand { .. }
                | PopupState::CommandOutput { .. }
                | PopupState::PatchPreview { .. }
        ) || self.deferred_notification_visible
        {
            return self.handle_popup_key(key);
        }
        if self.handle_ai_commit_cancel_key(key) {
            return Ok(());
        }

        // Popup takes priority
        if self.popup != PopupState::None {
            return self.handle_popup_key(key);
        }

        // Search input mode takes priority
        if self.search_active {
            return self.handle_search_key(key);
        }

        // Terminal-level shortcuts such as Cmd+1/Cmd+2 must not fall through
        // to character-only application shortcuts if the terminal forwards
        // their enhanced-keyboard events.
        if has_command_modifier(key.modifiers) {
            return Ok(());
        }

        // Text entry owns its characters, even when the prompt binding is remapped.
        if !self.diff_mode.active
            && !(self.diff_focused && self.diff_view.search_active)
            && controller::custom_commands::try_handle_prompt_key(self, key)?
        {
            return Ok(());
        }

        // These specialized modes own their own navigation. Never let a
        // partially entered list-motion leak into a mode or popup.
        if self.rebase_mode.active {
            self.pending_goto_top = false;
            return controller::rebase_mode::handle_key(self, key);
        }
        if self.diff_mode.active {
            self.pending_goto_top = false;
            return controller::diff_mode::handle_key(self, key);
        }

        if self.try_handle_panel_resize_key(key) {
            self.pending_goto_top = false;
            return Ok(());
        }
        // `gg` is a key sequence, not two simultaneous keys. Resolve it
        // before context handlers, allowing `G` to jump to the bottom.
        if self.try_handle_goto_top(key) {
            return Ok(());
        }
        let keybindings = &self.config.user_config.keybinding;

        // Smooth multi-row list navigation.
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('k'))
            && matches!(
                self.context_mgr.active(),
                ContextId::Files
                    | ContextId::Branches
                    | ContextId::Remotes
                    | ContextId::Tags
                    | ContextId::Commits
                    | ContextId::Reflog
            )
        {
            let ctx = self.context_mgr.active();
            let amount = if ctx == ContextId::Commits {
                22
            } else {
                self.config.user_config.gui.scroll_height.max(1)
            };

            let model = self.model.lock().unwrap();
            let len = self.context_mgr.list_len(&model);
            drop(model);

            if len == 0 {
                return Ok(());
            }

            // Important: base repeated Ctrl-J/K presses on the destination,
            // not on the intermediate animated position.
            let current_target = self.context_mgr.selection_target(ctx);

            let target = match key.code {
                KeyCode::Char('j') => current_target.saturating_add(amount).min(len - 1),

                KeyCode::Char('k') => current_target.saturating_sub(amount),

                _ => unreachable!(),
            };

            self.context_mgr.request_smooth_selection(ctx, target);

            return Ok(());
        }
        if matches_key(key, &keybindings.universal.toggle_diff_view_layout) {
            self.diff_view.toggle_view_layout();
            self.persist_diff_view_layout();
            return Ok(());
        }

        // When diff panel is focused, handle diff-specific keys
        // Ctrl-F stays in the same Files/CommitFiles context: grep dialog
        // over hunk contents (Enter jumps to the file in the current list).
        if controller::diff_grep::is_diff_grep_key(key)
            && !self.diff_view.search_active
            && matches!(
                self.context_mgr.active(),
                crate::gui::context::ContextId::Files
                    | crate::gui::context::ContextId::CommitFiles
                    | crate::gui::context::ContextId::StashFiles
                    | crate::gui::context::ContextId::BranchCommitFiles
            )
        {
            return controller::diff_grep::open_diff_grep_picker(self);
        }
        if self.diff_focused {
            return self.handle_diff_focused_key(key);
        }

        // Global keybindings
        if matches_key(key, &keybindings.universal.quit)
            || matches_key(key, &keybindings.universal.quit_alt1)
        {
            self.should_quit = true;
            return Ok(());
        }

        // Number keys 1-5 to jump to window (press again to cycle tabs)
        if key.modifiers == KeyModifiers::NONE
            && let KeyCode::Char(c @ '1'..='5') = key.code
        {
            let n = c.to_digit(10).unwrap();
            if let Some(window) = SideWindow::from_number(n) {
                // If we're in a sub-context (CommitFiles), pressing the parent window's
                // number key should exit the sub-context first.
                if self.context_mgr.active() == ContextId::CommitFiles
                    && window == SideWindow::Commits
                {
                    self.context_mgr.set_active(ContextId::Commits);
                    return Ok(());
                }
                if self.context_mgr.active() == ContextId::StashFiles && window == SideWindow::Stash
                {
                    self.context_mgr.set_active(ContextId::Stash);
                    return Ok(());
                }
                if (self.context_mgr.active() == ContextId::BranchCommits
                    || self.context_mgr.active() == ContextId::BranchCommitFiles)
                    && window == SideWindow::Branches
                {
                    if self.context_mgr.active() == ContextId::BranchCommitFiles {
                        self.context_mgr.set_active(ContextId::BranchCommits);
                    } else {
                        self.context_mgr.set_active(ContextId::Branches);
                    }
                    return Ok(());
                }
                if self.context_mgr.active() == ContextId::RemoteBranches
                    && window == SideWindow::Branches
                {
                    self.context_mgr.set_active(ContextId::Remotes);
                    return Ok(());
                }
                self.context_mgr.jump_to_window(window);
                return Ok(());
            }
        }

        // Tab to switch windows
        if matches_key(key, &keybindings.universal.toggle_panel) {
            self.exit_sub_contexts();
            self.context_mgr.next_window();
            return Ok(());
        }

        // Shift+Tab to switch windows in reverse
        if matches_key(key, &keybindings.universal.toggle_panel_reverse) {
            self.exit_sub_contexts();
            self.context_mgr.prev_window();
            return Ok(());
        }

        if self.try_handle_tree_navigation(key) {
            return Ok(());
        }
        let keybindings = &self.config.user_config.keybinding;

        // Arrow keys / h/l to switch windows
        if matches_key(key, &keybindings.universal.prev_block)
            || matches_key(key, &keybindings.universal.prev_block_alt)
        {
            self.exit_sub_contexts();
            self.context_mgr.prev_window();
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.next_block)
            || matches_key(key, &keybindings.universal.next_block_alt)
        {
            self.exit_sub_contexts();
            self.context_mgr.next_window();
            return Ok(());
        }

        // Navigation within current panel
        if matches_key(key, &keybindings.universal.prev_item)
            || matches_key(key, &keybindings.universal.prev_item_alt)
        {
            let model = self.model.lock().unwrap();
            self.context_mgr.move_selection(-1, &model);
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.next_item)
            || matches_key(key, &keybindings.universal.next_item_alt)
        {
            let model = self.model.lock().unwrap();
            self.context_mgr.move_selection(1, &model);
            return Ok(());
        }

        // Go to top is handled above (it may be a multi-key `gg`).
        if matches_key(key, &keybindings.universal.goto_bottom) {
            let model = self.model.lock().unwrap();
            let len = self.context_mgr.list_len(&model);
            if len > 0 {
                self.context_mgr.set_selection(len - 1);
            }
            return Ok(());
        }

        // Smooth preview scrolling (J/K by default). Keep this separate from
        // Ctrl-J/K, which performs smooth multi-row navigation in the active list.
        if matches_key(key, &keybindings.universal.scroll_down_main_alt1) {
            self.diff_view.request_smooth_scroll(22);
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.scroll_up_main_alt1) {
            self.diff_view.request_smooth_scroll(-22);
            return Ok(());
        }
        if key.code == KeyCode::PageDown {
            self.diff_view.scroll_down(20);
            return Ok(());
        }
        if key.code == KeyCode::PageUp {
            self.diff_view.scroll_up(20);
            return Ok(());
        }

        // Horizontal scroll (H/L)
        if matches_key(key, &keybindings.universal.scroll_left) {
            self.diff_view.scroll_left(4);
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.scroll_right) {
            self.diff_view.scroll_right(4);
            return Ok(());
        }

        // The same configurable hunk bindings work with either focus.
        if matches_key(key, &keybindings.universal.prev_hunk) {
            self.diff_view.prev_hunk();
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.next_hunk) {
            self.diff_view.next_hunk();
            return Ok(());
        }

        // Refresh
        if matches_key(key, &keybindings.universal.refresh) {
            self.needs_refresh = true;
            return Ok(());
        }

        // Rebase options menu (global — when rebasing/merging)
        if matches_key(key, &keybindings.universal.create_rebase_options_menu) {
            let model = self.model.lock().unwrap();
            let is_rebasing = model.is_rebasing;
            let is_merging = model.is_merging;
            let is_cherry_picking = model.is_cherry_picking;
            drop(model);

            // If rebasing, re-enter the interactive rebase view
            if is_rebasing {
                if !self.rebase_mode.active {
                    self.rebase_mode.in_progress_dismissed = false;
                    self.sync_rebase_progress_view();
                }
                return Ok(());
            }

            if is_merging || is_cherry_picking {
                return self.show_rebase_options_menu(false, is_merging, is_cherry_picking);
            }
        }

        // Push/Pull (global)
        if self.try_handle_global_repo_keys(key)? {
            return Ok(());
        }
        let keybindings = &self.config.user_config.keybinding;

        // Screen mode toggle (+ to enlarge, _ to shrink, matching lazygit)
        if matches_key(key, &keybindings.universal.next_screen_mode) {
            self.next_screen_mode();
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.prev_screen_mode) {
            self.prev_screen_mode();
            return Ok(());
        }

        // Diff/Compare mode (W)
        if key.code == KeyCode::Char('W') {
            self.diff_mode.enter(self.show_file_tree);
            self.clear_diff_view();
            return Ok(());
        }

        // Toggle command log (;)
        if key.code == KeyCode::Char(';') {
            self.show_command_log = !self.show_command_log;
            self.persist_command_log_visibility();
            return Ok(());
        }

        // Undo (u)
        if matches_key(key, &keybindings.universal.undo) {
            return self.undo();
        }

        // Redo (ctrl-z)
        if matches_key(key, &keybindings.universal.redo) {
            return self.redo();
        }

        // Patch building mode (<c-p>)
        if matches_key(key, &keybindings.universal.create_patch_options_menu) {
            if self.context_mgr.active() == ContextId::Commits || self.patch_building.active {
                return controller::patch_building::show_patch_menu(self);
            }
        }

        // Help popup (?)
        if key.code == KeyCode::Char('?') {
            self.show_command_palette();
            return Ok(());
        }

        // Start search
        if matches_key(key, &keybindings.universal.start_search) {
            self.search_active = true;
            self.search_query.clear();
            self.search_matches.clear();
            self.search_match_idx = 0;
            let mut ta = tui_textarea::TextArea::default();
            ta.set_cursor_line_style(ratatui::style::Style::default());
            self.search_textarea = Some(ta);
            return Ok(());
        }

        // Next/prev search match, or Esc to dismiss search results
        if !self.search_query.is_empty() {
            if key.code == KeyCode::Esc {
                self.search_query.clear();
                self.search_matches.clear();
                self.search_match_idx = 0;
                return Ok(());
            }
            if matches_key(key, &keybindings.universal.next_match) {
                self.goto_next_search_match();
                return Ok(());
            }
            if matches_key(key, &keybindings.universal.prev_match) {
                self.goto_prev_search_match();
                return Ok(());
            }
        }

        // Universal "I" key: interactive rebase picker
        // SHIFT alone is accepted because terminals report uppercase letters that way.
        // Modes that already claim keys (rebase/diff/search/popups) return earlier, so
        // these only fire in normal views.
        if plain_char_key(key, 'I') {
            self.show_interactive_rebase_picker();
            return Ok(());
        }

        // The configurable reset picker no longer consumes Vim's `G` motion.
        if matches_key(key, &keybindings.universal.reset_to_ref) {
            self.show_reset_picker();
            return Ok(());
        }

        // Read the same config key that the help palette and border hint use.
        let details_binding = if matches!(self.context_mgr.active(),
            ContextId::CommitFiles | ContextId::BranchCommitFiles | ContextId::StashFiles)
        {
            &keybindings.universal.toggle_commit_details_files
        } else {
            &keybindings.universal.toggle_commit_details
        };
        if matches_key(key, details_binding) && self.context_has_commit_details() {
            self.show_commit_details = !self.show_commit_details;
            self.persist_commit_details_visibility();
            return Ok(());
        }

        // Context-specific keybindings
        self.handle_context_key(key)?;

        // Custom commands (lowest priority — checked after built-in bindings)
        controller::custom_commands::try_handle_key(self, key)?;

        Ok(())
    }

    pub(super) fn active_file_tree(&self) -> bool {
        match self.context_mgr.active() {
            ContextId::Files => self.show_file_tree,
            ContextId::CommitFiles | ContextId::BranchCommitFiles | ContextId::StashFiles => {
                self.show_commit_file_tree
            }
            _ => false,
        }
    }

    pub(super) fn try_handle_tree_navigation(&mut self, key: KeyEvent) -> bool {
        let active = self.context_mgr.active();
        let selected = self.context_mgr.selected_active();
        let kb = &self.config.user_config.keybinding;
        let destination = match active {
            ContextId::Files if self.show_file_tree => {
                controller::tree::destination(key, kb, &self.file_tree_nodes, selected)
            }
            ContextId::CommitFiles | ContextId::BranchCommitFiles | ContextId::StashFiles
                if self.show_commit_file_tree =>
            {
                controller::tree::destination(key, kb, &self.commit_file_tree_nodes, selected)
            }
            _ => None,
        };
        if let Some(destination) = destination {
            if let Some(idx) = destination {
                self.context_mgr.set_selection(idx);
                self.needs_diff_refresh = true;
            }
            return true;
        }
        false
    }

    pub(super) fn handle_context_key(&mut self, key: KeyEvent) -> Result<()> {
        let keybindings = self.config.user_config.keybinding.clone();
        let active = self.context_mgr.active();

        match active {
            ContextId::Files => {
                controller::files::handle_key(self, key, &keybindings)?;
            }
            ContextId::Branches => {
                controller::branches::handle_key(self, key, &keybindings)?;
            }
            ContextId::Commits => {
                controller::commits::handle_key(self, key, &keybindings)?;
            }
            ContextId::Reflog => {
                controller::reflog::handle_key(self, key, &keybindings)?;
            }
            ContextId::Stash => {
                controller::stash::handle_key(self, key, &keybindings)?;
            }
            ContextId::Remotes => {
                controller::remotes::handle_key(self, key, &keybindings)?;
            }
            ContextId::Tags => {
                controller::tags::handle_key(self, key, &keybindings)?;
            }
            ContextId::Status => {
                controller::status::handle_key(self, key, &keybindings)?;
            }
            ContextId::Worktrees => {
                controller::worktrees::handle_key(self, key, &keybindings)?;
            }
            ContextId::Submodules => {
                controller::submodules::handle_key(self, key, &keybindings)?;
            }
            ContextId::RemoteBranches => {
                controller::remote_branches::handle_key(self, key, &keybindings)?;
            }
            ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                controller::commit_files::handle_key(self, key, &keybindings)?;
            }
            ContextId::BranchCommits => {
                controller::branch_commits::handle_key(self, key, &keybindings)?;
            }
        }

        Ok(())
    }

    pub(super) fn handle_diff_focused_search_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(ref mut ta) = self.diff_view.search_textarea {
            match key.code {
                KeyCode::Esc => {
                    self.diff_view.dismiss_search();
                }
                KeyCode::Enter => {
                    self.diff_view.dismiss_search();
                    if !self.diff_view.search_matches.is_empty() {
                        self.diff_view.search_match_idx = 0;
                        self.diff_view.scroll_to_current_match();
                    }
                }
                _ => {
                    textarea_input(ta, key);
                    self.diff_view.search_query = ta.lines().join("");
                    self.diff_view.update_search();
                }
            }
        }
        Ok(())
    }

    pub(super) fn handle_diff_focused_key(&mut self, key: KeyEvent) -> Result<()> {
        // Diff search input mode takes priority
        if self.diff_view.search_active {
            return self.handle_diff_focused_search_key(key);
        }

        // Handle text selection keys first (y to copy, e to edit, Esc to dismiss)
        if self.diff_view.selection.is_some() {
            let is_click = self.diff_view.selection.as_ref().unwrap().is_click;
            let can_edit = self.diff_view.file_exists_on_disk;
            match key.code {
                KeyCode::Char('e') if can_edit => {
                    let sel_ref = self.diff_view.selection.as_ref().unwrap();
                    let line = sel_ref.edit_line_number;
                    // Compute column from terminal position using the same layout as the mouse handler
                    let (top_row, top_col, _, _) = sel_ref.normalized();
                    let main_panel = self.compute_main_panel_rect();
                    let pl = DiffPanelLayout::compute(main_panel, &self.diff_view);
                    let (content_start, _) = pl.content_range(sel_ref.panel);
                    let column = if top_col >= content_start {
                        (top_col - content_start) as usize + self.diff_view.horizontal_scroll + 1
                    } else {
                        1
                    };
                    // Resolve the actual filename for multi-file diffs
                    let (line_idx, line_panel) = if top_row >= pl.inner_y {
                        self.diff_view
                            .line_chunk_panel_at_row(top_row, &pl, sel_ref.panel)
                            .map(|(line_idx, _, panel)| (line_idx, panel))
                            .unwrap_or_else(|| {
                                (
                                    self.diff_view.fallback_line_idx_for_row(top_row, &pl),
                                    sel_ref.panel,
                                )
                            })
                    } else {
                        (0, sel_ref.panel)
                    };
                    let filename = self.diff_view.file_at_line(line_idx).to_string();
                    self.diff_view.selection = None;
                    let abs_path = self.git.repo_path().join(&filename);
                    if !filename.is_empty() && abs_path.exists() {
                        let abs_path = abs_path.to_string_lossy().to_string();
                        let ln =
                            line.or_else(|| self.diff_view.file_line_number(line_idx, line_panel));
                        if let Ok(launch) =
                            self.config
                                .user_config
                                .os
                                .plan_edit(&abs_path, ln, Some(column))
                        {
                            let _ = self.launch_editor(launch);
                        }
                    }
                    return Ok(());
                }
                KeyCode::Char('y') if !is_click => {
                    let text = self.diff_view.selection.as_ref().unwrap().text.clone();
                    self.diff_view.selection = None;
                    if !text.is_empty() {
                        crate::os::platform::Platform::copy_to_clipboard(&text)?;
                    }
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.diff_view.selection = None;
                    return Ok(());
                }
                _ => {
                    self.diff_view.selection = None;
                    if is_click {
                        // Don't propagate click-state dismissal as a real keypress
                        return Ok(());
                    }
                }
            }
        }

        // Push/Pull are global — they fire even when the diff panel is focused.
        if self.try_handle_global_repo_keys(key)? {
            return Ok(());
        }

        let keybindings = &self.config.user_config.keybinding;

        // e / o on the diff panel (no active selection) mirror the Files tab:
        // open the working-tree file in the editor (at the first changed hunk)
        // or in the default program.
        if matches_key(key, &keybindings.universal.edit) {
            self.open_diff_file_in_editor();
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.open_file) {
            self.open_diff_file_in_default_program();
            return Ok(());
        }

        // Screen mode cycling works even when diff is focused
        if matches_key(key, &keybindings.universal.next_screen_mode) {
            self.next_screen_mode();
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.prev_screen_mode) {
            self.prev_screen_mode();
            return Ok(());
        }

        // Start diff content search (/)
        if matches_key(key, &keybindings.universal.start_search) {
            self.diff_view.start_search();
            return Ok(());
        }

        // n/N to navigate diff search matches
        if !self.diff_view.search_query.is_empty() {
            if matches_key(key, &keybindings.universal.next_match) {
                self.diff_view.next_search_match();
                return Ok(());
            }
            if matches_key(key, &keybindings.universal.prev_match) {
                self.diff_view.prev_search_match();
                return Ok(());
            }
        }

        if matches_key(key, &keybindings.universal.revert_block) {
            if self.context_mgr.active() == ContextId::Files {
                let hunk_idx = self
                    .diff_view
                    .selected_revert_hunk
                    .or(self.diff_view.hovered_revert_hunk);
                if let Some(hunk_idx) = hunk_idx {
                    self.diff_view.selected_revert_hunk = Some(hunk_idx);
                    self.show_hunk_context_menu(hunk_idx);
                }
            }
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.undo_revert_block) {
            if self.context_mgr.active() == ContextId::Files
                && !self.diff_view.revert_undo_stack.is_empty()
            {
                if let Err(err) = self.undo_last_revert_block() {
                    self.popup = PopupState::Message {
                        title: "Undo revert failed".to_string(),
                        message: format!("{}", err),
                        kind: MessageKind::Error,
                    };
                }
            }
            return Ok(());
        }

        if self.try_handle_tree_navigation(key) {
            return Ok(());
        }
        let keybindings = &self.config.user_config.keybinding;

        // Toggle command log (;)
        if key.code == KeyCode::Char(';') {
            self.show_command_log = !self.show_command_log;
            self.persist_command_log_visibility();
            return Ok(());
        }

        // Help popup
        if key.code == KeyCode::Char('?') {
            self.show_diff_command_palette();
            return Ok(());
        }

        // Number keys 1-5 to jump to sidebar panels (unfocus diff)
        // Use set_window instead of jump_to_window to avoid cycling tabs,
        // since the user is "arriving" from diff focus, not pressing the same key again.
        if let KeyCode::Char(c @ '1'..='5') = key.code {
            let n = c.to_digit(10).unwrap();
            if let Some(window) = SideWindow::from_number(n) {
                self.diff_focused = false;
                self.context_mgr.set_window(window);
                return Ok(());
            }
        }

        if matches_key(key, &keybindings.universal.goto_bottom) {
            self.diff_view.scroll_offset = self.diff_view.lines.len().saturating_sub(1);
            return Ok(());
        }

        // Configured H/L scroll keybindings
        if matches_key(key, &keybindings.universal.scroll_left) {
            self.diff_view.scroll_left(4);
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.scroll_right) {
            self.diff_view.scroll_right(4);
            return Ok(());
        }

        if matches_key(key, &keybindings.universal.next_hunk) {
            if self.context_mgr.active() == ContextId::Files {
                self.diff_view.cycle_next_revert_hunk();
            } else {
                self.diff_view.next_hunk();
            }
            return Ok(());
        }
        if matches_key(key, &keybindings.universal.prev_hunk) {
            if self.context_mgr.active() == ContextId::Files {
                self.diff_view.cycle_prev_revert_hunk();
            } else {
                self.diff_view.prev_hunk();
            }
            return Ok(());
        }
        if controller::tree::matches_key(key, &keybindings.universal.toggle_new_only) {
            use crate::pager::side_by_side::DiffSideView;
            self.diff_view.side_view = match self.diff_view.side_view {
                DiffSideView::NewOnly => DiffSideView::Both,
                _ => DiffSideView::NewOnly,
            };
            return Ok(());
        }
        if controller::tree::matches_key(key, &keybindings.universal.toggle_old_only) {
            use crate::pager::side_by_side::DiffSideView;
            self.diff_view.side_view = match self.diff_view.side_view {
                DiffSideView::OldOnly => DiffSideView::Both,
                _ => DiffSideView::OldOnly,
            };
            return Ok(());
        }

        match key.code {
            // Escape: clear revert-hunk selection first, then search, then unfocus diff
            KeyCode::Esc => {
                if self.diff_view.selected_revert_hunk.is_some() {
                    self.diff_view.selected_revert_hunk = None;
                } else if !self.diff_view.search_query.is_empty() {
                    self.diff_view.clear_search();
                } else {
                    self.diff_focused = false;
                }
            }
            // q quits the app (same as global behavior)
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            // j/k/up/down scroll line by line
            KeyCode::Char('j') | KeyCode::Down => {
                self.diff_view.scroll_down(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.diff_view.scroll_up(1);
            }
            // h/l/left/right scroll horizontally
            KeyCode::Char('h') | KeyCode::Left => {
                self.diff_view.scroll_left(4);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.diff_view.scroll_right(4);
            }
            // z toggles line wrapping
            KeyCode::Char('z') => {
                self.diff_view.wrap = !self.diff_view.wrap;
                self.diff_view.horizontal_scroll = 0;
                self.persist_diff_line_wrap();
            }
            // Page up/down for larger scrolling
            KeyCode::PageDown => {
                self.diff_view.scroll_down(20);
            }
            KeyCode::PageUp => {
                self.diff_view.scroll_up(20);
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn open_diff_file_in_editor(&mut self) {
        let rel_path = self.diff_view.filename.clone();
        if rel_path.is_empty() {
            return;
        }
        let abs_path_buf = self.git.repo_path().join(&rel_path);
        if !abs_path_buf.exists() {
            return;
        }
        let abs_path = abs_path_buf.to_string_lossy().to_string();

        // Pick the hunk currently at the top of the viewport (after `{`/`}`
        // navigation, scroll_offset sits on a hunk start). Fall back to the
        // most recent hunk before the viewport, then the first hunk.
        let active_hunk_idx = self
            .diff_view
            .hunk_starts
            .iter()
            .rev()
            .find(|&&h| h <= self.diff_view.scroll_offset)
            .copied()
            .or_else(|| self.diff_view.hunk_starts.first().copied());

        let active_hunk_line = active_hunk_idx.and_then(|idx| {
            self.diff_view
                .file_line_number(idx, DiffPanel::New)
                .or_else(|| self.diff_view.file_line_number(idx, DiffPanel::Old))
        });

        match self
            .config
            .user_config
            .os
            .plan_edit(&abs_path, active_hunk_line, Some(1))
        {
            Ok(launch) => {
                let _ = self.launch_editor(launch);
            }
            Err(_) => {
                let _ = crate::os::platform::Platform::open_file(&abs_path);
            }
        }
    }

    pub(super) fn open_diff_file_in_default_program(&mut self) {
        let rel_path = self.diff_view.filename.clone();
        if rel_path.is_empty() {
            return;
        }
        let abs_path_buf = self.git.repo_path().join(&rel_path);
        if !abs_path_buf.exists() {
            return;
        }
        let abs_path = abs_path_buf.to_string_lossy().to_string();
        if let Ok(launch) = self.config.user_config.os.plan_open(&abs_path) {
            let _ = self.launch_editor(launch);
        }
    }

    pub(super) fn handle_paste(&mut self, data: String) {
        if self.shell_command_job.is_some() {
            return;
        }
        if let PopupState::ShellCommand { textarea } = &mut self.popup {
            textarea.insert_str(&data.replace("\r\n", "\n").replace('\r', "\n"));
            return;
        }
        if data.is_empty() {
            return;
        }
        let popup_width = (self.layout.width * 60 / 100)
            .min(60)
            .max(30)
            .min(self.layout.width);
        let popup_inner = popup_width.saturating_sub(4) as usize;
        let config_width = self.config.user_config.git.commit.auto_wrap_width;
        let effective_width = if config_width > 0 {
            popup_inner.min(config_width)
        } else {
            popup_inner
        };
        let commit_effective_width = popup::commit_body_text_width(self.layout.width, config_width);
        match &mut self.popup {
            PopupState::Input {
                textarea,
                is_commit,
                confirm_focused,
                ..
            } => {
                if *confirm_focused {
                    return;
                }
                if *is_commit {
                    textarea.insert_str(&data);
                    if effective_width > 0 {
                        auto_wrap_textarea(textarea, effective_width);
                    }
                } else {
                    // Single-line input: strip newlines from pasted content.
                    let cleaned: String = data.replace('\r', "").replace('\n', " ");
                    textarea.insert_str(&cleaned);
                    if popup_inner > 0 {
                        soft_wrap_textarea(textarea, popup_inner);
                    }
                }
            }
            PopupState::CommitInput {
                focus,
                summary_textarea,
                body_textarea,
                body_state,
                mode,
                ..
            } => {
                if *mode != popup::CommitInputMode::Insert {
                    return;
                }
                match *focus {
                    popup::CommitInputFocus::Summary => {
                        // Split on first newline: first line into summary, rest into body.
                        match data.find('\n') {
                            Some(idx) => {
                                let s = data[..idx].replace('\r', "");
                                let b = data[idx + 1..].trim_start_matches('\n').to_string();
                                summary_textarea.insert_str(&s);
                                if !b.is_empty() {
                                    body_state.insert_str(&b);
                                    if commit_effective_width > 0 {
                                        body_state
                                            .render_into(body_textarea, commit_effective_width);
                                    }
                                }
                            }
                            None => {
                                summary_textarea.insert_str(&data);
                            }
                        }
                    }
                    popup::CommitInputFocus::Body => {
                        body_state.insert_str(&data);
                        if commit_effective_width > 0 {
                            body_state.render_into(body_textarea, commit_effective_width);
                        }
                    }
                }
            }
            PopupState::EmailPatch {
                focus,
                to_recipients,
                cc_recipients,
                to_selected,
                cc_selected,
                recipient_editor,
                reviewing,
                resolving_recipients,
                ..
            } => {
                if *resolving_recipients || *reviewing {
                    return;
                }
                let cleaned: String = data.replace('\r', "").replace('\n', " ");
                if cleaned.trim().is_empty() {
                    return;
                }
                if let Some(editor) = recipient_editor.as_mut() {
                    editor.textarea.insert_str(&cleaned);
                    return;
                }

                let pasted = popup::split_email_recipients(&cleaned);
                if pasted.is_empty() {
                    return;
                }
                match *focus {
                    popup::EmailPatchFocus::To => {
                        to_recipients.extend(pasted);
                        *to_selected = to_recipients.len().saturating_sub(1);
                    }
                    popup::EmailPatchFocus::Cc => {
                        cc_recipients.extend(pasted);
                        *cc_selected = cc_recipients.len().saturating_sub(1);
                    }
                }
            }
            PopupState::CommandPalette {
                selected,
                scroll_offset,
                search_textarea,
                ..
            } => {
                let cleaned: String = data.replace('\r', "").replace('\n', " ");
                search_textarea.insert_str(&cleaned);
                *selected = 0;
                *scroll_offset = 0;
            }
            PopupState::Checklist {
                items,
                selected,
                search_textarea,
                free_entry_category,
                ..
            } => {
                let cleaned: String = data.replace('\r', "").replace('\n', " ");
                search_textarea.insert_str(&cleaned);
                let after = search_textarea.lines().join("");
                crate::gui::popup::sync_checklist_free_entry(
                    items,
                    free_entry_category.as_deref(),
                    &after,
                );
                *selected = 0;
            }
            PopupState::RefPicker { core, .. } => {
                use crate::gui::popup::{REF_FREE_ENTRY_CATEGORY, sync_list_picker_free_entry};
                let cleaned: String = data.replace('\r', "").replace('\n', " ");
                core.search_textarea.insert_str(&cleaned);
                sync_list_picker_free_entry(core, REF_FREE_ENTRY_CATEGORY);
                // Paste always resets scroll (matches previous RefPicker paste behavior).
                core.scroll_offset = 0;
            }
            PopupState::ListPicker {
                core,
                free_entry_category,
                ..
            } => {
                use crate::gui::popup::sync_list_picker_prefer_free_entry;
                let cleaned: String = data.replace('\r', "").replace('\n', " ");
                core.search_textarea.insert_str(&cleaned);
                let category = free_entry_category.clone();
                sync_list_picker_prefer_free_entry(core, &category);
                core.scroll_offset = 0;
            }
            PopupState::ThemePicker { core, .. } => {
                let cleaned: String = data.replace('\r', "").replace('\n', " ");
                core.search_textarea.insert_str(&cleaned);
                let new_search = core.search_textarea.lines().join("");
                let new_lower = new_search.to_lowercase();
                if !new_lower.is_empty() {
                    if let Some(idx) = core
                        .items
                        .iter()
                        .position(|i| i.label.to_lowercase().contains(&new_lower))
                    {
                        core.selected = idx;
                        self.current_theme_index = idx;
                        core.scroll_offset = idx;
                    }
                }
            }
            _ => {}
        }
    }

    fn confirm_reflog_reset(&mut self, title: &'static str, target_idx: usize) -> Result<()> {
        let result = self
            .git
            .git_cmd()
            .args(&["reflog", "--format=%H", "-n", "20"])
            .run()?;
        if !result.success {
            return Ok(());
        }

        let Some(target_hash) = result.stdout.lines().nth(target_idx).map(str::to_string) else {
            return Ok(());
        };
        let short = &target_hash[..7.min(target_hash.len())];

        self.popup = PopupState::Confirm {
            title: title.to_string(),
            message: format!("{title} to reflog entry {target_idx}? ({short})"),
            on_confirm: Box::new(move |gui| {
                gui.git.reset_to_commit(&target_hash, "--mixed")?;
                gui.undo_reflog_idx = target_idx;
                gui.needs_refresh = true;
                Ok(())
            }),
        };
        Ok(())
    }

    pub(super) fn undo(&mut self) -> Result<()> {
        self.confirm_reflog_reset("Undo", self.undo_reflog_idx + 1)
    }

    pub(super) fn redo(&mut self) -> Result<()> {
        if self.undo_reflog_idx == 0 {
            return Ok(());
        }
        self.confirm_reflog_reset("Redo", self.undo_reflog_idx - 1)
    }

    pub(super) fn handle_search_key(&mut self, key: KeyEvent) -> Result<()> {
        if let PopupState::None = self.popup {
            // Search uses a textarea — forward keys to it
            if let Some(ref mut ta) = self.search_textarea {
                match key.code {
                    KeyCode::Esc => {
                        self.search_active = false;
                        self.search_query.clear();
                        self.search_matches.clear();
                        self.search_match_idx = 0;
                        self.search_textarea = None;
                    }
                    KeyCode::Enter => {
                        self.search_active = false;
                        // Jump to first match
                        if !self.search_matches.is_empty() {
                            self.search_match_idx = 0;
                            let idx = self.search_matches[0];
                            self.context_mgr.set_selection(idx);
                        }
                        self.search_textarea = None;
                    }
                    _ => {
                        textarea_input(ta, key);
                        // Sync textarea content back to search_query
                        self.search_query = ta.lines().join("");
                        self.update_search_matches();
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn update_search_matches(&mut self) {
        self.search_matches.clear();
        if self.search_query.is_empty() {
            return;
        }

        let query = self.search_query.to_lowercase();
        let model = self.model.lock().unwrap();
        let active = self.context_mgr.active();

        match active {
            ContextId::Files => {
                if self.show_file_tree {
                    // When file tree is active, indices are into file_tree_nodes
                    for (i, node) in self.file_tree_nodes.iter().enumerate() {
                        if node.path.to_lowercase().contains(&query)
                            || node.name.to_lowercase().contains(&query)
                        {
                            self.search_matches.push(i);
                        }
                    }
                } else {
                    for (i, file) in model.files.iter().enumerate() {
                        if file.name.to_lowercase().contains(&query) {
                            self.search_matches.push(i);
                        }
                    }
                }
            }
            ContextId::Branches => {
                for (i, branch) in model.branches.iter().enumerate() {
                    if branch.name.to_lowercase().contains(&query) {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Commits => {
                for (i, commit) in model.commits.iter().enumerate() {
                    if commit.name.to_lowercase().contains(&query)
                        || commit.hash.starts_with(&self.search_query)
                        || commit.author_name.to_lowercase().contains(&query)
                    {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Reflog => {
                for (i, commit) in model.reflog_commits.iter().enumerate() {
                    if commit.name.to_lowercase().contains(&query)
                        || commit.hash.starts_with(&self.search_query)
                    {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Stash => {
                for (i, entry) in model.stash_entries.iter().enumerate() {
                    if entry.name.to_lowercase().contains(&query) {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Tags => {
                for (i, tag) in model.tags.iter().enumerate() {
                    if tag.name.to_lowercase().contains(&query) {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Remotes => {
                for (i, remote) in model.remotes.iter().enumerate() {
                    if remote.name.to_lowercase().contains(&query) {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::RemoteBranches => {
                for (i, rb) in model.sub_remote_branches.iter().enumerate() {
                    if rb.name.to_lowercase().contains(&query) {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Worktrees => {
                for (i, wt) in model.worktrees.iter().enumerate() {
                    if wt.branch.to_lowercase().contains(&query)
                        || wt.path.to_lowercase().contains(&query)
                    {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::Submodules => {
                for (i, sub) in model.submodules.iter().enumerate() {
                    if sub.name.to_lowercase().contains(&query)
                        || sub.path.to_lowercase().contains(&query)
                    {
                        self.search_matches.push(i);
                    }
                }
            }
            ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                if self.show_commit_file_tree {
                    for (i, node) in self.commit_file_tree_nodes.iter().enumerate() {
                        if node.path.to_lowercase().contains(&query)
                            || node.name.to_lowercase().contains(&query)
                        {
                            self.search_matches.push(i);
                        }
                    }
                } else {
                    for (i, file) in model.commit_files.iter().enumerate() {
                        if file.name.to_lowercase().contains(&query) {
                            self.search_matches.push(i);
                        }
                    }
                }
            }
            ContextId::BranchCommits => {
                for (i, commit) in model.sub_commits.iter().enumerate() {
                    if commit.name.to_lowercase().contains(&query)
                        || commit.hash.to_lowercase().contains(&query)
                        || commit.author_name.to_lowercase().contains(&query)
                    {
                        self.search_matches.push(i);
                    }
                }
            }
            _ => {}
        }

        // Auto-jump to first match
        if !self.search_matches.is_empty() {
            self.search_match_idx = 0;
            let idx = self.search_matches[0];
            self.context_mgr.set_selection(idx);
        }
    }

    pub(super) fn goto_next_search_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_match_idx = (self.search_match_idx + 1) % self.search_matches.len();
        let idx = self.search_matches[self.search_match_idx];
        self.context_mgr.set_selection(idx);
    }

    pub(super) fn goto_prev_search_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_match_idx = if self.search_match_idx == 0 {
            self.search_matches.len() - 1
        } else {
            self.search_match_idx - 1
        };
        let idx = self.search_matches[self.search_match_idx];
        self.context_mgr.set_selection(idx);
    }
}
