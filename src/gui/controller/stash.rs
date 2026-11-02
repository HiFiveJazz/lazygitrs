use super::matches_key;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::config::KeybindingConfig;
use crate::git::GitCommands;
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
    let stash = gui
        .model
        .lock()
        .unwrap()
        .stash_entries
        .get(selected)
        .map(|entry| (entry.hash.clone(), entry.name.clone()));
    if let Some((hash, message)) = stash {
        super::enter_commit_files_view(
            gui,
            hash,
            message,
            crate::gui::context::ContextId::StashFiles,
            false,
        )?;
    }
    Ok(())
}

fn selected_stash(gui: &Gui) -> Option<(usize, String)> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    model
        .stash_entries
        .get(selected)
        .map(|entry| (entry.index, entry.name.clone()))
}

fn confirm_stash_action<F>(
    gui: &mut Gui,
    title: &'static str,
    prompt: &'static str,
    progress: &'static str,
    refresh_worktree: bool,
    action: F,
) -> Result<()>
where
    F: FnOnce(&GitCommands, usize) -> Result<()> + Send + 'static,
{
    let Some((index, name)) = selected_stash(gui) else {
        return Ok(());
    };

    gui.popup = PopupState::Confirm {
        title: title.to_string(),
        message: format!("{prompt}\n\n{name}"),
        on_confirm: Box::new(move |gui| {
            gui.start_background_stash_mutation(progress, refresh_worktree, move |git| {
                action(git, index)
            });
            Ok(())
        }),
    };
    Ok(())
}

fn pop_stash(gui: &mut Gui) -> Result<()> {
    confirm_stash_action(
        gui,
        "Pop stash",
        "Apply this stash and remove it from the stash list?",
        "Popping stash",
        true,
        GitCommands::stash_pop,
    )
}

fn apply_stash(gui: &mut Gui) -> Result<()> {
    confirm_stash_action(
        gui,
        "Apply stash",
        "Apply this stash and keep it in the stash list?",
        "Applying stash",
        true,
        GitCommands::stash_apply,
    )
}

fn drop_stash(gui: &mut Gui) -> Result<()> {
    confirm_stash_action(
        gui,
        "Drop stash",
        "Permanently remove this stash?",
        "Dropping stash",
        false,
        GitCommands::stash_drop,
    )
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
