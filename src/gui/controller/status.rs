use super::matches_key;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::config::KeybindingConfig;
use crate::gui::Gui;
use crate::gui::popup::{MenuItem, PopupState};
use crate::os::platform::Platform;

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    // Enter: show recent repos
    if key.code == KeyCode::Enter {
        return gui.show_recent_repos();
    }

    // y: copy menu (branch name, repo url, PR create url, PR url for current branch)
    if key.code == KeyCode::Char('y') {
        return copy_menu(gui);
    }

    // o: open in browser menu
    if matches_key(key, &keybindings.branches.create_pull_request) {
        return open_menu(gui);
    }

    Ok(())
}

fn current_branch_name(gui: &Gui) -> String {
    let model = gui.model.lock().unwrap();
    if let Some(b) = model.branches.iter().find(|b| b.head) {
        return b.name.clone();
    }
    model.head_branch_name.clone()
}

fn copy_menu(gui: &mut Gui) -> Result<()> {
    let branch = current_branch_name(gui);
    let branch_for_name = branch.clone();
    let branch_for_pr_create = branch.clone();
    let branch_for_pr = branch.clone();

    let mut items = vec![MenuItem {
        label: "Copy repo URL".to_string(),
        description: String::new(),
        key: Some("r".to_string()),
        action: Some(Box::new(move |gui| {
            let url = gui
                .git
                .get_repo_url_with_services(&gui.config.user_config.services)?;
            Platform::copy_to_clipboard(&url)?;
            Ok(())
        })),
    }];

    if !branch.is_empty() {
        items.insert(
            0,
            MenuItem {
                label: "Copy branch name".to_string(),
                description: String::new(),
                key: Some("n".to_string()),
                action: Some(Box::new(move |_gui| {
                    Platform::copy_to_clipboard(&branch_for_name)?;
                    Ok(())
                })),
            },
        );
        items.push(MenuItem {
            label: "Copy PR create URL".to_string(),
            description: String::new(),
            key: Some("c".to_string()),
            action: Some(Box::new(move |gui| {
                let url = gui.git.get_pr_create_url(&branch_for_pr_create)?;
                Platform::copy_to_clipboard(&url)?;
                Ok(())
            })),
        });
        super::push_pr_url_item(&mut items, branch_for_pr, super::PrUrlAction::Copy);
    }

    gui.popup = PopupState::Menu {
        title: "Copy to clipboard".to_string(),
        items,
        selected: 0,
        loading_index: None,
    };
    Ok(())
}

fn open_menu(gui: &mut Gui) -> Result<()> {
    let branch = current_branch_name(gui);
    let branch_for_pr_create = branch.clone();
    let branch_for_pr = branch.clone();

    let mut items = vec![MenuItem {
        label: "Open repo URL".to_string(),
        description: String::new(),
        key: Some("r".to_string()),
        action: Some(Box::new(move |gui| {
            let url = gui
                .git
                .get_repo_url_with_services(&gui.config.user_config.services)?;
            Platform::open_file(&url)?;
            Ok(())
        })),
    }];

    if !branch.is_empty() {
        items.push(MenuItem {
            label: "Open PR create URL".to_string(),
            description: String::new(),
            key: Some("c".to_string()),
            action: Some(Box::new(move |gui| {
                let url = gui.git.get_pr_create_url(&branch_for_pr_create)?;
                Platform::open_file(&url)?;
                Ok(())
            })),
        });
        super::push_pr_url_item(&mut items, branch_for_pr, super::PrUrlAction::Open);
    }

    gui.popup = PopupState::Menu {
        title: "Open in browser".to_string(),
        items,
        selected: 0,
        loading_index: None,
    };
    Ok(())
}
