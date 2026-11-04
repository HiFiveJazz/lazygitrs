//! Mouse, layout hit-testing, and hunk interaction.
//!
//! These remain direct inherent `Gui` methods; the module boundary only keeps
//! pointer-specific interaction logic out of the keyboard/event orchestration.

use super::*;

impl Gui {
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        // A mouse interaction cancels a pending multi-key motion (e.g. `g`).
        self.pending_goto_top = false;
        if self.shell_command_job.is_some() {
            return;
        }
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

        if !self.config.user_config.gui.mouse_events {
            return;
        }

        match &mut self.popup {
            PopupState::ShellCommand { .. } => return,
            PopupState::PatchPreview { .. } => return,
            PopupState::PatchFilePicker { state, .. } => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => state.move_selection(1),
                    MouseEventKind::ScrollUp => state.move_selection(-1),
                    _ => {}
                }
                return;
            }
            PopupState::EmailPatch { .. } => return,
            PopupState::PullDivergence { .. } => return,
            PopupState::CommandOutput {
                message, scroll, ..
            } => {
                let max = views::command_output_max_scroll(
                    message,
                    Rect::new(0, 0, self.layout.width, self.layout.height),
                );
                match mouse.kind {
                    MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(3).min(max),
                    MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                    _ => {}
                }
                // All events are modal, including clicks outside the popup.
                return;
            }
            _ => {}
        }

        // Sidebar divider drag (Normal mode only). Must run before text-select /
        // focus paths so the hit strip wins the gesture.
        if self.sidebar_resizing {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.apply_sidebar_ratio_from_mouse(mouse.column, mouse.row);
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.sidebar_resizing = false;
                    self.sidebar_resize_row_offset = 0;
                    return;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.apply_sidebar_ratio_from_mouse(mouse.column, mouse.row);
                    return;
                }
                _ => {
                    self.sidebar_resizing = false;
                    self.sidebar_resize_row_offset = 0;
                }
            }
        } else if self.popup == PopupState::None
            && self.screen_mode == ScreenMode::Normal
            && (!self.diff_mode.active || self.diff_mode.editing.is_none())
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.sidebar_divider_hit(mouse.column, mouse.row)
        {
            self.sidebar_resizing = true;
            self.sidebar_resize_row_offset = self.portrait_sidebar_resize_offset(mouse.row);
            self.diff_view.selection = None;
            self.apply_sidebar_ratio_from_mouse(mouse.column, mouse.row);
            return;
        }

        // ✦ AI-generate button on commit-message popups: handle clicks.
        if matches!(self.popup, PopupState::CommitInput { .. }) {
            let area = ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
            if let Some(btn_rect) = views::commit_ai_button_geometry(&self.popup, area) {
                let over = rect_contains(btn_rect, mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) if over => {
                        let configured = !self
                            .config
                            .user_config
                            .git
                            .commit
                            .generate_command
                            .trim()
                            .is_empty();
                        if configured {
                            self.trigger_ai_commit_generation_from_editor();
                        } else {
                            let url = "https://github.com/blankeos/lazygitrs#whats-different";
                            if let Err(e) = crate::os::platform::Platform::open_file(url) {
                                self.popup = PopupState::Message {
                                    title: "Error".to_string(),
                                    message: format!("Could not open browser: {}", e),
                                    kind: MessageKind::Error,
                                };
                            }
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }

        if matches!(self.popup, PopupState::CommitInput { .. }) {
            let area = ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
            if let Some(body_rect) = views::commit_description_textarea_geometry(&self.popup, area)
                && rect_contains(body_rect, mouse.column, mouse.row)
            {
                match mouse.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let rows: isize = if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                            3
                        } else {
                            -3
                        };
                        self.queue_commit_input_scroll(rows);
                        return;
                    }
                    _ => {}
                }
            }
        }

        // Rebase mode: scroll and click support
        if self.rebase_mode.active {
            match mouse.kind {
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                    // Use the viewport height stored by the renderer so this
                    // matches what's actually on screen (including resizes).
                    let list_h = self.rebase_mode.visible_height;
                    // List length includes entries + the base commit row appended at the bottom.
                    let list_len = self.rebase_mode.entries.len() + 1;
                    let delta: isize = if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                        3
                    } else {
                        -3
                    };
                    scroll::scroll_viewport(&mut self.rebase_mode.scroll, delta, list_len, list_h);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Compute the list area to determine which entry was clicked
                    let area =
                        ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
                    let outer = ratatui::layout::Layout::default()
                        .direction(ratatui::layout::Direction::Vertical)
                        .constraints([
                            ratatui::layout::Constraint::Min(1),
                            ratatui::layout::Constraint::Length(1),
                        ])
                        .split(area);
                    let block =
                        ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL);
                    let inner = block.inner(outer[0]);
                    let has_banner =
                        self.rebase_mode.phase == modes::rebase_mode::RebasePhase::InProgress;
                    let banner_h: u16 = if has_banner { 2 } else { 0 };
                    // List starts after: inner.y + info_line(1) + banner_h
                    let list_y = inner.y + 1 + banner_h;
                    let list_h = inner.height.saturating_sub(1 + banner_h) as usize;
                    if mouse.row >= list_y && mouse.row < list_y + list_h as u16 {
                        let row_in_list = (mouse.row - list_y) as usize;
                        let clicked_idx = self.rebase_mode.scroll + row_in_list;
                        if clicked_idx < self.rebase_mode.entries.len() {
                            self.rebase_mode.selected = clicked_idx;
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        // Diff mode has its own mouse handling
        if self.diff_mode.active {
            self.handle_diff_mode_mouse(mouse);
            return;
        }

        // Help popup intercepts mouse scroll and click
        if let PopupState::CommandPalette {
            sections,
            selected,
            scroll_offset,
            search_textarea,
        } = &mut self.popup
        {
            // Compute total display rows so we can clamp scroll
            let search_lower = search_textarea.lines().join("").to_lowercase();
            let has_search = !search_lower.is_empty();
            let total_rows: usize = sections
                .iter()
                .map(|s| {
                    let visible = if has_search {
                        s.entries
                            .iter()
                            .filter(|e| {
                                e.key.to_lowercase().contains(&search_lower)
                                    || e.description.to_lowercase().contains(&search_lower)
                            })
                            .count()
                    } else {
                        s.entries.len()
                    };
                    if visible > 0 { visible + 1 } else { 0 } // +1 for header
                })
                .sum();

            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    *scroll_offset = scroll_offset.saturating_sub(3);
                }
                MouseEventKind::ScrollDown => {
                    *scroll_offset = (*scroll_offset + 3).min(total_rows.saturating_sub(1));
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Click to select an entry in the help list
                    let area =
                        ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
                    let popup_width = (area.width * 70 / 100).min(72).max(36);
                    let content_height = total_rows.max(1);
                    let popup_height = (content_height as u16 + 5)
                        .min(area.height.saturating_sub(4))
                        .max(10);
                    let x = (area.width.saturating_sub(popup_width)) / 2;
                    let y = (area.height.saturating_sub(popup_height)) / 2;
                    let inner_y = y + 1; // border
                    let list_start = inner_y + 2; // search + separator
                    let inner_height = popup_height.saturating_sub(2); // borders
                    let list_height = inner_height.saturating_sub(3) as usize; // search + sep + hint

                    if mouse.row >= list_start
                        && mouse.row < list_start + list_height as u16
                        && mouse.column >= x
                        && mouse.column < x + popup_width
                    {
                        let row_in_list = (mouse.row - list_start) as usize;
                        let display_idx = *scroll_offset + row_in_list;

                        // Build flat display list to map display_idx to entry index
                        let mut di = 0usize;
                        let mut ei = 0usize;
                        let mut clicked_entry = None;
                        'sections: for section in sections.iter() {
                            let visible_entries: Vec<_> = section
                                .entries
                                .iter()
                                .filter(|e| {
                                    !has_search
                                        || e.key.to_lowercase().contains(&search_lower)
                                        || e.description.to_lowercase().contains(&search_lower)
                                })
                                .collect();
                            if !visible_entries.is_empty() {
                                if di == display_idx {
                                    // Clicked on a header — ignore
                                    break;
                                }
                                di += 1; // header
                                for _ in visible_entries {
                                    if di == display_idx {
                                        clicked_entry = Some(ei);
                                        break 'sections;
                                    }
                                    di += 1;
                                    ei += 1;
                                }
                            }
                        }
                        if let Some(entry_idx) = clicked_entry {
                            *selected = entry_idx;
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        // Free-entry list pickers (RefPicker / ListPicker) intercept mouse scroll and click
        if matches!(
            self.popup,
            PopupState::RefPicker { .. } | PopupState::ListPicker { .. }
        ) {
            let (core, w, h) = match &mut self.popup {
                PopupState::RefPicker { core, .. } | PopupState::ListPicker { core, .. } => {
                    (core, self.layout.width, self.layout.height)
                }
                _ => unreachable!(),
            };
            handle_list_picker_mouse(core, mouse, w, h);
            return;
        }

        // ThemePicker popup intercepts mouse scroll and click (respects search filter)
        if matches!(self.popup, PopupState::ThemePicker { .. }) {
            let (w, h) = (self.layout.width, self.layout.height);
            let PopupState::ThemePicker { core, .. } = &mut self.popup else {
                unreachable!();
            };
            handle_list_picker_mouse(core, mouse, w, h);
            self.current_theme_index = core.selected;
            return;
        }

        // Action menus: first click selects, second click on same item confirms.
        if matches!(self.popup, PopupState::Menu { .. }) {
            let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
                return;
            };
            let area = ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
            if let Some(idx) = views::menu_item_at(&self.popup, area, mouse.column, mouse.row) {
                let already_selected = matches!(
                    &self.popup,
                    PopupState::Menu { selected, .. } if *selected == idx
                );
                if already_selected {
                    self.execute_menu_action(Some(idx));
                } else if let PopupState::Menu { selected, .. } = &mut self.popup {
                    *selected = idx;
                }
            }
            return;
        }

        // Checklists: first click selects, second click on same item toggles.
        if matches!(self.popup, PopupState::Checklist { .. }) {
            let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
                return;
            };
            let area = ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
            if let Some(visible_idx) =
                views::checklist_item_at(&self.popup, area, mouse.column, mouse.row)
            {
                if let PopupState::Checklist {
                    items,
                    selected,
                    search_textarea,
                    ..
                } = &mut self.popup
                {
                    if *selected == visible_idx {
                        let search = search_textarea.lines().join("");
                        let visible_indices: Vec<usize> = items
                            .iter()
                            .enumerate()
                            .filter(|(_, it)| {
                                it.is_free_entry
                                    || search.is_empty()
                                    || it.label.to_lowercase().contains(&search.to_lowercase())
                            })
                            .map(|(i, _)| i)
                            .collect();
                        if let Some(&real_idx) = visible_indices.get(visible_idx) {
                            items[real_idx].checked = !items[real_idx].checked;
                        }
                    } else {
                        *selected = visible_idx;
                    }
                }
            }
            return;
        }

        let main_panel = self.compute_main_panel_rect();
        let pl = DiffPanelLayout::compute(main_panel, &self.diff_view);

        // Track mouse hover over the revert-block marker (for tooltip).
        if !self.diff_mode.active {
            let new_hover = self.revert_hunk_at_position(main_panel, &pl, mouse.column, mouse.row);
            if self.diff_view.hovered_revert_hunk != new_hover {
                self.diff_view.hovered_revert_hunk = new_hover;
            }
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let in_main = main_panel.x <= mouse.column
                    && mouse.column < main_panel.x + main_panel.width
                    && main_panel.y <= mouse.row
                    && mouse.row < main_panel.y + main_panel.height;

                // In Full screen mode, the main_panel covers everything.
                // If the sidebar is focused (not diff_focused), clicks should
                // go to the sidebar handler, not start a diff selection.
                let full_sidebar = self.screen_mode == ScreenMode::Full && !self.diff_focused;

                if in_main && !self.diff_view.is_empty() && !full_sidebar {
                    if self.try_handle_revert_block_click(main_panel, pl, mouse.column, mouse.row) {
                        self.diff_focused = true;
                        return;
                    }
                    // Clicks on the pinned sticky file header don't start a
                    // text selection — there is no content to select there.
                    if self.diff_view.is_sticky_row(mouse.row, &pl) {
                        self.diff_view.selection = None;
                        self.diff_focused = true;
                        return;
                    }
                    if let Some(panel) = pl.panel_at_x(mouse.column) {
                        self.diff_view.selection = Some(TextSelection {
                            panel,
                            start_col: mouse.column,
                            start_row: mouse.row,
                            end_col: mouse.column,
                            end_row: mouse.row,
                            dragging: true,
                            is_click: false,
                            text: String::new(),
                            edit_line_number: None,
                            edit_column_number: None,
                        });
                    } else {
                        self.diff_view.selection = None;
                    }
                    self.diff_focused = true;
                } else {
                    // Click outside diff — clear selection and handle normally
                    self.diff_view.selection = None;
                    self.handle_mouse_click(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(ref mut sel) = self.diff_view.selection {
                    if sel.dragging {
                        let (cmin, cmax) = pl.content_range(sel.panel);
                        // Allow dragging into gutter area of same panel (5 cols before content)
                        let col_min = cmin.saturating_sub(5);
                        sel.end_col = mouse.column.max(col_min).min(cmax.saturating_sub(1));
                        sel.end_row = mouse
                            .row
                            .max(pl.inner_y)
                            .min(pl.inner_end_y.saturating_sub(1));
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Finalize the selection
                if let Some(ref mut sel) = self.diff_view.selection {
                    sel.dragging = false;
                    // If start == end (just a click, no drag)
                    if sel.start_col == sel.end_col && sel.start_row == sel.end_row {
                        if self.diff_view.file_exists_on_disk {
                            // Keep as click-state to show the edit tooltip
                            sel.is_click = true;
                        } else {
                            self.diff_view.selection = None;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if self.is_in_commit_details_panel(mouse.column, mouse.row) {
                    self.commit_details_scroll = self.commit_details_scroll.saturating_sub(2);
                    return;
                }
                self.diff_view.selection = None;
                let in_diff = self.is_in_main_panel(mouse.column, mouse.row)
                    && (self.screen_mode != ScreenMode::Full || self.diff_focused);
                if mouse.modifiers.contains(KeyModifiers::SHIFT) && in_diff {
                    self.diff_view.scroll_left(4);
                } else if in_diff {
                    self.diff_view.scroll_up(3);
                } else {
                    self.scroll_sidebar_wheel(-3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.is_in_commit_details_panel(mouse.column, mouse.row) {
                    self.commit_details_scroll = self.commit_details_scroll.saturating_add(2);
                    return;
                }
                self.diff_view.selection = None;
                let in_diff = self.is_in_main_panel(mouse.column, mouse.row)
                    && (self.screen_mode != ScreenMode::Full || self.diff_focused);
                if mouse.modifiers.contains(KeyModifiers::SHIFT) && in_diff {
                    self.diff_view.scroll_right(4);
                } else if in_diff {
                    self.diff_view.scroll_down(3);
                } else {
                    self.scroll_sidebar_wheel(3);
                }
            }
            MouseEventKind::ScrollLeft => {
                if self.is_in_commit_details_panel(mouse.column, mouse.row) {
                    return;
                }
                if self.diff_focused
                    || (self.screen_mode != ScreenMode::Full
                        && self.is_in_main_panel(mouse.column, mouse.row))
                {
                    self.diff_view.scroll_left(4);
                }
            }
            MouseEventKind::ScrollRight => {
                if self.is_in_commit_details_panel(mouse.column, mouse.row) {
                    return;
                }
                if self.diff_focused
                    || (self.screen_mode != ScreenMode::Full
                        && self.is_in_main_panel(mouse.column, mouse.row))
                {
                    self.diff_view.scroll_right(4);
                }
            }
            _ => {}
        }
    }

    /// Wheel/touchpad gestures navigate commit rows (and trigger normal
    /// commit pagination) rather than scrolling a viewport with a stale cursor.
    /// Other sidebar lists keep their viewport-only scroll behavior.
    fn scroll_sidebar_wheel(&mut self, delta: isize) {
        let active_ctx = self.context_mgr.active();
        if matches!(active_ctx, ContextId::Commits | ContextId::BranchCommits) {
            let model = self.model.lock().unwrap();
            self.context_mgr.move_selection(delta, &model);
            self.context_mgr.viewport_manually_scrolled = false;
        } else {
            let model = self.model.lock().unwrap();
            let list_len = self.context_mgr.list_len(&model);
            drop(model);
            let visible_height = self.sidebar_visible_height();
            let mut offset = self.context_mgr.scroll_offset(active_ctx);
            scroll::scroll_viewport(&mut offset, delta, list_len, visible_height);
            self.context_mgr.set_scroll_offset(active_ctx, offset);
            self.context_mgr.viewport_manually_scrolled = true;
        }
    }

    pub(super) fn handle_diff_mode_mouse(&mut self, mouse: MouseEvent) {
        use self::modes::diff_mode::{DiffModeFocus, DiffModeSelector};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        use ratatui::layout::Rect;

        // Help popup intercepts mouse scroll
        if let PopupState::CommandPalette {
            sections,
            scroll_offset,
            search_textarea,
            ..
        } = &mut self.popup
        {
            let search_lower = search_textarea.lines().join("").to_lowercase();
            let has_search = !search_lower.is_empty();
            let total_rows: usize = sections
                .iter()
                .map(|s| {
                    let visible = if has_search {
                        s.entries
                            .iter()
                            .filter(|e| {
                                e.key.to_lowercase().contains(&search_lower)
                                    || e.description.to_lowercase().contains(&search_lower)
                            })
                            .count()
                    } else {
                        s.entries.len()
                    };
                    if visible > 0 { visible + 1 } else { 0 }
                })
                .sum();

            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    *scroll_offset = scroll_offset.saturating_sub(3);
                }
                MouseEventKind::ScrollDown => {
                    *scroll_offset = (*scroll_offset + 3).min(total_rows.saturating_sub(1));
                }
                _ => {}
            }
            return;
        }

        // Free-entry list pickers (RefPicker / ListPicker) intercept mouse scroll and click
        if matches!(
            self.popup,
            PopupState::RefPicker { .. } | PopupState::ListPicker { .. }
        ) {
            let (core, w, h) = match &mut self.popup {
                PopupState::RefPicker { core, .. } | PopupState::ListPicker { core, .. } => {
                    (core, self.layout.width, self.layout.height)
                }
                _ => unreachable!(),
            };
            handle_list_picker_mouse(core, mouse, w, h);
            return;
        }

        let area = Rect::new(0, 0, self.layout.width, self.layout.height);

        let compare_layout = self.compute_compare_layout();
        let sidebar = compare_layout.sidebar;

        let selector_a_rect = sidebar[0];
        let selector_b_rect = sidebar[1];
        let files_rect = sidebar[2];
        let commits_rect = sidebar[3];
        let diff_rect = compare_layout.diff;

        let col = mouse.column;
        let row = mouse.row;

        // Combobox dropdown mouse handling — intercepts clicks/scrolls when editing
        if self.diff_mode.editing.is_some() && !self.diff_mode.search_results.is_empty() {
            let anchor = if matches!(
                self.diff_mode.editing,
                Some(crate::gui::modes::diff_mode::DiffModeSelector::A)
            ) {
                selector_a_rect
            } else {
                selector_b_rect
            };
            let total = self.diff_mode.search_results.len();
            let max_items = 10usize.min(total);
            let dropdown_height = (max_items as u16) + 2;
            let available_height = area.height.saturating_sub(anchor.y + anchor.height);
            let dropdown_area = Rect {
                x: anchor.x,
                y: anchor.y + anchor.height,
                width: anchor.width,
                height: dropdown_height.min(available_height),
            };

            if rect_contains(dropdown_area, col, row) {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        // Click on a dropdown item — select it and confirm
                        let inner_y = row.saturating_sub(dropdown_area.y + 1); // +1 for top border
                        let clicked_idx = self.diff_mode.dropdown_scroll + inner_y as usize;
                        if clicked_idx < total {
                            self.diff_mode.search_selected = clicked_idx;
                            self.diff_mode.confirm_selection();
                            if self.diff_mode.has_both_refs() {
                                let _ = crate::gui::controller::diff_mode::reload_diff_files(self);
                                self.diff_mode.set_focus(DiffModeFocus::CommitFiles);
                            } else if self.diff_mode.ref_a.is_empty() {
                                self.diff_mode.set_focus(DiffModeFocus::SelectorA);
                                self.diff_mode.start_editing(DiffModeSelector::A);
                                let model = self.model.lock().unwrap();
                                self.diff_mode.search_refs(
                                    &model.branches,
                                    &model.tags,
                                    &model.commits,
                                    &model.remotes,
                                    &model.head_branch_name,
                                );
                            } else {
                                self.diff_mode.set_focus(DiffModeFocus::SelectorB);
                                self.diff_mode.start_editing(DiffModeSelector::B);
                                let model = self.model.lock().unwrap();
                                self.diff_mode.search_refs(
                                    &model.branches,
                                    &model.tags,
                                    &model.commits,
                                    &model.remotes,
                                    &model.head_branch_name,
                                );
                            }
                            self.needs_diff_refresh = true;
                        }
                        return;
                    }
                    MouseEventKind::ScrollUp => {
                        if self.diff_mode.search_selected > 0 {
                            self.diff_mode.search_selected =
                                self.diff_mode.search_selected.saturating_sub(3);
                            self.diff_mode.ensure_dropdown_visible(10);
                        }
                        return;
                    }
                    MouseEventKind::ScrollDown => {
                        let len = self.diff_mode.search_results.len();
                        if len > 0 {
                            self.diff_mode.search_selected =
                                (self.diff_mode.search_selected + 3).min(len - 1);
                            self.diff_mode.ensure_dropdown_visible(10);
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Check if click is in the diff panel — start text selection
                if rect_contains(diff_rect, col, row) && !self.diff_view.is_empty() {
                    let pl = DiffPanelLayout::compute(diff_rect, &self.diff_view);
                    if self.try_handle_revert_block_click(diff_rect, pl, col, row) {
                        self.diff_mode.set_focus(DiffModeFocus::DiffExploration);
                        return;
                    }
                    if self.diff_view.is_sticky_row(row, &pl) {
                        self.diff_view.selection = None;
                        self.diff_mode.set_focus(DiffModeFocus::DiffExploration);
                        return;
                    }
                    if let Some(panel) = pl.panel_at_x(col) {
                        self.diff_view.selection = Some(TextSelection {
                            panel,
                            start_col: col,
                            start_row: row,
                            end_col: col,
                            end_row: row,
                            dragging: true,
                            is_click: false,
                            text: String::new(),
                            edit_line_number: None,
                            edit_column_number: None,
                        });
                    } else {
                        self.diff_view.selection = None;
                    }
                    self.diff_mode.set_focus(DiffModeFocus::DiffExploration);
                } else {
                    self.diff_view.selection = None;

                    // Click on panels to switch focus
                    if rect_contains(selector_a_rect, col, row) {
                        self.diff_mode.set_focus(DiffModeFocus::SelectorA);
                        // Start editing on click
                        self.diff_mode.start_editing(DiffModeSelector::A);
                        let model = self.model.lock().unwrap();
                        self.diff_mode.search_refs(
                            &model.branches,
                            &model.tags,
                            &model.commits,
                            &model.remotes,
                            &model.head_branch_name,
                        );
                    } else if rect_contains(selector_b_rect, col, row) {
                        self.diff_mode.set_focus(DiffModeFocus::SelectorB);
                        // Start editing on click
                        self.diff_mode.start_editing(DiffModeSelector::B);
                        let model = self.model.lock().unwrap();
                        self.diff_mode.search_refs(
                            &model.branches,
                            &model.tags,
                            &model.commits,
                            &model.remotes,
                            &model.head_branch_name,
                        );
                    } else if rect_contains(files_rect, col, row) {
                        self.diff_mode.set_focus(DiffModeFocus::CommitFiles);
                        self.needs_diff_refresh = true;
                        // Click to select a file — use stored scroll offset
                        let inner_y = row.saturating_sub(files_rect.y + 1);
                        let len = self.diff_mode.visible_files_len();
                        let clicked_idx = self.diff_mode.diff_files_scroll + inner_y as usize;
                        if clicked_idx < len {
                            self.diff_mode.diff_files_selected = clicked_idx;
                            self.diff_mode.viewport_manually_scrolled = false;
                            self.needs_diff_refresh = true;
                        }
                    } else if rect_contains(commits_rect, col, row) {
                        self.diff_mode.set_focus(DiffModeFocus::Commits);
                        let inner_y = row.saturating_sub(commits_rect.y + 1);
                        let clicked_idx = self.diff_mode.commits_scroll + inner_y as usize;
                        if row > commits_rect.y
                            && row < commits_rect.bottom().saturating_sub(1)
                            && clicked_idx < self.diff_mode.commits.len()
                        {
                            self.diff_mode.commits_selected = clicked_idx;
                            self.diff_mode.commits_viewport_manually_scrolled = false;
                        }
                        self.needs_diff_refresh = true;
                    } else if rect_contains(diff_rect, col, row) {
                        self.diff_mode.set_focus(DiffModeFocus::DiffExploration);
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let pl = DiffPanelLayout::compute(diff_rect, &self.diff_view);
                if let Some(ref mut sel) = self.diff_view.selection {
                    if sel.dragging {
                        let (cmin, cmax) = pl.content_range(sel.panel);
                        let col_min = cmin.saturating_sub(5);
                        sel.end_col = col.max(col_min).min(cmax.saturating_sub(1));
                        sel.end_row = row.max(pl.inner_y).min(pl.inner_end_y.saturating_sub(1));
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(ref mut sel) = self.diff_view.selection {
                    sel.dragging = false;
                    if sel.start_col == sel.end_col && sel.start_row == sel.end_row {
                        if self.diff_view.file_exists_on_disk {
                            sel.is_click = true;
                        } else {
                            self.diff_view.selection = None;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if rect_contains(diff_rect, col, row) {
                    self.diff_view.selection = None;
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        self.diff_view.scroll_left(4);
                    } else {
                        self.diff_view.scroll_up(3);
                    }
                } else if rect_contains(files_rect, col, row) {
                    // Viewport-only scroll: move scroll offset without changing selection
                    let len = self.diff_mode.visible_files_len();
                    let visible_height = files_rect.height.saturating_sub(2) as usize;
                    scroll::scroll_viewport(
                        &mut self.diff_mode.diff_files_scroll,
                        -3,
                        len,
                        visible_height,
                    );
                    self.diff_mode.viewport_manually_scrolled = true;
                } else if rect_contains(commits_rect, col, row) {
                    scroll::scroll_viewport(
                        &mut self.diff_mode.commits_scroll,
                        -3,
                        self.diff_mode.commits.len(),
                        commits_rect.height.saturating_sub(2) as usize,
                    );
                    self.diff_mode.commits_viewport_manually_scrolled = true;
                }
            }
            MouseEventKind::ScrollDown => {
                if rect_contains(diff_rect, col, row) {
                    self.diff_view.selection = None;
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        self.diff_view.scroll_right(4);
                    } else {
                        self.diff_view.scroll_down(3);
                    }
                } else if rect_contains(files_rect, col, row) {
                    // Viewport-only scroll: move scroll offset without changing selection
                    let len = self.diff_mode.visible_files_len();
                    let visible_height = files_rect.height.saturating_sub(2) as usize;
                    scroll::scroll_viewport(
                        &mut self.diff_mode.diff_files_scroll,
                        3,
                        len,
                        visible_height,
                    );
                    self.diff_mode.viewport_manually_scrolled = true;
                } else if rect_contains(commits_rect, col, row) {
                    let visible_height = commits_rect.height.saturating_sub(2) as usize;
                    if self.diff_mode.commits_scroll + visible_height + 3
                        >= self.diff_mode.commits.len()
                    {
                        if let Err(error) = controller::diff_mode::load_more_commits(self) {
                            self.popup = PopupState::Message {
                                title: "Compare commits".into(),
                                message: error.to_string(),
                                kind: popup::MessageKind::Error,
                            };
                        }
                    }
                    scroll::scroll_viewport(
                        &mut self.diff_mode.commits_scroll,
                        3,
                        self.diff_mode.commits.len(),
                        visible_height,
                    );
                    self.diff_mode.commits_viewport_manually_scrolled = true;
                }
            }
            MouseEventKind::ScrollLeft => {
                if rect_contains(diff_rect, col, row) {
                    self.diff_view.scroll_left(4);
                }
            }
            MouseEventKind::ScrollRight => {
                if rect_contains(diff_rect, col, row) {
                    self.diff_view.scroll_right(4);
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_mouse_click(&mut self, col: u16, row: u16) {
        let fl = self.compute_current_frame_layout();

        // Commit details panel is non-focusable; swallow clicks that land there
        // so they don't leak into the diff view / sidebars.
        if let Some(details_rect) = fl.commit_details_panel
            && rect_contains(details_rect, col, row)
        {
            return;
        }

        // In Full screen mode with sidebar focused, the sidebar is rendered
        // in main_panel — treat clicks there as sidebar item selection.
        if self.screen_mode == ScreenMode::Full && !self.diff_focused {
            let panel_rect = fl.main_panel;
            if panel_rect.x <= col
                && col < panel_rect.x + panel_rect.width
                && panel_rect.y <= row
                && row < panel_rect.y + panel_rect.height
            {
                let inner_y = row.saturating_sub(panel_rect.y + 1);
                let active_ctx = self.context_mgr.active();
                let model = self.model.lock().unwrap();
                let list_len = self.context_mgr.list_len(&model);
                drop(model);
                let scroll_offset = self.context_mgr.scroll_offset(active_ctx);
                let clicked_idx = scroll_offset + inner_y as usize;
                if clicked_idx < list_len {
                    self.context_mgr.set_selection(clicked_idx);
                }
            }
            return;
        }

        // Check if click is in the main (diff) panel
        if fl.main_panel.x <= col
            && col < fl.main_panel.x + fl.main_panel.width
            && fl.main_panel.y <= row
            && row < fl.main_panel.y + fl.main_panel.height
        {
            if !self.diff_view.is_empty() {
                self.diff_focused = true;
            }
            return;
        }

        // Check which side panel was clicked
        for (i, &panel_rect) in fl.side_panels.iter().enumerate() {
            if panel_rect.x <= col
                && col < panel_rect.x + panel_rect.width
                && panel_rect.y <= row
                && row < panel_rect.y + panel_rect.height
            {
                self.diff_focused = false;
                if let Some(&window) = SideWindow::ALL.get(i) {
                    let is_title_bar = row == panel_rect.y;

                    if is_title_bar {
                        // Title bar click: switch to the clicked tab if identifiable.
                        let local_x = col.saturating_sub(panel_rect.x);
                        if let Some(tab_ctx) = window.tab_at_x(local_x) {
                            self.context_mgr.set_active(tab_ctx);
                        } else {
                            // Clicked title area but not on a specific tab label —
                            // just activate this window (restore last context).
                            let ctx = self.context_mgr.last_context_for_window(window);
                            self.context_mgr.set_active(ctx);
                        }
                    } else {
                        // Content area click.
                        let current_window = self.context_mgr.active_window();
                        if current_window != window {
                            // Switching to a different window — restore its last context.
                            let ctx = self.context_mgr.last_context_for_window(window);
                            self.context_mgr.set_active(ctx);
                        }
                        // Same window: don't call set_active, preserving any sub-view.

                        // Select the clicked item.
                        let inner_y = row.saturating_sub(panel_rect.y + 1); // +1 for border
                        let active_ctx = self.context_mgr.active();
                        let model = self.model.lock().unwrap();
                        let list_len = self.context_mgr.list_len(&model);
                        drop(model);

                        let scroll_offset = self.context_mgr.scroll_offset(active_ctx);
                        let clicked_idx = scroll_offset + inner_y as usize;
                        if clicked_idx < list_len {
                            self.context_mgr.set_selection(clicked_idx);
                        }
                    }
                }
                return;
            }
        }
    }

    pub(super) fn is_in_main_panel(&self, col: u16, row: u16) -> bool {
        let mp = self.compute_main_panel_rect();
        col >= mp.x && col < mp.x + mp.width && row >= mp.y && row < mp.y + mp.height
    }

    /// True if mouse is over the (non-focusable) commit details panel.
    pub(super) fn is_in_commit_details_panel(&self, col: u16, row: u16) -> bool {
        let fl = self.compute_current_frame_layout();
        fl.commit_details_panel
            .map(|r| rect_contains(r, col, row))
            .unwrap_or(false)
    }

    /// Compute the current frame layout using the same flags as views::render.
    /// This must match views.rs so mouse coords map to the rects actually drawn.
    pub(super) fn compute_current_frame_layout(&self) -> layout::FrameLayout {
        let area = ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height);
        let panel_count = SideWindow::ALL.len();
        let active_window = self.context_mgr.active_window();
        let active_panel_index = SideWindow::ALL
            .iter()
            .position(|w| *w == active_window)
            .unwrap_or(1);

        // Mirror views.rs: show_details when the active context is a commit
        // list (or drill-in commit files) with a valid selection.
        let show_details = self.details_panel_applies();

        layout::compute_layout_with_details(
            area,
            self.layout.side_panel_ratio,
            panel_count,
            active_panel_index,
            self.screen_mode,
            show_details,
            !self.diff_focused,
        )
    }

    pub(super) fn compute_compare_layout(&self) -> modes::diff_mode::CompareLayout {
        modes::diff_mode::CompareLayout::new(
            ratatui::layout::Rect::new(0, 0, self.layout.width, self.layout.height),
            self.layout.side_panel_ratio,
            self.screen_mode,
            &self.diff_mode,
        )
    }

    /// Content area above the status bar (side + main live here).
    pub(super) fn content_area_rect(&self) -> ratatui::layout::Rect {
        ratatui::layout::Rect::new(
            0,
            0,
            self.layout.width,
            self.layout.height.saturating_sub(1),
        )
    }

    /// Hit-test the side↔main split for drag-resize.
    /// Portrait: expanded panel bottom border and/or main (diff) top border.
    /// Landscape: ~3-col strip around the vertical split.
    pub(super) fn sidebar_divider_hit(&self, col: u16, row: u16) -> bool {
        if self.screen_mode != ScreenMode::Normal {
            return false;
        }
        let content = self.content_area_rect();
        if content.width == 0 || content.height == 0 || !rect_contains(content, col, row) {
            return false;
        }

        if self.diff_mode.active {
            return self.compute_compare_layout().divider_hit(content, col, row);
        }
        let fl = self.compute_current_frame_layout();

        if fl.portrait {
            // Either the expanded side panel's bottom border or the main/diff
            // panel's top border (collapsed panels may sit between them).
            if fl.side_panels.is_empty() {
                return row == content.y;
            }
            if fl.main_panel.height == 0 {
                let y = content.y + content.height.saturating_sub(1);
                return row == y;
            }
            // Top border of the diff box — single row only (content is y+1).
            if fl.main_panel.height > 0 && row == fl.main_panel.y {
                return true;
            }
            let active_window = self.context_mgr.active_window();
            let active_idx = SideWindow::ALL
                .iter()
                .position(|w| *w == active_window)
                .unwrap_or(1);
            // Match layout.rs: Status stays compact; Files expands instead.
            let expand_idx = if active_idx == 0 { 1 } else { active_idx };
            let Some(panel) = fl.side_panels.get(expand_idx) else {
                return false;
            };
            if panel.height == 0 {
                return false;
            }
            // Only the bottom border row — a taller strip steals clicks from the
            // last list items (content sits on bottom-1 with Borders::ALL).
            let bottom = panel.y + panel.height.saturating_sub(1);
            row == bottom
        } else if fl.side_panels.is_empty() {
            col == content.x
        } else if fl.main_panel.width == 0 {
            col == content.x + content.width.saturating_sub(1)
        } else {
            let divider_x = fl.main_panel.x;
            let lo = divider_x.saturating_sub(1);
            let hi = divider_x.saturating_add(1);
            col >= lo && col <= hi
        }
    }

    /// Rows to add when mapping a portrait grab to the side/main split.
    /// Main/diff top border: 0 (row is already the split). Expanded panel bottom:
    /// 1 + trailing collapsed panels so both grabs drive the same ratio.
    pub(super) fn portrait_sidebar_resize_offset(&self, row: u16) -> u16 {
        if self.diff_mode.active {
            return self.compute_compare_layout().resize_row_offset(row);
        }
        let fl = self.compute_current_frame_layout();
        if !fl.portrait {
            return 0;
        }
        if fl.main_panel.height > 0 && row == fl.main_panel.y {
            return 0;
        }
        let panel_count = SideWindow::ALL.len();
        let active_window = self.context_mgr.active_window();
        let active_idx = SideWindow::ALL
            .iter()
            .position(|w| *w == active_window)
            .unwrap_or(1);
        let expand_idx = if active_idx == 0 { 1 } else { active_idx };
        let collapsed: u16 = 1;
        let trailing =
            (panel_count.saturating_sub(expand_idx.saturating_add(1)) as u16) * collapsed;
        1 + trailing
    }

    pub(super) fn apply_sidebar_ratio_from_mouse(&mut self, col: u16, row: u16) {
        let content = self.content_area_rect();
        if content.width == 0 || content.height == 0 {
            return;
        }
        let portrait = if self.diff_mode.active {
            self.compute_compare_layout().portrait
        } else {
            self.compute_current_frame_layout().portrait
        };
        let ratio = if portrait {
            let side_end = row
                .saturating_sub(content.y)
                .saturating_add(self.sidebar_resize_row_offset)
                .min(content.height);
            side_end as f64 / content.height as f64
        } else {
            let pos = col.saturating_sub(content.x).min(content.width);
            pos as f64 / content.width as f64
        };
        self.layout.side_panel_ratio = ratio.clamp(0.0, 1.0);
    }

    /// True when the active context is one where commit-details makes sense
    /// (drives the commit-details toggle and layout-time `show_details`).
    pub(super) fn context_has_commit_details(&self) -> bool {
        matches!(
            self.context_mgr.active(),
            ContextId::Commits
                | ContextId::BranchCommits
                | ContextId::Reflog
                | ContextId::CommitFiles
                | ContextId::BranchCommitFiles
                | ContextId::StashFiles
        )
    }

    pub(super) fn details_panel_applies(&self) -> bool {
        if !self.show_commit_details {
            return false;
        }
        let ctx = self.context_mgr.active();
        let sel = self.context_mgr.selected(ctx);
        let model = self.model.lock().unwrap();
        match ctx {
            ContextId::Commits => sel < model.commits.len(),
            ContextId::BranchCommits => sel < model.sub_commits.len(),
            ContextId::Reflog => sel < model.reflog_commits.len(),
            ContextId::CommitFiles | ContextId::BranchCommitFiles | ContextId::StashFiles => {
                let hash = &self.commit_files_hash;
                !hash.is_empty()
                    && (model.commits.iter().any(|c| c.hash == *hash)
                        || model.sub_commits.iter().any(|c| c.hash == *hash)
                        || model.reflog_commits.iter().any(|c| c.hash == *hash))
            }
            _ => false,
        }
    }

    /// Compute the exact main panel Rect using the real layout engine.
    pub(super) fn compute_main_panel_rect(&self) -> ratatui::layout::Rect {
        self.compute_current_frame_layout().main_panel
    }

    pub(super) fn revert_hunk_at_position(
        &self,
        panel_rect: ratatui::layout::Rect,
        layout: &DiffPanelLayout,
        col: u16,
        row: u16,
    ) -> Option<usize> {
        if self.context_mgr.active() != ContextId::Files {
            return None;
        }
        if self.diff_view.is_empty() {
            return None;
        }
        if !rect_contains(panel_rect, col, row) {
            return None;
        }
        let divider_x = layout.divider_x()?;
        if col != divider_x {
            return None;
        }
        let (line_idx, chunk_idx) = self.diff_view.line_chunk_at_row(row, layout)?;
        if chunk_idx != 0 {
            return None;
        }
        self.diff_view.hunk_index_for_start_line(line_idx)
    }

    pub(super) fn try_handle_revert_block_click(
        &mut self,
        panel_rect: ratatui::layout::Rect,
        layout: DiffPanelLayout,
        col: u16,
        row: u16,
    ) -> bool {
        if self.diff_mode.active {
            return false;
        }
        let Some(hunk_idx) = self.revert_hunk_at_position(panel_rect, &layout, col, row) else {
            return false;
        };
        self.diff_view.selected_revert_hunk = Some(hunk_idx);
        self.show_hunk_context_menu(hunk_idx);
        true
    }

    /// Open the hunk action menu (shown when Enter is pressed on a selected
    /// or hovered revert hunk). Cancel is focused first so an accidental
    /// Enter doesn't revert anything.
    pub(super) fn show_hunk_context_menu(&mut self, hunk_idx: usize) {
        // Directory hovers have no single file to act on — Cancel only.
        let has_file = self.selected_file_index().is_some();
        // The Files pane shows unstaged + staged hunks in one buffer. Offer
        // the action matching the hunk the menu was opened on so the index
        // can't be applied to the wrong side of the index.
        let hunk_is_staged = has_file && self.diff_view.is_staged_hunk(hunk_idx);
        let mut items = vec![popup::MenuItem::cancel()];
        if has_file && !hunk_is_staged {
            items.push(popup::MenuItem {
                label: "Stage hunk".to_string(),
                description: String::new(),
                key: Some("s".to_string()),
                action: Some(Box::new(move |gui| {
                    if let Err(err) = gui.stage_selected_file_hunk(hunk_idx) {
                        gui.popup = PopupState::Message {
                            title: "Stage hunk failed".to_string(),
                            message: format!("{}", err),
                            kind: MessageKind::Error,
                        };
                    }
                    Ok(())
                })),
            });
            items.push(popup::MenuItem {
                label: "Revert hunk".to_string(),
                description: String::new(),
                key: Some("r".to_string()),
                action: Some(Box::new(move |gui| {
                    if let Err(err) = gui.revert_selected_file_hunk(hunk_idx) {
                        gui.popup = PopupState::Message {
                            title: "Revert block failed".to_string(),
                            message: format!("{}", err),
                            kind: MessageKind::Error,
                        };
                    }
                    Ok(())
                })),
            });
        }
        if hunk_is_staged {
            items.push(popup::MenuItem {
                label: "Unstage hunk".to_string(),
                description: String::new(),
                key: Some("s".to_string()),
                action: Some(Box::new(move |gui| {
                    if let Err(err) = gui.unstage_selected_file_hunk(hunk_idx) {
                        gui.popup = PopupState::Message {
                            title: "Unstage hunk failed".to_string(),
                            message: format!("{}", err),
                            kind: MessageKind::Error,
                        };
                    }
                    Ok(())
                })),
            });
        }

        self.popup = PopupState::Menu {
            title: "Hunk".to_string(),
            items,
            selected: 0,
            loading_index: None,
        };
    }

    /// Match the viewed HEAD block against the blocks of a freshly fetched
    /// per-side diff, returning their slice ranges for patch building.
    /// Matching runs on the side both diffs share (worktree for unstaged,
    /// HEAD for staged) so staged edits elsewhere in the file can't shift
    /// the mapping. Sorted deepest-first so sequential applies don't shift
    /// the line numbers of pending ones.
    pub(super) fn matching_side_blocks(
        &self,
        hunk_idx: usize,
        side_diff: &str,
        new_side: bool,
    ) -> Vec<(Option<(usize, usize)>, Option<(usize, usize)>)> {
        use crate::pager::side_by_side::DiffViewState as DVS;
        let view_spans = DVS::block_spans(&self.diff_view.lines, &self.diff_view.hunk_line_offsets);
        let Some(view) = view_spans.get(hunk_idx) else {
            return Vec::new();
        };
        let mut matched: Vec<(usize, Option<(usize, usize)>, Option<(usize, usize)>)> =
            DVS::block_spans_for_diff(side_diff, 4)
                .into_iter()
                .filter(|s| view.overlaps(s, new_side))
                .map(|s| {
                    let anchor = s.old.map(|(lo, _)| lo).unwrap_or(s.old_point);
                    (anchor, s.old, s.new)
                })
                .collect();
        matched.sort_by(|a, b| b.0.cmp(&a.0));
        matched
            .into_iter()
            .map(|(_, old, new)| (old, new))
            .collect()
    }

    pub(super) fn stage_selected_file_hunk(&mut self, hunk_idx: usize) -> Result<()> {
        let Some(file_idx) = self.selected_file_index() else {
            return Ok(());
        };
        let model = self.model.lock().unwrap();
        let Some(file) = model.files.get(file_idx).cloned() else {
            return Ok(());
        };
        drop(model);

        let path_refs: Vec<String> = file.diff_paths().into_iter().map(str::to_string).collect();
        let refs: Vec<&str> = path_refs.iter().map(String::as_str).collect();
        let diff = self.git.diff_file_paths(&refs)?;
        if diff.is_empty() {
            // Untracked / synthesized diffs have no unified diff to slice —
            // fall back to staging the whole file.
            if !file.tracked {
                self.git.stage_file(file.current_path())?;
                self.needs_files_refresh = true;
                self.needs_diff_refresh = true;
            }
            return Ok(());
        }

        // The view may be a HEAD buffer, so map the hunk onto the unstaged
        // diff's blocks via the shared worktree side.
        let targets = self.matching_side_blocks(hunk_idx, &diff, true);
        if targets.is_empty() {
            self.popup = PopupState::Message {
                title: "Stage hunk".to_string(),
                message: "That hunk moved — the diff was refreshed.".to_string(),
                kind: MessageKind::Info,
            };
            self.needs_diff_refresh = true;
            return Ok(());
        }
        for (want_old, want_new) in targets {
            if want_old.is_none() && want_new.is_none() {
                continue;
            }
            self.git
                .stage_visual_block(file.current_path(), &diff, want_old, want_new)?;
        }
        self.needs_files_refresh = true;
        self.needs_diff_refresh = true;
        Ok(())
    }

    pub(super) fn unstage_selected_file_hunk(&mut self, hunk_idx: usize) -> Result<()> {
        let Some(file_idx) = self.selected_file_index() else {
            return Ok(());
        };
        let model = self.model.lock().unwrap();
        let Some(file) = model.files.get(file_idx).cloned() else {
            return Ok(());
        };
        drop(model);

        let path_refs: Vec<String> = file.diff_paths().into_iter().map(str::to_string).collect();
        let refs: Vec<&str> = path_refs.iter().map(String::as_str).collect();
        let diff = self.git.diff_file_staged_paths(&refs)?;
        if diff.is_empty() {
            return Ok(());
        }

        // The view may be a HEAD buffer, so map the hunk onto the staged
        // diff's blocks via the shared HEAD side.
        let targets = self.matching_side_blocks(hunk_idx, &diff, false);
        if targets.is_empty() {
            self.popup = PopupState::Message {
                title: "Unstage hunk".to_string(),
                message: "That hunk moved — the diff was refreshed.".to_string(),
                kind: MessageKind::Info,
            };
            self.needs_diff_refresh = true;
            return Ok(());
        }
        for (want_old, want_new) in targets {
            if want_old.is_none() && want_new.is_none() {
                continue;
            }
            self.git
                .unstage_visual_block(file.current_path(), &diff, want_old, want_new)?;
        }
        self.needs_files_refresh = true;
        self.needs_diff_refresh = true;
        Ok(())
    }

    pub(super) fn revert_selected_file_hunk(&mut self, hunk_idx: usize) -> Result<()> {
        let Some(file_idx) = self.selected_file_index() else {
            return Ok(());
        };

        let model = self.model.lock().unwrap();
        let Some(file) = model.files.get(file_idx) else {
            return Ok(());
        };

        if !file.has_unstaged_changes {
            self.popup = PopupState::Message {
                title: "Revert block".to_string(),
                message: "Block revert is available only for unstaged changes.".to_string(),
                kind: MessageKind::Info,
            };
            return Ok(());
        }

        let file_name = file.name.clone();
        drop(model);

        let diff = self.git.diff_file(&file_name)?;
        if diff.is_empty() {
            return Ok(());
        }

        // The view may be a HEAD buffer, so map the hunk onto the unstaged
        // diff's blocks via the shared worktree side.
        let targets = self.matching_side_blocks(hunk_idx, &diff, true);
        if targets.is_empty() {
            self.popup = PopupState::Message {
                title: "Revert block".to_string(),
                message: "That hunk moved — the diff was refreshed.".to_string(),
                kind: MessageKind::Info,
            };
            self.needs_diff_refresh = true;
            return Ok(());
        }

        // Snapshot the working-tree file before reverting so the user can undo
        // (`u`) within this session. Only keep the snapshot if the revert
        // actually succeeds; otherwise we'd leak unrelated state into the stack.
        let abs_path = self.git.repo_path().join(&file_name);
        let pre_bytes = std::fs::read(&abs_path).ok();

        for (want_old, want_new) in targets {
            if want_old.is_none() && want_new.is_none() {
                continue;
            }
            self.git
                .revert_visual_block_in_worktree(&file_name, &diff, want_old, want_new)?;
        }

        if let Some(bytes) = pre_bytes {
            let stack = &mut self.diff_view.revert_undo_stack;
            if stack.len() >= crate::pager::side_by_side::REVERT_UNDO_STACK_CAP {
                stack.remove(0);
            }
            stack.push(crate::pager::side_by_side::RevertUndoEntry {
                file_path: file_name.clone(),
                pre_revert_bytes: bytes,
            });
            self.diff_view.revert_undo_high_water =
                self.diff_view.revert_undo_high_water.max(stack.len());
        }

        self.diff_view.selection = None;
        self.needs_files_refresh = true;
        self.needs_diff_refresh = true;
        Ok(())
    }

    pub(super) fn undo_last_revert_block(&mut self) -> Result<()> {
        let Some(entry) = self.diff_view.revert_undo_stack.pop() else {
            return Ok(());
        };
        let abs_path = self.git.repo_path().join(&entry.file_path);
        std::fs::write(&abs_path, &entry.pre_revert_bytes)
            .with_context(|| format!("failed to restore {}", entry.file_path))?;
        if self.diff_view.revert_undo_stack.is_empty() {
            self.diff_view.revert_undo_high_water = 0;
        }
        self.needs_files_refresh = true;
        self.needs_diff_refresh = true;
        Ok(())
    }

    /// Approximate visible height of the active sidebar panel (inner area minus borders).
    pub(super) fn sidebar_visible_height(&self) -> usize {
        let fl = self.compute_current_frame_layout();
        let active_window = self.context_mgr.active_window();
        let active_panel_index = SideWindow::ALL
            .iter()
            .position(|w| *w == active_window)
            .unwrap_or(1);
        // In Full screen mode with sidebar focused, the list is rendered in main_panel
        let panel_rect = if self.screen_mode == ScreenMode::Full && !self.diff_focused {
            fl.main_panel
        } else {
            fl.side_panels
                .get(active_panel_index)
                .copied()
                .unwrap_or(fl.main_panel)
        };
        // Subtract 2 for top/bottom borders
        panel_rect.height.saturating_sub(2) as usize
    }
}
