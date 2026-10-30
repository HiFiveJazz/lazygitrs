use anyhow::Result;

use crate::gui::Gui;
use crate::gui::popup::{MenuItem, MessageKind, PopupState};

/// Show the custom patch builder (legacy/global <c-p>, also reachable from Commit → Patch).
pub fn show_patch_menu(gui: &mut Gui) -> Result<()> {
    if gui.patch_building.active {
        // Already in patch building mode — show apply options
        if !gui.patch_building.has_selections() {
            gui.popup = PopupState::Message {
                title: "No files selected".to_string(),
                message: "Toggle files with space to include them in the patch.".to_string(),
                kind: MessageKind::Info,
            };
            return Ok(());
        }

        gui.popup = PopupState::Menu {
            title: "Patch options".to_string(),
            items: vec![
                MenuItem {
                    label: "Apply patch to index".to_string(),
                    description: "Stage the selected changes".to_string(),
                    key: Some("a".to_string()),
                    action: Some(Box::new(|gui| {
                        apply_patch(gui, false)?;
                        Ok(())
                    })),
                },
                MenuItem {
                    label: "Apply patch in reverse".to_string(),
                    description: "Remove the selected changes".to_string(),
                    key: Some("r".to_string()),
                    action: Some(Box::new(|gui| {
                        apply_patch(gui, true)?;
                        Ok(())
                    })),
                },
                MenuItem {
                    label: "Exit patch building mode".to_string(),
                    description: "Discard patch selections".to_string(),
                    key: Some("q".to_string()),
                    action: Some(Box::new(|gui| {
                        gui.patch_building.exit();
                        Ok(())
                    })),
                },
            ],
            selected: 0,
            loading_index: None,
        };
    } else {
        // Enter patch building mode
        let selected = gui.context_mgr.selected_active();
        let model = gui.model.lock().unwrap();
        if let Some(commit) = model.commits.get(selected) {
            let hash = commit.hash.clone();
            let short = commit.short_hash().to_string();
            drop(model);

            gui.popup = PopupState::Confirm {
                title: "Patch building".to_string(),
                message: format!(
                    "Enter patch building mode for commit {}?\n\
                     Use space to toggle files, then <c-p> to apply.",
                    short
                ),
                on_confirm: Box::new(move |gui| {
                    gui.patch_building.enter(hash);
                    Ok(())
                }),
            };
        }
    }
    Ok(())
}

fn apply_patch(gui: &mut Gui, reverse: bool) -> Result<()> {
    let commit = gui.patch_building.source_commit.clone();
    let files: Vec<String> = gui.patch_building.selected_files.iter().cloned().collect();

    // Build one patch for the full selection. The previous implementation ran
    // one `git diff` + one `git apply` per file and silently ignored apply
    // failures, which was both slow and capable of leaving a partially applied
    // selection. `git apply` without --reject is atomic across this combined
    // patch, so either all selected paths apply or none do.
    let parent = format!("{}^", commit);
    let mut diff = gui.git.git_cmd().args(&["diff", &parent, &commit, "--"]);
    for file in &files {
        diff = diff.arg(file);
    }
    let diff_result = diff.run_expecting_success()?;
    if diff_result.stdout.is_empty() {
        gui.patch_building.exit();
        return Ok(());
    }

    let mut apply = gui.git.git_cmd().arg("apply");
    if reverse {
        apply = apply.arg("--reverse");
    }
    apply
        .arg("--cached")
        .arg("-")
        .stdin(diff_result.stdout)
        .run_expecting_success()?;

    gui.patch_building.exit();
    gui.needs_refresh = true;
    Ok(())
}
