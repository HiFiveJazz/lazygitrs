use crate::config::KeybindingConfig;
use crate::config::keybindings::parse_key;
use crate::gui::popup::{MenuItem, MessageKind, PopupState, make_textarea};
use crate::gui::{Gui, RemoteOpKind};
use crate::os::platform::Platform;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

pub fn handle_key(gui: &mut Gui, key: KeyEvent, keybindings: &KeybindingConfig) -> Result<()> {
    if super::commits::matches_key(key, &keybindings.commits.open_log_menu) {
        return super::commits::show_filtering_menu(gui);
    }

    // Enter: view branch commits
    if key.code == KeyCode::Enter {
        return enter_branch_commits(gui);
    }

    // Checkout with space
    if key.code == KeyCode::Char(' ') {
        return checkout_branch(gui);
    }

    if key.code == KeyCode::Char('n') {
        return new_branch(gui);
    }

    // c: checkout (ref picker)
    if key.code == KeyCode::Char('c') {
        return checkout_picker(gui);
    }

    // -: checkout previous branch (git checkout -)
    if key.code == KeyCode::Char('-') {
        return checkout_previous(gui);
    }

    if key.code == KeyCode::Char('d') {
        return delete_branch(gui);
    }

    if matches_key(key, &keybindings.branches.merge_into_current_branch) {
        return merge_branch(gui);
    }

    if matches_key(key, &keybindings.branches.rebase_branch) {
        return rebase_branch(gui);
    }

    if matches_key(key, &keybindings.branches.rename_branch) {
        return rename_branch(gui);
    }

    if matches_key(key, &keybindings.branches.fast_forward) {
        return fast_forward(gui);
    }

    if matches_key(key, &keybindings.branches.set_upstream) {
        return set_upstream(gui);
    }

    // y: Copy to clipboard menu (repo url, PR create url, PR url)
    if key.code == KeyCode::Char('y') {
        return copy_to_clipboard_menu(gui);
    }

    // o: Open in browser menu (repo url, PR create url, PR url)
    if matches_key(key, &keybindings.branches.create_pull_request) {
        return open_in_browser_menu(gui);
    }

    Ok(())
}

fn enter_branch_commits(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        let name = branch.name.clone();
        drop(model);

        // Load commits for this branch
        let commits = gui.git.load_commits_for_branch(&name, 300)?;
        {
            let mut model = gui.model.lock().unwrap();
            model.set_sub_commits(commits);
        }
        gui.branch_commits_name = name;

        // Switch to BranchCommits context
        gui.context_mgr
            .set_active(crate::gui::context::ContextId::BranchCommits);
        gui.context_mgr.set_selection(0);
        gui.needs_diff_refresh = true;
    }
    Ok(())
}

fn checkout_branch(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        if branch.head {
            return Ok(()); // Already on this branch
        }
        let name = branch.name.clone();
        drop(model);
        start_async_checkout(gui, name);
    }
    Ok(())
}

fn checkout_previous(gui: &mut Gui) -> Result<()> {
    // Resolve @{-1} to an actual ref name before checking out. This is more
    // reliable than `git checkout -`, which can fail when the previous ref is
    // not a local branch or when the reflog entry has become invalid.
    let name = gui
        .git
        .previous_branch_name()
        .unwrap_or_else(|| "-".to_string());
    start_async_checkout(gui, name);
    Ok(())
}

fn checkout_picker(gui: &mut Gui) -> Result<()> {
    use crate::gui::popup::{ListPickerCore, ListPickerItem, make_command_palette_search_textarea};

    let model = gui.model.lock().unwrap();
    let mut items = Vec::new();

    if let Some(prev) = gui.git.previous_branch_name() {
        items.push(ListPickerItem {
            value: prev.clone(),
            // "[-]" prefix lets typing "-" jump here; label also contains
            // "previous branch" and "prev branch" for those search phrases.
            label: format!("[-] Go to previous branch (prev branch) — {}", prev),
            category: "Quick Actions".to_string(),
            description: None,
        });
    }

    for branch in &model.branches {
        if branch.head {
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

    for commit in &model.commits {
        items.push(ListPickerItem {
            value: commit.hash.clone(),
            label: format!("{} {}", commit.short_hash(), commit.name),
            category: "Commits".to_string(),
            description: None,
        });
    }

    drop(model);

    gui.popup = PopupState::RefPicker {
        title: "Checkout".to_string(),
        core: ListPickerCore {
            items,
            selected: 0,
            search_textarea: make_command_palette_search_textarea(),
            scroll_offset: 0,
        },
        on_confirm: Box::new(|gui, ref_name| {
            start_async_checkout(gui, ref_name.to_string());
            Ok(())
        }),
    };
    Ok(())
}

fn start_async_checkout(gui: &mut Gui, name: String) {
    if gui.remote_op_label.is_some() {
        return;
    }

    // Keep HEAD sourced from Git: a rejected checkout must not change the
    // displayed branch. Successful completion triggers the normal model refresh.
    gui.pending_checkout_by_name = Some(name.clone());
    gui.start_remote_op(
        RemoteOpKind::Other,
        "Checking out",
        &format!("Checking out {}", name),
        move |git| {
            git.checkout_branch(&name)?;
            Ok(())
        },
    );
}

fn new_branch(gui: &mut Gui) -> Result<()> {
    gui.popup = PopupState::Input {
        title: "New branch name".to_string(),
        textarea: make_textarea(""),
        on_confirm: Box::new(|gui, name| {
            if !name.is_empty() {
                let name = name.to_string();
                gui.start_background_mutation("Creating branch", move |git| {
                    git.create_branch(&name)
                });
            }
            Ok(())
        }),
        is_commit: false,
        confirm_focused: false,
    };
    Ok(())
}

fn delete_branch(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        if branch.head {
            return Ok(()); // Can't delete current branch
        }
        let name = branch.name.clone();
        let has_remote = branch.upstream.is_some();
        let upstream = branch.upstream.clone();
        drop(model);

        let name_local = name.clone();
        let name_remote = name.clone();
        let name_both = name.clone();
        let upstream_for_remote = upstream.clone();
        let upstream_for_both = upstream.clone();

        let mut items = vec![MenuItem {
            label: "Delete local branch".to_string(),
            description: String::new(),
            key: Some("c".to_string()),
            action: Some(Box::new(move |gui| {
                delete_local_branch_worktree_aware(gui, name_local.clone(), false)
            })),
        }];

        if has_remote {
            items.push(MenuItem {
                label: "Delete remote branch".to_string(),
                description: String::new(),
                key: Some("r".to_string()),
                action: Some(Box::new(move |gui| {
                    // Parse remote name from upstream (e.g. "origin/branch" -> "origin")
                    let remote = upstream_for_remote
                        .as_deref()
                        .and_then(|u| u.split('/').next())
                        .unwrap_or("origin")
                        .to_string();
                    let name = name_remote.clone();
                    gui.start_remote_op(
                        RemoteOpKind::Push,
                        "Deleting remote branch",
                        "",
                        move |git| git.delete_remote_branch(&remote, &name),
                    );
                    Ok(())
                })),
            });
        } else {
            items.push(MenuItem {
                label: "Delete remote branch".to_string(),
                description: "No remote tracking branch".to_string(),
                key: Some("r".to_string()),
                action: None,
            });
        }

        if has_remote {
            items.push(MenuItem {
                label: "Delete local and remote branch".to_string(),
                description: String::new(),
                key: Some("b".to_string()),
                action: Some(Box::new(move |gui| {
                    let remote = upstream_for_both
                        .as_deref()
                        .and_then(|u| u.split('/').next())
                        .unwrap_or("origin")
                        .to_string();
                    delete_local_and_remote_branch_worktree_aware(
                        gui,
                        name_both.clone(),
                        remote,
                    )
                })),
            });
        } else {
            items.push(MenuItem {
                label: "Delete local and remote branch".to_string(),
                description: "No remote tracking branch".to_string(),
                key: Some("b".to_string()),
                action: None,
            });
        }

        items.push(MenuItem {
            label: "Cancel".to_string(),
            description: String::new(),
            key: None,
            action: Some(Box::new(|_| Ok(()))),
        });

        gui.popup = PopupState::Menu {
            title: format!("Delete branch '{}'?", name),
            items,
            selected: 0,
            loading_index: None,
        };
    }
    Ok(())
}


fn maybe_prompt_worktree_branch_delete(
    gui: &mut Gui,
    name: &str,
    force: bool,
    remote: Option<String>,
) -> Result<bool> {
    let Some(worktree) = gui.git.linked_worktree_for_branch(name)? else {
        return Ok(false);
    };

    if worktree.is_main {
        gui.popup = PopupState::Message {
            title: "Branch is used by the main worktree".to_string(),
            message: format!(
                "Branch '{}' is checked out in the main worktree:\n\n  {}\n\nSwitch that worktree to another branch before deleting {}.",
                name,
                worktree.path,
                if remote.is_some() {
                    "it locally or remotely"
                } else {
                    "this branch"
                }
            ),
            kind: MessageKind::Info,
        };
        return Ok(true);
    }

    // Once the worktree is removed, a normal `git branch -d` may still fail if
    // the branch is not fully merged. Determine that before removing anything
    // so the confirmation can explicitly authorize `-D` rather than surfacing a
    // second error after the worktree has already gone.
    let requires_force = !force && gui.git.branch_delete_requires_force(name)?;
    let delete_force = force || requires_force;

    let path = worktree.path;
    let stale = !std::path::Path::new(&path).exists();
    let delete_description = if let Some(remote_name) = remote.as_deref() {
        format!(
            "force-delete the local branch, and delete {}/{}",
            remote_name, name
        )
    } else if delete_force {
        "force-delete the branch".to_string()
    } else {
        "delete the branch".to_string()
    };
    let merge_warning = if requires_force {
        "\n\nThis branch is not fully merged. Confirming will delete it with `git branch -D`."
    } else {
        ""
    };

    let message = if stale {
        format!(
            "Branch '{}' is still registered to a worktree whose directory no longer exists:\n\n  {}\n\nPrune the stale worktree metadata and {}?{}",
            name, path, delete_description, merge_warning
        )
    } else {
        format!(
            "Branch '{}' is checked out in another worktree:\n\n  {}\n\nRemove that worktree and {}?{}\n\nDirty worktrees are never force-removed.",
            name, path, delete_description, merge_warning
        )
    };

    let branch = name.to_string();
    gui.popup = PopupState::Confirm {
        title: "Branch is used by a worktree".to_string(),
        message,
        on_confirm: Box::new(move |gui| {
            let path = path.clone();
            let branch = branch.clone();
            if let Some(remote) = remote.clone() {
                // Keep remote deletion on the serialized network lane. `Other`
                // intentionally requests a full refresh afterward because this
                // operation also removes a worktree.
                gui.start_remote_op(
                    RemoteOpKind::Other,
                    "Deleting branch",
                    "",
                    move |git| {
                        git.remove_worktree_for_branch_deletion(&path)?;
                        git.delete_branch(&branch, delete_force)?;
                        git.delete_remote_branch(&remote, &branch)
                    },
                );
            } else {
                gui.start_background_mutation(
                    "Removing worktree and deleting branch",
                    move |git| {
                        git.remove_worktree_for_branch_deletion(&path)?;
                        git.delete_branch(&branch, delete_force)
                    },
                );
            }
            Ok(())
        }),
    };
    Ok(true)
}

fn delete_local_branch_worktree_aware(
    gui: &mut Gui,
    name: String,
    force: bool,
) -> Result<()> {
    if maybe_prompt_worktree_branch_delete(gui, &name, force, None)? {
        return Ok(());
    }

    match gui.git.delete_branch(&name, force) {
        Ok(()) => {
            gui.needs_refresh = true;
            Ok(())
        }
        Err(e) if !force && e.to_string().contains("not fully merged") => {
            let name_force = name.clone();
            gui.popup = PopupState::Confirm {
                title: "Force delete?".to_string(),
                message: format!(
                    "'{}' is not fully merged. Are you sure you want to delete it?",
                    name
                ),
                on_confirm: Box::new(move |gui| {
                    delete_local_branch_worktree_aware(gui, name_force.clone(), true)
                }),
            };
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn delete_local_and_remote_branch_worktree_aware(
    gui: &mut Gui,
    name: String,
    remote: String,
) -> Result<()> {
    if maybe_prompt_worktree_branch_delete(gui, &name, true, Some(remote.clone()))? {
        return Ok(());
    }

    gui.start_remote_op(RemoteOpKind::Push, "Deleting branch", "", move |git| {
        git.delete_branch(&name, true)?;
        git.delete_remote_branch(&remote, &name)
    });
    Ok(())
}

fn merge_branch(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        let name = branch.name.clone();
        let merge_args = gui.config.user_config.git.merging.args.clone();
        drop(model);

        gui.popup = PopupState::Confirm {
            title: "Merge".to_string(),
            message: format!("Merge '{}' into current branch?", name),
            on_confirm: Box::new(move |gui| {
                let name = name.clone();
                let merge_args = merge_args.clone();
                gui.start_background_mutation("Merging branch", move |git| {
                    git.merge_branch(&name, &merge_args)
                });
                Ok(())
            }),
        };
    }
    Ok(())
}

fn rebase_branch(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    let current_branch = model.head_branch_name.clone();
    if let Some(branch) = model.branches.get(selected) {
        let name = branch.name.clone();
        let is_same_branch = name == current_branch;
        let name_for_simple = name.clone();
        let name_for_interactive = name.clone();
        let name_for_base = name.clone();
        drop(model);

        let items = vec![
            MenuItem {
                label: format!("Simple rebase onto '{}'", name_for_simple),
                description: if is_same_branch {
                    "Already on this branch".to_string()
                } else {
                    String::new()
                },
                key: Some("s".to_string()),
                action: if is_same_branch {
                    None
                } else {
                    Some(Box::new(move |gui| {
                        let name = name_for_simple.clone();
                        gui.start_background_mutation("Rebasing branch", move |git| {
                            git.rebase_branch(&name)
                        });
                        Ok(())
                    }))
                },
            },
            MenuItem {
                label: format!("Interactive rebase onto '{}'", name_for_interactive),
                description: if is_same_branch {
                    "Already on this branch".to_string()
                } else {
                    String::new()
                },
                key: Some("i".to_string()),
                action: if is_same_branch {
                    None
                } else {
                    Some(Box::new(move |gui| {
                        enter_interactive_rebase_onto(gui, &name_for_interactive)?;
                        Ok(())
                    }))
                },
            },
            MenuItem {
                label: format!("Rebase onto base branch ({})", name_for_base),
                description: String::new(),
                key: Some("b".to_string()),
                action: Some(Box::new(move |gui| {
                    let name = name_for_base.clone();
                    gui.start_background_mutation("Rebasing branch", move |git| {
                        git.rebase_branch(&name)
                    });
                    Ok(())
                })),
            },
            MenuItem {
                label: "Cancel".to_string(),
                description: String::new(),
                key: None,
                action: Some(Box::new(|_gui| Ok(()))),
            },
        ];

        gui.popup = PopupState::Menu {
            title: format!("Rebase '{}'", current_branch),
            items,
            selected: 0,
            loading_index: None,
        };
    }
    Ok(())
}

/// Enter interactive rebase mode onto a specific branch/ref.
pub fn enter_interactive_rebase_onto(gui: &mut Gui, onto_ref: &str) -> Result<()> {
    let base_hash = gui.git.resolve_ref(onto_ref)?;
    let branch_name = {
        let model = gui.model.lock().unwrap();
        model.head_branch_name.clone()
    };
    super::commits::enter_interactive_rebase_for_base(gui, base_hash, branch_name)
}

fn rename_branch(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        let old_name = branch.name.clone();
        drop(model);

        let mut ta = make_textarea("");
        ta.insert_str(&old_name);
        gui.popup = PopupState::Input {
            title: format!("Rename branch '{}'", old_name),
            textarea: ta,
            on_confirm: Box::new(move |gui, new_name| {
                if !new_name.is_empty() && new_name != old_name {
                    let old_name = old_name.clone();
                    let new_name = new_name.to_string();
                    gui.start_background_mutation("Renaming branch", move |git| {
                        git.rename_branch(&old_name, &new_name)
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

fn fast_forward(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        if branch.upstream.is_some() {
            let name = branch.name.clone();
            drop(model);
            gui.start_remote_op(
                RemoteOpKind::Fetch,
                "Fetch",
                &format!("Fetching origin for {}...", name),
                |git| {
                    git.fetch("origin")?;
                    Ok(())
                },
            );
        }
    }
    Ok(())
}

fn set_upstream(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        let name = branch.name.clone();
        drop(model);

        gui.popup = PopupState::Confirm {
            title: "Set upstream".to_string(),
            message: format!("Set upstream of '{}' to origin/{}?", name, name),
            on_confirm: Box::new(move |gui| {
                let branch = name.clone();
                gui.start_remote_op(
                    RemoteOpKind::Push,
                    "Push",
                    &format!("Pushing -u origin {}...", branch),
                    move |git| {
                        git.push_with_upstream("origin", &branch)?;
                        Ok(())
                    },
                );
                Ok(())
            }),
        };
    }
    Ok(())
}

fn copy_to_clipboard_menu(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        let branch_name = branch.name.clone();
        let branch_for_name = branch_name.clone();
        let branch_for_pr_create = branch_name.clone();
        let branch_for_pr = branch_name.clone();
        drop(model);

        let mut items = vec![
            MenuItem {
                label: "Copy branch name".to_string(),
                description: String::new(),
                key: Some("n".to_string()),
                action: Some(Box::new(move |_gui| {
                    Platform::copy_to_clipboard(&branch_for_name)?;
                    Ok(())
                })),
            },
            MenuItem {
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
            },
            MenuItem {
                label: "Copy PR create URL".to_string(),
                description: String::new(),
                key: Some("c".to_string()),
                action: Some(Box::new(move |gui| {
                    let url = gui.git.get_pr_create_url(&branch_for_pr_create)?;
                    Platform::copy_to_clipboard(&url)?;
                    Ok(())
                })),
            },
        ];

        // PR URL may not be available — try to detect if branch has a PR
        let pr_branch = branch_for_pr.clone();
        let copy_pr_index = items.len();
        items.push(MenuItem {
            label: "Copy PR URL".to_string(),
            description: "(requires existing PR)".to_string(),
            key: Some("p".to_string()),
            action: Some(Box::new(move |gui| {
                use crate::gui::popup::MenuAsyncResult;
                let branch = pr_branch.clone();
                gui.start_menu_async(copy_pr_index, move |git| {
                    let url = git.get_pr_url(&branch)?;
                    Ok(MenuAsyncResult::CopyToClipboard(url))
                });
                Ok(())
            })),
        });

        gui.popup = PopupState::Menu {
            title: "Copy to clipboard".to_string(),
            items,
            selected: 0,
            loading_index: None,
        };
    }
    Ok(())
}

fn open_in_browser_menu(gui: &mut Gui) -> Result<()> {
    let selected = gui.context_mgr.selected_active();
    let model = gui.model.lock().unwrap();
    if let Some(branch) = model.branches.get(selected) {
        let branch_name = branch.name.clone();
        let branch_for_pr_create = branch_name.clone();
        let branch_for_pr = branch_name.clone();
        drop(model);

        let mut items = vec![
            MenuItem {
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
            },
            MenuItem {
                label: "Open PR create URL".to_string(),
                description: String::new(),
                key: Some("c".to_string()),
                action: Some(Box::new(move |gui| {
                    let url = gui.git.get_pr_create_url(&branch_for_pr_create)?;
                    Platform::open_file(&url)?;
                    Ok(())
                })),
            },
        ];

        let pr_branch = branch_for_pr.clone();
        let open_pr_index = items.len();
        items.push(MenuItem {
            label: "Open PR URL".to_string(),
            description: "(requires existing PR)".to_string(),
            key: Some("p".to_string()),
            action: Some(Box::new(move |gui| {
                use crate::gui::popup::MenuAsyncResult;
                let branch = pr_branch.clone();
                gui.start_menu_async(open_pr_index, move |git| {
                    let url = git.get_pr_url(&branch)?;
                    Ok(MenuAsyncResult::OpenUrl(url))
                });
                Ok(())
            })),
        });

        gui.popup = PopupState::Menu {
            title: "Open in browser".to_string(),
            items,
            selected: 0,
            loading_index: None,
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
