use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::config::KeybindingConfig;
use crate::gui::Gui;
use crate::gui::context::ContextId;

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    if super::matches_key(key, &keybindings.commits.open_log_menu) {
        return super::commits::show_filtering_menu(gui);
    }

    // Escape: go back to parent context (Branches or Tags)
    if key.code == KeyCode::Esc {
        let parent = gui.sub_commits_parent_context;
        gui.context_mgr.set_active(parent);
        {
            let mut model = gui.model.lock().unwrap();
            model.clear_sub_commits();
        }
        gui.branch_commits_name.clear();
        gui.sub_commits_parent_context = ContextId::Branches;
        gui.needs_diff_refresh = true;
        return Ok(());
    }

    // Enter: open commit files for the selected commit
    if key.code == KeyCode::Enter {
        return enter_branch_commit_files(gui);
    }

    // Open commit in browser
    if key.code == KeyCode::Char('o') {
        return open_commit_in_browser(gui);
    }

    // Copy to clipboard menu
    if key.code == KeyCode::Char('y') {
        return copy_to_clipboard(gui);
    }

    Ok(())
}

fn open_commit_in_browser(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.sub_commits.get(selected) {
        let hash = commit.hash.clone();
        drop(model);
        return super::commits::open_commit_in_browser_for(gui, hash);
    }
    Ok(())
}

fn copy_to_clipboard(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(commit) = model.sub_commits.get(selected) {
        let hash = commit.hash.clone();
        let subject = commit.name.clone();
        let author = commit.author_name.clone();
        let tags = commit.tags.clone();
        drop(model);
        super::commits::copy_commit_to_clipboard_menu_for(gui, hash, subject, author, tags);
    }
    Ok(())
}

fn enter_branch_commit_files(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let commit = gui
        .model
        .lock()
        .unwrap()
        .sub_commits
        .get(selected)
        .map(|commit| (commit.hash.clone(), commit.name.clone()));
    if let Some((hash, message)) = commit {
        super::enter_commit_files_view(gui, hash, message, ContextId::BranchCommitFiles, true)?;
    }
    Ok(())
}
