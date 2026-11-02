use super::matches_key;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::config::KeybindingConfig;
use crate::gui::Gui;
use crate::gui::context::ContextId;
use crate::gui::popup::PopupState;

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    // Escape: go back to Remotes list
    if key.code == KeyCode::Esc {
        gui.context_mgr.set_active(ContextId::Remotes);
        {
            let mut model = gui.model.lock().unwrap();
            model.sub_remote_branches.clear();
        }
        gui.remote_branches_name.clear();
        return Ok(());
    }

    // Enter: drill into commits for the selected remote branch
    if key.code == KeyCode::Enter {
        return enter_remote_branch_commits(gui);
    }

    // Space: checkout remote branch (creates local tracking branch)
    if key.code == KeyCode::Char(' ') {
        return checkout_remote_branch(gui);
    }

    // M: merge remote branch into current
    if matches_key(key, &keybindings.branches.merge_into_current_branch) {
        return merge_remote_branch(gui);
    }

    // r: rebase onto remote branch
    if matches_key(key, &keybindings.branches.rebase_branch) {
        return rebase_remote_branch(gui);
    }

    // d: delete remote branch
    if key.code == KeyCode::Char('d') {
        return delete_remote_branch(gui);
    }

    Ok(())
}

fn enter_remote_branch_commits(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let full_name = gui
        .model
        .lock()
        .unwrap()
        .sub_remote_branches
        .get(selected)
        .map(|branch| branch.full_name());
    if let Some(full_name) = full_name {
        super::enter_named_commit_history(gui, full_name, Some(ContextId::RemoteBranches))?;
    }
    Ok(())
}

fn selected_remote_branch(gui: &Gui) -> Option<(String, String)> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    model
        .sub_remote_branches
        .get(selected)
        .map(|branch| (branch.remote_name.clone(), branch.name.clone()))
}

fn checkout_remote_branch(gui: &mut Gui) -> Result<()> {
    if let Some((remote, branch)) = selected_remote_branch(gui) {
        gui.popup = PopupState::Confirm {
            title: "Checkout".to_string(),
            message: format!(
                "Checkout '{}/{}' as local branch '{}'?",
                remote, branch, branch
            ),
            on_confirm: Box::new(move |gui| {
                gui.git.checkout_remote_branch(&remote, &branch)?;
                gui.needs_refresh = true;
                Ok(())
            }),
        };
    }
    Ok(())
}

fn merge_remote_branch(gui: &mut Gui) -> Result<()> {
    if let Some((remote, branch)) = selected_remote_branch(gui) {
        let merge_args = gui.config.user_config.git.merging.args.clone();
        gui.popup = PopupState::Confirm {
            title: "Merge".to_string(),
            message: format!("Merge '{}/{}' into current branch?", remote, branch),
            on_confirm: Box::new(move |gui| {
                gui.git.merge_remote_branch(&remote, &branch, &merge_args)?;
                gui.needs_refresh = true;
                Ok(())
            }),
        };
    }
    Ok(())
}

fn rebase_remote_branch(gui: &mut Gui) -> Result<()> {
    if let Some((remote, branch)) = selected_remote_branch(gui) {
        gui.popup = PopupState::Confirm {
            title: "Rebase".to_string(),
            message: format!("Rebase onto '{}/{}'?", remote, branch),
            on_confirm: Box::new(move |gui| {
                gui.git.rebase_remote_branch(&remote, &branch)?;
                gui.request_history_refresh();
                Ok(())
            }),
        };
    }
    Ok(())
}

fn delete_remote_branch(gui: &mut Gui) -> Result<()> {
    if let Some((remote, branch)) = selected_remote_branch(gui) {
        gui.popup = PopupState::Confirm {
            title: "Delete remote branch".to_string(),
            message: format!(
                "Delete remote branch '{}/{}'?\nThis will push --delete to the remote.",
                remote, branch
            ),
            on_confirm: Box::new(move |gui| {
                gui.git.delete_remote_branch(&remote, &branch)?;
                gui.needs_refresh = true;
                Ok(())
            }),
        };
    }
    Ok(())
}
