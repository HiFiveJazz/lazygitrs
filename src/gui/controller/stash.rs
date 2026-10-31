use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::config::KeybindingConfig;
use crate::config::keybindings::parse_key;
use crate::gui::Gui;
use crate::gui::popup::{PopupState, make_textarea};

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    // Enter: view stash files
    if key.code == KeyCode::Enter {
        return enter_stash_files(gui);
    }

    // Pop stash. Default: Space.
    if matches_key(key, &keybindings.stash.pop_stash) {
        return pop_stash(gui);
    }

    // Apply stash while keeping it in the stash list. Default: a.
    if matches_key(key, &keybindings.stash.apply_stash) {
        return apply_stash(gui);
    }

    // Drop stash
    if key.code == KeyCode::Char('d') {
        return drop_stash(gui);
    }

    // Rename stash
    if matches_key(key, &keybindings.stash.rename_stash) {
        return rename_stash(gui);
    }

    Ok(())
}

fn enter_stash_files(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(entry) = model.stash_entries.get(selected) {
        let hash = entry.hash.clone();
        let message = entry.name.clone();
        drop(model);

        // Keep entering large stashes responsive. The selected file's diff is
        // loaded separately, so whole-stash line and hunk counts are unnecessary.
        let commit_files = gui.git.commit_files_without_stats(&hash)?;
        {
            let mut model = gui.model.lock().unwrap();
            model.commit_files = commit_files;
        }
        gui.commit_files_hash = hash;
        gui.commit_files_message = message;

        // Build file tree if tree view is enabled
        if gui.show_commit_file_tree {
            let model = gui.model.lock().unwrap();
            gui.commit_file_tree_nodes = crate::model::file_tree::build_commit_file_tree(
                &model.commit_files,
                &gui.commit_files_collapsed_dirs,
            );
            gui.context_mgr.commit_files_list_len_override = Some(gui.commit_file_tree_nodes.len());
        } else {
            gui.commit_file_tree_nodes.clear();
            gui.context_mgr.commit_files_list_len_override = None;
        }

        // Switch to StashFiles context
        gui.context_mgr
            .set_active(crate::gui::context::ContextId::StashFiles);
        gui.context_mgr.set_selection(0);
        gui.needs_diff_refresh = true;
    }
    Ok(())
}

fn pop_stash(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(entry) = model.stash_entries.get(selected) {
        let index = entry.index;
        let name = entry.name.clone();
        drop(model);

        gui.popup = PopupState::Confirm {
            title: "Pop stash".to_string(),
            message: format!(
                "Apply this stash and remove it from the stash list?\n\n{}",
                name
            ),
            on_confirm: Box::new(move |gui| {
                gui.start_background_stash_mutation("Popping stash", true, move |git| {
                    git.stash_pop(index)
                });
                Ok(())
            }),
        };
    }
    Ok(())
}

fn apply_stash(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(entry) = model.stash_entries.get(selected) {
        let index = entry.index;
        let name = entry.name.clone();
        drop(model);

        gui.popup = PopupState::Confirm {
            title: "Apply stash".to_string(),
            message: format!(
                "Apply this stash and keep it in the stash list?\n\n{}",
                name
            ),
            on_confirm: Box::new(move |gui| {
                gui.start_background_stash_mutation("Applying stash", true, move |git| {
                    git.stash_apply(index)
                });
                Ok(())
            }),
        };
    }
    Ok(())
}

fn drop_stash(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(entry) = model.stash_entries.get(selected) {
        let index = entry.index;
        let name = entry.name.clone();
        drop(model);

        gui.popup = PopupState::Confirm {
            title: "Drop stash".to_string(),
            message: format!(
                "Permanently remove this stash?\n\n{}",
                name
            ),
            on_confirm: Box::new(move |gui| {
                gui.start_background_stash_mutation("Dropping stash", false, move |git| {
                    git.stash_drop(index)
                });
                Ok(())
            }),
        };
    }
    Ok(())
}

fn rename_stash(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(entry) = model.stash_entries.get(selected) {
        let index = entry.index;
        let current_name = entry.name.clone();
        drop(model);

        let mut ta = make_textarea("");
        ta.insert_str(&current_name);
        gui.popup = PopupState::Input {
            title: "Rename stash".to_string(),
            textarea: ta,
            on_confirm: Box::new(move |gui, new_name| {
                if !new_name.is_empty() {
                    let new_name = new_name.to_string();
                    gui.start_background_stash_mutation("Renaming stash", false, move |git| {
                        git.stash_rename(index, &new_name)
                    });
                }
                Ok(())
            }),
            is_commit: false,
            confirm_focused: false,
        };
    }
    Ok(())
}

fn matches_key(key: KeyEvent, binding: &str) -> bool {
    if let Some(expected) = parse_key(binding) {
        key.code == expected.code && key.modifiers == expected.modifiers
    } else {
        false
    }
}
