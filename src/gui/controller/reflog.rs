use super::matches_key;
use anyhow::Result;
use crossterm::event::KeyEvent;

use crate::config::KeybindingConfig;
use crate::gui::Gui;
use crate::gui::popup::{MenuItem, PopupState};
use crate::os::platform::Platform;

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    // Enter: view commit files for reflog entry
    if key.code == crossterm::event::KeyCode::Enter {
        return enter_reflog_commit_files(gui);
    }

    // Checkout reflog entry
    if matches_key(key, &keybindings.commits.checkout_commit) {
        return checkout_reflog_entry(gui);
    }

    // View reset options
    if matches_key(key, &keybindings.commits.view_reset_options) {
        return show_reset_menu(gui);
    }

    // Cherry-pick
    if matches_key(key, &keybindings.commits.cherry_pick_copy) {
        return cherry_pick_reflog_entry(gui);
    }

    // Copy to clipboard
    if key.code == crossterm::event::KeyCode::Char('y') {
        return copy_to_clipboard_menu(gui);
    }

    // Open reflog commit in browser
    if key.code == crossterm::event::KeyCode::Char('o') {
        return open_in_browser(gui);
    }

    Ok(())
}

fn open_in_browser(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.reflog_commits.get(selected) {
        let hash = commit.hash.clone();
        drop(model);
        return super::commits::open_commit_in_browser_for(gui, hash);
    }
    Ok(())
}

fn enter_reflog_commit_files(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let commit = gui
        .model
        .lock()
        .unwrap()
        .reflog_commits
        .get(selected)
        .map(|commit| (commit.hash.clone(), commit.name.clone()));
    if let Some((hash, message)) = commit {
        gui.commit_files_parent_context = Some(crate::gui::context::ContextId::Reflog);
        super::enter_commit_files_view(
            gui,
            hash,
            message,
            crate::gui::context::ContextId::CommitFiles,
            true,
        )?;
    }
    Ok(())
}

fn checkout_reflog_entry(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.reflog_commits.get(selected) {
        let hash = commit.hash.clone();
        let short = commit.short_hash().to_string();
        drop(model);

        gui.popup = PopupState::Confirm {
            title: "Checkout reflog entry".to_string(),
            message: format!("Checkout commit {}? (detached HEAD)", short),
            on_confirm: Box::new(move |gui| {
                gui.git.checkout_branch(&hash)?;
                gui.needs_refresh = true;
                Ok(())
            }),
        };
    }
    Ok(())
}

fn show_reset_menu(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.reflog_commits.get(selected) {
        let hash = commit.hash.clone();
        drop(model);
        return super::commits::show_reset_menu_for_ref(gui, &hash);
    }
    Ok(())
}

fn cherry_pick_reflog_entry(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.reflog_commits.get(selected) {
        let hash = commit.hash.clone();
        let short = commit.short_hash().to_string();
        drop(model);

        if !gui.cherry_pick_clipboard.contains(&hash) {
            gui.cherry_pick_clipboard.push(hash);
        }

        let n = gui.cherry_pick_clipboard.len();
        gui.popup = PopupState::Message {
            title: "Cherry-pick".to_string(),
            message: format!(
                "Copied commit {} ({} commit{} copied)",
                short,
                n,
                if n == 1 { "" } else { "s" }
            ),
            kind: crate::gui::popup::MessageKind::Info,
        };
    }
    Ok(())
}

fn copy_to_clipboard_menu(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.reflog_commits.get(selected) {
        let hash = commit.hash.clone();
        let subject = commit.name.clone();
        drop(model);

        gui.popup = PopupState::Menu {
            title: "Copy to clipboard".to_string(),
            items: vec![
                MenuItem {
                    label: "Commit hash".to_string(),
                    description: String::new(),
                    key: None,
                    action: Some(Box::new(move |_gui| {
                        Platform::copy_to_clipboard(&hash)?;
                        Ok(())
                    })),
                },
                MenuItem {
                    label: "Commit subject".to_string(),
                    description: String::new(),
                    key: Some("s".to_string()),
                    action: Some(Box::new(move |_gui| {
                        Platform::copy_to_clipboard(&subject)?;
                        Ok(())
                    })),
                },
            ],
            selected: 0,
            loading_index: None,
        };
    }
    Ok(())
}
