use crate::config::keybindings::parse_key;
use crossterm::event::KeyEvent;

/// Match a configured keybinding exactly.
///
/// Keep this separate from `tree::matches_key`, which intentionally normalizes
/// shifted punctuation for terminal compatibility.
pub(super) fn matches_key(key: KeyEvent, binding: &str) -> bool {
    parse_key(binding).is_some_and(|expected| {
        key.code == expected.code && key.modifiers == expected.modifiers
    })
}

#[derive(Clone, Copy)]
pub(super) enum PrUrlAction {
    Copy,
    Open,
}

/// Append the async "existing PR" action shared by branch/status menus.
pub(super) fn push_pr_url_item(
    items: &mut Vec<crate::gui::popup::MenuItem>,
    branch: String,
    action: PrUrlAction,
) {
    use crate::gui::popup::{MenuAsyncResult, MenuItem};

    let index = items.len();
    let label = match action {
        PrUrlAction::Copy => "Copy PR URL",
        PrUrlAction::Open => "Open PR URL",
    };
    items.push(MenuItem {
        label: label.to_string(),
        description: "(requires existing PR)".to_string(),
        key: Some("p".to_string()),
        action: Some(Box::new(move |gui| {
            let branch = branch.clone();
            gui.start_menu_async(index, move |git| {
                let url = git.get_pr_url(&branch)?;
                Ok(match action {
                    PrUrlAction::Copy => MenuAsyncResult::CopyToClipboard(url),
                    PrUrlAction::Open => MenuAsyncResult::OpenUrl(url),
                })
            });
            Ok(())
        })),
    });
}

/// Append branch, remote-branch, and tag rows shared by ref pickers.
pub(super) fn append_named_ref_picker_items(
    items: &mut Vec<crate::gui::popup::ListPickerItem>,
    model: &crate::model::Model,
    skip_head: bool,
) {
    use crate::gui::popup::ListPickerItem;

    for branch in &model.branches {
        if skip_head && branch.head {
            continue;
        }
        items.push(ListPickerItem {
            value: branch.name.clone(),
            label: branch.name.clone(),
            category: "Branches".to_string(),
            description: None,
        });
    }

    for remote in &model.remotes {
        for branch in &remote.branches {
            let full_name = format!("{}/{}", remote.name, branch.name);
            items.push(ListPickerItem {
                value: full_name.clone(),
                label: full_name,
                category: "Remote Branches".to_string(),
                description: None,
            });
        }
    }

    for tag in &model.tags {
        items.push(ListPickerItem {
            value: tag.name.clone(),
            label: tag.name.clone(),
            category: "Tags".to_string(),
            description: None,
        });
    }
}

/// Load the selected commit's files and enter the requested file subview.
///
/// `include_stats` is false for stash browsing, where whole-commit hunk/line
/// statistics are unnecessary because the selected file preview is loaded
/// separately.
pub(super) fn enter_commit_files_view(
    gui: &mut crate::gui::Gui,
    hash: String,
    message: String,
    target: crate::gui::context::ContextId,
    include_stats: bool,
) -> anyhow::Result<()> {
    let commit_files = if include_stats {
        gui.git.commit_files(&hash)?
    } else {
        gui.git.commit_files_without_stats(&hash)?
    };
    gui.model.lock().unwrap().commit_files = commit_files;
    gui.commit_files_hash = hash;
    gui.commit_files_message = message;

    commit_files::update_commit_file_tree_state(gui);

    gui.context_mgr.set_active(target);
    gui.context_mgr.set_selection(0);
    gui.needs_diff_refresh = true;
    Ok(())
}

/// Load a branch/tag/remote ref into the shared commit-history subview.
pub(super) fn enter_named_commit_history(
    gui: &mut crate::gui::Gui,
    name: String,
    parent: Option<crate::gui::context::ContextId>,
) -> anyhow::Result<()> {
    let commits = gui.git.load_commits_for_branch(&name, 300)?;
    gui.model.lock().unwrap().set_sub_commits(commits);
    gui.branch_commits_name = name;
    if let Some(parent) = parent {
        gui.sub_commits_parent_context = parent;
    }
    gui.context_mgr
        .set_active(crate::gui::context::ContextId::BranchCommits);
    gui.context_mgr.set_selection(0);
    gui.needs_diff_refresh = true;
    Ok(())
}

/// Resolve a selected directory node to an absolute repository path.
pub(super) fn tree_dir_abs_path(
    gui: &crate::gui::Gui,
    nodes: &[crate::model::file_tree::FileTreeNode],
    selected: usize,
) -> Option<String> {
    let node = nodes.get(selected)?;
    if !node.is_dir {
        return None;
    }
    let path = if node.path == "." || node.path.is_empty() {
        gui.git.repo_path().to_path_buf()
    } else {
        gui.git.repo_path().join(&node.path)
    };
    Some(path.to_string_lossy().into_owned())
}

pub mod bisect;
pub mod branch_commits;
pub mod branches;
pub mod commit_files;
pub mod commits;
pub mod custom_commands;
pub mod diff_grep;
pub mod diff_mode;
pub mod files;
pub mod patch_building;
pub mod rebase_mode;
pub mod reflog;
pub mod remote_branches;
pub mod remotes;
pub mod stash;
pub mod status;
pub mod submodules;
pub mod tags;
pub mod worktrees;

pub mod tree;
