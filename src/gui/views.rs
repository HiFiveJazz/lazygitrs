#[path = "popup_render.rs"]
mod popup_render;
pub(crate) use popup_render::render_popup;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use std::collections::HashSet;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::theme::mix_colors;
use crate::config::{AppConfig, KeybindingConfig, Theme};
use crate::model::Model;
use crate::model::commit::{Commit, CommitStat};
use crate::model::file_tree::{CommitFileTreeNode, FileTreeNode};
use crate::pager::side_by_side::{self, DiffPanel, DiffPanelLayout, DiffViewLayout, DiffViewState};

use super::ScreenMode;
use super::context::{ContextId, ContextManager, SideWindow};
use super::layout::{self, LayoutState};
use super::popup::{
    COMMIT_BODY_GUTTER_WIDTH, CommitInputFocus, CommitInputMode, EmailPatchFocus, PopupState,
    commit_popup_height, commit_popup_width, list_picker_matching_indices,
};
use super::presentation;

pub fn render(
    frame: &mut Frame,
    model: &Model,
    ctx_mgr: &mut ContextManager,
    layout_state: &LayoutState,
    popup: &PopupState,
    config: &AppConfig,
    theme: &Theme,
    diff_view: &mut DiffViewState,
    commit_list_cache: &mut presentation::commits::CommitListCache,
    screen_mode: ScreenMode,
    show_file_tree: bool,
    file_tree_nodes: &[FileTreeNode],
    collapsed_dirs: &HashSet<String>,
    diff_focused: bool,
    search_state: Option<(&str, usize, usize)>,
    search_textarea: Option<&tui_textarea::TextArea<'_>>,
    command_log: &[String],
    show_command_log: bool,
    active_commit_filters: &[String],
    show_commit_file_tree: bool,
    commit_file_tree_nodes: &[CommitFileTreeNode],
    commit_files_collapsed_dirs: &HashSet<String>,
    commit_files_hash: &str,
    commit_files_message: &str,
    branch_commits_name: &str,
    remote_branches_name: &str,
    sub_commits_parent_context: ContextId,
    spinner_frame: usize,
    remote_op_label: Option<&str>,
    remote_op_success: bool,
    cherry_pick_clipboard: &[String],
    range_select_anchor: Option<usize>,
    diff_loading: bool,
    diff_loading_show: bool,
    commit_stats: &Arc<Mutex<HashMap<String, CommitStat>>>,
    commit_messages: &Arc<Mutex<HashMap<String, String>>>,
    commit_details_scroll: &mut u16,
    commit_details_scroll_hash: &mut String,
    show_commit_details: bool,
    ai_button_hovered: bool,
    ai_configured: bool,
) {
    let area = frame.area();
    let panel_count = SideWindow::ALL.len();
    let active_file_tree = match ctx_mgr.active() {
        ContextId::Files => show_file_tree,
        ContextId::CommitFiles | ContextId::BranchCommitFiles | ContextId::StashFiles => {
            show_commit_file_tree
        }
        _ => false,
    };

    // Determine which panel index is active so it gets expanded
    let active_window = ctx_mgr.active_window();
    let active_panel_index = SideWindow::ALL
        .iter()
        .position(|w| *w == active_window)
        .unwrap_or(1); // default to Files

    // Determine if the commit-details panel should be shown.  We show it when
    // the active context is a commit-listing context and a commit is selected.
    let current_commit: Option<&Commit> = resolve_current_commit(model, ctx_mgr, commit_files_hash);
    let show_details = show_commit_details && current_commit.is_some();

    let fl = layout::compute_layout_with_details(
        area,
        layout_state.side_panel_ratio,
        panel_count,
        active_panel_index,
        screen_mode,
        show_details,
        !diff_focused, // sidebar_focused_full: only meaningful in Full mode
    );

    // Full screen mode
    if screen_mode == ScreenMode::Full {
        if diff_focused {
            // Diff is focused: show diff fullscreen
            if !diff_view.is_empty() {
                let show_revert_markers = ctx_mgr.active() == ContextId::Files;
                side_by_side::render_diff(
                    frame,
                    fl.main_panel,
                    diff_view,
                    theme,
                    true,
                    diff_loading_show,
                    show_revert_markers,
                );
                side_by_side::render_diff_search_highlights(frame, fl.main_panel, diff_view, theme);
                side_by_side::render_diff_search_bar(frame, fl.main_panel, diff_view, theme);
            } else if diff_loading {
                let block = Block::default()
                    .title(" Diff ")
                    .borders(Borders::ALL)
                    .border_style(theme.active_border);
                if diff_loading_show {
                    let widget = Paragraph::new(" Loading diff...").block(block);
                    frame.render_widget(widget, fl.main_panel);
                } else {
                    frame.render_widget(block, fl.main_panel);
                }
            } else {
                let block = Block::default()
                    .title(" Diff ")
                    .borders(Borders::ALL)
                    .border_style(theme.active_border);
                let widget = Paragraph::new(" No changes to display").block(block);
                frame.render_widget(widget, fl.main_panel);
            }
        } else {
            // Sidebar is focused: show active sidebar panel fullscreen
            let ctx_id = ctx_mgr.active();
            let selected = ctx_mgr.selected(ctx_id);
            let title = if ctx_id == ContextId::CommitFiles
                || ctx_id == ContextId::StashFiles
                || ctx_id == ContextId::BranchCommitFiles
            {
                build_commit_files_title(ctx_id, commit_files_hash, commit_files_message, theme)
            } else if ctx_id == ContextId::BranchCommits {
                build_branch_commits_title(branch_commits_name, theme)
            } else if ctx_id == ContextId::Commits && !active_commit_filters.is_empty() {
                let filter_label = active_commit_filters.join(", ");
                Line::from(vec![
                    Span::raw(" Commits "),
                    Span::styled(
                        format!("[filter: {}] ", filter_label),
                        Style::default().fg(theme.accent_secondary),
                    ),
                ])
            } else {
                build_window_title(ctx_mgr.active_window(), ctx_id, ctx_mgr, theme)
            };
            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(theme.active_border);

            match ctx_id {
                ContextId::Status => {
                    render_status_main(frame, fl.main_panel, model, config, theme, block);
                }
                ContextId::Files => {
                    if show_file_tree {
                        let items = presentation::files::render_file_tree(
                            model,
                            theme,
                            file_tree_nodes,
                            collapsed_dirs,
                            fl.main_panel.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            fl.main_panel,
                            block,
                            items,
                            selected,
                            true,
                            theme,
                            ctx_mgr,
                            ctx_id,
                        );
                    } else {
                        let items = presentation::files::render_file_list(
                            model,
                            theme,
                            fl.main_panel.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            fl.main_panel,
                            block,
                            items,
                            selected,
                            true,
                            theme,
                            ctx_mgr,
                            ctx_id,
                        );
                    }
                }
                ContextId::Worktrees | ContextId::Submodules => {
                    let items = if ctx_id == ContextId::Worktrees {
                        render_worktree_list(model, theme)
                    } else {
                        render_submodule_list(model, theme)
                    };
                    render_list_ctx(
                        frame,
                        fl.main_panel,
                        block,
                        items,
                        selected,
                        true,
                        theme,
                        ctx_mgr,
                        ctx_id,
                    );
                }
                ContextId::Branches => {
                    let items = presentation::branches::render_branch_list(
                        model,
                        theme,
                        remote_op_label,
                        spinner_frame,
                        remote_op_success,
                    );
                    render_list_ctx(
                        frame,
                        fl.main_panel,
                        block,
                        items,
                        selected,
                        true,
                        theme,
                        ctx_mgr,
                        ctx_id,
                    );
                }
                ContextId::Remotes | ContextId::RemoteBranches => {
                    if ctx_mgr.active() == ContextId::RemoteBranches {
                        let rb_selected = ctx_mgr.selected(ContextId::RemoteBranches);
                        let rb_block = Block::default()
                            .title(format!(" Remote Branches ({}) ", remote_branches_name))
                            .borders(Borders::ALL)
                            .border_style(theme.active_border);
                        let items = presentation::remote_branches::render_remote_branch_list(
                            &model.sub_remote_branches,
                            &model.head_branch_name,
                            theme,
                        );
                        render_list_ctx(
                            frame,
                            fl.main_panel,
                            rb_block,
                            items,
                            rb_selected,
                            true,
                            theme,
                            ctx_mgr,
                            ContextId::RemoteBranches,
                        );
                    } else {
                        let items = presentation::remotes::render_remote_list(model, theme);
                        render_list_ctx(
                            frame,
                            fl.main_panel,
                            block,
                            items,
                            selected,
                            true,
                            theme,
                            ctx_mgr,
                            ctx_id,
                        );
                    }
                }
                ContextId::Tags => {
                    let items = presentation::tags::render_tag_list(model, theme);
                    render_list_ctx(
                        frame,
                        fl.main_panel,
                        block,
                        items,
                        selected,
                        true,
                        theme,
                        ctx_mgr,
                        ctx_id,
                    );
                }
                ContextId::Commits => {
                    let range = range_select_anchor.map(|a| (a.min(selected), a.max(selected)));
                    render_commit_list_ctx(
                        frame,
                        fl.main_panel,
                        block,
                        model,
                        theme,
                        cherry_pick_clipboard,
                        selected,
                        true,
                        range,
                        ctx_mgr,
                        ctx_id,
                        commit_list_cache,
                        false,
                        screen_mode != ScreenMode::Normal,
                    );
                }
                ContextId::Stash => {
                    let items = presentation::stash::render_stash_list(model, theme);
                    render_list_ctx(
                        frame,
                        fl.main_panel,
                        block,
                        items,
                        selected,
                        true,
                        theme,
                        ctx_mgr,
                        ctx_id,
                    );
                }
                ContextId::BranchCommits => {
                    render_commit_list_ctx(
                        frame,
                        fl.main_panel,
                        block,
                        model,
                        theme,
                        &[],
                        selected,
                        true,
                        None,
                        ctx_mgr,
                        ctx_id,
                        commit_list_cache,
                        true,
                        screen_mode != ScreenMode::Normal,
                    );
                }
                ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            fl.main_panel.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            fl.main_panel,
                            block,
                            items,
                            selected,
                            true,
                            theme,
                            ctx_mgr,
                            ctx_id,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            fl.main_panel.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            fl.main_panel,
                            block,
                            items,
                            selected,
                            true,
                            theme,
                            ctx_mgr,
                            ctx_id,
                        );
                    }
                }
                _ => {
                    let widget = Paragraph::new("").block(block);
                    frame.render_widget(widget, fl.main_panel);
                }
            }
            // Highlight `/` search matches on the full-mode list.
            if let Some((query, _, _)) = search_state {
                if !query.is_empty() {
                    render_list_search_highlights(frame, fl.main_panel, query, theme);
                }
            }
        }
        // Full-mode details strip (sidebar-focused only) — compact, above sidebar.
        if let (Some(details_rect), Some(commit)) = (fl.commit_details_panel, current_commit) {
            if commit_details_scroll_hash.as_str() != commit.hash.as_str() {
                *commit_details_scroll = 0;
                *commit_details_scroll_hash = commit.hash.clone();
            }
            render_commit_details_panel(
                frame,
                details_rect,
                commit,
                commit_stats,
                commit_messages,
                theme,
                true,
                commit_details_scroll,
            );
        }
        render_search_bar_or_status_bar(
            frame,
            fl.status_bar,
            search_state,
            search_textarea,
            ctx_mgr,
            diff_view,
            theme,
            model,
            diff_focused,
            !cherry_pick_clipboard.is_empty(),
            &config.user_config.keybinding,
            !active_commit_filters.is_empty(),
            active_file_tree,
        );
        // Render text selection highlight overlay and tooltip (must be before popup)
        render_selection_overlay(frame, diff_view, fl.main_panel, theme);
        if *popup != PopupState::None {
            render_popup(
                frame,
                popup,
                area,
                spinner_frame,
                theme,
                ai_button_hovered,
                ai_configured,
            );
        }
        render_command_log(frame, &fl, command_log, show_command_log, theme);
        return;
    }

    // Render sidebar panels — one per window
    for (i, window) in SideWindow::ALL.iter().enumerate() {
        if i >= fl.side_panels.len() {
            break;
        }
        let rect = fl.side_panels[i];
        let ctx_id = ctx_mgr.active_context_for_window(*window);
        let is_active = ctx_mgr.active_window() == *window;
        // An inactive Branches panel should show HEAD, not whichever other
        // branch the user last browsed. Keep its interactive selection intact.
        let selected = sidebar_selected_index(*window, ctx_id, ctx_mgr, model);

        let border_style = if is_active && !diff_focused {
            theme.active_border
        } else {
            theme.inactive_border
        };

        // Build title with tab indicators for multi-tab windows
        let title = if *window == SideWindow::Commits
            && ctx_id == ContextId::Commits
            && !active_commit_filters.is_empty()
        {
            let filter_label = active_commit_filters.join(", ");
            Line::from(vec![
                Span::raw(" Commits "),
                Span::styled(
                    format!("[filter: {}] ", filter_label),
                    Style::default().fg(theme.accent_secondary),
                ),
            ])
        } else {
            build_window_title(*window, ctx_id, ctx_mgr, theme)
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style);

        match ctx_id {
            ContextId::Status => {
                let inner_width = rect.width.saturating_sub(2) as usize;
                let status_line = render_status_sidebar(model, config, inner_width, theme);
                let widget = Paragraph::new(status_line).block(block);
                frame.render_widget(widget, rect);
            }
            ContextId::Files => {
                if show_file_tree {
                    let items = presentation::files::render_file_tree(
                        model,
                        theme,
                        file_tree_nodes,
                        collapsed_dirs,
                        rect.width.saturating_sub(2) as usize,
                    );
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                } else {
                    let items = presentation::files::render_file_list(
                        model,
                        theme,
                        rect.width.saturating_sub(2) as usize,
                    );
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                }
            }
            ContextId::Worktrees => {
                let items = render_worktree_list(model, theme);
                render_list_ctx(
                    frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                );
            }
            ContextId::Submodules => {
                let items = render_submodule_list(model, theme);
                render_list_ctx(
                    frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                );
            }
            ContextId::Branches => {
                // If BranchCommits or BranchCommitFiles is active, render that instead
                if ctx_mgr.active() == ContextId::BranchCommitFiles {
                    let cf_selected = ctx_mgr.selected(ContextId::BranchCommitFiles);
                    let cf_title = build_commit_files_title(
                        ContextId::BranchCommitFiles,
                        commit_files_hash,
                        commit_files_message,
                        theme,
                    );
                    let cf_block = Block::default()
                        .title(cf_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::BranchCommitFiles,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::BranchCommitFiles,
                        );
                    }
                } else if ctx_mgr.active() == ContextId::BranchCommits {
                    let bc_selected = ctx_mgr.selected(ContextId::BranchCommits);
                    let bc_title = build_branch_commits_title(branch_commits_name, theme);
                    let bc_block = Block::default()
                        .title(bc_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    render_commit_list_ctx(
                        frame,
                        rect,
                        bc_block,
                        model,
                        theme,
                        &[],
                        bc_selected,
                        is_active,
                        None,
                        ctx_mgr,
                        ContextId::BranchCommits,
                        commit_list_cache,
                        true,
                        screen_mode != ScreenMode::Normal,
                    );
                } else {
                    let items = presentation::branches::render_branch_list(
                        model,
                        theme,
                        remote_op_label,
                        spinner_frame,
                        remote_op_success,
                    );
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                }
            }
            ContextId::Remotes => {
                if ctx_mgr.active() == ContextId::BranchCommitFiles
                    && sub_commits_parent_context == ContextId::RemoteBranches
                {
                    let cf_selected = ctx_mgr.selected(ContextId::BranchCommitFiles);
                    let cf_title = build_commit_files_title(
                        ContextId::BranchCommitFiles,
                        commit_files_hash,
                        commit_files_message,
                        theme,
                    );
                    let cf_block = Block::default()
                        .title(cf_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::BranchCommitFiles,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::BranchCommitFiles,
                        );
                    }
                } else if ctx_mgr.active() == ContextId::BranchCommits
                    && sub_commits_parent_context == ContextId::RemoteBranches
                {
                    let bc_selected = ctx_mgr.selected(ContextId::BranchCommits);
                    let bc_title = build_branch_commits_title(branch_commits_name, theme);
                    let bc_block = Block::default()
                        .title(bc_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    render_commit_list_ctx(
                        frame,
                        rect,
                        bc_block,
                        model,
                        theme,
                        &[],
                        bc_selected,
                        is_active,
                        None,
                        ctx_mgr,
                        ContextId::BranchCommits,
                        commit_list_cache,
                        true,
                        screen_mode != ScreenMode::Normal,
                    );
                } else if ctx_mgr.active() == ContextId::RemoteBranches {
                    let rb_selected = ctx_mgr.selected(ContextId::RemoteBranches);
                    let rb_title = format!(" Remote Branches ({}) ", remote_branches_name);
                    let rb_block = Block::default()
                        .title(rb_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    let items = presentation::remote_branches::render_remote_branch_list(
                        &model.sub_remote_branches,
                        &model.head_branch_name,
                        theme,
                    );
                    render_list_ctx(
                        frame,
                        rect,
                        rb_block,
                        items,
                        rb_selected,
                        is_active,
                        theme,
                        ctx_mgr,
                        ContextId::RemoteBranches,
                    );
                } else {
                    let items = presentation::remotes::render_remote_list(model, theme);
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                }
            }
            ContextId::Tags => {
                // If BranchCommits or BranchCommitFiles is active (drill-down from Tags), render that instead
                if ctx_mgr.active() == ContextId::BranchCommitFiles {
                    let cf_selected = ctx_mgr.selected(ContextId::BranchCommitFiles);
                    let cf_title = build_commit_files_title(
                        ContextId::BranchCommitFiles,
                        commit_files_hash,
                        commit_files_message,
                        theme,
                    );
                    let cf_block = Block::default()
                        .title(cf_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::BranchCommitFiles,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::BranchCommitFiles,
                        );
                    }
                } else if ctx_mgr.active() == ContextId::BranchCommits {
                    let bc_selected = ctx_mgr.selected(ContextId::BranchCommits);
                    let bc_title = build_branch_commits_title(branch_commits_name, theme);
                    let bc_block = Block::default()
                        .title(bc_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    render_commit_list_ctx(
                        frame,
                        rect,
                        bc_block,
                        model,
                        theme,
                        &[],
                        bc_selected,
                        is_active,
                        None,
                        ctx_mgr,
                        ContextId::BranchCommits,
                        commit_list_cache,
                        true,
                        screen_mode != ScreenMode::Normal,
                    );
                } else {
                    let items = presentation::tags::render_tag_list(model, theme);
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                }
            }
            ContextId::Commits => {
                // If CommitFiles is active within this window, render that instead
                if ctx_mgr.active() == ContextId::CommitFiles {
                    let cf_selected = ctx_mgr.selected(ContextId::CommitFiles);
                    let cf_title = build_commit_files_title(
                        ContextId::CommitFiles,
                        commit_files_hash,
                        commit_files_message,
                        theme,
                    );
                    let cf_block = Block::default()
                        .title(cf_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::CommitFiles,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::CommitFiles,
                        );
                    }
                } else {
                    let range = if is_active {
                        range_select_anchor.map(|a| (a.min(selected), a.max(selected)))
                    } else {
                        None
                    };
                    render_commit_list_ctx(
                        frame,
                        rect,
                        block,
                        model,
                        theme,
                        cherry_pick_clipboard,
                        selected,
                        is_active,
                        range,
                        ctx_mgr,
                        ctx_id,
                        commit_list_cache,
                        false,
                        screen_mode != ScreenMode::Normal,
                    );
                }
            }
            ContextId::Reflog => {
                // If CommitFiles is active (drill-down from Reflog), render that instead
                if ctx_mgr.active() == ContextId::CommitFiles {
                    let cf_selected = ctx_mgr.selected(ContextId::CommitFiles);
                    let cf_title = build_commit_files_title(
                        ContextId::CommitFiles,
                        commit_files_hash,
                        commit_files_message,
                        theme,
                    );
                    let cf_block = Block::default()
                        .title(cf_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::CommitFiles,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            cf_block,
                            items,
                            cf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::CommitFiles,
                        );
                    }
                } else {
                    let items = presentation::reflog::render_reflog_list(model, theme);
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                }
            }
            ContextId::Stash => {
                // If StashFiles is active within this window, render that instead
                if ctx_mgr.active() == ContextId::StashFiles {
                    let sf_selected = ctx_mgr.selected(ContextId::StashFiles);
                    let sf_title = build_commit_files_title(
                        ContextId::StashFiles,
                        commit_files_hash,
                        commit_files_message,
                        theme,
                    );
                    let sf_block = Block::default()
                        .title(sf_title)
                        .borders(Borders::ALL)
                        .border_style(border_style);
                    if show_commit_file_tree {
                        let items = presentation::commit_files::render_commit_file_tree(
                            model,
                            theme,
                            commit_file_tree_nodes,
                            commit_files_collapsed_dirs,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            sf_block,
                            items,
                            sf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::StashFiles,
                        );
                    } else {
                        let items = presentation::commit_files::render_commit_file_list(
                            model,
                            theme,
                            rect.width.saturating_sub(2) as usize,
                        );
                        render_list_ctx(
                            frame,
                            rect,
                            sf_block,
                            items,
                            sf_selected,
                            is_active,
                            theme,
                            ctx_mgr,
                            ContextId::StashFiles,
                        );
                    }
                } else {
                    let items = presentation::stash::render_stash_list(model, theme);
                    render_list_ctx(
                        frame, rect, block, items, selected, is_active, theme, ctx_mgr, ctx_id,
                    );
                }
            }
            _ => {
                let widget = Paragraph::new("").block(block);
                frame.render_widget(widget, rect);
            }
        }
    }

    // Highlight `/` search matches on the active list (lazygit-style substring).
    // Applied after list widgets paint so selection styles stay intact.
    if let Some((query, _, _)) = search_state {
        if !query.is_empty() {
            if let Some(list_rect) = fl.side_panels.get(active_panel_index).copied() {
                render_list_search_highlights(frame, list_rect, query, theme);
            }
        }
    }

    // Render main panel (skipped when side panel is fully expanded)
    if fl.main_panel.width > 0 {
        if ctx_mgr.active() == ContextId::Status {
            // Status view: show logo + copyright in the main content area
            let status_block = Block::default()
                .title(" Status ")
                .borders(Borders::ALL)
                .border_style(theme.active_border);
            render_status_main(frame, fl.main_panel, model, config, theme, status_block);
        } else if !diff_view.is_empty() {
            let show_revert_markers = ctx_mgr.active() == ContextId::Files;
            side_by_side::render_diff(
                frame,
                fl.main_panel,
                diff_view,
                theme,
                diff_focused,
                diff_loading_show,
                show_revert_markers,
            );
            side_by_side::render_diff_search_highlights(frame, fl.main_panel, diff_view, theme);
            side_by_side::render_diff_search_bar(frame, fl.main_panel, diff_view, theme);
        } else if diff_loading {
            // Diff is being loaded — show empty panel during grace period, then "Loading..." after delay
            let block = Block::default()
                .title(" Diff ")
                .borders(Borders::ALL)
                .border_style(theme.inactive_border);
            if diff_loading_show {
                let widget = Paragraph::new(" Loading diff...").block(block);
                frame.render_widget(widget, fl.main_panel);
            } else {
                frame.render_widget(block, fl.main_panel);
            }
        } else {
            // Fallback: show info about selected item
            let block = Block::default()
                .title(" Diff ")
                .borders(Borders::ALL)
                .border_style(theme.inactive_border);

            let info = get_info_content(model, ctx_mgr);
            let widget = Paragraph::new(info).block(block);
            frame.render_widget(widget, fl.main_panel);
        }
    } // end main_panel.width > 0

    // Normal/Half mode: compact details box sits at the bottom of the active
    // sidebar panel (layout carves the rect out of the active side panel).
    if let (Some(details_rect), Some(commit)) = (fl.commit_details_panel, current_commit) {
        if commit_details_scroll_hash.as_str() != commit.hash.as_str() {
            *commit_details_scroll = 0;
            *commit_details_scroll_hash = commit.hash.clone();
        }
        render_commit_details_panel(
            frame,
            details_rect,
            commit,
            commit_stats,
            commit_messages,
            theme,
            true,
            commit_details_scroll,
        );
    }

    // Render status bar (or search bar if search is active)
    render_search_bar_or_status_bar(
        frame,
        fl.status_bar,
        search_state,
        search_textarea,
        ctx_mgr,
        diff_view,
        theme,
        model,
        diff_focused,
        !cherry_pick_clipboard.is_empty(),
        &config.user_config.keybinding,
        !active_commit_filters.is_empty(),
        active_file_tree,
    );

    // Render text selection highlight overlay and tooltip
    render_selection_overlay(frame, diff_view, fl.main_panel, theme);

    // Render popup overlay
    if *popup != PopupState::None {
        render_popup(
            frame,
            popup,
            area,
            spinner_frame,
            theme,
            ai_button_hovered,
            ai_configured,
        );
    }

    // Render command log last so it appears above everything
    render_command_log(frame, &fl, command_log, show_command_log, theme);
}

fn render_command_log(
    frame: &mut Frame,
    fl: &layout::FrameLayout,
    command_log: &[String],
    show_command_log: bool,
    theme: &Theme,
) {
    if !show_command_log || command_log.is_empty() {
        return;
    }

    let Some((log_rect, log_height)) = command_log_geometry(fl.main_panel, command_log.len())
    else {
        return;
    };

    let border_color = theme.cmd_log_border;
    let title_color = theme.cmd_log_title;
    let hint_color = theme.cmd_log_hint;
    let log_block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default().fg(title_color)),
            Span::styled(
                "Command Log",
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default().fg(title_color)),
        ]))
        .title_bottom(
            Line::from(vec![
                Span::styled(" ", Style::default().fg(hint_color)),
                Span::styled(
                    ";",
                    Style::default()
                        .fg(theme.cmd_log_timestamp)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" toggle ", Style::default().fg(hint_color)),
            ])
            .alignment(ratatui::layout::Alignment::Right),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let cmd_color = theme.cmd_log_text;
    let visible_count = command_log.len().min(log_height as usize);
    let log_lines: Vec<Line> = command_log
        .iter()
        .rev()
        .take(log_height as usize)
        .rev()
        .enumerate()
        .map(|(i, s)| {
            let is_latest = i == visible_count - 1;
            let fg = if is_latest {
                theme.cmd_log_timestamp
            } else {
                cmd_color
            };
            Line::from(vec![
                Span::styled(" $ ", Style::default().fg(theme.cmd_log_success)),
                Span::styled(s.to_string(), Style::default().fg(fg)),
            ])
        })
        .collect();

    frame.render_widget(Clear, log_rect);
    let log_widget = Paragraph::new(log_lines).block(log_block);
    frame.render_widget(log_widget, log_rect);
}

fn command_log_geometry(main_panel: Rect, command_count: usize) -> Option<(Rect, u16)> {
    if main_panel.width == 0 {
        return None;
    }

    let log_height = command_count
        .min(5)
        .min(main_panel.height.saturating_sub(1) as usize) as u16;
    if log_height == 0 {
        return None;
    }

    let log_width = main_panel.width.min(50);
    let log_x = main_panel
        .x
        .saturating_add(main_panel.width.saturating_sub(log_width));
    let log_y = main_panel
        .y
        .saturating_add(main_panel.height - log_height - 1);
    Some((
        Rect::new(log_x, log_y, log_width, log_height + 2),
        log_height,
    ))
}

fn wrap_popup_lines(message: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    message
        .lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                textwrap::wrap(line, width)
                    .into_iter()
                    .map(|line| line.into_owned())
                    .collect()
            }
        })
        .collect()
}

fn render_submodule_list<'a>(model: &Model, theme: &Theme) -> Vec<ListItem<'a>> {
    model
        .submodules
        .iter()
        .map(|sub| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {} ", sub.name),
                    Style::default().fg(theme.accent),
                ),
                Span::styled(sub.path.clone(), Style::default().fg(theme.text_dimmed)),
            ]))
        })
        .collect()
}

fn visible_popup_lines(wrapped: &[String], max_lines: usize) -> Vec<String> {
    if wrapped.len() <= max_lines {
        return wrapped.to_vec();
    }
    if max_lines == 0 {
        return Vec::new();
    }
    if max_lines == 1 {
        return vec![format!("... {} more lines", wrapped.len())];
    }

    let mut visible: Vec<String> = wrapped.iter().take(max_lines - 1).cloned().collect();
    let remaining = wrapped.len() - visible.len();
    visible.push(format!("... {} more lines", remaining));
    visible
}

fn clamped_popup_height(line_count: usize, fixed_rows: u16, area_height: u16) -> u16 {
    let line_rows = u16::try_from(line_count).unwrap_or(u16::MAX);
    line_rows.saturating_add(fixed_rows).min(area_height)
}

fn centered_popup_x_width(area: Rect) -> (u16, u16) {
    let popup_width = (area.width * 60 / 100).min(60).max(30).min(area.width);
    let x = (area.width.saturating_sub(popup_width)) / 2;
    (x, popup_width)
}

/// Returns the menu option index under `(col, row)`, matching `render_popup` layout.
pub fn menu_item_at(popup: &PopupState, area: Rect, col: u16, row: u16) -> Option<usize> {
    let PopupState::Menu { items, .. } = popup else {
        return None;
    };
    if items.is_empty() || area.width < 4 || area.height < 4 {
        return None;
    }
    let (x, popup_width) = centered_popup_x_width(area);
    // Matches `PopupState::Menu` in `render_popup`.
    let height = (items.len() as u16 + 2).min(area.height.saturating_sub(4));
    if height < 3 {
        return None;
    }
    let y = (area.height.saturating_sub(height)) / 2;
    if col < x || col >= x + popup_width {
        return None;
    }
    let list_start = y + 1; // below top border
    let list_rows = (height - 2) as usize; // exclude borders
    if row < list_start || row >= list_start + list_rows as u16 {
        return None;
    }
    let idx = (row - list_start) as usize;
    if idx < items.len() && idx < list_rows {
        Some(idx)
    } else {
        None
    }
}

/// Returns the visible checklist option index under `(col, row)`.
pub fn checklist_item_at(popup: &PopupState, area: Rect, col: u16, row: u16) -> Option<usize> {
    let PopupState::Checklist {
        items,
        search_textarea,
        ..
    } = popup
    else {
        return None;
    };
    let search = search_textarea.lines().join("");
    if area.width < 4 || area.height < 4 {
        return None;
    }
    let (x, popup_width) = centered_popup_x_width(area);
    let visible_count = items
        .iter()
        .filter(|it| {
            it.is_free_entry
                || search.is_empty()
                || it.label.to_lowercase().contains(&search.to_lowercase())
        })
        .count();
    // Matches `PopupState::Checklist` in `render_popup`.
    let content_lines = visible_count.max(1);
    let height = (content_lines as u16 + 6)
        .min(area.height.saturating_sub(4))
        .max(8.min(area.height));
    if height < 3 {
        return None;
    }
    let y = (area.height.saturating_sub(height)) / 2;
    if col < x || col >= x + popup_width {
        return None;
    }
    // inner.y = y+1; list starts at inner.y+2 (after search + separator)
    let list_start = y + 1 + 2;
    let inner_height = height.saturating_sub(2);
    let list_height = inner_height.saturating_sub(3); // search + sep + hint
    if list_height == 0 {
        return None;
    }
    if row < list_start || row >= list_start + list_height {
        return None;
    }
    let idx = (row - list_start) as usize;
    if idx < visible_count { Some(idx) } else { None }
}

#[cfg(test)]
mod tests {
    use super::{
        checklist_item_at, command_log_geometry, commit_body_viewport,
        email_recipient_row_allocation, format_key_hint, menu_item_at,
        pull_divergence_popup_geometry, render_popup,
    };
    use crate::config::Theme;
    use crate::gui::popup::{ChecklistItem, MenuItem, MessageKind, PopupState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    #[test]
    fn commit_body_viewport_slices_from_explicit_top_row() {
        use tui_textarea::CursorMove;

        let mut textarea =
            tui_textarea::TextArea::new((0..8).map(|i| format!("line {i}")).collect::<Vec<_>>());
        textarea.move_cursor(CursorMove::Jump(5, 0));

        let viewport = commit_body_viewport(&textarea, 3);
        assert_eq!(viewport.lines()[0], "line 3");
        assert_eq!(viewport.lines()[1], "line 4");
        assert_eq!(viewport.cursor(), (2, 0));
    }

    #[test]
    fn pull_divergence_popup_uses_large_dialog_geometry() {
        let area = Rect::new(0, 0, 120, 30);
        let rect = pull_divergence_popup_geometry(area).unwrap();
        assert_eq!(rect.width, 96);
        assert_eq!(rect.height, 20);
        assert_eq!(rect.x, 12);
        assert_eq!(rect.y, 5);
    }

    #[test]
    fn email_recipient_rows_reuse_space_from_shorter_list() {
        assert_eq!(email_recipient_row_allocation(1, 20, 12), (1, 11));
        assert_eq!(email_recipient_row_allocation(20, 1, 12), (11, 1));
        assert_eq!(email_recipient_row_allocation(6, 6, 12), (6, 6));
    }

    fn example_branches_with_stale_head_flag() -> crate::model::Model {
        let branch = |name: &str, head: bool| crate::model::Branch {
            name: name.to_string(),
            hash: String::new(),
            recency: String::new(),
            commit_time: 0,
            pushables: String::new(),
            pullables: String::new(),
            upstream: None,
            head,
        };
        crate::model::Model {
            repo_name: "lazygitrs".into(),
            head_branch_name: "main".into(),
            branches: vec![branch("main", false), branch("list/neoscroll", true)],
            ..Default::default()
        }
    }

    #[test]
    fn inactive_branches_follow_head_in_files_status_stash_and_commits() {
        use super::{ContextId, ContextManager, SideWindow, sidebar_selected_index};

        let model = example_branches_with_stale_head_flag();
        let mut contexts = ContextManager::new();
        contexts.set_active(ContextId::Branches);
        contexts.set_selection(1); // Browsing list/neoscroll, not checking it out.
        assert_eq!(
            sidebar_selected_index(SideWindow::Branches, ContextId::Branches, &contexts, &model),
            1
        );

        for active in [
            ContextId::Files,
            ContextId::Status,
            ContextId::Stash,
            ContextId::Commits,
        ] {
            contexts.set_active(active);
            assert_eq!(
                sidebar_selected_index(SideWindow::Branches, ContextId::Branches, &contexts, &model),
                0,
                "{active:?} should display the checked-out branch"
            );
            assert_eq!(contexts.selected(ContextId::Branches), 1);
        }

        contexts.set_active(ContextId::Branches);
        assert_eq!(
            sidebar_selected_index(SideWindow::Branches, ContextId::Branches, &contexts, &model),
            1,
            "returning to Branches must restore the browsed branch"
        );
    }

    #[test]
    fn status_uses_actual_head_name_not_stale_branch_flags() {
        let model = example_branches_with_stale_head_flag();
        let line = super::render_status_sidebar(&model, &Default::default(), 80, &Theme::default());
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("lazygitrs → main"), "{text}");
        assert!(!text.contains("list/neoscroll"), "{text}");
        let status_screen = render_tab(&model, super::ContextId::Status, super::ScreenMode::Normal);
        assert!(
            status_screen.iter().any(|row| row.contains("Branch: main")),
            "full status must also use the checked-out branch"
        );

        let detached = crate::model::Model {
            head_branch_name: String::new(),
            ..model
        };
        let line =
            super::render_status_sidebar(&detached, &Default::default(), 80, &Theme::default());
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("HEAD (no branch)"), "{text}");
    }

    fn render_tab(
        model: &crate::model::Model,
        context: super::ContextId,
        mode: super::ScreenMode,
    ) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        let mut contexts = super::ContextManager::new();
        contexts.set_active(context);
        terminal
            .draw(|frame| {
                super::render(
                    frame,
                    model,
                    &mut contexts,
                    &super::LayoutState::default(),
                    &PopupState::None,
                    &crate::config::AppConfig {
                        user_config: Default::default(),
                        app_state: Default::default(),
                        state_dir: Default::default(),
                        state_path: Default::default(),
                    },
                    &Theme::default(),
                    &mut super::DiffViewState::default(),
                    &mut super::presentation::commits::CommitListCache::default(),
                    mode,
                    false,
                    &[],
                    &Default::default(),
                    false,
                    None,
                    None,
                    &[],
                    false,
                    &[],
                    false,
                    &[],
                    &Default::default(),
                    "",
                    "",
                    "",
                    "",
                    super::ContextId::Commits,
                    0,
                    None,
                    false,
                    &[],
                    None,
                    false,
                    false,
                    &Default::default(),
                    &Default::default(),
                    &mut 0,
                    &mut String::new(),
                    false,
                    false,
                    false,
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(160)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    fn sample_worktree(
        path: &str,
        branch: &str,
        current: bool,
        main: bool,
    ) -> crate::model::Worktree {
        crate::model::Worktree {
            path: path.into(),
            branch: branch.into(),
            hash: String::new(),
            is_current: current,
            is_main: main,
        }
    }

    #[test]
    fn footer_shows_only_enabled_folding_in_active_file_tree_lists() {
        use super::{ContextId, ContextManager, DiffViewState, KeybindingConfig, Model};
        let mut kb = KeybindingConfig::default();
        kb.universal.tree_parent = "p".into();
        kb.universal.tree_child = "c".into();
        kb.universal.tree_prev_sibling.clear();
        kb.universal.tree_next_sibling = "n".into();
        let mut diff_view = DiffViewState::default();
        diff_view.load("test", "old\n", "new\n");
        for ctx in [
            ContextId::Files,
            ContextId::CommitFiles,
            ContextId::BranchCommitFiles,
            ContextId::StashFiles,
        ] {
            for focused in [false, true] {
                for tree in [false, true] {
                    for (binding, hint) in [("-", "-"), ("<c-f>", "ctrl+f"), ("", "")] {
                        kb.universal.fold_directory = binding.into();
                        let mut ctx_mgr = ContextManager::new();
                        ctx_mgr.set_active(ctx);
                        let mut terminal = Terminal::new(TestBackend::new(240, 1)).unwrap();
                        terminal
                            .draw(|frame| {
                                super::render_status_bar(
                                    frame,
                                    frame.area(),
                                    &ctx_mgr,
                                    &diff_view,
                                    &Theme::default(),
                                    &Model::default(),
                                    focused,
                                    false,
                                    &kb,
                                    false,
                                    tree,
                                )
                            })
                            .unwrap();
                        let text: String = (0..240)
                            .map(|x| terminal.backend().buffer().cell((x, 0)).unwrap().symbol())
                            .collect();
                        assert_eq!(
                            text.contains("fold/unfold"),
                            tree && !focused && !binding.is_empty(),
                            "{ctx:?} {focused} {tree} {binding}: {text}"
                        );
                        if tree && !focused && !binding.is_empty() {
                            assert!(text.contains(&format!("{hint} fold/unfold")), "{text}");
                        }
                        assert!(!text.contains("p/c nav"), "{text}");
                        assert!(!text.contains("n siblings"), "{text}");
                        assert!(!text.contains("details"), "{text}");
                    }
                }
            }
        }
    }

    #[test]
    fn footer_shows_configured_shell_prompt_in_list_and_diff_contexts() {
        use super::{ContextId, ContextManager, DiffViewState, KeybindingConfig, Model};
        let mut diff_view = DiffViewState::default();
        diff_view.load("test", "old\n", "new\n");
        for ctx in [
            ContextId::Files,
            ContextId::Commits,
            ContextId::CommitFiles,
            ContextId::BranchCommits,
            ContextId::BranchCommitFiles,
            ContextId::Reflog,
            ContextId::StashFiles,
        ] {
            for focused in [false, true] {
                for (binding, hint) in [(":", ":"), ("<c-x>", "ctrl+x"), ("", ""), ("  ", "")] {
                    let mut kb = KeybindingConfig::default();
                    kb.universal.custom_command_prompt = binding.into();
                    let mut ctx_mgr = ContextManager::new();
                    ctx_mgr.set_active(ctx);
                    let mut terminal = Terminal::new(TestBackend::new(240, 1)).unwrap();
                    terminal
                        .draw(|frame| {
                            super::render_status_bar(
                                frame,
                                frame.area(),
                                &ctx_mgr,
                                &diff_view,
                                &Theme::default(),
                                &Model::default(),
                                focused,
                                false,
                                &kb,
                                false,
                                false,
                            )
                        })
                        .unwrap();
                    let text: String = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect();
                    assert_eq!(
                        text.contains(" shell"),
                        !hint.is_empty(),
                        "{ctx:?} {focused} {binding}: {text}"
                    );
                    if !hint.is_empty() {
                        assert!(text.contains(&format!("{hint} shell")), "{text}");
                    }
                    assert_eq!(text.contains(": shell"), binding == ":", "{text}");
                    assert!(!text.contains("ctrl+g head"), "{text}");
                    assert!(!text.contains("ctrl+g files"), "{text}");
                }
            }
        }
    }

    #[test]
    fn commit_list_footers_omit_details_toggle() {
        for ctx in [
            super::ContextId::Commits,
            super::ContextId::BranchCommits,
            super::ContextId::Reflog,
        ] {
            let mut ctx_mgr = super::ContextManager::new();
            ctx_mgr.set_active(ctx);
            let mut terminal = Terminal::new(TestBackend::new(240, 1)).unwrap();
            terminal
                .draw(|frame| {
                    super::render_status_bar(
                        frame,
                        frame.area(),
                        &ctx_mgr,
                        &super::DiffViewState::default(),
                        &Theme::default(),
                        &super::Model::default(),
                        false,
                        false,
                        &super::KeybindingConfig::default(),
                        false,
                        false,
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(!text.contains("details"), "{ctx:?}: {text}");
            assert!(!text.contains("fold/unfold"), "{ctx:?}: {text}");
        }
    }

    #[test]
    fn worktrees_render_names_branches_and_main_label_in_every_mode() {
        let model = crate::model::Model {
            worktrees: vec![
                sample_worktree("/repos/quarta", "feat/main", true, true),
                sample_worktree(
                    "/repos/worktree-pfparser",
                    "experiment/parser",
                    false,
                    false,
                ),
            ],
            ..Default::default()
        };
        for mode in [
            super::ScreenMode::Normal,
            super::ScreenMode::Half,
            super::ScreenMode::Full,
        ] {
            let rows = render_tab(&model, super::ContextId::Worktrees, mode);
            let main = rows.iter().find(|row| row.contains("* quarta")).unwrap();
            assert!(main.contains("* quarta"));
            assert!(main.contains("feat/main (main worktree)"));
            assert!(!main.contains("/repos/"));
            let linked = rows
                .iter()
                .find(|row| row.contains("worktree-pfparser experiment/parser"))
                .unwrap();
            assert!(linked.contains("worktree-pfparser experiment/parser"));
            assert!(!linked.contains("(main worktree)"));
            assert!(!linked.contains("/repos/"));
            assert_eq!(main.find("feat/main"), linked.find("experiment/parser"));
        }
    }

    #[test]
    fn worktree_columns_use_unicode_display_width() {
        let model = crate::model::Model {
            worktrees: vec![
                sample_worktree("/repos/树", "branch-one", false, false),
                sample_worktree("/repos/abc", "branch-two", true, false),
            ],
            ..Default::default()
        };
        let rows = render_tab(&model, super::ContextId::Worktrees, super::ScreenMode::Full);
        assert!(rows.iter().any(|row| row.contains("树   branch-one")));
        assert!(rows.iter().any(|row| row.contains("* abc branch-two")));
    }

    #[test]
    fn submodules_render_in_every_mode_and_empty_lists_stay_blank() {
        let mut model = crate::model::Model::default();
        for mode in [
            super::ScreenMode::Normal,
            super::ScreenMode::Half,
            super::ScreenMode::Full,
        ] {
            let rows = render_tab(&model, super::ContextId::Submodules, mode);
            assert!(!rows.join("\n").contains("(no submodules)"));
        }
        model.submodules.push(crate::git::submodule::Submodule {
            name: "shared-lib".into(),
            path: "vendor/shared-lib".into(),
        });
        for mode in [
            super::ScreenMode::Normal,
            super::ScreenMode::Half,
            super::ScreenMode::Full,
        ] {
            let rows = render_tab(&model, super::ContextId::Submodules, mode);
            assert!(
                rows.iter()
                    .any(|row| row.contains("shared-lib vendor/shared-lib"))
            );
        }
    }

    #[test]
    fn format_key_hint_formats_modifiers_and_preserves_plain() {
        assert_eq!(format_key_hint("<c-s>"), "ctrl+s");
        assert_eq!(format_key_hint("<C-s>"), "ctrl+s");
        assert_eq!(format_key_hint("<ctrl-s>"), "ctrl+s");
        assert_eq!(format_key_hint("<c+s>"), "ctrl+s");
        assert_eq!(format_key_hint("Ctrl+S"), "ctrl+s");
        assert_eq!(format_key_hint("ctrl-s"), "ctrl+s");
        assert_eq!(format_key_hint("<c-g>"), "ctrl+g");
        assert_eq!(format_key_hint("<c-l>"), "ctrl+l");
        assert_eq!(format_key_hint("<c-f>"), "ctrl+f");
        assert_eq!(format_key_hint("<a-h>"), "alt+h");
        assert_eq!(format_key_hint("<alt-h>"), "alt+h");
        assert_eq!(format_key_hint("<s-f>"), "shift+f");
        assert_eq!(format_key_hint("<shift-f>"), "shift+f");
        assert_eq!(format_key_hint("<shift-tab>"), "shift+tab");
        assert_eq!(format_key_hint("<space>"), "space");
        assert_eq!(format_key_hint("<Space>"), "space");
        assert_eq!(format_key_hint("<Enter>"), "enter");
        assert_eq!(format_key_hint("<escape>"), "esc");
        assert_eq!(format_key_hint("f"), "f");
        assert_eq!(format_key_hint("F"), "F");
        assert_eq!(format_key_hint("ctrl+g"), "ctrl+g");
        assert_eq!(format_key_hint(""), "");
    }

    #[test]
    fn status_bar_has_no_removed_head_toggle_hint() {
        use super::{ContextId, ContextManager, render_status_bar};
        use crate::model::Model;

        let mut terminal = Terminal::new(TestBackend::new(160, 1)).unwrap();
        let mut ctx_mgr = ContextManager::new();
        for context in [
            ContextId::Files,
            ContextId::Commits,
            ContextId::CommitFiles,
            ContextId::Reflog,
        ] {
            for diff_focused in [false, true] {
                ctx_mgr.set_active(context);
                let mut diff_view = crate::pager::side_by_side::DiffViewState::new();
                diff_view.load_from_diff_output("file", "@@ -1 +1 @@\n-old\n+new\n");
                terminal
                    .draw(|frame| {
                        render_status_bar(
                            frame,
                            Rect::new(0, 0, 160, 1),
                            &ctx_mgr,
                            &diff_view,
                            &Theme::default(),
                            &Model::default(),
                            diff_focused,
                            false,
                            &crate::config::KeybindingConfig::default(),
                            false,
                            false,
                        );
                    })
                    .unwrap();
                let content: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(!content.contains("ctrl+g head"), "{content}");
                assert!(!content.contains("ctrl+g files"), "{content}");
            }
        }
    }

    #[test]
    fn command_log_is_hidden_when_main_panel_is_absent() {
        assert_eq!(command_log_geometry(Rect::default(), 1), None);
    }

    #[test]
    fn command_log_visible_lines_are_clamped_to_short_main_panel() {
        let (rect, visible_lines) =
            command_log_geometry(Rect::new(10, 4, 80, 2), 5).expect("log should fit");

        assert_eq!(visible_lines, 1);
        assert_eq!(rect, Rect::new(40, 4, 50, 3));
    }

    #[test]
    fn long_error_message_popup_renders_in_short_terminal() {
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let message = (0..40)
            .map(|i| {
                format!(
                    "hint: divergent branches need reconciliation before pull can continue ({i})"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let popup = PopupState::Message {
            title: "Pull error".to_string(),
            message,
            kind: MessageKind::Error,
        };

        terminal
            .draw(|frame| {
                render_popup(
                    frame,
                    &popup,
                    Rect::new(0, 0, 40, 8),
                    0,
                    &Theme::default(),
                    false,
                    false,
                );
            })
            .expect("long popup message should render without panicking");
    }

    #[test]
    fn command_output_popup_scrolls_to_final_line_and_handles_tiny_terminals() {
        let message = (0..100)
            .map(|i| format!("output-line-{i:03}\n"))
            .collect::<String>();
        for (width, height) in [(80, 24), (20, 8), (4, 4), (3, 3), (1, 1)] {
            let area = Rect::new(0, 0, width, height);
            for last in [false, true] {
                let popup = PopupState::CommandOutput {
                    title: "exit 7".into(),
                    message: message.clone(),
                    kind: MessageKind::Error,
                    scroll: if last {
                        super::command_output_max_scroll(&message, area)
                    } else {
                        0
                    },
                };
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        render_popup(frame, &popup, area, 0, &Theme::default(), false, false)
                    })
                    .unwrap();
                if width >= 20 {
                    let buffer = terminal.backend().buffer();
                    let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
                    assert!(text.contains("exit 7"), "{text}");
                    let line = if last {
                        "output-line-099"
                    } else {
                        "output-line-000"
                    };
                    assert!(text.contains(line), "{width}x{height}: {text}");
                }
            }
        }
    }

    #[test]
    fn shell_footers_use_bold_keys_and_muted_action_labels() {
        let theme = Theme::default();
        for (popup, hint, action) in [
            (
                PopupState::ShellCommand {
                    textarea: crate::gui::popup::make_textarea(""),
                },
                " enter confirm  esc cancel",
                "confirm",
            ),
            (
                PopupState::CommandOutput {
                    title: "Command output".into(),
                    message: "result".into(),
                    kind: MessageKind::Info,
                    scroll: 0,
                },
                " enter close  esc close  j/k scroll  y copy  pgup/pgdn page  g/G top/bottom",
                "close",
            ),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
            terminal
                .draw(|frame| {
                    render_popup(
                        frame,
                        &popup,
                        Rect::new(0, 0, 120, 24),
                        0,
                        &theme,
                        false,
                        false,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let (row, text) = (0..24)
                .find_map(|y| {
                    let text: String = (0..120).map(|x| buffer[(x, y)].symbol()).collect();
                    text.contains(hint).then_some((y, text))
                })
                .expect("styled footer should be visible in the rendered popup");
            for key in ["enter", "esc"] {
                let x = text[..text.find(key).unwrap()].chars().count() as u16;
                for col in x..x + key.len() as u16 {
                    let cell = &buffer[(col, row)];
                    assert_eq!(cell.fg, theme.text);
                    assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
                }
            }
            let x = text[..text.find(action).unwrap()].chars().count() as u16;
            for col in x..x + action.len() as u16 {
                let cell = &buffer[(col, row)];
                assert_eq!(cell.fg, theme.text_dimmed);
                assert!(!cell.modifier.contains(ratatui::style::Modifier::BOLD));
            }
        }
    }

    #[test]
    fn literal_shell_prompt_renders_in_small_terminals() {
        let mut textarea = crate::gui::popup::make_textarea("");
        textarea.insert_str("printf '%s' 'multiple   spaces'\n# next line");
        let popup = PopupState::ShellCommand { textarea };
        for (width, height) in [(80, 24), (20, 8), (4, 4), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    render_popup(
                        frame,
                        &popup,
                        Rect::new(0, 0, width, height),
                        0,
                        &Theme::default(),
                        false,
                        false,
                    )
                })
                .unwrap();
        }
    }

    fn sample_menu() -> PopupState {
        PopupState::Menu {
            title: "Copy".to_string(),
            items: vec![
                MenuItem {
                    label: "commit hash".to_string(),
                    description: String::new(),
                    key: Some("c".to_string()),
                    action: None,
                },
                MenuItem {
                    label: "commit message".to_string(),
                    description: String::new(),
                    key: Some("m".to_string()),
                    action: None,
                },
                MenuItem {
                    label: "author name".to_string(),
                    description: String::new(),
                    key: Some("a".to_string()),
                    action: None,
                },
            ],
            selected: 0,
            loading_index: None,
        }
    }

    #[test]
    fn menu_item_at_hits_first_and_second_options() {
        let area = Rect::new(0, 0, 80, 24);
        let popup = sample_menu();
        // height = 3 items + 2 borders = 5; y = (24-5)/2 = 9; list starts at y+1 = 10
        let x = (area
            .width
            .saturating_sub((area.width * 60 / 100).min(60).max(30)))
            / 2
            + 2;
        assert_eq!(menu_item_at(&popup, area, x, 10), Some(0));
        assert_eq!(menu_item_at(&popup, area, x, 11), Some(1));
        assert_eq!(menu_item_at(&popup, area, x, 12), Some(2));
        assert_eq!(menu_item_at(&popup, area, x, 9), None); // border
        assert_eq!(menu_item_at(&popup, area, x, 13), None); // below
    }

    #[test]
    fn checklist_item_at_skips_search_and_separator() {
        let area = Rect::new(0, 0, 80, 30);
        let popup = PopupState::Checklist {
            title: "Pick".to_string(),
            items: vec![
                ChecklistItem {
                    label: "one".to_string(),
                    checked: false,
                    is_free_entry: false,
                },
                ChecklistItem {
                    label: "two".to_string(),
                    checked: true,
                    is_free_entry: false,
                },
            ],
            selected: 0,
            search_textarea: crate::gui::popup::make_checklist_search_textarea(),
            free_entry_category: None,
            on_confirm: Box::new(|_gui, _ids| Ok(())),
        };
        // height = max(8, 2+6)=8; y=(30-8)/2=11; list_start=y+1+2=14
        let x = (area
            .width
            .saturating_sub((area.width * 60 / 100).min(60).max(30)))
            / 2
            + 2;
        assert_eq!(checklist_item_at(&popup, area, x, 14), Some(0));
        assert_eq!(checklist_item_at(&popup, area, x, 15), Some(1));
        assert_eq!(checklist_item_at(&popup, area, x, 12), None); // search row
        assert_eq!(checklist_item_at(&popup, area, x, 13), None); // separator
    }

    #[test]
    fn format_key_hint_normalizes_standard_and_custom_bindings() {
        use super::format_key_hint;

        assert_eq!(format_key_hint("<c-s>"), "ctrl+s");
        assert_eq!(format_key_hint("<c-l>"), "ctrl+l");
        assert_eq!(format_key_hint("<c-L>"), "ctrl+l");
        assert_eq!(format_key_hint("ctrl+l"), "ctrl+l");
        assert_eq!(format_key_hint("<space>"), "space");
        assert_eq!(format_key_hint("<esc>"), "esc");
        assert_eq!(format_key_hint("<enter>"), "enter");
        assert_eq!(format_key_hint("c-s"), "ctrl+s");
        assert_eq!(format_key_hint("alt-x"), "alt+x");
        assert_eq!(format_key_hint("shift-tab"), "shift+tab");
    }

    #[test]
    fn render_status_bar_displays_filter_branch_and_esc_reset() {
        use super::{DiffViewState, render_status_bar};
        use crate::config::KeybindingConfig;
        use crate::gui::context::{ContextId, ContextManager};
        use crate::model::Model;

        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let model = Model::default();
        let keybindings = KeybindingConfig::default();
        let mut ctx_mgr = ContextManager::new();
        ctx_mgr.set_active(ContextId::Commits);
        let diff_view = DiffViewState::new();
        let theme = Theme::default();

        // With active filters
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect::new(0, 0, 120, 1),
                    &ctx_mgr,
                    &diff_view,
                    &theme,
                    &model,
                    false,
                    false,
                    &keybindings,
                    true,
                    false,
                );
            })
            .expect("should render status bar with active filters");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("reset filter"),
            "expected 'reset filter' in status bar: {}",
            content
        );
        assert!(
            content.contains("filter branch"),
            "expected 'filter branch' in status bar: {}",
            content
        );

        assert!(!content.contains("ctrl+g files"), "{content}");

        // Without active filters
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect::new(0, 0, 120, 1),
                    &ctx_mgr,
                    &diff_view,
                    &theme,
                    &model,
                    false,
                    false,
                    &keybindings,
                    false,
                    false,
                );
            })
            .expect("should render status bar without active filters");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(
            !content.contains("reset filter"),
            "did not expect 'reset filter' when no filters active: {}",
            content
        );
        assert!(
            content.contains("filter branch"),
            "expected 'filter branch' in status bar: {}",
            content
        );
    }
}

/// Build a window title like " 4 Commit Files (abc1234 feat: some change) ".
fn build_branch_commits_title<'a>(branch_name: &str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::raw(" 3 Commits "),
        Span::styled(
            format!("({})", branch_name),
            Style::default().fg(theme.accent_secondary),
        ),
        Span::raw(" "),
    ])
}

fn build_commit_files_title<'a>(
    ctx: ContextId,
    commit_hash: &str,
    commit_message: &str,
    theme: &Theme,
) -> Line<'a> {
    let short = if commit_hash.len() > 7 {
        &commit_hash[..7]
    } else {
        commit_hash
    };
    let prefix = match ctx {
        ContextId::StashFiles => " 5 Stash Files ",
        ContextId::BranchCommitFiles => " 3 Commit Files ",
        _ => " 4 Commit Files ",
    };
    let mut spans = vec![
        Span::raw(prefix),
        Span::styled(
            format!("({}", short),
            Style::default().fg(theme.accent_secondary),
        ),
    ];
    if !commit_message.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            commit_message.to_string(),
            Style::default().fg(theme.text_dimmed),
        ));
    }
    spans.push(Span::styled(
        ") ",
        Style::default().fg(theme.accent_secondary),
    ));
    Line::from(spans)
}

fn build_window_title<'a>(
    window: SideWindow,
    active_ctx: ContextId,
    _ctx_mgr: &ContextManager,
    theme: &Theme,
) -> Line<'a> {
    let tabs = window.tabs();
    let key = window.key_label();

    if tabs.len() == 1 {
        return Line::from(format!(" {} {} ", key, tabs[0].title()));
    }

    let mut spans = vec![Span::raw(format!(" {} ", key))];

    for (i, ctx) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(theme.text_dimmed)));
        }
        if *ctx == active_ctx {
            spans.push(Span::styled(
                ctx.title(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                ctx.title(),
                Style::default().fg(theme.text_dimmed),
            ));
        }
    }

    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Display selection for a sidebar window. An inactive local Branches panel
/// follows HEAD regardless of which other panel has focus, but its remembered
/// selection remains unchanged for when the user returns to Branches.
fn sidebar_selected_index(
    window: SideWindow,
    ctx: ContextId,
    ctx_mgr: &ContextManager,
    model: &Model,
) -> usize {
    if window == SideWindow::Branches
        && ctx == ContextId::Branches
        && ctx_mgr.active_window() != SideWindow::Branches
    {
        model
            .branches
            .iter()
            .position(|branch| {
                !model.head_branch_name.is_empty() && branch.name == model.head_branch_name
            })
            .unwrap_or_else(|| ctx_mgr.selected(ctx))
    } else {
        ctx_mgr.selected(ctx)
    }
}

/// Compact 1-line status for the sidebar: "reponame → branch          +N -N"
fn render_status_sidebar<'a>(
    model: &Model,
    _config: &AppConfig,
    inner_width: usize,
    theme: &Theme,
) -> Line<'a> {
    // Determine the working-tree state prefix (rebasing/merging/cherry-picking)
    let state_prefix = if model.is_rebasing {
        Some("rebasing")
    } else if model.is_merging {
        Some("merging")
    } else if model.is_cherry_picking {
        Some("cherry-picking")
    } else {
        None
    };

    // HEAD's symbolic name is the source of truth. A branch metadata refresh
    // can arrive out of order after checkout, leaving its cached `head` flag
    // briefly pointing at the previously checked-out branch.
    let head_branch = model
        .branches
        .iter()
        .find(|b| !model.head_branch_name.is_empty() && b.name == model.head_branch_name);
    let branch_name = if model.head_branch_name.is_empty() {
        "HEAD (no branch)".to_string()
    } else {
        model.head_branch_name.clone()
    };
    let ahead_behind = head_branch.and_then(|b| b.ahead_behind());

    let repo_name = model.repo_name.clone();

    // Build the right-side stats string to measure its width
    let additions = model.total_additions;
    let deletions = model.total_deletions;
    let has_changes = additions > 0 || deletions > 0;

    let stats_text = if has_changes {
        let mut s = String::new();
        if additions > 0 {
            s.push_str(&format!("+{}", additions));
        }
        if additions > 0 && deletions > 0 {
            s.push(' ');
        }
        if deletions > 0 {
            s.push_str(&format!("-{}", deletions));
        }
        s
    } else {
        String::new()
    };

    // Build ahead/behind prefix
    let ab_text = match ahead_behind {
        Some((ahead, behind)) if ahead > 0 && behind > 0 => format!("↑{}↓{} ", ahead, behind),
        Some((ahead, _)) if ahead > 0 => format!("↑{} ", ahead),
        Some((_, behind)) if behind > 0 => format!("↓{} ", behind),
        _ => String::new(),
    };

    let mut spans = Vec::new();

    // When in a special state (rebasing, merging, etc.), show lazygit-style:
    //   (rebasing) reponame → <hash>
    if let Some(state) = state_prefix {
        let right_side = if model.is_rebasing && !model.rebase_onto_hash.is_empty() {
            model.rebase_onto_hash.clone()
        } else {
            branch_name.clone()
        };

        let prefix = format!("({})", state);
        let left_len = 1
            + prefix.len()
            + 1
            + repo_name.len()
            + 1
            + UnicodeWidthStr::width("→ ")
            + right_side.len();
        let right_len = if has_changes { stats_text.len() + 1 } else { 0 };
        let padding = if has_changes {
            inner_width.saturating_sub(left_len + right_len).max(1)
        } else {
            inner_width.saturating_sub(left_len + right_len)
        };

        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            prefix,
            Style::default().fg(theme.accent_secondary),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{} ", repo_name),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("→ ", Style::default().fg(theme.text_dimmed)));
        spans.push(Span::styled(
            right_side,
            Style::default().fg(theme.accent_secondary),
        ));

        if has_changes {
            spans.push(Span::raw(" ".repeat(padding)));
            if additions > 0 {
                spans.push(Span::styled(
                    format!("+{}", additions),
                    Style::default().fg(theme.file_staged.fg.unwrap_or(theme.accent)),
                ));
            }
            if additions > 0 && deletions > 0 {
                spans.push(Span::raw(" "));
            }
            if deletions > 0 {
                spans.push(Span::styled(
                    format!("-{}", deletions),
                    Style::default().fg(theme.file_unstaged.fg.unwrap_or(theme.change_deleted)),
                ));
            }
            spans.push(Span::raw(" "));
        }

        return Line::from(spans);
    }

    // Normal state: " ↑N reponame → branch"
    let left_len = 1
        + UnicodeWidthStr::width(ab_text.as_str())
        + repo_name.len()
        + 1
        + UnicodeWidthStr::width("→ ")
        + branch_name.len();
    let right_len = if has_changes { stats_text.len() + 1 } else { 0 };
    let padding = if has_changes {
        inner_width.saturating_sub(left_len + right_len).max(1)
    } else {
        inner_width.saturating_sub(left_len + right_len)
    };

    if !ab_text.is_empty() {
        spans.push(Span::styled(
            format!(" {}", ab_text),
            Style::default().fg(theme.file_staged.fg.unwrap_or(theme.accent)),
        ));
    } else {
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        format!("{} ", repo_name),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled("→ ", Style::default().fg(theme.text_dimmed)));
    spans.push(Span::styled(
        branch_name,
        Style::default().fg(theme.branch_local.fg.unwrap_or(theme.accent)),
    ));

    if has_changes {
        spans.push(Span::raw(" ".repeat(padding)));
        if additions > 0 {
            spans.push(Span::styled(
                format!("+{}", additions),
                Style::default().fg(theme.file_staged.fg.unwrap_or(theme.accent)),
            ));
        }
        if additions > 0 && deletions > 0 {
            spans.push(Span::raw(" "));
        }
        if deletions > 0 {
            spans.push(Span::styled(
                format!("-{}", deletions),
                Style::default().fg(theme.file_unstaged.fg.unwrap_or(theme.change_deleted)),
            ));
        }
        spans.push(Span::raw(" "));
    }

    Line::from(spans)
}

/// Full status view for the main content area: logo + copyright + repo info
fn render_status_main<'a>(
    frame: &mut Frame,
    rect: Rect,
    model: &Model,
    _config: &AppConfig,
    theme: &crate::config::Theme,
    block: Block<'a>,
) {
    let branch_name = if model.head_branch_name.is_empty() {
        "HEAD (no branch)"
    } else {
        model.head_branch_name.as_str()
    };

    let logo = include_str!("../../logo.txt");
    let mut lines: Vec<Line> = logo
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme.accent),
            ))
        })
        .collect();

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Copyright 2026 Carlo Taleon (Blankeos)",
        Style::default().fg(theme.text_dimmed),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(format!(" Branch: {}", branch_name)));
    if !model.repo_url.is_empty() {
        lines.push(Line::from(format!(" Repo:   {}", model.repo_url)));
    }
    lines.push(Line::from(format!(" Commits: {}", model.commits.len())));
    lines.push(Line::from(format!(" Files: {}", model.files.len())));
    lines.push(Line::from(format!(
        " Version: v{}",
        env!("CARGO_PKG_VERSION")
    )));

    if !model.contributors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Contributors",
            Style::default().fg(theme.text_dimmed),
        )));
        for (name, count) in model.contributors.iter().take(10) {
            lines.push(Line::from(format!("   {:>4}  {}", count, name)));
        }
    }

    // In-progress operation banners
    if model.is_rebasing {
        lines.push(Line::from(Span::styled(
            " REBASING",
            Style::default().fg(theme.accent_secondary),
        )));
    }
    if model.is_merging {
        lines.push(Line::from(Span::styled(
            " MERGING",
            Style::default().fg(theme.accent_secondary),
        )));
    }
    if model.is_cherry_picking {
        lines.push(Line::from(Span::styled(
            " CHERRY-PICKING",
            Style::default().fg(theme.accent_secondary),
        )));
    }
    if model.is_bisecting {
        lines.push(Line::from(Span::styled(
            " BISECTING",
            Style::default().fg(theme.accent_secondary),
        )));
    }

    let widget = Paragraph::new(lines).block(block);
    frame.render_widget(widget, rect);
}

fn render_worktree_list<'a>(model: &Model, theme: &Theme) -> Vec<ListItem<'a>> {
    let names: Vec<_> = model
        .worktrees
        .iter()
        .map(|wt| {
            std::path::Path::new(&wt.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&wt.path)
        })
        .collect();
    let name_width = names.iter().map(|name| name.width()).max().unwrap_or(0);
    model
        .worktrees
        .iter()
        .zip(names)
        .map(|(wt, name)| {
            let marker = if wt.is_current { "* " } else { "  " };
            let line = Line::from(vec![
                Span::styled(marker.to_string(), Style::default().fg(theme.accent)),
                Span::styled(name.to_string(), Style::default().fg(theme.text)),
                Span::raw(" ".repeat(name_width - name.width() + 1)),
                Span::styled(wt.branch.clone(), Style::default().fg(theme.ref_head)),
                Span::styled(
                    if wt.is_main { " (main worktree)" } else { "" },
                    Style::default().fg(theme.text),
                ),
            ]);
            ListItem::new(line)
        })
        .collect()
}

/// Render a list using persistent scroll offsets from ContextManager.
fn render_list_ctx(
    frame: &mut Frame,
    rect: Rect,
    block: Block<'_>,
    items: Vec<ListItem<'_>>,
    selected: usize,
    is_active: bool,
    theme: &crate::config::Theme,
    ctx_mgr: &mut ContextManager,
    ctx: ContextId,
) {
    render_list_with_range_ctx(
        frame, rect, block, items, selected, is_active, theme, None, ctx_mgr, ctx,
    );
}

/// Render a list with range selection using persistent scroll offsets from ContextManager.
fn render_list_with_range_ctx(
    frame: &mut Frame,
    rect: Rect,
    block: Block<'_>,
    items: Vec<ListItem<'_>>,
    selected: usize,
    is_active: bool,
    theme: &crate::config::Theme,
    range: Option<(usize, usize)>,
    ctx_mgr: &mut ContextManager,
    ctx: ContextId,
) {
    let visible_height = block.inner(rect).height as usize;
    let follow = !ctx_mgr.viewport_manually_scrolled;

    let mut so = ctx_mgr.tick_scroll_animation(ctx);

    if follow && visible_height > 0 {
        let target = super::scroll::visible_target(selected, so, visible_height);

        ctx_mgr.request_smooth_scroll(ctx, target);
        so = ctx_mgr.tick_scroll_animation(ctx);
    }

    render_list_with_range_raw(
        frame, rect, block, items, selected, is_active, theme, range, &mut so, false,
    );

    ctx_mgr.set_scroll_offset(ctx, so);
}

fn visual_range_style(theme: &crate::config::Theme, current: bool) -> Style {
    let style = Style::default().bg(theme.graph_colors[0]);
    if current {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

#[allow(clippy::too_many_arguments)]
fn render_commit_list_ctx(
    frame: &mut Frame,
    rect: Rect,
    block: Block<'_>,
    model: &Model,
    theme: &crate::config::Theme,
    cherry_picked: &[String],
    selected: usize,
    is_active: bool,
    range: Option<(usize, usize)>,
    ctx_mgr: &mut ContextManager,
    ctx: ContextId,
    cache: &mut presentation::commits::CommitListCache,
    sub_commits: bool,
    full: bool,
) {
    let total_len = if sub_commits {
        model.sub_commits.len()
    } else {
        model.commits.len()
    };
    if total_len == 0 {
        frame.render_widget(block, rect);
        return;
    }

    let visible_height = block.inner(rect).height as usize;
    if visible_height == 0 {
        frame.render_widget(block, rect);
        return;
    }

    let mut offset = ctx_mgr.tick_scroll_animation(ctx);

    if !ctx_mgr.viewport_manually_scrolled {
        let target = super::scroll::visible_target(selected, offset, visible_height);

        ctx_mgr.request_smooth_scroll(ctx, target);
        offset = ctx_mgr.tick_scroll_animation(ctx);
    }

    offset = offset.min(total_len.saturating_sub(visible_height));
    ctx_mgr.set_scroll_offset(ctx, offset);

    let items = if sub_commits {
        presentation::commits::render_sub_commit_list_window(
            model,
            theme,
            offset,
            visible_height,
            full,
            cache,
        )
    } else {
        presentation::commits::render_commit_list_window(
            model,
            theme,
            cherry_picked,
            offset,
            visible_height,
            full,
            cache,
        )
    };
    let visible_items: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let idx = offset + i;
            let in_visual_range = is_active && range.is_some_and(|(lo, hi)| idx >= lo && idx <= hi);
            if in_visual_range {
                item.style(visual_range_style(theme, idx == selected))
            } else if is_active && idx == selected {
                item.style(theme.selected_line)
            } else {
                item
            }
        })
        .collect();

    frame.render_widget(List::new(visible_items).block(block), rect);
}

/// Highlight contiguous `/` search matches in a list panel (lazygit-style).
/// Scans already-painted buffer cells so item styling (selection, colors) is preserved.
fn render_list_search_highlights(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    theme: &crate::config::Theme,
) {
    if query.is_empty() || area.width < 3 || area.height < 3 {
        return;
    }

    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    if query_lower.is_empty() {
        return;
    }

    let highlight = theme.search_match;
    let buf = frame.buffer_mut();
    let buf_area = *buf.area();

    let inner_x = area.x + 1;
    let inner_end_x = area.x + area.width.saturating_sub(1);
    let inner_y = area.y + 1;
    let inner_end_y = area.y + area.height.saturating_sub(1);

    for y in inner_y..inner_end_y {
        if y >= buf_area.y + buf_area.height {
            break;
        }

        let mut row_chars: Vec<(u16, char)> = Vec::new();
        for x in inner_x..inner_end_x.min(buf_area.x + buf_area.width) {
            if let Some(cell) = buf.cell((x, y)) {
                let ch = cell.symbol().chars().next().unwrap_or(' ');
                row_chars.push((x, ch));
            }
        }
        if row_chars.is_empty() {
            continue;
        }

        let row_text: String = row_chars.iter().map(|(_, c)| *c).collect();
        let row_lower = row_text.to_lowercase();
        let row_lower_chars: Vec<char> = row_lower.chars().collect();

        let mut start = 0usize;
        while start + query_lower.len() <= row_lower_chars.len() {
            let is_match = row_lower_chars[start..start + query_lower.len()]
                .iter()
                .zip(query_lower.iter())
                .all(|(a, b)| a == b);
            if is_match {
                for i in 0..query_lower.len() {
                    let (x, _) = row_chars[start + i];
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(highlight);
                    }
                }
                start += query_lower.len();
            } else {
                start += 1;
            }
        }
    }
}

fn render_list_with_range_raw(
    frame: &mut Frame,
    rect: Rect,
    block: Block<'_>,
    items: Vec<ListItem<'_>>,
    selected: usize,
    is_active: bool,
    theme: &crate::config::Theme,
    range: Option<(usize, usize)>,
    scroll_offset: &mut usize,
    follow_selection: bool,
) {
    if items.is_empty() {
        frame.render_widget(block, rect);
        return;
    }

    let inner = block.inner(rect);
    let visible_height = inner.height as usize;

    // Ensure selected item is visible, only adjusting scroll when necessary.
    // Skip when viewport was manually scrolled (mouse scroll) to avoid snapping back.
    if visible_height == 0 {
        frame.render_widget(block, rect);
        return;
    }
    if follow_selection {
        super::scroll::ensure_visible(selected, scroll_offset, visible_height);
    }
    // Clamp scroll offset to valid range
    let max_offset = items.len().saturating_sub(visible_height);
    if *scroll_offset > max_offset {
        *scroll_offset = max_offset;
    }
    let offset = *scroll_offset;

    let visible_items: Vec<ListItem> = items
        .into_iter()
        .skip(offset)
        .take(visible_height)
        .enumerate()
        .map(|(i, item)| {
            let idx = i + offset;
            let in_visual_range = is_active && range.is_some_and(|(lo, hi)| idx >= lo && idx <= hi);
            if in_visual_range {
                item.style(visual_range_style(theme, idx == selected))
            } else if is_active && idx == selected {
                item.style(theme.selected_line)
            } else {
                item
            }
        })
        .collect();

    let list = List::new(visible_items).block(block);
    frame.render_widget(list, rect);
}

fn get_info_content<'a>(model: &Model, ctx_mgr: &ContextManager) -> Vec<Line<'a>> {
    let active = ctx_mgr.active();
    let selected = ctx_mgr.selected_active();

    match active {
        ContextId::Files => {
            if model.files.is_empty() {
                vec![Line::from(" No modified files")]
            } else {
                vec![Line::from(" Select a file to view diff")]
            }
        }
        ContextId::Commits => {
            if let Some(commit) = model.commits.get(selected) {
                vec![
                    Line::from(format!(" Commit: {}", commit.short_hash())),
                    Line::from(format!(
                        " Author: {} <{}>",
                        super::presentation::text::plain_text(&commit.author_name),
                        super::presentation::text::plain_text(&commit.author_email)
                    )),
                    Line::from(format!(
                        " Message: {}",
                        super::presentation::text::plain_text(&commit.name)
                    )),
                ]
            } else {
                vec![Line::from(" No commit selected")]
            }
        }
        ContextId::Branches => {
            if let Some(branch) = model.branches.get(selected) {
                let mut lines = vec![
                    Line::from(format!(" Branch: {}", branch.name)),
                    Line::from(format!(" Hash: {}", branch.hash)),
                ];
                if let Some(ref upstream) = branch.upstream {
                    lines.push(Line::from(format!(" Upstream: {}", upstream)));
                }
                lines
            } else {
                vec![Line::from(" No branch selected")]
            }
        }
        ContextId::Stash => {
            if let Some(entry) = model.stash_entries.get(selected) {
                vec![
                    Line::from(format!(" Stash: {}", entry.ref_name())),
                    Line::from(format!(" {}", entry.name)),
                ]
            } else {
                vec![Line::from(" No stash entries")]
            }
        }
        ContextId::Remotes => {
            if let Some(remote) = model.remotes.get(selected) {
                let mut lines = vec![Line::from(format!(" Remote: {}", remote.name))];
                for url in &remote.urls {
                    lines.push(Line::from(format!(" URL: {}", url)));
                }
                lines.push(Line::from(format!(" Branches: {}", remote.branches.len())));
                for branch in &remote.branches {
                    lines.push(Line::from(format!("   {} ({})", branch.name, branch.hash)));
                }
                lines
            } else {
                vec![Line::from(" No remotes")]
            }
        }
        ContextId::RemoteBranches => {
            if let Some(rb) = model.sub_remote_branches.get(selected) {
                vec![
                    Line::from(format!(" Branch: {}/{}", rb.remote_name, rb.name)),
                    Line::from(format!(" Hash: {}", rb.hash)),
                ]
            } else {
                vec![Line::from(" No remote branches")]
            }
        }
        ContextId::Tags => {
            if let Some(tag) = model.tags.get(selected) {
                let mut lines = vec![
                    Line::from(format!(" Tag: {}", tag.name)),
                    Line::from(format!(" Hash: {}", tag.hash)),
                ];
                if !tag.message.is_empty() {
                    lines.push(Line::from(format!(" Message: {}", tag.message)));
                }
                lines
            } else {
                vec![Line::from(" No tags")]
            }
        }
        ContextId::Worktrees => {
            if let Some(wt) = model.worktrees.get(selected) {
                vec![
                    Line::from(format!(" Worktree: {}", wt.branch)),
                    Line::from(format!(" Path: {}", wt.path)),
                    Line::from(format!(" Hash: {}", wt.hash)),
                ]
            } else {
                vec![Line::from(" No worktrees")]
            }
        }
        ContextId::Submodules => {
            if let Some(sub) = model.submodules.get(selected) {
                vec![
                    Line::from(format!(" Submodule: {}", sub.name)),
                    Line::from(format!(" Path: {}", sub.path)),
                ]
            } else {
                vec![Line::from(" No submodules")]
            }
        }
        _ => vec![Line::from(" lazygitrs")],
    }
}

pub(super) fn format_key_hint(key: &str) -> String {
    let key = key.trim();
    let inner = if let Some(stripped) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) {
        stripped.trim()
    } else {
        key
    };
    let inner_lower = inner.to_ascii_lowercase();
    if let Some(rest) = inner_lower
        .strip_prefix("c-")
        .or_else(|| inner_lower.strip_prefix("c+"))
        .or_else(|| inner_lower.strip_prefix("ctrl-"))
        .or_else(|| inner_lower.strip_prefix("ctrl+"))
    {
        format!("ctrl+{}", rest)
    } else if let Some(rest) = inner_lower
        .strip_prefix("a-")
        .or_else(|| inner_lower.strip_prefix("a+"))
        .or_else(|| inner_lower.strip_prefix("alt-"))
        .or_else(|| inner_lower.strip_prefix("alt+"))
    {
        format!("alt+{}", rest)
    } else if let Some(rest) = inner_lower
        .strip_prefix("s-")
        .or_else(|| inner_lower.strip_prefix("s+"))
        .or_else(|| inner_lower.strip_prefix("shift-"))
        .or_else(|| inner_lower.strip_prefix("shift+"))
    {
        format!("shift+{}", rest)
    } else if inner_lower == "enter" || inner_lower == "return" {
        "enter".to_string()
    } else if inner_lower == "escape" || inner_lower == "esc" {
        "esc".to_string()
    } else if inner_lower == "space" {
        "space".to_string()
    } else if inner_lower == "backtab" || inner_lower == "shift-tab" || inner_lower == "shift+tab" {
        "shift+tab".to_string()
    } else {
        key.to_string()
    }
}

fn render_search_bar_or_status_bar(
    frame: &mut Frame,
    status_bar: Rect,
    search_state: Option<(&str, usize, usize)>,
    search_textarea: Option<&tui_textarea::TextArea<'_>>,
    ctx_mgr: &ContextManager,
    diff_view: &DiffViewState,
    theme: &Theme,
    model: &Model,
    diff_focused: bool,
    has_copied_commits: bool,
    keybindings: &KeybindingConfig,
    has_active_filters: bool,
    active_file_tree: bool,
) {
    if let Some((query, match_count, current_match)) = search_state {
        let match_info = if match_count > 0 {
            format!(" {}/{}", current_match + 1, match_count)
        } else if !query.is_empty() {
            " (no matches)".to_string()
        } else {
            String::new()
        };

        if let Some(ta) = search_textarea {
            // Render: "/" prefix + textarea + match info
            let prefix_width = 2u16; // " /"
            let suffix_text = match_info;
            let suffix_width = suffix_text.len() as u16;
            let ta_width = status_bar.width.saturating_sub(prefix_width + suffix_width);

            let prefix_rect = Rect::new(status_bar.x, status_bar.y, prefix_width, 1);
            let prefix = Paragraph::new(Span::styled(
                " /",
                Style::default().fg(theme.accent_secondary),
            ));
            frame.render_widget(prefix, prefix_rect);

            let ta_rect = Rect::new(status_bar.x + prefix_width, status_bar.y, ta_width, 1);
            frame.render_widget(ta, ta_rect);

            if !suffix_text.is_empty() {
                let suffix_rect = Rect::new(
                    status_bar.x + prefix_width + ta_width,
                    status_bar.y,
                    suffix_width,
                    1,
                );
                let suffix = Paragraph::new(Span::styled(
                    suffix_text,
                    Style::default().fg(theme.accent_secondary),
                ));
                frame.render_widget(suffix, suffix_rect);
            }
        } else {
            let bar = Paragraph::new(Span::styled(
                format!(" /{}{}", query, match_info),
                Style::default().fg(theme.accent_secondary),
            ));
            frame.render_widget(bar, status_bar);
        }
    } else {
        render_status_bar(
            frame,
            status_bar,
            ctx_mgr,
            diff_view,
            theme,
            model,
            diff_focused,
            has_copied_commits,
            keybindings,
            has_active_filters,
            active_file_tree,
        );
    }
}

fn render_status_bar(
    frame: &mut Frame,
    rect: Rect,
    ctx_mgr: &ContextManager,
    diff_view: &DiffViewState,
    _theme: &crate::config::Theme,
    model: &Model,
    diff_focused: bool,
    has_copied_commits: bool,
    keybindings: &KeybindingConfig,
    has_active_filters: bool,
    active_file_tree: bool,
) {
    let mut hints: Vec<(&str, &str)> = Vec::new();
    let mut emphasized: Vec<&str> = Vec::new();
    let active = ctx_mgr.active();
    let open_log_menu_key = format_key_hint(&keybindings.commits.open_log_menu);
    let fold_key = format_key_hint(&keybindings.universal.fold_directory);
    let shell_key = format_key_hint(&keybindings.universal.custom_command_prompt);

    if active_file_tree && !diff_focused && !fold_key.is_empty() {
        hints.push((fold_key.as_str(), "fold/unfold"));
    }

    // When in a special state (rebasing/merging/cherry-picking), show those options prominently
    if model.is_rebasing {
        hints.push(("m", "continue/abort/skip rebase"));
    } else if model.is_merging {
        hints.push(("m", "continue/abort merge"));
    } else if model.is_cherry_picking {
        hints.push(("m", "continue/abort cherry-pick"));
    }

    if diff_focused && !diff_view.is_empty() {
        // Diff-focused hint set: only the diff-relevant keys, kept tight.
        // Revert-related keys are grouped together at the front so users see
        // enter right next to its cycle keys. enter itself only appears when a
        // hunk is actually selected (pressing it otherwise is a no-op).
        if active == ContextId::Files {
            let has_selection = diff_view.selected_revert_hunk.is_some();
            let has_undo = !diff_view.revert_undo_stack.is_empty();
            let mut idx = 0;
            if has_selection {
                hints.insert(idx, ("enter", "hunk menu"));
                emphasized.push("enter");
                idx += 1;
            }
            hints.insert(idx, ("{/}", "cycle hunks"));
            idx += 1;
            if has_undo {
                hints.insert(idx, ("u", "undo revert"));
            }
        } else {
            hints.push(("{/}", "prev/next hunk"));
        }
        hints.push(("[/]", "side view"));
        let view_layout_hint = match diff_view.view_layout {
            DiffViewLayout::SideBySide => "unified view",
            DiffViewLayout::Unified => "split view",
        };
        hints.push(("\\", view_layout_hint));
    } else {
        // Sidebar-focused: context-specific hints.
        let view_layout_hint = match diff_view.view_layout {
            DiffViewLayout::SideBySide => "unified view",
            DiffViewLayout::Unified => "split view",
        };
        match active {
            ContextId::Files => {
                hints.extend([
                    ("c", "commit"),
                    ("a", "stage all"),
                    ("space", "toggle"),
                    ("`", "tree"),
                ]);
                hints.extend([
                    ("\\", view_layout_hint),
                    ("d", "discard"),
                    ("D", "reset/nuke"),
                    ("e", "edit"),
                    ("o", "open"),
                ]);
            }
            ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                hints.extend([("enter", "focus diff"), ("`", "tree"), ("y", "copy")]);
                hints.push(("\\", view_layout_hint));
            }
            ContextId::BranchCommits => {
                hints.extend([("enter", "commit files"), ("\\", view_layout_hint)]);
            }
            ContextId::Reflog => {
                hints.extend([("enter", "commit files"), ("\\", view_layout_hint)]);
            }
            ContextId::Branches => {
                hints.extend([
                    ("space", "checkout"),
                    ("n", "new"),
                    ("d", "delete"),
                    ("M", "merge"),
                    ("r", "rebase"),
                ]);
            }
            ContextId::Commits => {
                if has_active_filters {
                    hints.push(("esc", "reset filter"));
                }
                if has_copied_commits {
                    hints.push(("V", "paste (cherry-pick)"));
                }
                // Keep the gutter intentionally small and tutorial-like. Less-common
                // actions such as Patch, Drop, and scrolling remain discoverable in `?`.
                hints.extend([
                    ("C", "copy (cherry-pick)"),
                    ("r", "reword"),
                    ("g", "reset"),
                    ("t", "revert"),
                    ("\\", view_layout_hint),
                ]);
                if !open_log_menu_key.is_empty() {
                    hints.push((open_log_menu_key.as_str(), "filter branch"));
                }
            }
            ContextId::Stash => {
                hints.extend([
                    ("g", "pop"),
                    ("space", "apply"),
                    ("d", "drop"),
                    ("\\", view_layout_hint),
                ]);
            }
            ContextId::Remotes => {
                hints.extend([
                    ("enter", "branches"),
                    ("f", "fetch"),
                    ("e", "edit"),
                    ("P", "push"),
                    ("p", "pull"),
                ]);
            }
            ContextId::RemoteBranches => {
                hints.extend([
                    ("enter", "commits"),
                    ("space", "checkout"),
                    ("M", "merge"),
                    ("r", "rebase"),
                    ("d", "delete"),
                ]);
            }
            ContextId::Tags => {
                hints.extend([("n", "new"), ("d", "delete"), ("P", "push")]);
            }
            ContextId::Worktrees => {
                hints.extend([("space", "switch"), ("n", "new"), ("d", "remove")]);
            }
            ContextId::Submodules => {
                hints.extend([
                    ("space", "update"),
                    ("a", "add"),
                    ("d", "remove"),
                    ("e", "enter"),
                ]);
            }
            _ => {}
        }
        if !diff_view.is_empty() {
            // Keep the Commits footer focused on high-value tutorial actions.
            // Preview scrolling is documented in `?` instead of consuming
            // scarce footer width.
            if active != ContextId::Commits {
                hints.push(("J/K", "scroll diff"));
            }
            hints.push(("{/}", "hunks"));
        }
    }

    // Global hints (always last). Panel/list navigation is intentionally
    // omitted from the footer; it is discoverable in `?` and otherwise
    // overwhelms the contextual tutorial-style hints.
    if !shell_key.is_empty() {
        hints.push((shell_key.as_str(), "shell"));
    }
    hints.push(("q", "quit"));

    let key_style = Style::default()
        .fg(_theme.text)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let key_emphasis_style = Style::default()
        .fg(_theme.accent)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let desc_style = Style::default().fg(_theme.text_dimmed);
    let spans: Vec<Span> = hints
        .iter()
        .flat_map(|(key, desc)| {
            let style = if emphasized.contains(key) {
                key_emphasis_style
            } else {
                key_style
            };
            vec![
                Span::styled(format!(" {} ", key), style),
                Span::styled(format!("{} ", desc), desc_style),
            ]
        })
        .collect();

    let bar = Paragraph::new(Line::from(spans));
    frame.render_widget(bar, rect);
}

/// Render mouse text selection highlight overlay and copy tooltip on the diff view.
/// `panel_rect` is the main diff panel Rect — selection is rendered only within the selected side.
pub fn render_selection_overlay(
    frame: &mut Frame,
    diff_view: &mut DiffViewState,
    panel_rect: Rect,
    theme: &Theme,
) {
    use crate::pager::ChangeType;

    let selection = match &diff_view.selection {
        Some(sel) => sel.clone(),
        None => return,
    };

    let (top_row, top_col, bot_row, bot_col) = selection.normalized();
    let is_click = selection.is_click;

    // For non-click selections, bail on empty (single point).
    if !is_click && top_row == bot_row && top_col == bot_col {
        return;
    }

    // Use the same centralized layout that render_diff and the mouse handler use.
    let pl = DiffPanelLayout::compute(panel_rect, diff_view);
    let (content_start, content_end) = pl.content_range(selection.panel);

    // Compute the actual file line number at the top of the selection/click for editAtLine.
    let edit_line_number: Option<usize> = if top_row >= pl.inner_y {
        let (line_idx, panel) = diff_view
            .line_chunk_panel_at_row(top_row, &pl, selection.panel)
            .map(|(line_idx, _, panel)| (line_idx, panel))
            .unwrap_or_else(|| {
                (
                    diff_view.fallback_line_idx_for_row(top_row, &pl),
                    selection.panel,
                )
            });
        diff_view.file_line_number(line_idx, panel)
    } else {
        None
    };
    // Compute the file column number (1-based) from the terminal click position.
    let edit_column_number: Option<usize> = if top_col >= content_start {
        Some((top_col - content_start) as usize + diff_view.horizontal_scroll + 1)
    } else {
        Some(1)
    };
    if let Some(ref mut sel) = diff_view.selection {
        sel.edit_line_number = edit_line_number;
        sel.edit_column_number = edit_column_number;
    }

    let buf = frame.buffer_mut();
    let buf_area = *buf.area();

    // --- Click state: highlight the clicked cell and show "e edit" tooltip ---
    if is_click {
        // Highlight the single clicked cell
        if top_row >= pl.inner_y
            && top_row < pl.inner_end_y
            && top_col >= content_start
            && top_col < content_end
            && top_row < buf_area.y + buf_area.height
        {
            let highlight_style = Style::default().bg(theme.popup_border).fg(Color::Black);
            if let Some(cell) = buf.cell_mut((top_col, top_row)) {
                cell.set_style(highlight_style);
            }
        }

        if diff_view.file_exists_on_disk {
            let tooltip_style = Style::default().bg(theme.selected_bg).fg(theme.text_strong);
            let key_style = Style::default()
                .bg(theme.selected_bg)
                .fg(theme.accent_secondary)
                .add_modifier(Modifier::BOLD);

            let parts: &[(&str, Style)] = &[
                (" ", tooltip_style),
                ("e", key_style),
                (" edit ", tooltip_style),
            ];
            let tooltip_width: u16 = parts.iter().map(|(s, _)| s.len() as u16).sum();
            let tooltip_x = top_col
                .saturating_sub(tooltip_width / 2)
                .max(content_start)
                .min(content_end.saturating_sub(tooltip_width));
            let tooltip_y = (top_row + 1).min(pl.inner_end_y.saturating_sub(1));

            if tooltip_y < buf_area.y + buf_area.height {
                let mut col = tooltip_x;
                for (text, style) in parts {
                    for ch in text.chars() {
                        if col >= content_end {
                            break;
                        }
                        if let Some(cell) = buf.cell_mut((col, tooltip_y)) {
                            cell.set_char(ch);
                            cell.set_style(*style);
                        }
                        col += 1;
                    }
                }
            }
        }
        return;
    }

    // --- Drag selection: highlight text and show tooltip ---
    let mut extracted_text = String::new();

    let row_start = top_row.max(pl.inner_y);
    let row_end = bot_row.min(pl.inner_end_y.saturating_sub(1));

    let highlight_style = Style::default().bg(theme.popup_border).fg(Color::Black);

    for (i, row) in (row_start..=row_end).enumerate() {
        if row >= buf_area.y + buf_area.height {
            break;
        }

        // Map terminal row to diff line index.
        let line_idx = diff_view
            .line_chunk_at_row(row, &pl)
            .map(|(line_idx, _)| line_idx)
            .unwrap_or_else(|| diff_view.fallback_line_idx_for_row(row, &pl));
        if let Some(diff_line) = diff_view.lines.get(line_idx) {
            // Skip file header separator lines.
            if diff_line.file_header.is_some() {
                continue;
            }
            // Skip slash-fill rows (the empty side for Insert/Delete lines).
            let is_slash_fill = diff_view.view_layout == DiffViewLayout::SideBySide
                && match selection.panel {
                    DiffPanel::Old => diff_line.change_type == ChangeType::Insert,
                    DiffPanel::New => diff_line.change_type == ChangeType::Delete,
                };
            if is_slash_fill {
                continue;
            }
        }

        // Column range: intersection of mouse selection cols with panel content cols.
        let sel_col_start = if row == top_row { top_col } else { 0 };
        let sel_col_end = if row == bot_row { bot_col } else { u16::MAX };
        let hl_start = sel_col_start.max(content_start);
        let hl_end = sel_col_end.min(content_end);

        if hl_start >= hl_end {
            continue;
        }

        let mut row_text = String::new();
        for col in hl_start..hl_end {
            if col >= buf_area.x + buf_area.width {
                break;
            }
            if let Some(cell) = buf.cell_mut((col, row)) {
                row_text.push_str(cell.symbol());
                cell.set_style(highlight_style);
            }
        }

        let trimmed = row_text.trim_end();
        if !trimmed.is_empty() {
            if !extracted_text.is_empty() {
                extracted_text.push('\n');
            }
            extracted_text.push_str(trimmed);
        } else if i > 0 && i < (row_end - row_start) as usize {
            // Preserve blank lines in the middle of the selection.
            extracted_text.push('\n');
        }
    }

    // Store extracted text for the copy action.
    if let Some(ref mut sel) = diff_view.selection {
        sel.text = extracted_text;
    }

    // Tooltip below the selection (only after drag finishes).
    if !selection.dragging {
        let tooltip_style = Style::default().bg(theme.selected_bg).fg(theme.text_strong);
        let key_style = Style::default()
            .bg(theme.selected_bg)
            .fg(theme.accent_secondary)
            .add_modifier(Modifier::BOLD);

        // Build parts conditionally: include "e edit" only if file is on disk.
        let mut parts: Vec<(&str, Style)> = Vec::new();
        if diff_view.file_exists_on_disk {
            parts.push((" ", tooltip_style));
            parts.push(("e", key_style));
            parts.push((" edit  ", tooltip_style));
        } else {
            parts.push((" ", tooltip_style));
        }
        parts.push(("y", key_style));
        parts.push((" copy  ", tooltip_style));
        parts.push(("esc", key_style));
        parts.push((" ", tooltip_style));

        let tooltip_width: u16 = parts.iter().map(|(s, _)| s.len() as u16).sum();
        let tooltip_x = bot_col
            .saturating_sub(tooltip_width / 2)
            .max(content_start)
            .min(content_end.saturating_sub(tooltip_width));
        let tooltip_y = (bot_row + 1).min(pl.inner_end_y.saturating_sub(1));

        if tooltip_y < buf_area.y + buf_area.height {
            let mut col = tooltip_x;
            for (text, style) in &parts {
                for ch in text.chars() {
                    if col >= content_end {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((col, tooltip_y)) {
                        cell.set_char(ch);
                        cell.set_style(*style);
                    }
                    col += 1;
                }
            }
        }
    }
}

const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Geometry of the ✦ AI-generate button shown inside the commit-message popup.
/// The button sits on the "Summary" label row, right-aligned inside the popup
/// box (not on the border).
pub fn commit_ai_button_geometry(popup: &PopupState, area: Rect) -> Option<Rect> {
    if area.width < 10 || area.height < 6 {
        return None;
    }
    let popup_width = commit_popup_width(area.width);
    if popup_width < 16 {
        return None;
    }
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let ta_height: u16 = match popup {
        PopupState::CommitInput {
            show_help: false, ..
        } => commit_popup_height(area.height),
        _ => return None,
    };
    if ta_height < 7 {
        return None;
    }
    let ta_y = (area.height.saturating_sub(ta_height)) / 2;

    // Button: 3 cells (" ✦ ") on the Summary label row (inner.y = ta_y + 1),
    // right-aligned within the inner area (1 col border + 1 col right padding).
    let btn_w: u16 = 3;
    let btn_x = x + popup_width.saturating_sub(btn_w + 2);
    let btn_y = ta_y + 1;
    Some(Rect::new(btn_x, btn_y, btn_w, 1))
}

/// Geometry of the Description textarea inside the two-field commit popup.
pub fn commit_description_textarea_geometry(popup: &PopupState, area: Rect) -> Option<Rect> {
    if area.width < 4 || area.height < 4 {
        return None;
    }

    let PopupState::CommitInput { .. } = popup else {
        return None;
    };

    let popup_width = commit_popup_width(area.width);
    let ta_height = commit_popup_height(area.height);
    let ta_y = (area.height.saturating_sub(ta_height)) / 2;
    let ta_rect = Rect::new(
        (area.width.saturating_sub(popup_width)) / 2,
        ta_y,
        popup_width,
        ta_height,
    );
    let inner = ta_rect.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    if inner.height <= 5 {
        return None;
    }

    let body_height = inner.height.saturating_sub(5);
    let gutter_width = COMMIT_BODY_GUTTER_WIDTH.min(inner.width.saturating_sub(1));
    (body_height > 0).then(|| {
        Rect::new(
            inner.x + gutter_width,
            inner.y + 4,
            inner.width.saturating_sub(gutter_width),
            body_height,
        )
    })
}

/// Tooltip rect placed one row above the popup, right-aligned with the button.
fn commit_ai_tooltip_rect(area: Rect, btn_rect: Rect, tip_w: u16) -> Rect {
    let popup_width = commit_popup_width(area.width);
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let ta_height = commit_popup_height(area.height);
    let ta_y = (area.height.saturating_sub(ta_height)) / 2;

    let mut tip_x = (btn_rect.x + btn_rect.width).saturating_sub(tip_w);
    // Keep tooltip within the popup's horizontal bounds when possible.
    let popup_right = x + popup_width;
    if tip_x + tip_w > popup_right {
        tip_x = popup_right.saturating_sub(tip_w);
    }
    if tip_x + tip_w > area.width {
        tip_x = area.width.saturating_sub(tip_w);
    }
    let tip_y = if ta_y >= 1 {
        ta_y - 1
    } else if ta_y + ta_height < area.height {
        ta_y + ta_height
    } else {
        btn_rect.y
    };
    Rect::new(tip_x, tip_y, tip_w, 1)
}

pub fn render_loading_overlay(
    frame: &mut Frame,
    area: Rect,
    spinner_frame: usize,
    theme: &Theme,
    title: &str,
    message: &str,
    hint: Option<(&str, &str)>,
) {
    let spinner = SPINNER_CHARS[(spinner_frame / 8) % SPINNER_CHARS.len()];
    let label = if message.is_empty() {
        title.to_string()
    } else if message.len() <= 40 {
        message.to_string()
    } else {
        // Prefer title when message is long (e.g. "Pushing branch to remote...").
        title.to_string()
    };
    render_compact_overlay(frame, area, theme, &spinner.to_string(), &label, hint);
}

pub fn render_success_overlay(frame: &mut Frame, area: Rect, theme: &Theme, label: &str) {
    render_compact_overlay(frame, area, theme, "✓", label, None);
}

fn render_compact_overlay(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    marker: &str,
    label: &str,
    hint: Option<(&str, &str)>,
) {
    if area.width < 4 || area.height < 3 {
        return;
    }

    let content = format!(" {marker} {label} ");
    let mut width = (content.chars().count() as u16)
        .saturating_add(2)
        .min(area.width);
    width = width.max(12).min(area.width.saturating_sub(2).max(1));
    let height = if hint.is_some() { 4u16 } else { 3u16 };
    let height = height.min(area.height);
    let x = area.width.saturating_sub(width).saturating_sub(1);
    let y = area.height.saturating_sub(height).saturating_sub(1);
    let popup_rect = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent));

    let mut text = vec![Line::from(vec![
        Span::styled(format!(" {marker} "), Style::default().fg(theme.accent)),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some((key, desc)) = hint {
        text.push(Line::from(vec![
            Span::styled(
                format!(" {key}"),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {desc}"), Style::default().fg(theme.text_dimmed)),
        ]));
    }

    frame.render_widget(Paragraph::new(text).block(block), popup_rect);
}

fn command_output_geometry(area: Rect) -> (Rect, usize) {
    let width = (area.width.saturating_mul(9) / 10)
        .min(120)
        .max(4)
        .min(area.width);
    let height = (area.height.saturating_mul(4) / 5).max(4).min(area.height);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    (rect, usize::from(height.saturating_sub(3)))
}

pub(super) fn command_output_max_scroll(message: &str, area: Rect) -> usize {
    let (rect, rows) = command_output_geometry(area);
    wrap_popup_lines(message, usize::from(rect.width.saturating_sub(2)))
        .len()
        .saturating_sub(rows)
}

fn hard_wrap_preserve(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in line.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !current.is_empty() && current_width.saturating_add(ch_width) > width {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(ch_width);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn email_header_label_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent_secondary)
        .add_modifier(Modifier::BOLD)
}

fn email_header_value_style(theme: &Theme) -> Style {
    Style::default().fg(theme.text_strong)
}

fn email_header_label(line: &str) -> Option<&'static str> {
    ["From:", "To:", "Cc:", "Subject:", "Date:"]
        .into_iter()
        .find(|header| line.starts_with(header))
}

fn is_email_header_line(line: &str) -> bool {
    email_header_label(line).is_some()
}

fn styled_email_header_line(line: &str, theme: &Theme) -> Line<'static> {
    if let Some(label) = email_header_label(line) {
        let value = &line[label.len()..];
        Line::from(vec![
            Span::styled(label.to_string(), email_header_label_style(theme)),
            Span::styled(value.to_string(), email_header_value_style(theme)),
        ])
    } else {
        Line::from(Span::styled(
            line.to_string(),
            email_header_value_style(theme),
        ))
    }
}

fn patch_line_style(line: &str, theme: &Theme, in_diff: bool) -> Style {
    if in_diff && (line.starts_with("diff --git ") || line.starts_with("index ")) {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else if in_diff && line.starts_with("@@") {
        Style::default()
            .fg(theme.accent_secondary)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("-- ") || line.starts_with("From ") {
        Style::default().fg(theme.text_dimmed)
    } else if in_diff && line.starts_with("+++ ") {
        theme.diff_add.add_modifier(Modifier::BOLD)
    } else if in_diff && line.starts_with("--- ") {
        theme.diff_remove.add_modifier(Modifier::BOLD)
    } else if in_diff && line.starts_with('+') {
        theme.diff_add.bg(theme.diff_add_bg)
    } else if in_diff && line.starts_with('-') {
        theme.diff_remove.bg(theme.diff_remove_bg)
    } else if line == "---" {
        Style::default().fg(theme.text_dimmed)
    } else {
        Style::default().fg(theme.text)
    }
}

/// Wrap a format-patch body while preserving enough parser state to avoid
/// coloring commit-message prose as diff content. In particular, kernel prose
/// often contains identifiers such as `->get_parent()` at the start of a line;
/// those are only deletions after the first `diff --git` marker.
fn wrap_patch_preview_lines(message: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut in_diff = false;
    let mut in_initial_headers = true;
    let mut previous_was_header = false;

    for source in message.lines() {
        if source.starts_with("diff --git ") {
            in_diff = true;
            in_initial_headers = false;
            previous_was_header = false;
        }

        let current_is_header = is_email_header_line(source);
        let header_continuation = in_initial_headers
            && previous_was_header
            && (source.starts_with(' ') || source.starts_with('\t'));

        if source.is_empty() {
            out.push(Line::from(String::new()));
            in_initial_headers = false;
            previous_was_header = false;
            continue;
        }

        for part in hard_wrap_preserve(source, width) {
            if current_is_header || header_continuation {
                out.push(styled_email_header_line(&part, theme));
            } else {
                out.push(Line::from(Span::styled(
                    part.clone(),
                    patch_line_style(&part, theme, in_diff),
                )));
            }
        }

        previous_was_header = current_is_header || header_continuation;
    }
    if out.is_empty() {
        out.push(Line::from(String::new()));
    }
    out
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        let separator = usize::from(!current.is_empty());
        if !current.is_empty() && current_width + separator + word_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }

        if word_width > width && current.is_empty() {
            let parts = hard_wrap_preserve(word, width);
            for (index, part) in parts.into_iter().enumerate() {
                if index == 0 {
                    current_width = UnicodeWidthStr::width(part.as_str());
                    current = part;
                } else {
                    lines.push(std::mem::take(&mut current));
                    current_width = UnicodeWidthStr::width(part.as_str());
                    current = part;
                }
            }
            continue;
        }

        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn email_preview_header_lines(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    date: &str,
    width: usize,
) -> Vec<String> {
    const LABEL_WIDTH: usize = 9;
    let value_width = width.saturating_sub(LABEL_WIDTH).max(1);
    let to_value = if to.is_empty() {
        "(none)".to_string()
    } else {
        to.iter()
            .map(|value| value.trim())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cc_value = if cc.is_empty() {
        "(none)".to_string()
    } else {
        cc.iter()
            .map(|value| value.trim())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let values = [
        ("From:", from.trim(), 2usize),
        ("To:", to_value.as_str(), 2usize),
        ("Cc:", cc_value.as_str(), 3usize),
        ("Subject:", subject.trim(), 2usize),
        ("Date:", date.trim(), 1usize),
    ];
    let mut lines = Vec::new();
    for (label, value, max_lines) in values {
        let mut chunks = wrap_words(value, value_width);
        let truncated = chunks.len() > max_lines;
        chunks.truncate(max_lines);
        if truncated {
            if let Some(last) = chunks.last_mut() {
                while UnicodeWidthStr::width(last.as_str()) + 1 > value_width && !last.is_empty() {
                    last.pop();
                }
                last.push('…');
            }
        }
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index == 0 {
                lines.push(format!("{:<width$}{}", label, chunk, width = LABEL_WIDTH));
            } else {
                lines.push(format!("{:<width$}{}", "", chunk, width = LABEL_WIDTH));
            }
        }
    }
    lines
}

fn email_preview_body_layout(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    date: &str,
    area: Rect,
) -> Option<(Rect, usize, usize)> {
    let rect = email_popup_geometry(area)?;
    let inner = rect.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.height < 8 {
        return None;
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    let status_y = footer_y.saturating_sub(1);
    let available = status_y.saturating_sub(inner.y);
    let width = usize::from(inner.width).max(1);
    let header_lines = email_preview_header_lines(from, to, cc, subject, date, width);
    // Keep a few rows for the patch even with unusually large recipient lists.
    let max_header_rows = usize::from(available.saturating_sub(4)).min(12).max(1);
    let header_rows = header_lines.len().min(max_header_rows);
    let divider_rows = usize::from(available > header_rows as u16);
    let body_rows = usize::from(available)
        .saturating_sub(header_rows)
        .saturating_sub(divider_rows)
        .max(1);
    Some((inner, header_rows, body_rows))
}

pub(super) fn patch_preview_metrics(message: &str, area: Rect) -> (usize, usize) {
    let (rect, rows) = command_output_geometry(area);
    let width = usize::from(rect.width.saturating_sub(2));
    let total = message
        .lines()
        .map(|line| hard_wrap_preserve(line, width).len())
        .sum::<usize>();
    (rows.max(1), total.max(1))
}

fn email_recipient_lines(
    recipients: &[String],
    selected: usize,
    focused: bool,
    max_rows: usize,
    show_add_hint: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if recipients.is_empty() {
        return vec![Line::from(Span::styled(
            if show_add_hint {
                "   (none — press a to add)"
            } else {
                "   (none)"
            },
            Style::default().fg(theme.text_dimmed),
        ))];
    }

    let max_rows = max_rows.max(1);
    let selected = selected.min(recipients.len().saturating_sub(1));
    let start = if recipients.len() <= max_rows {
        0
    } else {
        selected
            .saturating_sub(max_rows / 2)
            .min(recipients.len().saturating_sub(max_rows))
    };
    let end = (start + max_rows).min(recipients.len());
    let number_width = recipients.len().to_string().len().max(1);

    (start..end)
        .map(|index| {
            let is_selected = index == selected;
            let marker = if is_selected && focused { "›" } else { " " };
            let number = format!("{:>width$}", index + 1, width = number_width);
            let recipient_style = if is_selected && focused {
                Style::default()
                    .fg(theme.accent_secondary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let number_style = if is_selected && focused {
                Style::default().fg(theme.accent_secondary)
            } else {
                Style::default().fg(theme.text_dimmed)
            };
            Line::from(vec![
                Span::styled(format!("{marker} {number} "), number_style),
                Span::styled(recipients[index].clone(), recipient_style),
            ])
        })
        .collect()
}

fn email_recipient_row_allocation(to_len: usize, cc_len: usize, budget: usize) -> (usize, usize) {
    let budget = budget.max(2);
    let to_need = to_len.max(1);
    let cc_need = cc_len.max(1);

    let mut to_rows = (budget / 2).max(1);
    let mut cc_rows = budget.saturating_sub(to_rows).max(1);

    if to_need < to_rows {
        cc_rows = cc_rows.saturating_add(to_rows - to_need);
        to_rows = to_need;
    }
    if cc_need < cc_rows {
        to_rows = to_rows.saturating_add(cc_rows - cc_need);
        cc_rows = cc_need;
    }

    to_rows = to_rows.min(to_need);
    cc_rows = cc_rows.min(cc_need);

    // If both lists are longer than their first allocation, spend any leftover
    // rows on the currently under-filled side rather than leaving dead space.
    let used = to_rows.saturating_add(cc_rows);
    if used < budget {
        let mut remaining = budget - used;
        let to_extra = to_need.saturating_sub(to_rows).min(remaining);
        to_rows += to_extra;
        remaining -= to_extra;
        cc_rows += cc_need.saturating_sub(cc_rows).min(remaining);
    }

    (to_rows.max(1), cc_rows.max(1))
}

fn pull_divergence_popup_geometry(area: Rect) -> Option<Rect> {
    let width = (area.width.saturating_mul(82) / 100)
        .min(96)
        .max(56)
        .min(area.width);
    if width < 36 || area.height < 12 {
        return None;
    }
    let desired_height = 20u16.min(area.height.saturating_sub(2));
    if desired_height < 12 {
        return None;
    }
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(desired_height) / 2,
        width,
        desired_height,
    ))
}

fn email_popup_geometry(area: Rect) -> Option<Rect> {
    let width = (area.width.saturating_mul(82) / 100)
        .min(96)
        .max(48)
        .min(area.width);
    if width < 28 || area.height < 14 {
        return None;
    }
    let desired_height = (area.height.saturating_mul(82) / 100).max(20).min(34);
    let height = desired_height.min(area.height.saturating_sub(2));
    if height < 14 {
        return None;
    }
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    ))
}

pub(super) fn email_preview_max_scroll(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    date: &str,
    body: &str,
    area: Rect,
) -> usize {
    let Some((inner, _header_rows, body_rows)) =
        email_preview_body_layout(from, to, cc, subject, date, area)
    else {
        return 0;
    };
    let width = usize::from(inner.width).max(1);
    let total = body
        .lines()
        .map(|line| hard_wrap_preserve(line, width).len())
        .sum::<usize>()
        .max(1);
    total.saturating_sub(body_rows)
}

fn render_email_send_confirmation(
    frame: &mut Frame,
    parent: Rect,
    to: &[String],
    cc: &[String],
    theme: &Theme,
) {
    if parent.width < 36 || parent.height < 14 {
        return;
    }

    let width = (parent.width.saturating_mul(62) / 100)
        .max(42)
        .min(72)
        .min(parent.width.saturating_sub(4));
    let content_width = usize::from(width.saturating_sub(4)).max(1);
    let to_value = if to.is_empty() {
        "(none)".to_string()
    } else {
        to.iter()
            .map(|value| value.trim())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cc_value = if cc.is_empty() {
        "(none)".to_string()
    } else {
        cc.iter()
            .map(|value| value.trim())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut recipient_lines = Vec::new();
    for (label, value) in [("To:", to_value.as_str()), ("Cc:", cc_value.as_str())] {
        const LABEL_WIDTH: usize = 5;
        let value_width = content_width.saturating_sub(LABEL_WIDTH).max(1);
        let mut chunks = wrap_words(value, value_width);
        let truncated = chunks.len() > 3;
        chunks.truncate(3);
        if truncated {
            if let Some(last) = chunks.last_mut() {
                while UnicodeWidthStr::width(last.as_str()) + 1 > value_width && !last.is_empty() {
                    last.pop();
                }
                last.push('…');
            }
        }
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index == 0 {
                recipient_lines.push(format!("{:<width$}{}", label, chunk, width = LABEL_WIDTH));
            } else {
                recipient_lines.push(format!("{:<width$}{}", "", chunk, width = LABEL_WIDTH));
            }
        }
    }

    let desired_height = (recipient_lines.len() as u16)
        .saturating_add(7)
        .max(10)
        .min(parent.height.saturating_sub(4));
    let rect = Rect::new(
        parent.x + parent.width.saturating_sub(width) / 2,
        parent.y + parent.height.saturating_sub(desired_height) / 2,
        width,
        desired_height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .title(" Send this patch? ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent)),
        rect,
    );
    let inner = rect.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });

    let prompt_style = Style::default()
        .fg(theme.text_strong)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Confirm the final recipients before sending.",
            prompt_style,
        ))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let recipient_style = email_header_value_style(theme);
    let mut y = inner.y.saturating_add(2);
    for line in recipient_lines {
        if y >= inner.y + inner.height.saturating_sub(2) {
            break;
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(line, recipient_style))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    let send_label = " Enter Send ";
    let cancel_label = " Esc Cancel ";
    let gap = 3u16;
    let send_width = UnicodeWidthStr::width(send_label) as u16;
    let cancel_width = UnicodeWidthStr::width(cancel_label) as u16;
    let total = send_width.saturating_add(gap).saturating_add(cancel_width);
    let start_x = inner.x + inner.width.saturating_sub(total) / 2;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            send_label,
            Style::default()
                .fg(Color::Black)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect::new(start_x, footer_y, send_width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            cancel_label,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))),
        Rect::new(
            start_x.saturating_add(send_width).saturating_add(gap),
            footer_y,
            cancel_width,
            1,
        ),
    );
}

fn commit_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn commit_body_viewport_top(
    textarea: &tui_textarea::TextArea<'static>,
    requested_top: usize,
    visible_rows: usize,
) -> usize {
    let visible_rows = visible_rows.max(1);
    let line_count = textarea.lines().len().max(1);
    let max_top = line_count.saturating_sub(visible_rows);
    let cursor_row = textarea.cursor().0.min(line_count.saturating_sub(1));
    let mut top = requested_top.min(max_top);

    // Ordinary Vim motions are allowed to pull the viewport back to the cursor.
    // Neoscroll updates `requested_top` and the cursor together, so while an
    // animation is running this simply preserves the explicitly tracked view.
    if cursor_row < top {
        top = cursor_row;
    } else if cursor_row >= top.saturating_add(visible_rows) {
        top = cursor_row
            .saturating_add(1)
            .saturating_sub(visible_rows)
            .min(max_top);
    }
    top
}

/// Build a display-only textarea starting at an explicitly tracked visual row.
///
/// The source textarea remains the editor state; this clone owns no persistent
/// viewport state. That makes smooth scrolling deterministic instead of relying
/// on tui-textarea's internal cursor-following scroll offset.
fn commit_body_viewport(
    textarea: &tui_textarea::TextArea<'static>,
    top: usize,
) -> tui_textarea::TextArea<'static> {
    let lines = textarea.lines();
    let top = top.min(lines.len().saturating_sub(1));
    let (cursor_row, cursor_col) = textarea.cursor();
    let relative_cursor = (cursor_row.saturating_sub(top), cursor_col);

    let mut view = textarea.clone();
    view.set_lines(lines[top..].to_vec(), relative_cursor);

    // `set_lines` intentionally clears content-local state. Recreate a visual
    // selection in viewport-relative coordinates so explicit scrolling does not
    // break Vim characterwise/linewise highlighting.
    if let Some((start, end)) = textarea.selection_range() {
        let translate = |(row, col): (usize, usize)| {
            if row < top {
                (0usize, 0usize)
            } else {
                (row - top, col)
            }
        };
        let cursor_is_start = textarea.cursor() == start;
        let start = translate(start);
        let end = translate(end);
        view.cancel_selection();
        if cursor_is_start {
            view.move_cursor(tui_textarea::CursorMove::Jump(end.0 as u16, end.1 as u16));
            view.start_selection();
            view.move_cursor(tui_textarea::CursorMove::Jump(
                start.0 as u16,
                start.1 as u16,
            ));
        } else {
            view.move_cursor(tui_textarea::CursorMove::Jump(
                start.0 as u16,
                start.1 as u16,
            ));
            view.start_selection();
            view.move_cursor(tui_textarea::CursorMove::Jump(end.0 as u16, end.1 as u16));
        }
    }
    view
}

/// Paint a subtle Illuminate-style background behind every visible occurrence
/// of the word under the Normal-mode cursor. This is applied directly to the
/// rendered buffer so it composes with the active-line background without
/// disturbing TextArea selections or Eyeliner's temporary foreground colors.
fn highlight_commit_word_occurrences(
    frame: &mut Frame,
    textarea: &tui_textarea::TextArea<'static>,
    area: Rect,
    background: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (cursor_row, cursor_col) = textarea.cursor();
    let Some(cursor_line) = textarea.lines().get(cursor_row) else {
        return;
    };
    let chars: Vec<char> = cursor_line.chars().collect();
    if cursor_col >= chars.len() || !commit_word_char(chars[cursor_col]) {
        return;
    }

    let mut word_start = cursor_col;
    while word_start > 0 && commit_word_char(chars[word_start - 1]) {
        word_start -= 1;
    }
    let mut word_end = cursor_col + 1;
    while word_end < chars.len() && commit_word_char(chars[word_end]) {
        word_end += 1;
    }
    let word: String = chars[word_start..word_end].iter().collect();
    if word.is_empty() {
        return;
    }

    let Some(rendered_cursor) = textarea.rendered_cursor_position() else {
        return;
    };
    let viewport_top = cursor_row.saturating_sub(rendered_cursor.y.saturating_sub(area.y) as usize);

    for visible_row in 0..area.height as usize {
        let row = viewport_top + visible_row;
        let Some(line) = textarea.lines().get(row) else {
            break;
        };
        let line_chars: Vec<char> = line.chars().collect();
        let mut i = 0usize;
        while i < line_chars.len() {
            if !commit_word_char(line_chars[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < line_chars.len() && commit_word_char(line_chars[i]) {
                i += 1;
            }
            let candidate: String = line_chars[start..i].iter().collect();
            if candidate != word {
                continue;
            }

            let prefix: String = line_chars[..start].iter().collect();
            let matched: String = line_chars[start..i].iter().collect();
            let x_offset = UnicodeWidthStr::width(prefix.as_str()) as u16;
            let match_width = UnicodeWidthStr::width(matched.as_str()) as u16;
            if x_offset >= area.width || match_width == 0 {
                continue;
            }
            let width = match_width.min(area.width.saturating_sub(x_offset));
            frame.buffer_mut().set_style(
                Rect::new(area.x + x_offset, area.y + visible_row as u16, width, 1),
                Style::default().bg(background),
            );
        }
    }
}

fn compact_picker_path(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from)
        && let Ok(relative) = path.strip_prefix(&home)
    {
        if relative.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", relative.to_string_lossy());
    }
    path.to_string_lossy().into_owned()
}

fn patch_picker_visible_slice(len: usize, selected: usize, height: usize) -> (usize, usize) {
    if height == 0 || len == 0 {
        return (0, 0);
    }
    let height = height.min(len);
    let half = height / 2;
    let start = selected
        .saturating_sub(half)
        .min(len.saturating_sub(height));
    (start, start + height)
}

fn human_file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn patch_picker_entry_line(
    entry: &super::popup::PatchPickerEntry,
    width: u16,
    selected: bool,
    display_number: Option<usize>,
    number_width: usize,
    theme: &Theme,
) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let folder_icon = if entry.is_dir { "󰉋 " } else { "" };
    let marker_style = Style::default().fg(theme.accent_secondary);
    // Keep folders on the theme's primary accent, matching likely patch files.
    // This follows the active color theme instead of hard-coding terminal blue.
    let folder_style = Style::default().fg(theme.accent);
    let name_style = if entry.is_dir {
        folder_style
    } else if entry.likely_patch {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.text)
    };
    let number_prefix = display_number
        .map(|number| format!("{number:>number_width$} "))
        .unwrap_or_default();
    let number_style = if selected {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.text_dimmed)
    };
    let fixed = UnicodeWidthStr::width(number_prefix.as_str())
        + UnicodeWidthStr::width(marker)
        + UnicodeWidthStr::width(folder_icon);
    let available = usize::from(width).saturating_sub(fixed);
    let mut name = entry.name.clone();
    if UnicodeWidthStr::width(name.as_str()) > available {
        while UnicodeWidthStr::width(name.as_str()) > available.saturating_sub(1)
            && !name.is_empty()
        {
            name.pop();
        }
        if available > 0 {
            name.push('…');
        }
    }
    let mut spans = vec![
        Span::styled(number_prefix, number_style),
        Span::styled(marker.to_string(), marker_style),
    ];
    if entry.is_dir {
        spans.push(Span::styled(folder_icon.to_string(), folder_style));
    }
    spans.push(Span::styled(name, name_style));
    Line::from(spans)
}

fn render_patch_picker_entries(
    frame: &mut Frame,
    rect: Rect,
    entries: &[super::popup::PatchPickerEntry],
    selected: Option<usize>,
    show_relative_numbers: bool,
    theme: &Theme,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let selected_idx = selected.unwrap_or(0).min(entries.len().saturating_sub(1));
    let (start, end) =
        patch_picker_visible_slice(entries.len(), selected_idx, usize::from(rect.height));
    let number_width = entries.len().max(1).to_string().len();
    let mut items = Vec::new();
    for (index, entry) in entries.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index);
        let display_number = if show_relative_numbers {
            Some(if is_selected {
                index + 1
            } else {
                index.abs_diff(selected_idx)
            })
        } else {
            None
        };
        let line = patch_picker_entry_line(
            entry,
            rect.width,
            is_selected,
            display_number,
            number_width,
            theme,
        );
        let item = if is_selected {
            ListItem::new(line).style(Style::default().bg(theme.selected_bg))
        } else {
            ListItem::new(line)
        };
        items.push(item);
    }
    frame.render_widget(List::new(items), rect);
}

fn render_patch_picker_preview(
    frame: &mut Frame,
    rect: Rect,
    state: &super::popup::PatchFilePickerState,
    theme: &Theme,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    match &state.preview {
        super::popup::PatchPickerPreview::Empty => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Empty directory",
                    Style::default().fg(theme.text_dimmed),
                )),
                rect,
            );
        }
        super::popup::PatchPickerPreview::Directory(entries) => {
            render_patch_picker_entries(frame, rect, entries, None, false, theme);
        }
        super::popup::PatchPickerPreview::Binary { size } => {
            let lines = vec![
                Line::from(Span::styled(
                    "Binary file",
                    Style::default().fg(theme.text_dimmed),
                )),
                Line::from(Span::styled(
                    human_file_size(*size),
                    Style::default().fg(theme.text),
                )),
            ];
            frame.render_widget(Paragraph::new(lines), rect);
        }
        super::popup::PatchPickerPreview::Error(error) => {
            frame.render_widget(
                Paragraph::new(Span::styled(error.clone(), theme.diff_remove)),
                rect,
            );
        }
        super::popup::PatchPickerPreview::Text {
            lines,
            truncated,
            likely_patch,
        } => {
            if lines.is_empty() {
                return;
            }
            let visible = usize::from(rect.height).max(1);
            let end = visible.min(lines.len());

            let mut in_diff = false;
            let mut rendered = Vec::new();
            for source in lines.iter().take(end) {
                if source.starts_with("diff --git ") {
                    in_diff = true;
                }
                let content_style = if *likely_patch {
                    patch_line_style(source, theme, in_diff)
                } else {
                    Style::default().fg(theme.text)
                };
                rendered.push(Line::from(Span::styled(source.clone(), content_style)));
            }
            if *truncated && end == lines.len() && rendered.len() < visible {
                rendered.push(Line::from(Span::styled(
                    "… preview truncated …",
                    Style::default().fg(theme.text_dimmed),
                )));
            }
            frame.render_widget(Paragraph::new(rendered), rect);
        }
    }
}

pub(super) fn patch_file_picker_preview_height(area_height: u16) -> usize {
    let popup_height = (area_height * 78 / 100)
        .max(16)
        .min(area_height.saturating_sub(2).max(1));
    // popup border (2), inner path/spacer/footer (3), preview title row (1)
    usize::from(popup_height.saturating_sub(6).max(1))
}

fn render_patch_file_picker(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &super::popup::PatchFilePickerState,
) {
    let width = (area.width * 92 / 100)
        .max(60)
        .min(area.width.saturating_sub(2).max(1));
    let height = (area.height * 78 / 100)
        .max(16)
        .min(area.height.saturating_sub(2).max(1));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" Apply patch ")
            .border_style(Style::default().fg(theme.accent)),
        popup,
    );

    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height < 5 || inner.width < 12 {
        return;
    }

    let path_rect = Rect::new(inner.x, inner.y, inner.width, 1);
    let current_path = compact_picker_path(&state.current_dir);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 󰉋 ", Style::default().fg(theme.accent)),
            Span::styled(current_path, Style::default().fg(theme.text_strong)),
        ])),
        path_rect,
    );

    let footer_y = inner.y + inner.height.saturating_sub(1);
    let content_y = inner.y + 2;
    let content_h = footer_y.saturating_sub(content_y);
    let content = Rect::new(inner.x, content_y, inner.width, content_h);

    let left_w = (content.width * 24 / 100).max(12);
    let center_w = (content.width * 34 / 100).max(18);
    let left_w = left_w.min(content.width);
    let center_w = center_w.min(content.width.saturating_sub(left_w));
    let right_w = content.width.saturating_sub(left_w + center_w);
    let parent_rect = Rect::new(content.x, content.y, left_w, content.height);
    let current_rect = Rect::new(content.x + left_w, content.y, center_w, content.height);
    let preview_rect = Rect::new(
        content.x + left_w + center_w,
        content.y,
        right_w,
        content.height,
    );

    if parent_rect.width > 0 {
        frame.render_widget(
            Block::default()
                .title(" Parent ")
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(theme.text_dimmed)),
            parent_rect,
        );
        let list = Rect::new(
            parent_rect.x,
            parent_rect.y + 1,
            parent_rect.width.saturating_sub(1),
            parent_rect.height.saturating_sub(1),
        );
        render_patch_picker_entries(
            frame,
            list,
            &state.parent_entries,
            Some(state.parent_selected),
            false,
            theme,
        );
    }

    if current_rect.width > 0 {
        frame.render_widget(
            Block::default()
                .title(" Files ")
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(theme.text_dimmed)),
            current_rect,
        );
        let list = Rect::new(
            current_rect.x,
            current_rect.y + 1,
            current_rect.width.saturating_sub(1),
            current_rect.height.saturating_sub(1),
        );
        render_patch_picker_entries(
            frame,
            list,
            &state.entries,
            Some(state.selected),
            true,
            theme,
        );
    }

    if preview_rect.width > 0 {
        let selected = state.selected_entry();
        let title = selected
            .map(|entry| {
                if entry.is_dir {
                    format!(" {} / ", entry.name)
                } else {
                    format!(" {} · {} ", entry.name, human_file_size(entry.size))
                }
            })
            .unwrap_or_else(|| " Preview ".to_string());
        frame.render_widget(
            Block::default().title(title).borders(Borders::NONE),
            preview_rect,
        );
        let preview = Rect::new(
            preview_rect.x + 1,
            preview_rect.y + 1,
            preview_rect.width.saturating_sub(2),
            preview_rect.height.saturating_sub(1),
        );
        render_patch_picker_preview(frame, preview, state, theme);
    }

    let key = |text: &str| {
        Span::styled(
            text.to_string(),
            Style::default().fg(theme.accent_secondary),
        )
    };
    let dim = |text: &str| Span::styled(text.to_string(), Style::default().fg(theme.text_dimmed));
    let footer = Line::from(vec![
        key(" j/k"),
        dim(" navigate  "),
        key("20j/20k"),
        dim(" jump  "),
        key("g/G"),
        dim(" top/bottom  "),
        key("c-j/c-k"),
        dim(" smooth move  "),
        key("h/l"),
        dim(" parent/open  "),
        key("enter"),
        dim(" select  "),
        key("r"),
        dim(" repo  "),
        key("~"),
        dim(" home  "),
        key("d"),
        dim(" downloads  "),
        key("q"),
        dim(" close"),
    ]);
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

use super::popup::ListPickerCore;

/// Shared rendering for searchable list picker popups (RefPicker, ThemePicker, etc.).
fn render_list_picker(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    core: &ListPickerCore,
    title: &str,
    width_pct: u16,
    width_max: u16,
    width_min: u16,
    hints: &[(&str, &str)],
) {
    let search = core.search_textarea.lines().join("");
    let search_lower = search.trim().to_lowercase();
    let matching = list_picker_matching_indices(&core.items, &search);

    // Build display rows from matching items only (search filters the list).
    let has_categories = matching
        .iter()
        .any(|&i| core.items.get(i).is_some_and(|it| !it.category.is_empty()));
    // (is_header, label, item_idx, description)
    let mut display: Vec<(bool, String, Option<usize>, Option<String>)> = Vec::new();
    if has_categories {
        let mut last_cat = String::new();
        for &ei in &matching {
            let Some(item) = core.items.get(ei) else {
                continue;
            };
            if !item.category.is_empty() && item.category != last_cat {
                display.push((true, item.category.clone(), None, None));
                last_cat = item.category.clone();
            }
            display.push((
                false,
                item.label.clone(),
                Some(ei),
                item.description.clone(),
            ));
        }
    } else {
        for &ei in &matching {
            let Some(item) = core.items.get(ei) else {
                continue;
            };
            display.push((
                false,
                item.label.clone(),
                Some(ei),
                item.description.clone(),
            ));
        }
    }

    // Popup frame
    let popup_width = (area.width * width_pct / 100)
        .min(width_max)
        .max(width_min)
        .min(area.width);
    let max_popup = (area.height * 60 / 100).max(10);
    let popup_height = max_popup.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_rect = Rect::new(x, y, popup_width, popup_height);
    frame.render_widget(Clear, popup_rect);

    let block = Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent));
    frame.render_widget(block, popup_rect);

    let inner = popup_rect.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.height < 3 {
        return;
    }

    // Search bar
    let prefix_width = 2u16;
    let prefix_rect = Rect::new(inner.x, inner.y, prefix_width, 1);
    let prefix_style = if search.is_empty() {
        Style::default().fg(theme.text_dimmed)
    } else {
        Style::default().fg(theme.accent_secondary)
    };
    frame.render_widget(
        Paragraph::new(Span::styled("  ", prefix_style)),
        prefix_rect,
    );
    let ta_width = inner.width.saturating_sub(prefix_width);
    let ta_rect = Rect::new(inner.x + prefix_width, inner.y, ta_width, 1);
    frame.render_widget(&core.search_textarea, ta_rect);

    // Separator
    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    let sep = "─".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(sep, Style::default().fg(theme.text_dimmed))),
        sep_area,
    );

    // Content area
    let list_start = inner.y + 2;
    let list_height = inner.height.saturating_sub(3) as usize; // search + sep + hint
    let list_area = Rect::new(inner.x, list_start, inner.width, list_height as u16);

    let max_scroll = display.len().saturating_sub(list_height);
    let effective_scroll = core.scroll_offset.min(max_scroll);

    let visible_display: Vec<&(bool, String, Option<usize>, Option<String>)> = display
        .iter()
        .skip(effective_scroll)
        .take(list_height)
        .collect();

    let content_w = list_area.width as usize;
    let mut list_items: Vec<ListItem> = Vec::new();
    for (is_header, label, item_idx, description) in visible_display {
        if *is_header {
            let line = Line::from(vec![Span::styled(
                format!(" {} ", label),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )]);
            list_items.push(ListItem::new(line));
        } else {
            let is_selected = *item_idx == Some(core.selected);

            let base_fg = if is_selected {
                theme.text_strong
            } else {
                theme.text
            };
            let highlight_fg = theme.accent_secondary;

            // ▸ marker for selected item
            let marker = if is_selected { "▸ " } else { "  " };
            let mut spans = vec![Span::styled(
                marker,
                Style::default().fg(theme.accent_secondary),
            )];

            // Build label spans with search match highlighting.
            // Multi-word queries highlight every token in either order,
            // mirroring `list_picker_matching_indices` (token-AND).
            if !search_lower.is_empty() {
                let tokens = super::popup::list_picker_search_tokens(&search);
                let ranges = super::popup::list_picker_highlight_ranges(label, &tokens);
                if ranges.is_empty() {
                    spans.push(Span::styled(label.clone(), Style::default().fg(base_fg)));
                } else {
                    let match_style = Style::default()
                        .fg(highlight_fg)
                        .add_modifier(Modifier::BOLD);
                    let base_style = Style::default().fg(base_fg);
                    let mut cursor = 0usize;
                    for (s, e) in ranges {
                        if s > cursor {
                            if let Some(chunk) = label.get(cursor..s) {
                                if !chunk.is_empty() {
                                    spans.push(Span::styled(chunk.to_string(), base_style));
                                }
                            }
                        }
                        if let Some(chunk) = label.get(s..e) {
                            spans.push(Span::styled(chunk.to_string(), match_style));
                        }
                        cursor = e;
                    }
                    if let Some(rest) = label.get(cursor..) {
                        if !rest.is_empty() {
                            spans.push(Span::styled(rest.to_string(), base_style));
                        }
                    }
                }
            } else {
                let style = if is_selected {
                    Style::default().fg(base_fg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(base_fg)
                };
                spans.push(Span::styled(label.clone(), style));
            }

            if let Some(desc) = description.as_deref().filter(|d| !d.is_empty()) {
                let used: usize = spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                let desc_w = UnicodeWidthStr::width(desc);
                let pad = content_w.saturating_sub(used).saturating_sub(desc_w);
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                spans.push(Span::styled(
                    desc.to_string(),
                    Style::default().fg(theme.text_dimmed),
                ));
            }

            let line = Line::from(spans);

            if is_selected {
                list_items.push(ListItem::new(line).style(Style::default().bg(theme.selected_bg)));
            } else {
                list_items.push(ListItem::new(line));
            }
        }
    }

    let list = List::new(list_items);
    frame.render_widget(list, list_area);

    // Hint bar
    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    let mut hint_spans = Vec::new();
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i == 0 {
            hint_spans.push(Span::styled(
                format!(" {}", key),
                Style::default().fg(theme.accent_secondary),
            ));
        } else {
            hint_spans.push(Span::styled(
                key.to_string(),
                Style::default().fg(theme.accent_secondary),
            ));
        }
        hint_spans.push(Span::styled(
            format!(": {}  ", desc),
            Style::default().fg(theme.text_dimmed),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(hint_spans)), hint_area);
}

/// Resolve the commit to display in the details panel based on the active
/// context.  Returns `None` when the context isn't commit-listing or nothing
/// is selected.  For `CommitFiles`/`BranchCommitFiles`/`StashFiles` we look
/// up the commit by the drilled-in hash.
fn resolve_current_commit<'a>(
    model: &'a Model,
    ctx_mgr: &ContextManager,
    commit_files_hash: &str,
) -> Option<&'a Commit> {
    let ctx = ctx_mgr.active();
    let sel = ctx_mgr.selected(ctx);
    match ctx {
        ContextId::Commits => model.commits.get(sel),
        ContextId::BranchCommits => model.sub_commits.get(sel),
        ContextId::Reflog => model.reflog_commits.get(sel),
        ContextId::CommitFiles | ContextId::BranchCommitFiles | ContextId::StashFiles => {
            if commit_files_hash.is_empty() {
                return None;
            }
            find_commit_by_hash(model, commit_files_hash)
        }
        _ => None,
    }
}

fn find_commit_by_hash<'a>(model: &'a Model, hash: &str) -> Option<&'a Commit> {
    model
        .commits
        .iter()
        .find(|c| c.hash == hash)
        .or_else(|| model.sub_commits.iter().find(|c| c.hash == hash))
        .or_else(|| model.reflog_commits.iter().find(|c| c.hash == hash))
}

fn render_commit_details_panel(
    frame: &mut Frame,
    rect: Rect,
    commit: &Commit,
    commit_stats: &Arc<Mutex<HashMap<String, CommitStat>>>,
    commit_messages: &Arc<Mutex<HashMap<String, String>>>,
    theme: &Theme,
    compact: bool,
    scroll: &mut u16,
) {
    let stat_owned = commit_stats
        .lock()
        .ok()
        .and_then(|map| map.get(&commit.hash).copied());
    let message_owned = commit_messages
        .lock()
        .ok()
        .and_then(|map| map.get(&commit.hash).cloned());
    presentation::commit_details::render_commit_details(
        frame,
        rect,
        commit,
        stat_owned.as_ref(),
        message_owned.as_deref(),
        theme,
        compact,
        scroll,
    );
}
