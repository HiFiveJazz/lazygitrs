use super::matches_key;
use crate::config::KeybindingConfig;
use crate::gui::popup::{
    CommitInputFocus, CommitInputKind, CommitInputMode, MenuItem, PopupState,
    make_commit_body_textarea, make_commit_summary_textarea, make_textarea,
    reset_commit_input_view,
};
use crate::gui::{Gui, RemoteOpKind};
use crate::os::platform::Platform;
use crate::pager::side_by_side::DiffPanel;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    if super::diff_grep::is_diff_grep_key(key) {
        return super::diff_grep::open_diff_grep_picker(gui);
    }
    if matches_key(key, &keybindings.commits.open_log_menu) {
        return super::commits::show_files_filtering_menu(gui);
    }

    // Folding is explicit; Enter focuses file or combined directory diffs.
    if gui.show_file_tree && super::tree::matches_key(key, &keybindings.universal.fold_directory) {
        if let Some(node) = gui.file_tree_nodes.get(gui.context_mgr.selected_active()) {
            if node.is_dir {
                let path = node.path.clone();
                if !gui.collapsed_dirs.remove(&path) {
                    gui.collapsed_dirs.insert(path);
                }
                gui.update_file_tree_state();
                gui.context_mgr.viewport_manually_scrolled = false;
                gui.needs_diff_refresh = true;
            }
        }
        return Ok(());
    }
    if key.code == KeyCode::Enter {
        if !gui.diff_view.is_empty() {
            gui.diff_focused = true;
        }
        return Ok(());
    }

    // Both v and V select whole file rows. Visual is a temporary overlay:
    // entering or exiting it never changes marks or the Git index.
    if matches!(key.code, KeyCode::Char('v' | 'V')) {
        if gui.file_visual_anchor.is_some() {
            gui.file_visual_anchor = None;
            gui.show_action_toast("Normal");
        } else {
            gui.file_visual_anchor = Some(gui.context_mgr.selected_active());
            gui.show_action_toast("Visual");
        }
        return Ok(());
    }
    if key.code == KeyCode::Esc {
        if gui.file_visual_anchor.is_some() {
            // Return to the underlying state of every selected file. Purple
            // selection never changes green staged files or orange marks.
            gui.file_visual_anchor = None;
            gui.show_action_toast("Normal");
        } else if cursor_is_appended(gui) {
            // A green row represents the entire appended/staged group, not a
            // hidden subset of orange marks. Esc moves that group to Marked.
            let files = staged_action_files(gui);
            promote_files_to_marks(gui, &files);
            gui.show_action_toast(format!("{} files marked", files.len()));
        } else if !gui.marked_file_paths.is_empty() {
            gui.marked_file_paths.clear();
            gui.marked_file_discarding.clear();
            gui.marked_file_committing.clear();
            gui.show_action_toast("Marks cleared");
        }
        return Ok(());
    }
    if key.code == KeyCode::Char(' ') {
        return toggle_file_marks(gui);
    }
    // `x` toggles the actual state of the cursor file (or Visual range),
    // not the saved orange mark group. a remains the bulk staging command.
    if key.code == KeyCode::Char('x') {
        return toggle_stage(gui);
    }

    if matches_key(key, &keybindings.files.commit_changes) {
        return open_commit_prompt(gui);
    }
    if matches_key(key, &keybindings.files.generate_ai_commit) {
        return open_ai_commit_prompt(gui);
    }

    if matches_key(key, &keybindings.files.toggle_staged_all) {
        // Priority: live Visual > cursor's appended group > saved marks >
        // no selection (stage all). The current row can therefore undo a
        // stage-all operation with a single `a`.
        if gui.file_visual_anchor.is_some() {
            return toggle_stage_marked(gui);
        }
        if cursor_is_appended(gui) {
            let staged = staged_action_files(gui);
            promote_files_to_marks(gui, &staged);
            return Ok(());
        }
        if !gui.marked_file_paths.is_empty() {
            return toggle_stage_marked(gui);
        }
        return stage_all_files(gui);
    }

    if matches_key(key, &keybindings.files.stash_all_changes) {
        return stash_changes(gui);
    }

    if matches_key(key, &keybindings.files.view_stash_options) {
        return open_stash_options(gui);
    }

    if key.code == KeyCode::Char('d') {
        return discard_file(gui);
    }

    if matches_key(key, &keybindings.files.discard_all_unstaged) {
        return discard_all_unstaged(gui);
    }

    if matches_key(key, &keybindings.files.ignore_file) {
        return ignore_file(gui);
    }

    // Amend last commit
    if matches_key(key, &keybindings.files.amend_last_commit) {
        return amend_commit(gui);
    }

    // Toggle file tree view
    if matches_key(key, &keybindings.files.toggle_tree_view) {
        gui.file_visual_anchor = None;
        gui.show_file_tree = !gui.show_file_tree;
        gui.show_commit_file_tree = gui.show_file_tree;
        gui.update_file_tree_state();
        gui.persist_file_tree_visibility();
        // Reset selection when toggling view modes
        gui.context_mgr.set_selection(0);
        return Ok(());
    }

    // Open file in editor
    if matches_key(key, &keybindings.universal.edit) {
        return open_in_editor(gui);
    }

    // Open file in default program
    if matches_key(key, &keybindings.universal.open_file) {
        return open_in_default_program(gui);
    }

    // Copy to clipboard
    if key.code == KeyCode::Char('y') {
        return copy_to_clipboard_menu(gui);
    }

    // Fetch
    if matches_key(key, &keybindings.files.fetch) {
        gui.start_remote_op(
            RemoteOpKind::Fetch,
            "Fetch",
            "Fetching from all remotes...",
            |git| {
                git.fetch_all()?;
                Ok(())
            },
        );
        return Ok(());
    }

    Ok(())
}

/// Resolve the Visual Line range to file indices. A selected collapsed
/// directory contributes its descendants, as it does for ordinary Space.
/// Visual mode is only an overlay; exiting it does not persist new marks.
fn visual_file_indices(gui: &Gui) -> Vec<usize> {
    let Some(anchor) = gui.file_visual_anchor else { return Vec::new(); };
    let current = gui.context_mgr.selected_active();
    let (lo, hi) = (anchor.min(current), anchor.max(current));
    if gui.show_file_tree {
        let mut indices = std::collections::HashSet::new();
        for node in gui.file_tree_nodes.get(lo..=hi).unwrap_or(&[]) {
            if node.is_dir {
                indices.extend(node.child_file_indices.iter().copied());
            } else if let Some(index) = node.file_index {
                indices.insert(index);
            }
        }
        let mut indices: Vec<usize> = indices.into_iter().collect();
        indices.sort_unstable();
        indices
    } else {
        let len = gui.model.lock().unwrap().files.len();
        (lo..=hi).filter(|&i| i < len).collect()
    }
}

/// The live Visual range is the primary selection. Persistent orange marks
/// are used only when Visual is inactive. Appended (green) files cannot also
/// belong to the persistent mark set.
fn visual_action_files(gui: &Gui) -> Vec<crate::model::File> {
    let indices: std::collections::HashSet<usize> =
        visual_file_indices(gui).into_iter().collect();
    let model = gui.model.lock().unwrap();
    model.files.iter().enumerate().filter_map(|(i, f)| {
        indices.contains(&i).then(|| f.clone())
    }).collect()
}

fn marked_action_files(gui: &Gui) -> Vec<crate::model::File> {
    if gui.file_visual_anchor.is_some() {
        return visual_action_files(gui);
    }
    let model = gui.model.lock().unwrap();
    model.files.iter().filter(|file| {
        gui.marked_file_paths.contains(file.current_path()) && !file.has_staged_changes
    }).cloned().collect()
}

fn staged_action_files(gui: &Gui) -> Vec<crate::model::File> {
    let model = gui.model.lock().unwrap();
    model.files.iter().filter(|file| file.has_staged_changes).cloned().collect()
}

fn cursor_is_appended(gui: &Gui) -> bool {
    let indices = selected_row_indices(gui);
    let model = gui.model.lock().unwrap();
    indices.iter().any(|&i| model.files.get(i).is_some_and(|f| f.has_staged_changes))
}

/// Update the index and the selection state together. No file can be orange
/// and green at the same time: staging removes its orange mark, unstaging
/// promotes it to orange. The serialized worker confirms the Git operation.
fn transition_file_states(gui: &mut Gui, files: &[crate::model::File], stage: bool) {
    let mut paths = Vec::new();
    let mut changed = std::collections::HashSet::new();
    for file in files {
        if stage && (file.has_unstaged_changes || !file.tracked) {
            paths.push(file.git_add_path().to_owned());
            changed.insert(file.current_path().to_owned());
        } else if !stage && file.has_staged_changes {
            paths.extend(file.git_reset_paths().into_iter().map(str::to_owned));
            changed.insert(file.current_path().to_owned());
        }
    }
    if paths.is_empty() {
        return;
    }
    {
        let mut model = gui.model.lock().unwrap();
        for file in &mut model.files {
            if changed.contains(file.current_path()) {
                if stage {
                    let _ = file.optimistic_stage();
                } else if !file.optimistic_unstage() {
                    // Complex index statuses (e.g. partially staged renames)
                    // are reconciled by the worker's authoritative refresh.
                    file.has_staged_changes = false;
                    file.has_unstaged_changes = true;
                }
            }
        }
    }
    if stage {
        for path in &changed {
            gui.marked_file_paths.remove(path);
        }
    } else {
        gui.marked_file_paths.extend(changed);
    }
    gui.rebuild_file_tree_from_model();
    gui.enqueue_stage_then_refresh(paths, stage);
}

/// Accept a selection as persistent orange marks. If a selected file was
/// previously appended, Esc/Space also unstage it so the two states are
/// actually exclusive in Git and not only in the renderer.
fn promote_files_to_marks(gui: &mut Gui, files: &[crate::model::File]) {
    transition_file_states(gui, files, false);
    for file in files {
        if !file.has_staged_changes {
            gui.marked_file_paths.insert(file.current_path().to_owned());
        }
    }
}

/// Accept Visual files without changing the Git index (used for commit and
/// discard actions, where an unstage operation would be inappropriate).
fn save_visual_selection_as_marks(gui: &mut Gui) -> usize {
    if gui.file_visual_anchor.is_none() {
        return 0;
    }
    let files = visual_action_files(gui);
    for file in &files {
        if !file.has_staged_changes {
            gui.marked_file_paths.insert(file.current_path().to_owned());
        }
    }
    gui.file_visual_anchor = None;
    files.len()
}

fn selected_row_indices(gui: &Gui) -> Vec<usize> {
    let selected = gui.context_mgr.selected_active();
    if gui.show_file_tree {
        gui.file_tree_nodes.get(selected).map_or_else(Vec::new, |node| {
            if node.is_dir {
                node.child_file_indices.clone()
            } else {
                node.file_index.into_iter().collect()
            }
        })
    } else {
        let len = gui.model.lock().unwrap().files.len();
        if selected < len {
            vec![selected]
        } else {
            Vec::new()
        }
    }
}

fn toggle_file_marks(gui: &mut Gui) -> Result<()> {
    let visual = gui.file_visual_anchor.is_some();
    let indices = if visual { visual_file_indices(gui) } else { selected_row_indices(gui) };
    let files = {
        let model = gui.model.lock().unwrap();
        indices.iter().filter_map(|&idx| model.files.get(idx).cloned()).collect::<Vec<_>>()
    };
    if files.is_empty() {
        return Ok(());
    }
    // Space on a green bar first removes the file from the Git index. This
    // prevents an invisible marked+appended overlap even during fast repeats.
    // When every target is orange, Space removes all its marks, even during
    // Visual mode. Otherwise Space marks the entire range (unstaging green).
    let all_marked = files.iter().all(|file| {
        !file.has_staged_changes && gui.marked_file_paths.contains(file.current_path())
    });
    if all_marked {
        for file in &files {
            gui.marked_file_paths.remove(file.current_path());
        }
    } else {
        promote_files_to_marks(gui, &files);
    }
    gui.file_visual_anchor = None;
    // Visual Space applies to the selected range, not to a cursor and next
    // row. Leave the cursor in place when accepting a multi-row selection.
    if visual {
        return Ok(());
    }
    if gui.show_file_tree {
        // Skip directories and every colored (marked or appended) file.
        let selected = gui.context_mgr.selected_active();
        let model = gui.model.lock().unwrap();
        let next = next_file_row_after_toggle(selected, gui.file_tree_nodes.len(), |idx| {
            gui.file_tree_nodes.get(idx)
                .and_then(|node| node.file_index)
                .and_then(|file_idx| model.files.get(file_idx))
                .is_some_and(|file| !file.has_staged_changes
                    && !gui.marked_file_paths.contains(file.current_path()))
        });
        drop(model);
        gui.context_mgr.set_selection(next);
    } else {
        let model = gui.model.lock().unwrap();
        gui.context_mgr.move_selection(1, &model);
    }
    Ok(())
}

/// Batch staging always transitions selected paths between orange and green.
/// When all selected files are staged, the group is unstaged and marked;
/// otherwise, any remaining unstaged files are staged and their marks cleared.
fn toggle_stage_marked(gui: &mut Gui) -> Result<()> {
    let files = marked_action_files(gui);
    if files.is_empty() {
        return toggle_stage(gui);
    }
    let stage = files.iter().any(|file| file.has_unstaged_changes || !file.tracked);
    transition_file_states(gui, &files, stage);
    gui.file_visual_anchor = None;
    Ok(())
}

fn discard_marked_files(gui: &mut Gui, files: Vec<crate::model::File>) {
    let count = files.len();
    save_visual_selection_as_marks(gui);
    gui.marked_file_discarding.extend(files.iter().map(|file| file.current_path().to_owned()));
    gui.marked_file_committing.clear();
    let execute = move |gui: &mut Gui| {
        gui.file_visual_anchor = None;
        gui.start_background_mutation("Discard selected changes", move |git| {
            git.discard_files(&files)
        });
        Ok(())
    };
    if gui.config.user_config.gui.skip_discard_change_warning {
        let _ = execute(gui);
    } else {
        gui.popup = PopupState::Confirm {
            title: format!("Discard {count} selected files?"),
            message: format!("Permanently discard changes in {count} selected files, including staged changes and untracked files?\n\nOther files will be preserved."),
            on_confirm: Box::new(execute),
        };
    }
}

fn open_marked_commit_prompt(gui: &mut Gui, files: Vec<crate::model::File>) {
    let count = files.len();
    save_visual_selection_as_marks(gui);
    gui.popup = PopupState::CommitInput {
        kind: CommitInputKind::Commit,
        summary_textarea: make_commit_summary_textarea(),
        body_textarea: make_commit_body_textarea(),
        body_state: crate::gui::popup::BodySoftWrap::new(),
        focus: CommitInputFocus::Summary,
        mode: CommitInputMode::Insert,
        pending_command: Default::default(),
        visual_line_anchor: None,
        undo_stack: Vec::new(),
        show_help: false,
        body_scroll_remaining: 0,
        body_scroll_top: 0,
        on_confirm: Box::new(move |gui, message| {
            if !message.is_empty() {
                let message = message.to_string();
                for file in &files {
                    gui.marked_file_paths.remove(file.current_path());
                    gui.marked_file_committing.insert(file.current_path().to_owned());
                }
                gui.start_remote_op(
                    RemoteOpKind::LocalAppend,
                    "Commit selected",
                    &format!("Committing {count} marked files..."),
                    move |git| git.create_commit_for_files(&message, &files),
                );
            }
            Ok(())
        }),
    };
}

fn next_file_row_after_toggle<F>(selected: usize, len: usize, is_file: F) -> usize
where
    F: Fn(usize) -> bool,
{
    if len <= 1 {
        return selected.min(len.saturating_sub(1));
    }

    for idx in selected.saturating_add(1)..len {
        if is_file(idx) {
            return idx;
        }
    }

    selected.min(len - 1)
}

fn advance_selection_after_file_toggle(gui: &mut Gui, selected: usize) -> bool {
    let next = if gui.show_file_tree {
        next_file_row_after_toggle(selected, gui.file_tree_nodes.len(), |idx| {
            gui.file_tree_nodes
                .get(idx)
                .is_some_and(|node| node.file_index.is_some())
        })
    } else {
        let len = gui.model.lock().unwrap().files.len();
        next_file_row_after_toggle(selected, len, |_| true)
    };

    if next == selected {
        return false;
    }

    gui.context_mgr.set_selection(next);
    true
}

fn toggle_stage(gui: &mut Gui) -> Result<()> {
    let visual = gui.file_visual_anchor.is_some();
    let selected = gui.context_mgr.selected_active();
    let indices = if visual {
        visual_file_indices(gui)
    } else {
        selected_row_indices(gui)
    };
    let files = {
        let model = gui.model.lock().unwrap();
        indices.iter().filter_map(|&idx| model.files.get(idx).cloned()).collect::<Vec<_>>()
    };
    if files.is_empty() {
        return Ok(());
    }

    // x is a state-specific toggle, not a generic batch stage/unstage:
    //   unmarked -> appended (Git add)
    //   appended -> unmarked (Git reset, no orange mark)
    //   marked   -> unmarked (clear mark, leave Git alone)
    // Each file is treated independently, including a Visual range or a
    // directory with a mix of states.
    let mut stage = Vec::new();
    let mut unstage = Vec::new();
    for file in &files {
        if file.has_staged_changes {
            unstage.push(file.clone());
        } else if !gui.marked_file_paths.contains(file.current_path()) {
            stage.push(file.clone());
        }
    }
    transition_file_states(gui, &stage, true);
    transition_file_states(gui, &unstage, false);
    // transition_file_states ordinarily converts unstaged files to orange
    // marks for the bulk `a` action. `x` must end with *no* orange marks on
    // every file it touches, including files that were already marked.
    for file in &files {
        gui.marked_file_paths.remove(file.current_path());
    }
    gui.file_visual_anchor = None;

    // Keep the existing x navigation for a single file, but not for a
    // directory or a Visual range, which represent multiple file targets.
    let is_dir = gui.show_file_tree
        && gui.file_tree_nodes.get(selected).is_some_and(|node| node.is_dir);
    if !visual && !is_dir && advance_selection_after_file_toggle(gui, selected) {
        gui.needs_diff_refresh = true;
    }
    Ok(())
}

/// With no explicit selection, `a` *always* stages all remaining changes.
/// Unstaging is an explicit operation on saved marks or a Visual range.
fn stage_all_files(gui: &mut Gui) -> Result<()> {
    let mut model = gui.model.lock().unwrap();
    let mut changed = false;
    for file in &mut model.files {
        if file.has_unstaged_changes || !file.tracked {
            let _ = file.optimistic_stage();
            changed = true;
        }
    }
    drop(model);
    if changed {
        // A global append leaves only green bars, not hidden orange marks.
        gui.marked_file_paths.clear();
        gui.rebuild_file_tree_from_model();
        gui.enqueue_stage_all_then_refresh(true);
    }
    Ok(())
}

fn open_commit_prompt(gui: &mut Gui) -> Result<()> {
    if gui.ai_commit_generation_active() {
        return Ok(());
    }

    let head_is_detached = gui.model.lock().unwrap().head_branch_name.is_empty();
    if head_is_detached {
        gui.popup = PopupState::Confirm {
            title: "Detached HEAD".to_string(),
            message: "You are in a detached HEAD, not a branch. Are you sure you want to commit?"
                .to_string(),
            on_confirm: Box::new(open_commit_prompt_after_detached_head_warning),
        };
        return Ok(());
    }

    open_commit_prompt_after_detached_head_warning(gui)
}

fn open_commit_prompt_after_detached_head_warning(gui: &mut Gui) -> Result<()> {
    let files = marked_action_files(gui);
    if !files.is_empty() {
        open_marked_commit_prompt(gui, files);
        return Ok(());
    }
    let model = gui.model.lock().unwrap();
    let any_staged = model.files.iter().any(|f| f.has_staged_changes);
    let no_files = model.files.is_empty();
    drop(model);

    if no_files {
        gui.popup = PopupState::Confirm {
            title: "No files".to_string(),
            message: "No files to stage. Create an empty commit?".to_string(),
            on_confirm: Box::new(|gui| {
                if let Some(mut saved) = gui.saved_commit_popup.take() {
                    reset_commit_input_view(&mut saved, gui.commit_body_wrap_width());
                    gui.popup = saved;
                } else {
                    gui.popup = PopupState::CommitInput {
                        kind: CommitInputKind::Commit,
                        summary_textarea: make_commit_summary_textarea(),
                        body_textarea: make_commit_body_textarea(),
                        body_state: crate::gui::popup::BodySoftWrap::new(),
                        focus: CommitInputFocus::Summary,
                        mode: CommitInputMode::Insert,
                        pending_command: Default::default(),
                        visual_line_anchor: None,
                        undo_stack: Vec::new(),
                        show_help: false,
                        body_scroll_remaining: 0,
                        body_scroll_top: 0,
                        on_confirm: Box::new(|gui, message| {
                            if !message.is_empty() {
                                let message = message.to_string();
                                gui.start_remote_op(
                                    RemoteOpKind::LocalAppend,
                                    "Empty commit",
                                    "Creating empty commit...",
                                    move |git| {
                                        git.create_empty_commit(&message)?;
                                        Ok(())
                                    },
                                );
                            }
                            Ok(())
                        }),
                    };
                }
                Ok(())
            }),
        };
        return Ok(());
    }

    if !any_staged {
        // No files staged — ask to commit all, like lazygit
        gui.popup = PopupState::Confirm {
            title: "No files staged".to_string(),
            message: "You have not staged any files. Commit all files?".to_string(),
            on_confirm: Box::new(|gui| {
                gui.git.stage_all()?;
                if let Some(mut saved) = gui.saved_commit_popup.take() {
                    reset_commit_input_view(&mut saved, gui.commit_body_wrap_width());
                    gui.popup = saved;
                } else {
                    gui.popup = PopupState::CommitInput {
                        kind: CommitInputKind::Commit,
                        summary_textarea: make_commit_summary_textarea(),
                        body_textarea: make_commit_body_textarea(),
                        body_state: crate::gui::popup::BodySoftWrap::new(),
                        focus: CommitInputFocus::Summary,
                        mode: CommitInputMode::Insert,
                        pending_command: Default::default(),
                        visual_line_anchor: None,
                        undo_stack: Vec::new(),
                        show_help: false,
                        body_scroll_remaining: 0,
                        body_scroll_top: 0,
                        on_confirm: Box::new(|gui, message| {
                            if !message.is_empty() {
                                let message = message.to_string();
                                gui.start_remote_op(
                                    RemoteOpKind::LocalAppend,
                                    "Commit",
                                    "Creating commit...",
                                    move |git| {
                                        git.create_commit(&message, false)?;
                                        Ok(())
                                    },
                                );
                            }
                            Ok(())
                        }),
                    };
                }
                Ok(())
            }),
        };
        return Ok(());
    }

    if let Some(mut saved) = gui.saved_commit_popup.take() {
        reset_commit_input_view(&mut saved, gui.commit_body_wrap_width());
        gui.popup = saved;
        return Ok(());
    }

    gui.popup = PopupState::CommitInput {
        kind: CommitInputKind::Commit,
        summary_textarea: make_commit_summary_textarea(),
        body_textarea: make_commit_body_textarea(),
        body_state: crate::gui::popup::BodySoftWrap::new(),
        focus: CommitInputFocus::Summary,
        mode: CommitInputMode::Insert,
        pending_command: Default::default(),
        visual_line_anchor: None,
        undo_stack: Vec::new(),
        show_help: false,
        body_scroll_remaining: 0,
        body_scroll_top: 0,
        on_confirm: Box::new(|gui, message| {
            if !message.is_empty() {
                let message = message.to_string();
                gui.start_remote_op(
                    RemoteOpKind::LocalAppend,
                    "Commit",
                    "Creating commit...",
                    move |git| {
                        git.create_commit(&message, false)?;
                        Ok(())
                    },
                );
            }
            Ok(())
        }),
    };
    Ok(())
}

fn open_ai_commit_prompt(gui: &mut Gui) -> Result<()> {
    if gui.ai_commit_generation_active() {
        return Ok(());
    }

    let model = gui.model.lock().unwrap();
    let any_staged = model.files.iter().any(|f| f.has_staged_changes);
    let no_files = model.files.is_empty();
    drop(model);

    if no_files {
        gui.popup = PopupState::Message {
            title: "No files".to_string(),
            message: "Nothing to diff — AI commit needs file changes.".to_string(),
            kind: crate::gui::popup::MessageKind::Error,
        };
        return Ok(());
    }

    if !any_staged {
        gui.popup = PopupState::Confirm {
            title: "No files staged".to_string(),
            message: "You have not staged any files. Stage all and generate AI commit message?"
                .to_string(),
            on_confirm: Box::new(|gui| {
                gui.git.stage_all()?;
                gui.popup = PopupState::CommitInput {
                    kind: CommitInputKind::Commit,
                    summary_textarea: make_commit_summary_textarea(),
                    body_textarea: make_commit_body_textarea(),
                    body_state: crate::gui::popup::BodySoftWrap::new(),
                    focus: CommitInputFocus::Summary,
                    mode: CommitInputMode::Insert,
                    pending_command: Default::default(),
                    visual_line_anchor: None,
                    undo_stack: Vec::new(),
                    show_help: false,
                    body_scroll_remaining: 0,
                    body_scroll_top: 0,
                    on_confirm: Box::new(|gui, message| {
                        if !message.is_empty() {
                            let message = message.to_string();
                            gui.start_remote_op(
                                RemoteOpKind::LocalAppend,
                                "Commit",
                                "Creating commit...",
                                move |git| {
                                    git.create_commit(&message, false)?;
                                    Ok(())
                                },
                            );
                        }
                        Ok(())
                    }),
                };
                gui.trigger_ai_commit_generation_from_editor();
                Ok(())
            }),
        };
        return Ok(());
    }

    gui.popup = PopupState::CommitInput {
        kind: CommitInputKind::Commit,
        summary_textarea: make_commit_summary_textarea(),
        body_textarea: make_commit_body_textarea(),
        body_state: crate::gui::popup::BodySoftWrap::new(),
        focus: CommitInputFocus::Summary,
        mode: CommitInputMode::Insert,
        pending_command: Default::default(),
        visual_line_anchor: None,
        undo_stack: Vec::new(),
        show_help: false,
        body_scroll_remaining: 0,
        body_scroll_top: 0,
        on_confirm: Box::new(|gui, message| {
            if !message.is_empty() {
                let message = message.to_string();
                gui.start_remote_op(
                    RemoteOpKind::LocalAppend,
                    "Commit",
                    "Creating commit...",
                    move |git| {
                        git.create_commit(&message, false)?;
                        Ok(())
                    },
                );
            }
            Ok(())
        }),
    };
    gui.trigger_ai_commit_generation_from_editor();
    Ok(())
}

fn copy_to_clipboard_menu(gui: &mut Gui) -> Result<()> {
    let Some(file_idx) = gui.selected_file_index() else {
        return Ok(());
    };
    let model = gui.model.lock().unwrap();
    let Some(file) = model.files.get(file_idx) else {
        return Ok(());
    };
    let file_name = file.display_name.clone();
    let rel_path = file.name.clone();
    let current_path = file.current_path().to_string();
    let diff_paths: Vec<String> = file.diff_paths().into_iter().map(str::to_string).collect();
    let old_path = file
        .rename_paths()
        .map_or_else(|| file.name.clone(), |(old, _)| old.to_string());
    let is_added = file.added;
    let is_deleted = file.deleted;
    drop(model);

    let abs_path = gui
        .git
        .repo_path()
        .join(&current_path)
        .to_string_lossy()
        .to_string();
    let file_name_copy = file_name.clone();
    let rel_path_copy = rel_path.clone();
    let path_for_old = old_path.clone();
    let path_for_new = current_path.clone();

    gui.popup = PopupState::Menu {
        title: "Copy to clipboard".to_string(),
        items: vec![
            MenuItem {
                label: "File name".to_string(),
                description: String::new(),
                key: Some("n".to_string()),
                action: Some(Box::new(move |_gui| {
                    Platform::copy_to_clipboard(&file_name_copy)?;
                    Ok(())
                })),
            },
            MenuItem {
                label: "Relative path".to_string(),
                description: String::new(),
                key: Some("p".to_string()),
                action: Some(Box::new(move |_gui| {
                    Platform::copy_to_clipboard(&rel_path_copy)?;
                    Ok(())
                })),
            },
            MenuItem {
                label: "Absolute path".to_string(),
                description: String::new(),
                key: Some("P".to_string()),
                action: Some(Box::new(move |_gui| {
                    Platform::copy_to_clipboard(&abs_path)?;
                    Ok(())
                })),
            },
            MenuItem {
                label: "Old content (HEAD)".to_string(),
                description: if !is_added {
                    String::new()
                } else {
                    "File is new — no old content".to_string()
                },
                key: Some("o".to_string()),
                action: if !is_added {
                    Some(Box::new(move |gui| {
                        let content = gui.git.file_content_at_commit("HEAD", &path_for_old)?;
                        Platform::copy_to_clipboard(&content)?;
                        Ok(())
                    }))
                } else {
                    None
                },
            },
            MenuItem {
                label: "New content (working tree)".to_string(),
                description: if !is_deleted {
                    String::new()
                } else {
                    "File was deleted — no new content".to_string()
                },
                key: Some("w".to_string()),
                action: if !is_deleted {
                    Some(Box::new(move |gui| {
                        let content = gui.git.file_content(&path_for_new)?;
                        Platform::copy_to_clipboard(&content)?;
                        Ok(())
                    }))
                } else {
                    None
                },
            },
            MenuItem {
                label: "Diff of selected file".to_string(),
                description: String::new(),
                key: Some("s".to_string()),
                action: Some(Box::new(move |gui| {
                    let path_refs: Vec<&str> = diff_paths.iter().map(String::as_str).collect();
                    let mut diff = gui.git.diff_file_paths(&path_refs).unwrap_or_default();
                    let staged = gui
                        .git
                        .diff_file_staged_paths(&path_refs)
                        .unwrap_or_default();
                    if !staged.is_empty() {
                        if !diff.is_empty() {
                            diff.push('\n');
                        }
                        diff.push_str(&staged);
                    }
                    Platform::copy_to_clipboard(&diff)?;
                    Ok(())
                })),
            },
            MenuItem {
                label: "Diff of all files".to_string(),
                description: String::new(),
                key: Some("a".to_string()),
                action: Some(Box::new(|gui| {
                    let diff = gui.git.diff_all().unwrap_or_default();
                    Platform::copy_to_clipboard(&diff)?;
                    Ok(())
                })),
            },
            MenuItem::cancel(),
        ],
        selected: 0,
        loading_index: None,
    };
    Ok(())
}

/// Absolute path of the selected directory node in tree view, if any.
/// Returns None for file nodes or when the tree view is off.
fn selected_dir_abs_path(gui: &Gui) -> Option<String> {
    if !gui.show_file_tree {
        return None;
    }
    super::tree_dir_abs_path(gui, &gui.file_tree_nodes, gui.context_mgr.selected_active())
}

fn open_in_editor(gui: &mut Gui) -> Result<()> {
    // Directories: open the folder in the editor (e.g. `code <dir>`).
    if let Some(dir_abs) = selected_dir_abs_path(gui) {
        match gui.config.user_config.os.plan_open_dir(&dir_abs) {
            Ok(launch) => gui.launch_editor(launch)?,
            Err(_) => Platform::open_file(&dir_abs)?,
        }
        return Ok(());
    }
    let Some(file_idx) = gui.selected_file_index() else {
        return Ok(());
    };
    let model = gui.model.lock().unwrap();
    if let Some(file) = model.files.get(file_idx) {
        let rel_path = file.current_path().to_string();
        drop(model);

        let abs_path = gui
            .git
            .repo_path()
            .join(&rel_path)
            .to_string_lossy()
            .to_string();
        // Jump to first changed hunk if the diff for this file is loaded.
        let first_hunk_line = if gui.diff_view.filename == rel_path {
            gui.diff_view.hunk_starts.first().and_then(|&idx| {
                gui.diff_view
                    .file_line_number(idx, DiffPanel::New)
                    .or_else(|| gui.diff_view.file_line_number(idx, DiffPanel::Old))
            })
        } else {
            None
        };

        match gui
            .config
            .user_config
            .os
            .plan_edit(&abs_path, first_hunk_line, Some(1))
        {
            Ok(launch) => gui.launch_editor(launch)?,
            Err(_) => Platform::open_file(&abs_path)?,
        }
    }
    Ok(())
}

fn open_in_default_program(gui: &mut Gui) -> Result<()> {
    // Directories: open the folder with `os.open` (native file viewer by
    // default), falling back to the platform opener.
    if let Some(dir_abs) = selected_dir_abs_path(gui) {
        match gui.config.user_config.os.plan_open(&dir_abs) {
            Ok(launch) => gui.launch_editor(launch)?,
            Err(_) => Platform::open_file(&dir_abs)?,
        }
        return Ok(());
    }
    let Some(file_idx) = gui.selected_file_index() else {
        return Ok(());
    };
    let model = gui.model.lock().unwrap();
    if let Some(file) = model.files.get(file_idx) {
        let rel_path = file.current_path().to_string();
        drop(model);

        let abs_path = gui
            .git
            .repo_path()
            .join(&rel_path)
            .to_string_lossy()
            .to_string();
        let launch = gui.config.user_config.os.plan_open(&abs_path)?;
        gui.launch_editor(launch)?;
    }
    Ok(())
}

fn has_files_to_stash(gui: &Gui) -> bool {
    !gui.model.lock().unwrap().files.is_empty()
}

fn open_stash_options(gui: &mut Gui) -> Result<()> {
    if !has_files_to_stash(gui) {
        gui.show_action_toast("No changes to stash");
        return Ok(());
    }
    gui.popup = PopupState::Menu {
        title: "Stash options".to_string(),
        items: vec![
            MenuItem {
                label: "Stash all changes".to_string(),
                description: String::new(),
                key: Some("a".to_string()),
                action: Some(Box::new(|gui| {
                    open_stash_message_prompt(gui, StashKind::All);
                    Ok(())
                })),
            },
            MenuItem {
                label: "Stash all changes and keep index".to_string(),
                description: String::new(),
                key: Some("i".to_string()),
                action: Some(Box::new(|gui| {
                    open_stash_message_prompt(gui, StashKind::KeepIndex);
                    Ok(())
                })),
            },
            MenuItem {
                label: "Stash all changes including untracked files".to_string(),
                description: String::new(),
                key: Some("U".to_string()),
                action: Some(Box::new(|gui| {
                    open_stash_message_prompt(gui, StashKind::IncludeUntracked);
                    Ok(())
                })),
            },
            MenuItem {
                label: "Stash staged changes".to_string(),
                description: String::new(),
                key: Some("s".to_string()),
                action: Some(Box::new(|gui| {
                    open_stash_message_prompt(gui, StashKind::Staged);
                    Ok(())
                })),
            },
            MenuItem {
                label: "Stash unstaged changes".to_string(),
                description: String::new(),
                key: Some("u".to_string()),
                action: Some(Box::new(|gui| {
                    open_stash_message_prompt(gui, StashKind::Unstaged);
                    Ok(())
                })),
            },
            MenuItem::cancel(),
        ],
        selected: 0,
        loading_index: None,
    };
    Ok(())
}

enum StashKind {
    All,
    KeepIndex,
    IncludeUntracked,
    Staged,
    Unstaged,
}

fn open_stash_message_prompt(gui: &mut Gui, kind: StashKind) {
    gui.popup = PopupState::Input {
        title: "Stash message (leave empty for default)".to_string(),
        textarea: make_textarea(""),
        on_confirm: Box::new(move |gui, message| {
            let message = message.to_string();
            gui.start_background_stash_mutation("Creating stash", true, move |git| match kind {
                StashKind::All => git.stash_save(&message),
                StashKind::KeepIndex => git.stash_keep_index(&message),
                StashKind::IncludeUntracked => git.stash_include_untracked(&message),
                StashKind::Staged => git.stash_staged(&message),
                StashKind::Unstaged => git.stash_unstaged(&message),
            });
            Ok(())
        }),
        is_commit: false,
        confirm_focused: false,
    };
}

fn stash_changes(gui: &mut Gui) -> Result<()> {
    if !has_files_to_stash(gui) {
        gui.show_action_toast("No changes to stash");
        return Ok(());
    }
    gui.popup = PopupState::Input {
        title: "Stash message (leave empty for default)".to_string(),
        textarea: make_textarea(""),
        on_confirm: Box::new(|gui, message| {
            let message = message.to_string();
            gui.start_background_stash_mutation("Creating stash", true, move |git| {
                git.stash_save(&message)
            });
            Ok(())
        }),
        is_commit: false,
        confirm_focused: false,
    };
    Ok(())
}

fn discard_all_unstaged(gui: &mut Gui) -> Result<()> {
    // Restore tracked files from the index and delete untracked files; never
    // reset HEAD or the index, so all staged changes stay intact.
    if gui.config.user_config.gui.skip_discard_change_warning {
        gui.start_background_mutation("Discard unstaged changes", |git| {
            git.discard_all_unstaged_changes()
        });
    } else {
        gui.popup = PopupState::Confirm {
            title: "Discard all unstaged changes".to_string(),
            message: "Discard ALL unstaged changes, including deleting untracked files and directories?\n\nStaged changes and ignored files will be preserved.".to_string(),
            on_confirm: Box::new(|gui| {
                gui.start_background_mutation("Discard unstaged changes", |git| {
                    git.discard_all_unstaged_changes()
                });
                Ok(())
            }),
        };
    }
    Ok(())
}

fn discard_file(gui: &mut Gui) -> Result<()> {
    let selected = marked_action_files(gui);
    if !selected.is_empty() {
        discard_marked_files(gui, selected);
        return Ok(());
    }
    // If in tree view and a directory is selected, discard all child files
    if gui.show_file_tree {
        let selected = gui.context_mgr.selected_active();
        if let Some(node) = gui.file_tree_nodes.get(selected) {
            if node.is_dir {
                let child_indices = node.child_file_indices.clone();
                let model = gui.model.lock().unwrap();
                let files: Vec<_> = child_indices
                    .iter()
                    .filter_map(|&i| model.files.get(i).cloned())
                    .collect();
                let dir_name = node.name.clone();
                drop(model);

                if files.is_empty() {
                    return Ok(());
                }

                if !gui.config.user_config.gui.skip_discard_change_warning {
                    let files_clone = files.clone();
                    gui.popup = PopupState::Menu {
                        title: format!("Discard all changes in '{}'?", dir_name),
                        items: vec![
                            MenuItem {
                                label: "Discard".to_string(),
                                description: "discard all changes".to_string(),
                                key: Some("d".to_string()),
                                action: Some(Box::new(move |gui| {
                                    gui.git.discard_files(&files_clone)?;
                                    gui.needs_refresh = true;
                                    Ok(())
                                })),
                            },
                            MenuItem {
                                label: "Cancel".to_string(),
                                description: String::new(),
                                key: Some("c".to_string()),
                                action: Some(Box::new(|_| Ok(()))),
                            },
                        ],
                        selected: 0,
                        loading_index: None,
                    };
                } else {
                    gui.git.discard_files(&files)?;
                    gui.needs_refresh = true;
                }
                return Ok(());
            }
        }
    }

    let Some(file_idx) = gui.selected_file_index() else {
        return Ok(());
    };
    let model = gui.model.lock().unwrap();
    if let Some(file) = model.files.get(file_idx) {
        let file = file.clone();
        let display = file.display_name.clone();
        drop(model);

        if !gui.config.user_config.gui.skip_discard_change_warning {
            let file_clone = file.clone();
            gui.popup = PopupState::Menu {
                title: format!("Discard changes to '{}'?", display),
                items: vec![
                    MenuItem {
                        label: "Discard".to_string(),
                        description: "discard all changes".to_string(),
                        key: Some("d".to_string()),
                        action: Some(Box::new(move |gui| {
                            gui.git.discard_file(&file_clone)?;
                            gui.needs_refresh = true;
                            Ok(())
                        })),
                    },
                    MenuItem {
                        label: "Cancel".to_string(),
                        description: String::new(),
                        key: Some("c".to_string()),
                        action: Some(Box::new(|_| Ok(()))),
                    },
                ],
                selected: 0,
                loading_index: None,
            };
        } else {
            gui.git.discard_file(&file)?;
            gui.needs_refresh = true;
        }
    }
    Ok(())
}

fn ignore_file(gui: &mut Gui) -> Result<()> {
    let Some(file_idx) = gui.selected_file_index() else {
        return Ok(());
    };
    let model = gui.model.lock().unwrap();
    if let Some(file) = model.files.get(file_idx) {
        let name = file.current_path().to_string();
        let display = file.display_name.clone();
        drop(model);

        let name_for_exclude = name.clone();
        gui.popup = PopupState::Menu {
            title: format!("Ignore '{}'", display),
            items: vec![
                MenuItem {
                    label: "Add to .gitignore".to_string(),
                    description: String::new(),
                    key: Some("i".to_string()),
                    action: Some(Box::new(move |gui| {
                        gui.git.ignore_file(&name)?;
                        gui.needs_refresh = true;
                        Ok(())
                    })),
                },
                MenuItem {
                    label: "Add to .git/info/exclude".to_string(),
                    description: String::new(),
                    key: Some("e".to_string()),
                    action: Some(Box::new(move |gui| {
                        gui.git.exclude_file(&name_for_exclude)?;
                        gui.needs_refresh = true;
                        Ok(())
                    })),
                },
                MenuItem::cancel(),
            ],
            selected: 0,
            loading_index: None,
        };
    }
    Ok(())
}

fn amend_commit(gui: &mut Gui) -> Result<()> {
    gui.popup = PopupState::Confirm {
        title: "Amend".to_string(),
        message: "Amend last commit with staged changes?".to_string(),
        on_confirm: Box::new(|gui| {
            gui.start_remote_op(
                RemoteOpKind::LocalRewriteHead,
                "Amend",
                "Amending commit...",
                |git| {
                    git.amend_commit()?;
                    Ok(())
                },
            );
            Ok(())
        }),
    };
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::next_file_row_after_toggle;

    #[test]
    fn file_toggle_advances_and_stays_at_bottom() {
        assert_eq!(next_file_row_after_toggle(0, 3, |_| true), 1);
        assert_eq!(next_file_row_after_toggle(1, 3, |_| true), 2);
        assert_eq!(next_file_row_after_toggle(2, 3, |_| true), 2);
    }

    #[test]
    fn file_toggle_keeps_single_file_selected() {
        assert_eq!(next_file_row_after_toggle(0, 1, |_| true), 0);
    }

    #[test]
    fn file_toggle_skips_directory_rows() {
        // root, file A, directory, file B
        let file_rows = [false, true, false, true];
        assert_eq!(
            next_file_row_after_toggle(1, file_rows.len(), |idx| file_rows[idx]),
            3
        );
        assert_eq!(
            next_file_row_after_toggle(3, file_rows.len(), |idx| file_rows[idx]),
            3
        );
    }
}
