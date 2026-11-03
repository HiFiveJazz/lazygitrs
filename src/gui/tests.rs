//! GUI regression tests kept separate from the production state/event module.
use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn contextual_commit_patch_wins_over_global_pull() {
    let kb = crate::config::KeybindingConfig::default();
    let key = crate::config::keybindings::parse_key(&kb.commits.patch_options).unwrap();
    assert!(context_claims_global_repo_key(ContextId::Commits, key, &kb));
    assert!(!context_claims_global_repo_key(ContextId::Files, key, &kb));
}

#[test]
fn contextual_tag_push_wins_over_global_push() {
    let kb = crate::config::KeybindingConfig::default();
    let key = KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT);
    assert!(context_claims_global_repo_key(ContextId::Tags, key, &kb));
    assert!(!context_claims_global_repo_key(ContextId::Files, key, &kb));
}

#[test]
fn streamed_metadata_does_not_invalidate_files_diff() {
    assert!(streamed_model_part_affects_diff(
        ContextId::Files,
        &ModelPart::Files(Vec::new())
    ));

    assert!(!streamed_model_part_affects_diff(
        ContextId::Files,
        &ModelPart::FileStats(Vec::new())
    ));
    assert!(!streamed_model_part_affects_diff(
        ContextId::Files,
        &ModelPart::DiffStats {
            added: 1,
            deleted: 2,
        }
    ));
    assert!(!streamed_model_part_affects_diff(
        ContextId::Files,
        &ModelPart::Head {
            hash: "deadbeef".to_string(),
            branch_name: "main".to_string(),
        }
    ));
    assert!(!streamed_model_part_affects_diff(
        ContextId::Files,
        &ModelPart::Tags(Vec::new())
    ));
}

#[test]
fn streamed_diff_sources_only_invalidate_their_own_context() {
    assert!(streamed_model_part_affects_diff(
        ContextId::Commits,
        &ModelPart::Commits(Vec::new())
    ));
    assert!(streamed_model_part_affects_diff(
        ContextId::Reflog,
        &ModelPart::Reflog(Vec::new())
    ));
    assert!(streamed_model_part_affects_diff(
        ContextId::Stash,
        &ModelPart::Stash(Vec::new())
    ));

    assert!(!streamed_model_part_affects_diff(
        ContextId::Commits,
        &ModelPart::UnpushedCommitHashes(Vec::new())
    ));
    assert!(!streamed_model_part_affects_diff(
        ContextId::Files,
        &ModelPart::Commits(Vec::new())
    ));
    assert!(!streamed_model_part_affects_diff(
        ContextId::BranchCommits,
        &ModelPart::Branches(Vec::new())
    ));
}

#[test]
fn split_commit_message_keeps_summary_and_body_separate() {
    assert_eq!(
        split_commit_message("feat: add editor\n\nExplain the change.\nKeep this line."),
        (
            "feat: add editor".to_string(),
            "Explain the change.\nKeep this line.".to_string()
        )
    );
}

#[test]
fn split_commit_message_handles_subject_only() {
    assert_eq!(
        split_commit_message("fix: subject only"),
        ("fix: subject only".to_string(), String::new())
    );
}

#[test]
fn keyboard_enhancement_preserves_terminal_text_input() {
    let flags = keyboard_enhancement_flags();

    assert_eq!(
        flags,
        crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
    );
    assert!(
        !flags
            .contains(crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES)
    );
    assert!(!flags.contains(crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
}

#[test]
fn plain_character_shortcuts_reject_extra_modifiers() {
    assert!(plain_char_key(
        KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT),
        'I'
    ));
    assert!(!plain_char_key(
        KeyEvent::new(
            KeyCode::Char('I'),
            KeyModifiers::SHIFT | KeyModifiers::SUPER
        ),
        'I'
    ));
    // Global reset picker (G) uses the same plain-char matching as I.
    assert!(plain_char_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        'G'
    ));
    assert!(!plain_char_key(
        KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::SHIFT | KeyModifiers::CONTROL
        ),
        'G'
    ));
    // Lowercase g must not match the global reset picker binding.
    assert!(!plain_char_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        'G'
    ));
}

#[test]
fn mouse_capture_does_not_request_passive_motion_events() {
    let mut ansi = String::new();
    EnableMouseCaptureWithoutHover
        .write_ansi(&mut ansi)
        .unwrap();

    assert!(ansi.contains("\x1b[?1000h"));
    assert!(ansi.contains("\x1b[?1002h"));
    assert!(ansi.contains("\x1b[?1006h"));
    assert!(!ansi.contains("\x1b[?1003h"));
}

#[test]
fn latest_background_worker_coalesces_rapid_jobs() {
    let (job_tx, job_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    spawn_latest_background_worker(job_rx);

    for value in 1..=3 {
        let done_tx = done_tx.clone();
        job_tx
            .send(Box::new(move || {
                done_tx.send(value).unwrap();
            }))
            .unwrap();
    }

    assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 3);
    assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn commit_stat_prefetch_prioritizes_forward_navigation() {
    assert_eq!(
        commit_stat_prefetch_indices(12, 3),
        vec![4, 5, 6, 7, 8, 9, 2, 1]
    );
    assert_eq!(commit_stat_prefetch_indices(4, 3), vec![2, 1]);
    assert_eq!(commit_stat_prefetch_indices(1, 0), Vec::<usize>::new());
}

#[test]
fn commit_stat_prefetch_workers_return_results() {
    let repo = TempRepo::new("commit-stat-prefetch");
    let hash = repo.commit("file", "prefetch");
    let git = Arc::new(crate::git::GitCommands::new(repo.path()).unwrap());
    let (job_tx, job_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    spawn_commit_stat_prefetch_workers(job_rx, result_tx, Arc::clone(&git));

    job_tx
        .send(CommitStatPrefetchJob {
            generation: 7,
            hash: hash.clone(),
        })
        .unwrap();

    let result = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(result.generation, 7);
    assert_eq!(result.hash, hash);
    assert!(result.result.is_ok());
}

#[test]
fn diff_scheduler_starts_immediately_and_keeps_latest_overflow_job() {
    let (scheduler_tx, scheduler_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let generation = Arc::new(AtomicU64::new(1));
    let executed = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    spawn_diff_scheduler(
        scheduler_rx,
        scheduler_tx.clone(),
        result_tx,
        Arc::clone(&generation),
    );

    for value in 1..=3 {
        let executed = Arc::clone(&executed);
        let release_rx = Arc::clone(&release_rx);
        scheduler_tx
            .send(DiffSchedulerEvent::Job(DiffJob {
                generation: 1,
                diff_key: format!("commit:{value}"),
                load: Box::new(move || {
                    executed.fetch_or(1 << value, Ordering::Relaxed);
                    release_rx.lock().unwrap().recv().unwrap();
                    DiffPayload::Empty
                }),
            }))
            .unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(1);
    while executed.load(Ordering::Relaxed).count_ones() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(executed.load(Ordering::Relaxed).count_ones(), 2);

    release_tx.send(()).unwrap();
    let _ = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while executed.load(Ordering::Relaxed) & (1 << 3) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(executed.load(Ordering::Relaxed) & (1 << 3), 0);

    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
}

#[test]
fn diff_preview_cache_moves_recent_views_and_enforces_entry_limit() {
    let mut cache = DiffPreviewCache::default();
    for index in 0..=DIFF_PREVIEW_CACHE_ENTRIES {
        let mut view = DiffViewState::new();
        view.filename = format!("file-{index}");
        view.lines.push(crate::pager::DiffLine {
            old_line: Some((1, "old".to_string())),
            new_line: Some((1, "new".to_string())),
            change_type: crate::pager::ChangeType::Modified,
            old_segments: None,
            new_segments: None,
            file_header: None,
            preview_placeholder: None,
            section_index: 0,
        });
        cache.insert(format!("key-{index}"), view);
    }

    assert_eq!(cache.entries.len(), DIFF_PREVIEW_CACHE_ENTRIES);
    assert!(cache.take("key-0").is_none());
    let restored = cache.take(&format!("key-{DIFF_PREVIEW_CACHE_ENTRIES}"));
    assert_eq!(
        restored.map(|view| view.filename),
        Some(format!("file-{DIFF_PREVIEW_CACHE_ENTRIES}"))
    );
}

#[test]
fn immutable_diff_keys_are_hash_scoped_only() {
    assert!(diff_key_is_immutable("Commits:abc123"));
    assert!(diff_key_is_immutable("Reflog:abc123"));
    assert!(diff_key_is_immutable("BranchCommits:abc123"));
    assert!(diff_key_is_immutable("Stash:abc123"));
    // Working-tree and ref-relative diffs can go stale on refresh.
    assert!(!diff_key_is_immutable("Files:file:src/main.rs"));
    assert!(!diff_key_is_immutable(
        "DiffMode:main..dev:file:src/main.rs"
    ));
    // Prefix cousins must not ride along.
    assert!(!diff_key_is_immutable("CommitFiles:abc:file:src/main.rs"));
    assert!(!diff_key_is_immutable("StashFiles:abc:file:src/main.rs"));
    assert!(!diff_key_is_immutable(
        "BranchCommitFiles:abc:file:src/main.rs"
    ));
}

#[test]
fn retain_immutable_keeps_commit_diffs_and_recomputes_bytes() {
    let mut cache = DiffPreviewCache::default();
    for key in ["Commits:abc", "Files:file:a.rs", "Stash:def"] {
        let mut view = DiffViewState::new();
        view.filename = key.to_string();
        view.lines.push(crate::pager::DiffLine {
            old_line: Some((1, "old".to_string())),
            new_line: Some((1, "new".to_string())),
            change_type: crate::pager::ChangeType::Modified,
            old_segments: None,
            new_segments: None,
            file_header: None,
            preview_placeholder: None,
            section_index: 0,
        });
        cache.insert(key.to_string(), view);
    }

    cache.retain_immutable();

    assert!(cache.contains("Commits:abc"));
    assert!(cache.contains("Stash:def"));
    assert!(!cache.contains("Files:file:a.rs"));
    let expected: usize = cache.entries.iter().map(|e| e.estimated_bytes).sum();
    assert_eq!(cache.estimated_bytes, expected);
}

#[test]
fn file_status_refresh_invalidates_stale_files_previews() {
    let repo = TempRepo::new("files-preview-invalidation");
    let mut gui = tree_test_gui(&repo);

    let mut stale_file = DiffViewState::new();
    stale_file.load("src/deep/a.txt", "base\n", "changed\n");
    gui.diff_preview_cache
        .insert("Files:file:src/deep/a.txt".into(), stale_file);

    let mut stale_dir = DiffViewState::new();
    stale_dir.load("src", "base\n", "changed\n");
    gui.diff_preview_cache
        .insert("Files:dir:src".into(), stale_dir);

    let mut immutable_commit = DiffViewState::new();
    immutable_commit.load("commit.txt", "old\n", "new\n");
    gui.diff_preview_cache
        .insert("Commits:deadbeef".into(), immutable_commit);

    // Simulate the stale Files preview that is still visible while the
    // newly-selected file is loading. Once status refreshes, it must not
    // be stashed back into the revisit cache under its old identity.
    gui.displayed_diff_key = "Files:file:src/deep/b.txt".into();
    gui.diff_view.load("src/deep/b.txt", "base\n", "changed\n");

    gui.invalidate_files_diff_previews();

    assert!(!gui.diff_preview_cache.contains("Files:file:src/deep/a.txt"));
    assert!(!gui.diff_preview_cache.contains("Files:dir:src"));
    assert!(gui.diff_preview_cache.contains("Commits:deadbeef"));
    assert!(gui.displayed_diff_key.is_empty());
    assert!(!gui.diff_view.is_empty());

    gui.stash_displayed_diff();
    assert!(!gui.diff_preview_cache.contains("Files:file:src/deep/b.txt"));
}

#[test]
fn prefetch_workers_always_deliver_a_result() {
    // begin_diff_request skips spawning an interactive job while a
    // prefetch for the key is in flight, so a lost result would leave the
    // pane loading forever.
    let (job_tx, job_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    spawn_diff_prefetch_workers(job_rx, result_tx);

    for index in 0..8 {
        job_tx
            .send(DiffPrefetchJob {
                diff_key: format!("Commits:{index}"),
                load: Box::new(|| DiffPayload::Empty),
            })
            .unwrap();
    }

    let mut keys = HashSet::new();
    for _ in 0..8 {
        let result = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(result.is_prefetch);
        keys.insert(result.diff_key);
    }
    assert_eq!(keys.len(), 8);
}

fn wait_for_shell_command(gui: &mut Gui) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while gui.shell_command_job.is_some() {
        controller::custom_commands::receive_command_result(gui);
        assert!(Instant::now() < deadline, "shell job did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start_test_custom_command(gui: &mut Gui, command: &str, show_output: bool) {
    let config = Arc::get_mut(&mut gui.config).unwrap();
    config.user_config.custom_commands = vec![crate::config::user_config::CustomCommand {
        key: "<c-x>".into(),
        command: command.into(),
        show_output,
        ..Default::default()
    }];
    gui.handle_key(parse_key("<c-x>").unwrap()).unwrap();
}

#[test]
fn shell_prompt_binding_works_in_lists_and_diff_focus_and_respects_remapping() {
    let repo = TempRepo::new("shell-prompt-bindings");
    let mut gui = tree_test_gui(&repo);
    for context in [
        ContextId::Files,
        ContextId::Commits,
        ContextId::CommitFiles,
        ContextId::StashFiles,
        ContextId::Branches,
    ] {
        gui.context_mgr.set_active(context);
        for focused in [false, true] {
            gui.diff_focused = focused;
            for binding in [":", "q", "?", "<c-x>", "<tab>"] {
                Arc::get_mut(&mut gui.config)
                    .unwrap()
                    .user_config
                    .keybinding
                    .universal
                    .custom_command_prompt = binding.into();
                gui.handle_key(parse_key(binding).unwrap()).unwrap();
                assert!(
                    matches!(gui.popup, PopupState::ShellCommand { .. }),
                    "{context:?}, {focused}, {binding}"
                );
                assert!(!gui.should_quit);
                gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
            }
        }
    }
    Arc::get_mut(&mut gui.config)
        .unwrap()
        .user_config
        .keybinding
        .universal
        .custom_command_prompt
        .clear();
    gui.diff_focused = false;
    gui.context_mgr.set_active(ContextId::Files);
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    assert!(gui.popup == PopupState::None);
    gui.show_command_palette();
    let PopupState::CommandPalette { sections, .. } = &gui.popup else {
        panic!("palette");
    };
    assert!(
        !sections
            .iter()
            .flat_map(|s| &s.entries)
            .any(|e| e.description == "Execute shell command")
    );
}

#[test]
fn shell_prompt_does_not_steal_search_or_popup_input() {
    let repo = TempRepo::new("shell-text-priority");
    let mut gui = tree_test_gui(&repo);
    gui.search_active = true;
    gui.search_textarea = Some(popup::make_textarea(""));
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    assert!(gui.popup == PopupState::None);
    assert_eq!(gui.search_query, ":");
    gui.search_active = false;
    gui.diff_focused = true;
    gui.diff_view.search_active = true;
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    assert!(gui.popup == PopupState::None);
    gui.diff_view.search_active = false;
    gui.popup = PopupState::Input {
        title: "Other input".into(),
        textarea: popup::make_textarea(""),
        on_confirm: Box::new(|_, _| Ok(())),
        is_commit: false,
        confirm_focused: false,
    };
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    assert!(
        matches!(&gui.popup, PopupState::Input { textarea, .. } if textarea.lines().join("") == ":")
    );
}

#[test]
fn shell_prompt_keeps_literal_paste_through_resize_and_blank_submit_is_noop() {
    let repo = TempRepo::new("shell-literal-input");
    let mut gui = tree_test_gui(&repo);
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    let command = "printf '%s' 'multiple   spaces; $quoted'\n# pasted newline";
    gui.handle_paste(command.into());
    gui.handle_resize(25, 8);
    assert!(
        matches!(&gui.popup, PopupState::ShellCommand { textarea } if textarea.lines().join("\n") == command)
    );
    gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
    assert!(gui.shell_command_job.is_none());
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    gui.handle_paste("  ".into());
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    assert!(gui.popup == PopupState::None);
    assert!(gui.shell_command_job.is_none());
}

#[test]
fn shell_modal_defers_refresh_errors_and_rebase_notifications_in_order() {
    for phase in 0..3 {
        let repo = TempRepo::new("shell-refresh-notification-ownership");
        let mut gui = tree_test_gui(&repo);
        match phase {
            0 => {
                gui.handle_key(parse_key(":").unwrap()).unwrap();
                gui.handle_paste("printf typed-command".into());
            }
            1 => start_test_custom_command(&mut gui, "printf shell-result", true),
            _ => {
                gui.popup = PopupState::CommandOutput {
                    title: "Command output".into(),
                    message: "shell-result".into(),
                    kind: MessageKind::Info,
                    scroll: 0,
                };
            }
        }
        let (tx, rx) = mpsc::channel();
        gui.files_refresh_rx = Some(rx);
        gui.files_refresh_in_progress = true;
        tx.send(Err(anyhow::anyhow!("refresh fixture failure")))
            .unwrap();
        gui.receive_files_refresh();
        assert!(!gui.files_refresh_in_progress);
        assert!(gui.files_refresh_rx.is_none());
        // Repeated auto-refresh failures are coalesced while the shell is open.
        gui.show_error("Refresh failed", anyhow::anyhow!("refresh fixture failure"));
        assert_eq!(gui.deferred_notifications.len(), 1);
        gui.rebase_mode.active = true;
        gui.rebase_mode.phase = RebasePhase::InProgress;
        gui.rebase_mode.branch_name = "main".into();
        gui.rebase_mode.total_count = 3;
        gui.model.lock().unwrap().is_rebasing = false;
        gui.after_model_refresh().unwrap();
        assert!(
            !gui.rebase_mode.active,
            "model/view refresh must still apply"
        );
        assert_eq!(gui.deferred_notifications.len(), 2);
        gui.remote_op_label = Some("Fetch".into());
        gui.remote_op_tx
            .send((
                RemoteOpKind::Fetch,
                Err(anyhow::anyhow!("queued fetch failure")),
            ))
            .unwrap();
        gui.receive_deferred_notifications();
        gui.receive_remote_op_results();
        match phase {
            0 => assert!(matches!(&gui.popup, PopupState::ShellCommand { textarea }
                if textarea.lines().join("\n") == "printf typed-command")),
            1 => {
                assert!(matches!(gui.popup, PopupState::Loading { .. }));
                wait_for_shell_command(&mut gui);
                gui.receive_deferred_notifications();
                assert!(
                    matches!(&gui.popup, PopupState::CommandOutput { message, .. }
                    if message.contains("shell-result"))
                );
            }
            _ => assert!(
                matches!(&gui.popup, PopupState::CommandOutput { message, .. }
                if message == "shell-result")
            ),
        }
        gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
        gui.receive_deferred_notifications();
        assert!(
            matches!(&gui.popup, PopupState::Message { title, message, .. }
            if title == "Refresh failed" && message == "refresh fixture failure")
        );
        gui.receive_remote_op_results();
        assert!(gui.remote_op_label.is_some());
        gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
        gui.receive_deferred_notifications();
        assert!(
            matches!(&gui.popup, PopupState::Message { title, message, .. }
            if title == "Rebase ended" && message.contains("completed or been aborted"))
        );
        gui.receive_remote_op_results();
        assert!(gui.remote_op_label.is_some());
        gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
        gui.receive_deferred_notifications();
        gui.receive_remote_op_results();
        assert!(matches!(&gui.popup, PopupState::Message { message, .. }
            if message == "queued fetch failure"));
        assert!(gui.deferred_notifications.is_empty());
    }
}

#[test]
fn shell_modal_defers_filter_picker_and_notifications_defer_ai_results() {
    let repo = TempRepo::new("shell-filter-and-notification-ai");
    let mut gui = tree_test_gui(&repo);
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    let (tx, rx) = mpsc::channel();
    gui.filter_paths_rx = Some(rx);
    tx.send(Ok(vec!["src".into()])).unwrap();
    gui.receive_filter_paths();
    assert!(matches!(gui.popup, PopupState::ShellCommand { .. }));
    assert!(gui.filter_paths_rx.is_some());
    gui.show_error("Refresh failed", anyhow::anyhow!("fixture failure"));
    gui.ai_commit_job = Some(AiCommitJob {
        generation: 1,
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_armed_at: Some(Instant::now()),
    });
    gui.ai_commit_tx
        .send(AiCommitResult {
            generation: 1,
            result: Err(anyhow::anyhow!("AI fixture failure")),
        })
        .unwrap();
    gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
    gui.receive_deferred_notifications();
    gui.receive_ai_commit_results();
    gui.receive_filter_paths();
    assert!(matches!(&gui.popup, PopupState::Message { title, .. }
        if title == "Refresh failed"));
    assert!(gui.ai_commit_job.is_some());
    assert!(gui.filter_paths_rx.is_some());
    gui.commit_page_loading = true;
    gui.commit_page_tx
        .send(CommitPageResult {
            generation: gui.commit_page_generation,
            replace: false,
            prepend: false,
            skip: 0,
            limit: DEFAULT_COMMIT_LIMIT,
            result: Err(anyhow::anyhow!("page fixture failure")),
        })
        .unwrap();
    gui.receive_commit_page_results();
    assert!(!gui.commit_page_loading);
    assert_eq!(gui.deferred_notifications.len(), 1);
    // Notification Escape is modal too, not the second half of AI cancel.
    gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
    assert!(gui.ai_commit_job.is_some());
    gui.receive_deferred_notifications();
    gui.receive_ai_commit_results();
    assert!(matches!(&gui.popup, PopupState::Message { title, .. }
        if title == "Commits"));
    assert!(gui.ai_commit_job.is_some());
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    gui.receive_deferred_notifications();
    gui.receive_ai_commit_results();
    assert!(matches!(&gui.popup, PopupState::Message { title, .. }
        if title == "AI generation failed"));
    assert!(gui.ai_commit_job.is_none());
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    gui.receive_filter_paths();
    assert!(matches!(&gui.popup, PopupState::ListPicker { core, .. }
        if core.items.iter().any(|item| item.value == "src")));
}

#[test]
fn shell_completion_keeps_output_and_defers_invalid_comparison_error() {
    let repo = TempRepo::new("shell-comparison-refresh-error");
    let mut gui = tree_test_gui(&repo);
    start_test_custom_command(&mut gui, "printf shell-result", true);
    gui.diff_mode.enter(true);
    gui.diff_mode.ref_a = "nonexistent-ref".into();
    gui.diff_mode.ref_b = "HEAD".into();
    wait_for_shell_command(&mut gui);
    assert!(
        matches!(&gui.popup, PopupState::CommandOutput { message, .. }
        if message == "shell-result")
    );
    assert_eq!(gui.deferred_notifications.len(), 1);
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    gui.receive_deferred_notifications();
    assert!(matches!(&gui.popup, PopupState::Message { title, kind, .. }
        if title == "Diff error" && *kind == MessageKind::Error));
}

#[test]
fn shell_popup_escape_does_not_arm_or_cancel_ai_generation() {
    let repo = TempRepo::new("shell-ai-escape-priority");
    let mut gui = tree_test_gui(&repo);
    for output in [false, true] {
        for armed in [false, true] {
            let cancel = Arc::new(AtomicBool::new(false));
            gui.ai_commit_job = Some(AiCommitJob {
                generation: 1,
                cancel: Arc::clone(&cancel),
                cancel_armed_at: armed.then(Instant::now),
            });
            let original_armed = gui.ai_commit_job.as_ref().unwrap().cancel_armed_at;
            gui.pending_commit_popup = Some(PopupState::None);
            if output {
                gui.popup = PopupState::CommandOutput {
                    title: "Command output".into(),
                    message: "done".into(),
                    kind: MessageKind::Info,
                    scroll: 0,
                };
            } else {
                gui.handle_key(parse_key(":").unwrap()).unwrap();
            }
            gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
            assert!(gui.popup == PopupState::None);
            assert!(!cancel.load(Ordering::Relaxed));
            assert_eq!(
                gui.ai_commit_job.as_ref().unwrap().cancel_armed_at,
                original_armed
            );
            assert!(gui.pending_commit_popup.is_some());
            assert!(gui.saved_commit_popup.is_none());
            gui.pending_commit_popup = None;
            gui.ai_commit_job = None;
        }
    }
}

#[test]
fn shell_modal_defers_ctrl_g_result_without_losing_command_or_commit_draft() {
    let repo = TempRepo::new("shell-ai-popup-ownership");
    let mut gui = tree_test_gui(&repo);
    repo.git(&["add", "."]);
    gui.model.lock().unwrap().files = gui.git.load_files().unwrap();
    gui.handle_key(parse_key("c").unwrap()).unwrap();
    assert!(matches!(gui.popup, PopupState::CommitInput { .. }));
    gui.pending_commit_popup = Some(std::mem::replace(&mut gui.popup, PopupState::None));
    // Deterministic completion of a Ctrl-G job, without an external agent.
    gui.ai_commit_job = Some(AiCommitJob {
        generation: 1,
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_armed_at: None,
    });
    gui.ai_commit_tx
        .send(AiCommitResult {
            generation: 1,
            result: Ok(Some("feat: generated summary\n\nGenerated body".into())),
        })
        .unwrap();
    gui.remote_op_tx
        .send((
            RemoteOpKind::Fetch,
            Err(anyhow::anyhow!("concurrent fetch failed")),
        ))
        .unwrap();
    gui.handle_key(parse_key(":").unwrap()).unwrap();
    gui.handle_paste("printf shell-result".into());
    gui.receive_ai_commit_results();
    gui.receive_remote_op_results();
    gui.receive_remote_op_results();
    assert!(matches!(&gui.popup, PopupState::ShellCommand { textarea }
        if textarea.lines().join("\n") == "printf shell-result"));
    assert!(gui.pending_commit_popup.is_some());
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    gui.receive_ai_commit_results();
    gui.receive_remote_op_results();
    assert!(matches!(gui.popup, PopupState::Loading { .. }));
    wait_for_shell_command(&mut gui);
    gui.receive_ai_commit_results();
    assert!(
        matches!(&gui.popup, PopupState::CommandOutput { message, .. }
        if message.contains("shell-result"))
    );
    assert!(gui.pending_commit_popup.is_some());
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    gui.receive_ai_commit_results();
    assert!(gui.pending_commit_popup.is_none());
    assert!(gui.ai_commit_job.is_none());
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        summary_textarea, body_state, ..
    } if summary_textarea.lines().join("") == "feat: generated summary"
        && body_state.raw() == "Generated body"));
    // This is the main-loop receiver order after shell output closes.
    gui.receive_remote_op_results();
    assert!(matches!(&gui.popup, PopupState::Message { message, .. }
        if message.contains("concurrent fetch failed")));
    assert!(
        matches!(&gui.saved_commit_popup, Some(PopupState::CommitInput {
        summary_textarea, body_state, ..
    }) if summary_textarea.lines().join("") == "feat: generated summary"
        && body_state.raw() == "Generated body")
    );
    gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
    gui.handle_key(parse_key("c").unwrap()).unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { summary_textarea, .. }
        if summary_textarea.lines().join("") == "feat: generated summary")
    );
}

#[test]
fn shell_modal_defers_remote_error_until_output_is_dismissed() {
    let repo = TempRepo::new("shell-remote-popup-ownership");
    let mut gui = tree_test_gui(&repo);
    gui.remote_op_label = Some("Fetch".into());
    gui.remote_op_tx
        .send((RemoteOpKind::Fetch, Err(anyhow::anyhow!("fetch failed"))))
        .unwrap();
    start_test_custom_command(&mut gui, "printf shell-result", true);
    gui.receive_remote_op_results();
    assert!(matches!(gui.popup, PopupState::Loading { .. }));
    wait_for_shell_command(&mut gui);
    gui.receive_remote_op_results();
    assert!(
        matches!(&gui.popup, PopupState::CommandOutput { message, .. }
        if message.contains("shell-result"))
    );
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    gui.receive_remote_op_results();
    assert!(matches!(&gui.popup, PopupState::Message { message, .. }
        if message.contains("fetch failed")));
    assert!(gui.remote_op_label.is_none());
}

#[test]
fn shell_popups_capture_mouse_without_changing_underlying_navigation() {
    use crate::gui::modes::diff_mode::DiffModeFocus;
    use crossterm::event::{MouseButton, MouseEventKind};

    let repo = TempRepo::new("shell-modal-mouse");
    let mut gui = tree_test_gui(&repo);
    Arc::get_mut(&mut gui.config)
        .unwrap()
        .user_config
        .gui
        .mouse_events = true;
    gui.layout.update_size(100, 30);
    for compare in [false, true] {
        if compare {
            gui.diff_mode.enter(true);
            gui.diff_mode.set_focus(DiffModeFocus::Commits);
        }
        let focus = gui.diff_mode.focus;
        let context = gui.context_mgr.active();
        let selection = gui.context_mgr.selected_active();
        let diff_scroll = gui.diff_view.scroll_offset;
        for output in [false, true] {
            let message = (0..100).map(|i| format!("line {i}\n")).collect::<String>();
            gui.popup = if output {
                PopupState::CommandOutput {
                    title: "output".into(),
                    message: message.clone(),
                    kind: MessageKind::Info,
                    scroll: 0,
                }
            } else {
                let mut textarea = popup::make_textarea("");
                textarea.insert_str("typed command");
                PopupState::ShellCommand { textarea }
            };
            for (column, row) in [(1, 1), (50, 12), (99, 29)] {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Drag(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                    MouseEventKind::ScrollDown,
                ] {
                    gui.handle_mouse(MouseEvent {
                        kind,
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    });
                }
            }
            if output {
                assert!(matches!(
                    gui.popup,
                    PopupState::CommandOutput { scroll: 9, .. }
                ));
                for _ in 0..100 {
                    gui.handle_mouse(MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        column: 50,
                        row: 12,
                        modifiers: KeyModifiers::NONE,
                    });
                }
                let max = views::command_output_max_scroll(&message, Rect::new(0, 0, 100, 30));
                assert!(
                    matches!(gui.popup, PopupState::CommandOutput { scroll, .. } if scroll == max)
                );
                gui.handle_mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 50,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                });
                assert!(
                    matches!(gui.popup, PopupState::CommandOutput { scroll, .. } if scroll == max.saturating_sub(3))
                );
            } else {
                assert!(matches!(&gui.popup, PopupState::ShellCommand { textarea }
                    if textarea.lines().join("") == "typed command"));
            }
            assert_eq!(gui.context_mgr.active(), context);
            assert_eq!(gui.context_mgr.selected_active(), selection);
            assert_eq!(gui.diff_view.scroll_offset, diff_scroll);
            assert_eq!(gui.diff_mode.focus, focus);
            assert!(gui.diff_mode.editing.is_none());
            assert!(!gui.sidebar_resizing);
        }
    }
}

#[test]
fn custom_command_is_nonblocking_cancelable_and_refreshes_after_cancellation() {
    let repo = TempRepo::new("shell-cancellation");
    let mut gui = tree_test_gui(&repo);
    let started = Instant::now();
    start_test_custom_command(&mut gui, "sleep 30", true);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(gui.shell_command_job.is_some());
    assert_eq!(gui.input_wait_timeout(), Duration::from_millis(16));
    gui.handle_key(parse_key("q").unwrap()).unwrap();
    assert!(!gui.should_quit);
    gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
    wait_for_shell_command(&mut gui);
    assert!(gui.needs_refresh);
    assert!(gui.needs_diff_refresh);
    assert!(
        matches!(&gui.popup, PopupState::CommandOutput { title, kind: MessageKind::Error, .. } if title == "Command cancelled")
    );
}

#[test]
fn custom_command_logs_and_reports_failure_stdout_and_stderr_without_losing_filters() {
    let repo = TempRepo::new("shell-output");
    let mut gui = tree_test_gui(&repo);
    gui.commit_path_filter = Some("src".into());
    gui.commit_author_filter = vec!["Example".into()];
    gui.screen_mode = ScreenMode::Half;
    start_test_custom_command(
        &mut gui,
        "printf partial; printf warning >&2; exit 7",
        false,
    );
    wait_for_shell_command(&mut gui);
    assert!(
        matches!(&gui.popup, PopupState::CommandOutput { title, message, kind: MessageKind::Error, .. }
        if title.contains("exit 7") && message.contains("partial") && message.contains("warning"))
    );
    let log = gui.command_log.lock().unwrap();
    assert!(log.iter().any(|line| line.contains("$ printf partial")));
    assert!(
        log.iter()
            .any(|line| line.contains("Command failed (exit 7)"))
    );
    drop(log);
    assert_eq!(gui.commit_path_filter.as_deref(), Some("src"));
    assert_eq!(gui.commit_author_filter, vec!["Example"]);
    assert_eq!(gui.screen_mode, ScreenMode::Half);
    gui.handle_key(parse_key("<esc>").unwrap()).unwrap();
    start_test_custom_command(&mut gui, "printf changed > created-by-command", false);
    wait_for_shell_command(&mut gui);
    assert!(gui.popup == PopupState::None);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("created-by-command")).unwrap(),
        "changed"
    );
    assert!(gui.needs_refresh);
}

#[test]
fn command_output_can_scroll_to_last_line_and_back() {
    let repo = TempRepo::new("shell-scroll");
    let mut gui = tree_test_gui(&repo);
    gui.layout.update_size(80, 24);
    let message = (0..100).map(|i| format!("line {i}\n")).collect::<String>();
    gui.popup = PopupState::CommandOutput {
        title: "output".into(),
        message: message.clone(),
        kind: MessageKind::Info,
        scroll: 0,
    };
    gui.handle_key(parse_key("G").unwrap()).unwrap();
    let max = views::command_output_max_scroll(&message, Rect::new(0, 0, 80, 24));
    assert!(matches!(gui.popup, PopupState::CommandOutput { scroll, .. } if scroll == max));
    gui.handle_key(parse_key("g").unwrap()).unwrap();
    assert!(matches!(
        gui.popup,
        PopupState::CommandOutput { scroll: 0, .. }
    ));
    gui.handle_key(parse_key("j").unwrap()).unwrap();
    assert!(matches!(
        gui.popup,
        PopupState::CommandOutput { scroll: 1, .. }
    ));
    gui.handle_key(parse_key("<enter>").unwrap()).unwrap();
    assert!(gui.popup == PopupState::None);
}

#[test]
fn shell_completion_preserves_compare_commit_files_tree_search_and_focus() {
    use crate::gui::modes::diff_mode::DiffModeFocus;

    let repo = TempRepo::new("shell-compare-refresh");
    let mut gui = tree_test_gui(&repo);
    // Both configured commands and shell prompts use this result path.
    start_test_custom_command(&mut gui, "printf refreshed", true);
    gui.diff_mode.enter(true);
    gui.diff_mode.ref_a = "HEAD~2".into();
    gui.diff_mode.ref_b = "HEAD".into();
    controller::diff_mode::reload_diff_files(&mut gui).unwrap();
    gui.diff_mode.commits_selected = 1;
    controller::diff_mode::open_selected_commit_files(&mut gui).unwrap();
    let commit = gui.diff_mode.files_commit.clone();
    gui.diff_mode.collapsed_dirs.insert("src".into());
    controller::diff_mode::update_diff_mode_tree(&mut gui);
    gui.diff_mode.file_search_query = "src".into();
    gui.diff_mode.set_focus(DiffModeFocus::DiffExploration);
    let source = gui.diff_mode.diff_source;
    let files = gui.diff_mode.diff_files.clone();
    wait_for_shell_command(&mut gui);
    assert_eq!(gui.diff_mode.focus, DiffModeFocus::DiffExploration);
    assert_eq!(gui.diff_mode.sidebar_focus, DiffModeFocus::CommitFiles);
    assert_eq!(gui.diff_mode.diff_source, source);
    assert_eq!(gui.diff_mode.files_commit, commit);
    assert_eq!(gui.diff_mode.diff_files.len(), files.len());
    for (actual, expected) in gui.diff_mode.diff_files.iter().zip(files) {
        assert_eq!(actual.name, expected.name);
    }
    assert!(gui.diff_mode.collapsed_dirs.contains("src"));
    assert_eq!(gui.diff_mode.file_search_query, "src");
    assert!(matches!(
        gui.popup,
        PopupState::CommandOutput {
            kind: MessageKind::Info,
            ..
        }
    ));
}

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lazygitrs-gui-test-{}-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        let _ = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&path)
            .output();
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(&self.path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn commit(&self, path: &str, contents: &str) -> String {
        std::fs::write(self.path.join(path), contents).unwrap();
        self.git(&["add", path]);
        self.git(&["commit", "-m", contents]);
        self.git(&["rev-parse", "HEAD"])
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn checkout_test_gui(repo: &TempRepo) -> Gui {
    repo.commit("file", "main contents");
    repo.git(&["checkout", "-b", "feature"]);
    repo.commit("file", "feature contents");
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    // Finish startup loading before exercising checkout and its refresh.
    for _ in gui.initial_load_rx.take().unwrap() {}
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.context_mgr.set_active(ContextId::Branches);
    gui
}

fn request_test_checkout(gui: &mut Gui, key: char, target: &str) {
    let selected = gui
        .model
        .lock()
        .unwrap()
        .branches
        .iter()
        .position(|b| b.name == target)
        .unwrap_or(0);
    gui.context_mgr.set_selection(selected);
    gui.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
        .unwrap();
    if key == 'c' {
        let PopupState::RefPicker { core, .. } = &mut gui.popup else {
            panic!("expected checkout ref picker");
        };
        core.search_textarea = popup::make_command_palette_search_textarea();
        core.search_textarea.insert_str(target);
        core.selected = core
            .items
            .iter()
            .position(|i| i.value == target)
            .unwrap_or(core.items.len());
        gui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
    }
}

fn wait_for_test_checkout(gui: &mut Gui) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while gui.remote_op_label.is_some() {
        assert!(Instant::now() < deadline, "checkout did not finish");
        gui.receive_remote_op_results();
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "manual checkout visibility benchmark; timing is machine-dependent"]
fn checkout_visibility_latency() {
    for auto_refresh in [true, false] {
        let repo = TempRepo::new("checkout-latency");
        let mut gui = checkout_test_gui(&repo);
        let mut config = crate::config::AppConfig::default();
        config.user_config.git.auto_refresh = auto_refresh;
        gui.config = Arc::new(config);
        let mut samples = Vec::new();
        for target in ["main", "feature"].into_iter().cycle().take(10) {
            let start = Instant::now();
            request_test_checkout(&mut gui, ' ', target);
            loop {
                if let Some(rx) = &gui.initial_load_rx {
                    while let Ok(part) = rx.try_recv() {
                        let mut model = gui.model.lock().unwrap();
                        match part {
                            ModelPart::Branches(branches) => model.branches = branches,
                            ModelPart::Head { hash, branch_name } => {
                                model.head_hash = hash;
                                model.head_branch_name = branch_name;
                            }
                            _ => {}
                        }
                    }
                }
                gui.receive_remote_op_results();
                {
                    let model = gui.model.lock().unwrap();
                    if model.head_branch_name == target
                        && model
                            .branches
                            .first()
                            .is_some_and(|b| b.head && b.name == target)
                    {
                        samples.push(start.elapsed().as_secs_f64() * 1000.0);
                        break;
                    }
                }
                assert!(start.elapsed() < Duration::from_secs(10));
                // Mirror the main loop's input wait before dispatching refresh.
                std::thread::sleep(gui.input_wait_timeout());
                if gui.needs_refresh && !gui.refresh_in_progress && gui.initial_load_rx.is_none() {
                    gui.start_background_refresh();
                }
            }
            if let Some(rx) = gui.initial_load_rx.take() {
                for _ in rx {}
            }
            gui.refresh_in_progress = false;
            gui.needs_refresh = false;
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "auto_refresh={auto_refresh}: median={:.1}ms, min={:.1}ms, max={:.1}ms (10 checkouts)",
            (samples[4] + samples[5]) / 2.0,
            samples[0],
            samples[9]
        );
    }
}

fn assert_checkout_head(gui: &Gui, branch: &str, hash: &str) {
    let model = gui.model.lock().unwrap();
    let heads: Vec<_> = model
        .branches
        .iter()
        .filter(|b| b.head)
        .map(|b| b.name.as_str())
        .collect();
    assert_eq!(heads, vec![branch]);
    assert_eq!(model.head_branch_name, branch);
    assert_eq!(model.head_hash, hash);
    assert_eq!(gui.git.current_branch_name().unwrap(), branch);
    assert_eq!(gui.git.head_hash().unwrap(), hash);
}

#[test]
fn failed_checkout_preserves_head_after_error_dismissal_and_retry() {
    for key in [' ', '-', 'c'] {
        let repo = TempRepo::new("failed-checkout");
        let mut gui = checkout_test_gui(&repo);
        let head = repo.git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.path().join("file"), "local changes").unwrap();

        for _ in 0..2 {
            request_test_checkout(&mut gui, key, "main");
            assert_eq!(gui.remote_op_label.as_deref(), Some("Checking out"));
            wait_for_test_checkout(&mut gui);
            assert!(matches!(
                &gui.popup,
                PopupState::Message { message, kind: MessageKind::Error, .. }
                    if message.contains("would be overwritten by checkout")
            ));
            assert_checkout_head(&gui, "feature", &head);
            assert!(gui.pending_checkout_by_name.is_none());
            gui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            assert!(matches!(gui.popup, PopupState::None));
            assert_checkout_head(&gui, "feature", &head);
            assert_eq!(
                std::fs::read_to_string(repo.path().join("file")).unwrap(),
                "local changes"
            );
        }
    }
}

#[test]
fn missing_checkout_ref_preserves_head_and_offers_branch_creation() {
    let repo = TempRepo::new("missing-checkout-ref");
    let mut gui = checkout_test_gui(&repo);
    let head = repo.git(&["rev-parse", "HEAD"]);
    request_test_checkout(&mut gui, 'c', "brand-new-branch");
    wait_for_test_checkout(&mut gui);
    assert_checkout_head(&gui, "feature", &head);
    assert!(matches!(
        &gui.popup,
        PopupState::Confirm { title, .. } if title == "Branch not found"
    ));
    gui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    gui.refresh().unwrap();
    assert_checkout_head(&gui, "brand-new-branch", &head);
}

#[test]
fn tag_checkout_updates_detached_head_before_refresh() {
    let repo = TempRepo::new("tag-checkout");
    let mut gui = checkout_test_gui(&repo);
    let head = repo.git(&["rev-parse", "main"]);
    repo.git(&["tag", "release", "main"]);
    gui.model.lock().unwrap().tags = gui.git.load_tags().unwrap();
    request_test_checkout(&mut gui, 'c', "release");
    wait_for_test_checkout(&mut gui);
    assert!(gui.refresh_in_progress);
    assert!(matches!(gui.popup, PopupState::None));
    let model = gui.model.lock().unwrap();
    assert!(!model.branches.iter().any(|b| b.head));
    assert!(model.head_branch_name.is_empty());
    assert_eq!(model.head_hash, head);
    assert_eq!(gui.git.head_hash().unwrap(), head);
    assert_eq!(repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD");
    drop(model);
    gui.refresh().unwrap();
    assert_eq!(gui.model.lock().unwrap().head_hash, head);
}

#[test]
fn successful_checkout_updates_head_before_refresh() {
    for key in [' ', '-', 'c'] {
        let repo = TempRepo::new("successful-checkout");
        let mut gui = checkout_test_gui(&repo);
        request_test_checkout(&mut gui, key, "main");
        let pending_head = gui
            .model
            .lock()
            .unwrap()
            .branches
            .iter()
            .find(|b| b.head)
            .map(|b| b.name.clone());
        wait_for_test_checkout(&mut gui);
        assert_eq!(pending_head.as_deref(), Some("feature"));
        assert!(gui.refresh_in_progress);
        assert!(gui.initial_load_rx.is_some());
        assert!(matches!(gui.popup, PopupState::None));
        assert!(gui.pending_checkout_by_name.is_none());
        {
            let model = gui.model.lock().unwrap();
            assert_eq!(
                model.branches.first().map(|branch| branch.name.as_str()),
                Some("main")
            );
        }
        assert_eq!(gui.context_mgr.selected(ContextId::Branches), 0);
        assert_eq!(gui.context_mgr.scroll_offset(ContextId::Branches), 0);
        assert_checkout_head(&gui, "main", &repo.git(&["rev-parse", "main"]));
        gui.refresh().unwrap();
        assert_checkout_head(&gui, "main", &repo.git(&["rev-parse", "main"]));
    }
}

#[test]
fn new_checkout_cancels_previous_checkout_refresh_immediately() {
    let repo = TempRepo::new("checkout-supersedes-refresh");
    let mut gui = checkout_test_gui(&repo);

    request_test_checkout(&mut gui, ' ', "main");
    wait_for_test_checkout(&mut gui);
    assert!(gui.refresh_in_progress);
    assert_eq!(gui.initial_load_expected, CHECKOUT_MODEL_PART_COUNT);
    assert!(gui.initial_load_rx.is_some());

    // Starting another checkout should disconnect the old targeted refresh
    // immediately, rather than keeping its workers alive until this second
    // checkout finishes.
    request_test_checkout(&mut gui, ' ', "feature");
    assert!(gui.remote_op_label.is_some());
    assert!(!gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_none());

    wait_for_test_checkout(&mut gui);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_checkout_head(&gui, "feature", &repo.git(&["rev-parse", "feature"]));
}

#[test]
fn checkout_discards_pre_checkout_refresh_results() {
    let repo = TempRepo::new("checkout-stale-refresh");
    let mut gui = checkout_test_gui(&repo);
    let old_head = repo.git(&["rev-parse", "HEAD"]);
    let (model_tx, model_rx) = mpsc::channel();
    let (files_tx, files_rx) = mpsc::channel();
    gui.initial_load_rx = Some(model_rx);
    gui.initial_load_received = 5;
    gui.refresh_in_progress = true;
    gui.files_refresh_rx = Some(files_rx);
    gui.files_refresh_in_progress = true;
    model_tx
        .send(ModelPart::Head {
            hash: old_head.clone(),
            branch_name: "feature".into(),
        })
        .unwrap();
    files_tx.send(Ok(Vec::new())).unwrap();

    request_test_checkout(&mut gui, ' ', "main");
    wait_for_test_checkout(&mut gui);
    assert_checkout_head(&gui, "main", &repo.git(&["rev-parse", "main"]));
    assert!(
        model_tx
            .send(ModelPart::Head {
                hash: old_head,
                branch_name: "feature".into(),
            })
            .is_err()
    );
    assert!(files_tx.send(Ok(Vec::new())).is_err());
    assert!(gui.files_refresh_rx.is_none());
    assert!(!gui.files_refresh_in_progress);
    assert!(gui.refresh_in_progress);
    assert_eq!(gui.initial_load_received, 0);
    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();
    assert_eq!(parts.len(), CHECKOUT_MODEL_PART_COUNT);
    assert!(parts.iter().any(|part| matches!(
        part, ModelPart::Head { branch_name, .. } if branch_name == "main"
    )));
    assert!(parts.iter().any(|part| matches!(
        part, ModelPart::Branches(branches)
            if branches.iter().any(|b| b.head && b.name == "main")
    )));
}

#[test]
fn failed_checkout_requeues_refresh_invalidated_at_checkout_start() {
    let repo = TempRepo::new("failed-checkout-refresh-recovery");
    let mut gui = checkout_test_gui(&repo);
    let (model_tx, model_rx) = mpsc::channel();
    gui.initial_load_rx = Some(model_rx);
    gui.initial_load_received = 3;
    gui.initial_load_expected = MODEL_PART_COUNT;
    gui.refresh_in_progress = true;

    std::fs::write(repo.path().join("file"), "local changes").unwrap();
    request_test_checkout(&mut gui, ' ', "main");
    assert!(gui.checkout_invalidated_refresh);
    wait_for_test_checkout(&mut gui);

    assert!(!gui.checkout_invalidated_refresh);
    assert!(!gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_none());
    assert!(gui.needs_refresh);
    assert!(
        model_tx
            .send(ModelPart::DiffStats {
                added: 1,
                deleted: 1
            })
            .is_err()
    );
}

#[test]
fn pending_refresh_keeps_input_polling_fast() {
    let repo = TempRepo::new("refresh-polling");
    let mut gui = checkout_test_gui(&repo);
    let mut config = crate::config::AppConfig::default();
    config.user_config.git.auto_refresh = false;
    gui.config = Arc::new(config);
    gui.needs_refresh = false;
    assert_eq!(gui.input_wait_timeout(), Duration::from_millis(200));
    gui.needs_refresh = true;
    assert_eq!(gui.input_wait_timeout(), Duration::from_millis(16));
}

#[test]
fn periodic_refresh_uses_targeted_model_parts() {
    let repo = TempRepo::new("periodic-targeted-refresh");
    let mut gui = checkout_test_gui(&repo);

    assert!(gui.start_background_periodic_refresh());
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, PERIODIC_MODEL_PART_COUNT);

    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();

    assert_eq!(parts.len(), PERIODIC_MODEL_PART_COUNT);
    assert!(parts.iter().any(|part| matches!(part, ModelPart::Files(_))));
    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::Submodules(_)))
    );
}

#[test]
fn periodic_refresh_does_not_requeue_while_refresh_is_running() {
    let repo = TempRepo::new("periodic-refresh-no-requeue");
    let mut gui = checkout_test_gui(&repo);

    let mut config = crate::config::AppConfig::default();
    config.user_config.git.auto_refresh = true;
    config.user_config.refresher.refresh_interval = 1;
    gui.config = Arc::new(config);

    gui.last_refresh_at = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();

    assert!(gui.periodic_refresh_due());

    gui.refresh_in_progress = true;

    assert!(!gui.periodic_refresh_due());
}

#[test]
fn checkout_uses_targeted_background_refresh() {
    let repo = TempRepo::new("checkout-targeted-refresh");
    let mut gui = checkout_test_gui(&repo);

    request_test_checkout(&mut gui, ' ', "main");
    wait_for_test_checkout(&mut gui);

    assert!(!gui.needs_refresh);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, CHECKOUT_MODEL_PART_COUNT);

    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();

    assert_eq!(parts.len(), CHECKOUT_MODEL_PART_COUNT);

    let main_hash = repo.git(&["rev-parse", "main"]);
    assert!(parts.iter().any(|part| matches!(
        part,
        ModelPart::Branches(branches)
            if branches.first().is_some_and(|branch| branch.head && branch.name == "main")
    )));
    assert!(parts.iter().any(|part| matches!(
        part,
        ModelPart::Worktrees(worktrees)
            if worktrees.iter().any(|worktree| {
                worktree.is_current
                    && worktree.branch == "main"
                    && worktree.hash == main_hash
            })
    )));
    assert!(parts.iter().any(|part| matches!(
        part,
        ModelPart::Reflog(entries)
            if entries.first().is_some_and(|entry| entry.hash == main_hash)
    )));

    let checkout_commits = parts
        .iter()
        .find_map(|part| match part {
            ModelPart::Commits(commits) => Some(commits),
            _ => None,
        })
        .expect("checkout should refresh commits");
    assert_eq!(
        checkout_commits.first().map(|commit| commit.hash.as_str()),
        Some(main_hash.as_str())
    );
    let feature_hash = repo.git(&["rev-parse", "feature"]);
    assert!(
        !checkout_commits
            .iter()
            .any(|commit| commit.hash == feature_hash),
        "current-branch commits must not retain the unrelated feature tip"
    );

    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Stash(_))));
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::Remotes(_)))
    );
    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Tags(_))));
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::RepoUrl(_)))
    );
}

#[test]
fn fetch_and_checkout_keeps_normal_refresh_behavior() {
    let repo = TempRepo::new("remote-op-refresh");
    let mut gui = checkout_test_gui(&repo);
    let head = repo.git(&["rev-parse", "HEAD"]);

    gui.start_remote_op(RemoteOpKind::FetchAndCheckout, "Fetch", "", |_| Ok(()));

    wait_for_test_checkout(&mut gui);

    assert_checkout_head(&gui, "feature", &head);
    assert!(gui.needs_refresh);
    assert!(!gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_none());
    assert!(matches!(gui.popup, PopupState::None));
}

#[test]
fn append_refresh_clears_stale_file_tree_after_committing_last_file() {
    let repo = TempRepo::new("append-refresh-file-tree");
    let mut gui = checkout_test_gui(&repo);
    gui.show_file_tree = true;

    std::fs::write(repo.path().join("file"), "committed contents").unwrap();
    repo.git(&["add", "file"]);
    {
        let files = gui.git.refresh_files_status_only().unwrap();
        gui.model.lock().unwrap().set_files(files);
    }
    gui.rebuild_file_tree_from_model();
    assert!(!gui.file_tree_nodes.is_empty());

    repo.git(&["commit", "-m", "consume final staged file"]);
    assert!(gui.try_fast_append_refresh());

    assert!(gui.model.lock().unwrap().files.is_empty());
    assert!(gui.file_tree_nodes.is_empty());
    assert_eq!(gui.context_mgr.files_list_len_override, Some(0));
}

#[test]
fn append_refresh_moves_head_decoration_without_global_reload() {
    let repo = TempRepo::new("append-refresh");
    let mut gui = checkout_test_gui(&repo);
    let old_head = gui.model.lock().unwrap().head_hash.clone();

    repo.git(&["commit", "--allow-empty", "-m", "appended head"]);
    let new_head = repo.git(&["rev-parse", "HEAD"]);

    assert!(gui.try_fast_append_refresh());

    let model = gui.model.lock().unwrap();
    assert_eq!(model.head_hash, new_head);
    assert_eq!(
        model.commits.first().map(|commit| commit.hash.as_str()),
        Some(new_head.as_str())
    );
    let old = model
        .commits
        .iter()
        .find(|commit| commit.hash == old_head)
        .expect("old HEAD stays in loaded history");
    assert!(
        !old.refs
            .iter()
            .any(|reference| reference == "HEAD -> feature")
    );
}

#[test]
fn head_rewrite_refresh_clears_stale_file_tree_after_amend() {
    let repo = TempRepo::new("head-rewrite-file-tree");
    let mut gui = checkout_test_gui(&repo);
    gui.show_file_tree = true;

    std::fs::write(repo.path().join("file"), "amended contents").unwrap();
    repo.git(&["add", "file"]);
    {
        let files = gui.git.refresh_files_status_only().unwrap();
        gui.model.lock().unwrap().set_files(files);
    }
    gui.rebuild_file_tree_from_model();
    assert!(!gui.file_tree_nodes.is_empty());

    repo.git(&["commit", "--amend", "-m", "rewritten with file"]);
    assert!(gui.try_fast_head_rewrite_refresh());

    assert!(gui.model.lock().unwrap().files.is_empty());
    assert!(gui.file_tree_nodes.is_empty());
    assert_eq!(gui.context_mgr.files_list_len_override, Some(0));
}

#[test]
fn head_rewrite_refresh_replaces_only_first_commit() {
    let repo = TempRepo::new("head-rewrite-refresh");
    let mut gui = checkout_test_gui(&repo);
    let old_head = gui.model.lock().unwrap().head_hash.clone();
    let old_len = gui.model.lock().unwrap().commits.len();

    repo.git(&["commit", "--amend", "-m", "rewritten head"]);
    let new_head = repo.git(&["rev-parse", "HEAD"]);
    assert_ne!(old_head, new_head);

    assert!(gui.try_fast_head_rewrite_refresh());

    let model = gui.model.lock().unwrap();
    assert_eq!(model.head_hash, new_head);
    assert_eq!(
        model.commits.first().map(|commit| commit.hash.as_str()),
        Some(new_head.as_str())
    );
    assert_eq!(model.commits.len(), old_len);
    assert_eq!(model.commits[0].name, "rewritten head");
}

#[test]
fn push_uses_targeted_background_refresh() {
    let repo = TempRepo::new("push-targeted-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.remote_op_label = Some("Pushing".into());
    gui.remote_op_tx
        .send((
            RemoteOpKind::Push,
            Ok(RemoteOpCompletion {
                head: None,
                pull_outcome: None,
                refs_changed: None,
            }),
        ))
        .unwrap();
    gui.receive_remote_op_results();

    assert!(!gui.needs_refresh);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, PUSH_MODEL_PART_COUNT);

    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();
    assert_eq!(parts.len(), PUSH_MODEL_PART_COUNT);
    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::Branches(_)))
    );
    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::Remotes(_)))
    );
    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::UnpushedCommitHashes(_)))
    );
    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Files(_))));
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::Commits(_)))
    );
}

#[test]
fn startup_fetch_is_visible_until_network_check_finishes() {
    let repo = TempRepo::new("startup-fetch-visible");
    let mut gui = checkout_test_gui(&repo);

    gui.start_startup_fetch();

    assert!(gui.auto_fetch_in_flight);
    assert!(gui.startup_fetch_in_flight);
    assert_eq!(gui.remote_op_label.as_deref(), Some("Fetching"));

    // A repository with no remotes makes `git fetch --all` a quick no-op,
    // which is useful here: verify the automatic lane still clears the
    // startup indicator instead of showing a success toast or error modal.
    for _ in 0..100 {
        gui.receive_auto_fetch_results();
        if !gui.auto_fetch_in_flight {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!gui.auto_fetch_in_flight);
    assert!(!gui.startup_fetch_in_flight);
    assert!(gui.remote_op_label.is_none());
    assert!(gui.remote_op_success_at.is_none());
}

#[test]
fn startup_fetch_respects_auto_fetch_setting() {
    let repo = TempRepo::new("startup-fetch-disabled");
    let mut gui = checkout_test_gui(&repo);
    let mut config = crate::config::AppConfig::default();
    config.user_config.git.auto_fetch = false;
    gui.config = Arc::new(config);

    gui.start_startup_fetch();

    assert!(!gui.auto_fetch_in_flight);
    assert!(!gui.startup_fetch_in_flight);
    assert!(gui.remote_op_label.is_none());
}

#[test]
fn auto_fetch_waits_for_interval_and_user_idle() {
    let repo = TempRepo::new("auto-fetch-idle-priority");
    let mut gui = checkout_test_gui(&repo);
    let mut config = crate::config::AppConfig::default();
    config.user_config.git.auto_fetch = true;
    config.user_config.refresher.fetch_interval = 60;
    gui.config = Arc::new(config);

    // Startup initializes the interval clock, so even an idle UI must not
    // immediately compete with the user's first checkout/push/pull.
    gui.last_auto_fetch_at = Some(Instant::now());
    gui.last_user_activity_at = Instant::now() - AUTO_FETCH_IDLE_GRACE;
    gui.maybe_start_auto_fetch();
    assert!(!gui.auto_fetch_in_flight);

    // Once the interval is due, recent input still keeps speculative fetch
    // out of the network lane.
    gui.last_auto_fetch_at = Some(Instant::now() - Duration::from_secs(61));
    gui.last_user_activity_at = Instant::now();
    gui.maybe_start_auto_fetch();
    assert!(!gui.auto_fetch_in_flight);

    // After a short idle window the background fetch is allowed to start.
    gui.last_user_activity_at = Instant::now() - AUTO_FETCH_IDLE_GRACE;
    gui.maybe_start_auto_fetch();
    assert!(gui.auto_fetch_in_flight);
    assert!(!gui.startup_fetch_in_flight);
    assert!(gui.remote_op_label.is_none());

    // Ensure the worker is finished before the temporary repository drops.
    let result = gui
        .auto_fetch_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("background fetch should finish");
    assert!(result.is_ok());
}

#[test]
fn fetch_uses_targeted_background_refresh() {
    let repo = TempRepo::new("fetch-targeted-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.remote_op_label = Some("Fetching".into());
    gui.remote_op_tx
        .send((
            RemoteOpKind::Fetch,
            Ok(RemoteOpCompletion {
                head: None,
                pull_outcome: None,
                refs_changed: Some(true),
            }),
        ))
        .unwrap();
    gui.receive_remote_op_results();

    assert!(!gui.needs_refresh);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, FETCH_MODEL_PART_COUNT);

    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();

    assert_eq!(parts.len(), FETCH_MODEL_PART_COUNT);

    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::Branches(_)))
    );
    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::Commits(_)))
    );
    assert!(
        parts
            .iter()
            .any(|part| matches!(part, ModelPart::Remotes(_)))
    );
    assert!(parts.iter().any(|part| matches!(part, ModelPart::Tags(_))));
    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Files(_))));
    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Stash(_))));
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::Head { .. }))
    );
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::DiffStats { .. }))
    );
}

#[test]
fn no_op_fetch_skips_model_refresh() {
    let repo = TempRepo::new("fetch-no-op-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.start_remote_op(RemoteOpKind::Fetch, "Fetch", "", |_| Ok(()));
    wait_for_test_checkout(&mut gui);

    assert!(!gui.needs_refresh);
    assert!(!gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_none());
}

#[test]
fn no_op_pull_skips_model_refresh() {
    let repo = TempRepo::new("pull-no-op-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.start_remote_op(RemoteOpKind::Pull, "Pull", "", |_| Ok(()));
    wait_for_test_checkout(&mut gui);

    assert!(!gui.needs_refresh);
    assert!(!gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_none());
}

#[test]
fn auto_fetch_uses_targeted_background_refresh() {
    let repo = TempRepo::new("auto-fetch-targeted-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.auto_fetch_in_flight = true;
    gui.auto_fetch_tx.send(Ok(true)).unwrap();

    gui.receive_auto_fetch_results();

    assert!(!gui.auto_fetch_in_flight);
    assert!(!gui.needs_refresh);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, FETCH_MODEL_PART_COUNT);

    // Drain the targeted refresh so its worker threads finish before the
    // temporary repository is dropped.
    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();

    assert_eq!(parts.len(), FETCH_MODEL_PART_COUNT);
}

#[test]
fn pull_uses_targeted_background_refresh() {
    let repo = TempRepo::new("pull-targeted-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.remote_op_label = Some("Pulling".into());
    gui.remote_op_tx
        .send((
            RemoteOpKind::Pull,
            Ok(RemoteOpCompletion {
                head: None,
                pull_outcome: Some(PullOutcome::FastForwarded {
                    upstream: "origin/main".to_string(),
                    behind: 2,
                }),
                refs_changed: Some(true),
            }),
        ))
        .unwrap();
    gui.receive_remote_op_results();

    // Pull should start the targeted streaming refresh instead of queueing
    // the normal full-model refresh.
    assert!(!gui.needs_refresh);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, PULL_MODEL_PART_COUNT);

    // Drain the targeted refresh so its worker threads finish before this
    // test drops the temporary repository.
    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();

    assert_eq!(parts.len(), PULL_MODEL_PART_COUNT);

    // These are deliberately omitted from post-pull refreshes because a
    // successful pull cannot change them.
    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Stash(_))));
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::RepoUrl(_)))
    );
}

#[test]
fn divergent_pull_opens_strategy_menu_instead_of_git_error() {
    let repo = TempRepo::new("pull-divergence-menu");
    let mut gui = checkout_test_gui(&repo);

    gui.remote_op_label = Some("Pulling".into());
    gui.remote_op_tx
        .send((
            RemoteOpKind::Pull,
            Ok(RemoteOpCompletion {
                head: None,
                pull_outcome: Some(PullOutcome::Diverged {
                    upstream: "origin/main".to_string(),
                    ahead: 3,
                    behind: 17,
                }),
                refs_changed: Some(false),
            }),
        ))
        .unwrap();
    gui.receive_remote_op_results();

    let PopupState::PullDivergence {
        upstream,
        ahead,
        behind,
        selected,
        ..
    } = &gui.popup
    else {
        panic!("diverged pull should open the guided reconciliation dialog");
    };
    assert_eq!(upstream, "origin/main");
    assert_eq!((*ahead, *behind, *selected), (3, 17, 0));
    assert!(gui.remote_op_success_at.is_none());
}

#[test]
fn view_divergent_commits_starts_compare_loading_off_ui_thread() {
    let repo = TempRepo::new("pull-divergence-compare-async");
    let mut gui = checkout_test_gui(&repo);
    gui.popup = PopupState::PullDivergence {
        branch: "feature".into(),
        upstream: "main".into(),
        ahead: 1,
        behind: 2,
        selected: 2,
    };

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
        .unwrap();

    assert!(gui.diff_mode.active);
    assert_eq!(gui.diff_mode.ref_a, "HEAD");
    assert_eq!(gui.diff_mode.ref_b, "main");
    assert_eq!(gui.diff_mode.ahead_behind, Some((1, 2)));
    assert!(gui.diff_mode.comparison_loading);
    assert!(gui.compare_load_rx.is_some());
    assert!(matches!(gui.popup, PopupState::None));

    // Let the tiny test comparison finish before TempRepo removes its directory.
    for _ in 0..100 {
        gui.receive_compare_load_results();
        if gui.compare_load_rx.is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(gui.compare_load_rx.is_none());
    assert!(!gui.diff_mode.comparison_loading);
}

#[test]
fn pull_with_remote_ref_changes_but_unchanged_head_uses_fetch_refresh() {
    let repo = TempRepo::new("pull-unchanged-head-refresh");
    let mut gui = checkout_test_gui(&repo);

    gui.remote_op_label = Some("Pulling".into());
    gui.remote_op_tx
        .send((
            RemoteOpKind::Pull,
            Ok(RemoteOpCompletion {
                head: None,
                pull_outcome: Some(PullOutcome::Diverged {
                    upstream: "origin/main".to_string(),
                    ahead: 1,
                    behind: 2,
                }),
                refs_changed: Some(true),
            }),
        ))
        .unwrap();
    gui.receive_remote_op_results();

    assert!(!gui.needs_refresh);
    assert!(gui.refresh_in_progress);
    assert!(gui.initial_load_rx.is_some());
    assert_eq!(gui.initial_load_expected, FETCH_MODEL_PART_COUNT);

    let parts: Vec<_> = gui.initial_load_rx.take().unwrap().into_iter().collect();

    assert_eq!(parts.len(), FETCH_MODEL_PART_COUNT);
    assert!(!parts.iter().any(|part| matches!(part, ModelPart::Files(_))));
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::DiffStats { .. }))
    );
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::Reflog(_)))
    );
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ModelPart::Submodules(_)))
    );
}

#[test]
fn successful_checkout_without_head_metadata_still_refreshes() {
    let repo = TempRepo::new("checkout-metadata-fallback");
    let mut gui = checkout_test_gui(&repo);
    repo.git(&["checkout", "main"]);
    gui.remote_op_label = Some("Checking out".into());
    gui.pending_checkout_by_name = Some("main".into());
    gui.remote_op_tx
        .send((
            RemoteOpKind::Other,
            Ok(RemoteOpCompletion {
                head: None,
                pull_outcome: None,
                refs_changed: None,
            }),
        ))
        .unwrap();
    gui.receive_remote_op_results();
    assert!(matches!(gui.popup, PopupState::None));
    assert!(gui.pending_checkout_by_name.is_none());
    assert!(gui.refresh_in_progress);
    gui.refresh().unwrap();
    assert_checkout_head(&gui, "main", &repo.git(&["rev-parse", "main"]));
}

#[test]
fn busy_checkout_does_not_change_head_or_pending_checkout() {
    for key in [' ', '-', 'c'] {
        let repo = TempRepo::new("busy-checkout");
        let mut gui = checkout_test_gui(&repo);
        let head = repo.git(&["rev-parse", "HEAD"]);
        for pending in [None, Some("already-pending".to_string())] {
            gui.remote_op_label = Some("Busy".to_string());
            gui.pending_checkout_by_name = pending.clone();
            request_test_checkout(&mut gui, key, "main");
            assert_checkout_head(&gui, "feature", &head);
            assert_eq!(gui.pending_checkout_by_name, pending);
            assert_eq!(gui.remote_op_label.as_deref(), Some("Busy"));
        }
    }
}

#[test]
fn tree_compare_navigation_folding_and_focus_preserve_current_layout() {
    use crate::gui::modes::diff_mode::DiffModeFocus;
    let repo = TempRepo::new("tree-compare");
    let mut gui = tree_test_gui(&repo);
    gui.diff_mode.enter(true);
    gui.diff_mode.diff_files = gui.model.lock().unwrap().commit_files.clone();
    gui.diff_mode.tree_nodes = gui.commit_file_tree_nodes.clone();
    for focus in [DiffModeFocus::CommitFiles, DiffModeFocus::DiffExploration] {
        gui.diff_mode.set_focus(focus);
        gui.diff_mode.diff_files_selected = 0;
        gui.diff_mode.viewport_manually_scrolled = true;
        gui.needs_diff_refresh = false;
        gui.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(gui.diff_mode.diff_files_selected, 1);
        assert!(!gui.diff_mode.viewport_manually_scrolled);
        assert!(gui.needs_diff_refresh);
        gui.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(gui.diff_mode.diff_files_selected, 4);
        gui.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(gui.diff_mode.diff_files_selected, 1);
    }
    for mode in [ScreenMode::Normal, ScreenMode::Half, ScreenMode::Full] {
        gui.screen_mode = mode;
        gui.diff_mode.set_focus(DiffModeFocus::CommitFiles);
        gui.diff_mode.diff_files_selected = 0;
        gui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(gui.diff_mode.focus, DiffModeFocus::DiffExploration);
        assert_eq!(gui.screen_mode, mode);
        gui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(gui.diff_mode.focus, DiffModeFocus::CommitFiles);
        assert_eq!(gui.screen_mode, mode);
    }
    gui.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.diff_mode.tree_nodes.len(), 1);
    gui.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.diff_mode.tree_nodes.len(), 5);
    Arc::get_mut(&mut gui.config)
        .unwrap()
        .user_config
        .keybinding
        .universal
        .tree_child = "<c-t>".into();
    gui.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.diff_mode.diff_files_selected, 0);
    gui.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(gui.diff_mode.diff_files_selected, 1);
}

#[test]
fn tree_commit_details_conflict_is_limited_to_active_tree_bindings() {
    let repo = TempRepo::new("tree-details");
    let mut gui = tree_test_gui(&repo);
    gui.context_mgr.set_active(ContextId::CommitFiles);
    let details = gui.show_commit_details;
    gui.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.context_mgr.selected_active(), 1);
    assert_eq!(gui.show_commit_details, details);
    // Other tree actions remapped to apostrophe also take precedence, even at a boundary.
    let kb = &mut Arc::get_mut(&mut gui.config)
        .unwrap()
        .user_config
        .keybinding;
    kb.universal.tree_child.clear();
    kb.universal.tree_parent = "'".into();
    gui.handle_key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.context_mgr.selected_active(), 0);
    gui.handle_key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.show_commit_details, details);
    gui.show_commit_file_tree = false;
    gui.handle_key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE))
        .unwrap();
    assert_ne!(gui.show_commit_details, details);
}

#[test]
fn tree_default_child_key_preserves_commit_details_toggle() {
    let repo = TempRepo::new("tree-default-details");
    let mut gui = tree_test_gui(&repo);
    for ctx in [ContextId::CommitFiles, ContextId::BranchCommitFiles] {
        for tree in [false, true] {
            gui.context_mgr.set_active(ctx);
            gui.context_mgr.set_selection(0);
            gui.show_commit_file_tree = tree;
            let details = gui.show_commit_details;
            gui.handle_key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE))
                .unwrap();
            assert_ne!(gui.show_commit_details, details, "{ctx:?} {tree}");
            assert_eq!(gui.context_mgr.selected_active(), 0);
            gui.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(gui.context_mgr.selected_active(), usize::from(tree));
            assert_ne!(gui.show_commit_details, details);
        }
    }
}

#[test]
fn apostrophe_toggles_details_and_is_documented_in_commit_context_help() {
    let repo = TempRepo::new("apostrophe-details-help");
    let mut gui = tree_test_gui(&repo);
    for ctx in [
        ContextId::Commits,
        ContextId::BranchCommits,
        ContextId::Reflog,
        ContextId::CommitFiles,
        ContextId::BranchCommitFiles,
        ContextId::StashFiles,
    ] {
        gui.popup = PopupState::None;
        gui.context_mgr.set_active(ctx);
        let details = gui.show_commit_details;
        gui.handle_key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE))
            .unwrap();
        assert_ne!(gui.show_commit_details, details, "{ctx:?}");
        gui.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT))
            .unwrap();
        let PopupState::CommandPalette { sections, .. } = &gui.popup else {
            panic!("expected help palette");
        };
        let details_entries: Vec<_> = sections
            .iter()
            .flat_map(|s| &s.entries)
            .filter(|e| e.description == "Toggle commit details panel")
            .collect();
        // Stash-file help did not previously include a details entry.
        if ctx != ContextId::StashFiles {
            assert_eq!(details_entries.len(), 1, "{ctx:?}");
            assert_eq!(details_entries[0].key, "'", "{ctx:?}");
        }
    }
}

#[test]
fn tree_help_is_only_available_in_active_tree_contexts() {
    use crate::gui::modes::diff_mode::DiffModeFocus;
    let repo = TempRepo::new("tree-contextual-help");
    let mut gui = tree_test_gui(&repo);
    for ctx in [
        ContextId::Files,
        ContextId::CommitFiles,
        ContextId::BranchCommitFiles,
        ContextId::StashFiles,
        ContextId::Commits,
    ] {
        for tree in [false, true] {
            for diff in [false, true] {
                gui.popup = PopupState::None;
                gui.context_mgr.set_active(ctx);
                gui.show_file_tree = tree;
                gui.show_commit_file_tree = tree;
                gui.diff_focused = diff;
                gui.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT))
                    .unwrap();
                let PopupState::CommandPalette { sections, .. } = &gui.popup else {
                    panic!("expected help palette");
                };
                let tree_section = sections.iter().find(|s| s.title == "File Tree");
                assert_eq!(
                    tree_section.is_some(),
                    tree && ctx != ContextId::Commits,
                    "{ctx:?} tree={tree} diff={diff}"
                );
                if let Some(section) = tree_section {
                    assert!(
                        section
                            .entries
                            .iter()
                            .any(|e| e.key == "." && e.description == "Select first visible child")
                    );
                    assert_eq!(section.entries.iter().any(|e| e.key == "-"), !diff);
                }
            }
        }
    }
    gui.diff_mode.enter(true);
    for tree in [false, true] {
        for focus in [
            DiffModeFocus::CommitFiles,
            DiffModeFocus::DiffExploration,
            DiffModeFocus::Commits,
            DiffModeFocus::SelectorA,
            DiffModeFocus::SelectorB,
        ] {
            gui.popup = PopupState::None;
            gui.diff_mode.show_tree = tree;
            gui.diff_mode.set_focus(focus);
            gui.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT))
                .unwrap();
            let PopupState::CommandPalette { sections, .. } = &gui.popup else {
                panic!("expected compare help palette");
            };
            let tree_section = sections.iter().find(|s| s.title == "File Tree");
            assert_eq!(
                tree_section.is_some(),
                tree && matches!(
                    focus,
                    DiffModeFocus::CommitFiles | DiffModeFocus::DiffExploration
                )
            );
            if let Some(section) = tree_section {
                assert!(section.entries.iter().any(|e| e.key == "."));
                assert_eq!(
                    section.entries.iter().any(|e| e.key == "-"),
                    focus == DiffModeFocus::CommitFiles
                );
            }
        }
    }
}

#[test]
fn tree_fold_binding_does_not_override_flat_list_actions() {
    use crate::gui::modes::diff_mode::DiffModeFocus;
    let repo = TempRepo::new("tree-flat-fold");
    let mut gui = tree_test_gui(&repo);
    // A remapped fold key must not swallow a flat list's tree-toggle action.
    Arc::get_mut(&mut gui.config)
        .unwrap()
        .user_config
        .keybinding
        .universal
        .fold_directory = "`".into();
    for ctx in [
        ContextId::Files,
        ContextId::CommitFiles,
        ContextId::BranchCommitFiles,
        ContextId::StashFiles,
    ] {
        gui.context_mgr.set_active(ctx);
        gui.show_file_tree = false;
        gui.show_commit_file_tree = false;
        gui.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE))
            .unwrap();
        assert!(gui.active_file_tree(), "{ctx:?}");
    }
    gui.diff_mode.enter(false);
    gui.diff_mode.set_focus(DiffModeFocus::CommitFiles);
    gui.handle_key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE))
        .unwrap();
    assert!(gui.diff_mode.show_tree);
}

fn tree_test_gui(repo: &TempRepo) -> Gui {
    std::fs::create_dir_all(repo.path.join("src/deep")).unwrap();
    repo.commit("src/deep/a.txt", "base\n");
    repo.commit("src/deep/b.txt", "base\n");
    repo.commit("top.txt", "base\n");
    for path in ["src/deep/a.txt", "src/deep/b.txt", "top.txt"] {
        std::fs::write(repo.path.join(path), "changed\n").unwrap();
    }
    let mut config = crate::config::AppConfig::default();
    config.state_path = repo.path.join("state.yml");
    let mut gui = Gui::new(
        config,
        crate::git::GitCommands::new(repo.path()).unwrap(),
        None,
        false,
    )
    .unwrap();
    let files = gui.git.load_files().unwrap();
    gui.model.lock().unwrap().files = files;
    gui.show_file_tree = true;
    gui.show_commit_file_tree = true;
    gui.update_file_tree_state();
    gui.model.lock().unwrap().commit_files = ["src/deep/a.txt", "src/deep/b.txt", "top.txt"]
        .into_iter()
        .map(|name| crate::model::CommitFile {
            name: name.into(),
            status: crate::model::FileChangeStatus::Modified,
            hunk_count: 0,
            additions: 0,
            deletions: 0,
        })
        .collect();
    controller::commit_files::update_commit_file_tree_state(&mut gui);
    gui.diff_view.load("test.txt", "old\n", "new\n");
    gui
}

#[test]
fn tree_enter_and_escape_preserve_all_screen_modes() {
    let repo = TempRepo::new("tree-layout");
    let mut gui = tree_test_gui(&repo);
    for ctx in [
        ContextId::Files,
        ContextId::CommitFiles,
        ContextId::BranchCommitFiles,
        ContextId::StashFiles,
    ] {
        for mode in [ScreenMode::Normal, ScreenMode::Half, ScreenMode::Full] {
            for tree in [false, true] {
                gui.context_mgr.set_active(ctx);
                gui.context_mgr.set_selection(0);
                gui.show_file_tree = tree;
                gui.show_commit_file_tree = tree;
                gui.screen_mode = mode;
                gui.diff_focused = false;
                gui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                    .unwrap();
                assert!(gui.diff_focused, "{ctx:?} {tree}");
                assert_eq!(gui.screen_mode, mode);
                gui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                    .unwrap();
                assert!(!gui.diff_focused);
                assert_eq!(gui.screen_mode, mode);
                assert_eq!(gui.context_mgr.active(), ctx);
            }
        }
    }
}

#[test]
fn tree_navigation_resumes_viewport_following_in_sidebar_and_diff() {
    let repo = TempRepo::new("tree-navigation");
    let mut gui = tree_test_gui(&repo);
    for ctx in [
        ContextId::Files,
        ContextId::CommitFiles,
        ContextId::BranchCommitFiles,
        ContextId::StashFiles,
    ] {
        for diff in [false, true] {
            gui.context_mgr.set_active(ctx);
            gui.context_mgr.set_selection(0);
            gui.diff_focused = diff;
            gui.context_mgr.viewport_manually_scrolled = true;
            gui.needs_diff_refresh = false;
            gui.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(gui.context_mgr.selected_active(), 1);
            assert!(!gui.context_mgr.viewport_manually_scrolled);
            assert!(gui.needs_diff_refresh);
            gui.context_mgr.set_scroll_offset(ctx, 100);
            gui.context_mgr.ensure_scroll_visible(ctx, 2);
            assert!(gui.context_mgr.scroll_offset(ctx) <= 1);
            gui.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT))
                .unwrap();
            assert_eq!(gui.context_mgr.selected_active(), 4);
            gui.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT))
                .unwrap();
            assert_eq!(gui.context_mgr.selected_active(), 1);
            gui.handle_key(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(gui.context_mgr.selected_active(), 0);
        }
    }
}

#[test]
fn tree_custom_and_disabled_keys_work_without_swallowing_flat_diff_keys() {
    let repo = TempRepo::new("tree-custom-bindings");
    let mut gui = tree_test_gui(&repo);
    for diff in [false, true] {
        gui.diff_focused = diff;
        gui.context_mgr.set_selection(0);
        Arc::get_mut(&mut gui.config)
            .unwrap()
            .user_config
            .keybinding
            .universal
            .tree_child = "<c-t>".into();
        let details = gui.show_commit_details;
        gui.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(gui.context_mgr.selected_active(), 1);
        assert_eq!(gui.show_commit_details, details);
        gui.context_mgr.set_selection(0);
        Arc::get_mut(&mut gui.config)
            .unwrap()
            .user_config
            .keybinding
            .universal
            .tree_child
            .clear();
        gui.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(gui.context_mgr.selected_active(), 0);
    }
    // A remap to a diff shortcut must not claim that key outside tree contexts.
    Arc::get_mut(&mut gui.config)
        .unwrap()
        .user_config
        .keybinding
        .universal
        .tree_child = "j".into();
    gui.show_file_tree = false;
    gui.diff_focused = true;
    gui.diff_view.load("long", "", &"line\n".repeat(100));
    gui.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.diff_view.scroll_offset, 1);
}

#[test]
fn tree_fold_is_explicit_and_enter_does_not_collapse_directories() {
    let repo = TempRepo::new("tree-fold");
    let mut gui = tree_test_gui(&repo);
    for ctx in [ContextId::Files, ContextId::CommitFiles] {
        gui.context_mgr.set_active(ctx);
        gui.context_mgr.set_selection(0);
        gui.diff_focused = false;
        gui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(gui.diff_focused);
        gui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        gui.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            if ctx == ContextId::Files {
                gui.file_tree_nodes.len()
            } else {
                gui.commit_file_tree_nodes.len()
            },
            1
        );
        gui.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            if ctx == ContextId::Files {
                gui.file_tree_nodes.len()
            } else {
                gui.commit_file_tree_nodes.len()
            },
            5
        );
        Arc::get_mut(&mut gui.config)
            .unwrap()
            .user_config
            .keybinding
            .universal
            .fold_directory = "_".into();
        let kb = gui.config.user_config.keybinding.clone();
        gui.handle_context_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            if ctx == ContextId::Files {
                gui.file_tree_nodes.len()
            } else {
                gui.commit_file_tree_nodes.len()
            },
            5
        );
        if ctx == ContextId::Files {
            controller::files::handle_key(
                &mut gui,
                KeyEvent::new(KeyCode::Char('_'), KeyModifiers::SHIFT),
                &kb,
            )
            .unwrap();
        } else {
            controller::commit_files::handle_key(
                &mut gui,
                KeyEvent::new(KeyCode::Char('_'), KeyModifiers::SHIFT),
                &kb,
            )
            .unwrap();
        }
        assert_eq!(
            if ctx == ContextId::Files {
                gui.file_tree_nodes.len()
            } else {
                gui.commit_file_tree_nodes.len()
            },
            1
        );
        Arc::get_mut(&mut gui.config)
            .unwrap()
            .user_config
            .keybinding
            .universal
            .fold_directory = "-".into();
    }
}

#[test]
fn shift_d_discards_only_unstaged_changes_after_confirmation() {
    let repo = TempRepo::new("files-discard-unstaged");
    let mut gui = checkout_test_gui(&repo);
    gui.context_mgr.set_active(ContextId::Files);

    // Simulate Space-staging some files, leaving a couple of new files
    // unstaged, and modifying a staged file further in the worktree.
    std::fs::write(repo.path().join("file"), "staged version").unwrap();
    std::fs::write(repo.path().join("staged-new"), "keep staged addition").unwrap();
    repo.git(&["add", "file", "staged-new"]);
    std::fs::write(repo.path().join("file"), "unstaged version").unwrap();
    std::fs::write(repo.path().join("untracked"), "discard me").unwrap();
    std::fs::create_dir(repo.path().join("new-dir")).unwrap();
    std::fs::write(repo.path().join("new-dir/other"), "discard too").unwrap();
    std::fs::write(repo.path().join(".gitignore"), "ignored-file\n").unwrap();
    repo.git(&["add", ".gitignore"]);
    std::fs::write(repo.path().join("ignored-file"), "leave ignored alone").unwrap();

    let kb = gui.config.user_config.keybinding.clone();
    controller::files::handle_key(&mut gui, parse_key("D").unwrap(), &kb).unwrap();
    let PopupState::Confirm { title, message, .. } = &gui.popup else {
        panic!("D should confirm discarding only unstaged changes");
    };
    assert_eq!(title, "Discard all unstaged changes");
    assert!(message.contains("including deleting untracked files and directories"));
    assert!(message.contains("Staged changes and ignored files will be preserved"));

    // Confirm by the same key path used by the live UI, then wait for the
    // existing background-mutation worker to finish.
    gui.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)).unwrap();
    assert_eq!(gui.background_mutation_label.as_deref(), Some("Discard unstaged changes"));
    for _ in 0..100 {
        gui.receive_ui_jobs();
        if gui.background_mutation_label.is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gui.background_mutation_label.is_none(), "discard worker completed");
    assert_eq!(std::fs::read_to_string(repo.path().join("file")).unwrap(), "staged version");
    assert!(repo.git(&["diff", "--", "file"]).is_empty());
    assert!(!repo.git(&["diff", "--cached", "--", "file"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("staged-new")).unwrap(),
        "keep staged addition"
    );
    assert!(!repo.git(&["diff", "--cached", "--", "staged-new"]).is_empty());
    assert!(!repo.path().join("untracked").exists());
    assert!(!repo.path().join("new-dir").exists());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("ignored-file")).unwrap(),
        "leave ignored alone"
    );
}

#[test]
fn discard_all_unstaged_removes_untracked_from_empty_index() {
    let repo = TempRepo::new("discard-untracked-only");
    std::fs::write(repo.path().join("not-staged"), "delete me").unwrap();
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    git.discard_all_unstaged_changes().unwrap();
    assert!(!repo.path().join("not-staged").exists());
}

#[test]
fn shift_d_cancel_does_not_discard_changes() {
    let repo = TempRepo::new("files-discard-cancel");
    let mut gui = checkout_test_gui(&repo);
    gui.context_mgr.set_active(ContextId::Files);
    std::fs::write(repo.path().join("file"), "do not discard").unwrap();

    let kb = gui.config.user_config.keybinding.clone();
    controller::files::handle_key(&mut gui, parse_key("D").unwrap(), &kb).unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
    assert!(matches!(gui.popup, PopupState::None));
    assert_eq!(std::fs::read_to_string(repo.path().join("file")).unwrap(), "do not discard");
}

#[test]
fn ctrl_g_in_files_opens_ai_commit_generation() {
    let repo = TempRepo::new("ctrl-g-generation");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    for staged in [false, true] {
        if staged {
            repo.git(&["add", "file"]);
        }
        let mut config = crate::config::AppConfig::default();
        config.user_config.git.commit.generate_command.clear();
        let git = crate::git::GitCommands::new(repo.path()).unwrap();
        let mut gui = Gui::new(config, git, None, false).unwrap();
        *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
        gui.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(gui.context_mgr.active(), ContextId::Files);
        if staged {
            assert!(matches!(
                &gui.popup,
                PopupState::Message { title, .. } if title == "AI generation unavailable"
            ));
        } else {
            assert!(matches!(
                &gui.popup,
                PopupState::Confirm { title, .. } if title == "No files staged"
            ));
        }
    }
}

#[test]
fn stash_defaults_open_wrapped_confirmations_without_full_refresh() {
    let repo = TempRepo::new("stash-targeted-refresh");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "stashed").unwrap();
    repo.git(&["stash", "push", "-m", "a very long stash subject that should live in the popup body instead of being clipped by the border title"]);

    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    for _ in gui.initial_load_rx.take().unwrap() {}
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.context_mgr.set_active(ContextId::Stash);
    gui.context_mgr.set_selection(0);

    // `a` applies and keeps the stash. The long subject belongs in the
    // wrapped message body; the title stays compact and cannot be clipped.
    gui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::Confirm { title, message, .. }
        if title == "Apply stash"
            && message.contains("very long stash subject")
            && message.contains("keep it in the stash list"))
    );

    // Confirm apply, then wait for the stash-specific refresh. It must not
    // queue the expensive generic full-model refresh.
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while gui.background_mutation_label.is_some() {
        gui.receive_ui_jobs();
        assert!(Instant::now() < deadline, "stash apply did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    gui.receive_ui_jobs();
    assert!(!gui.needs_refresh);
    assert_eq!(gui.model.lock().unwrap().stash_entries.len(), 1);
    assert!(!gui.model.lock().unwrap().files.is_empty());

    // Space is Pop by default and gets the same non-truncated confirmation.
    gui.context_mgr.set_active(ContextId::Stash);
    gui.context_mgr.set_selection(0);
    gui.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::Confirm { title, message, .. }
        if title == "Pop stash"
            && message.contains("very long stash subject")
            && message.contains("remove it from the stash list"))
    );
}

#[test]
fn alt_s_in_commit_editor_toggles_visible_signoff() {
    let repo = TempRepo::new("commit-signoff");
    repo.git(&["config", "user.name", "Test User"]);
    repo.git(&["config", "user.email", "test@example.com"]);
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, focus, .. }
        if body_state.raw() == "Signed-off-by: Test User <test@example.com>"
            && *focus == popup::CommitInputFocus::Body)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.raw().is_empty())
    );
}

#[test]
fn commit_editor_vim_modes_move_between_fields_and_delete_visual_selection() {
    let repo = TempRepo::new("commit-vim-modes");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    assert!(
        matches!(&gui.popup, PopupState::CommitInput { mode, focus, .. }
        if *mode == popup::CommitInputMode::Insert
            && *focus == popup::CommitInputFocus::Summary)
    );

    for c in "subject".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { mode, focus, .. }
        if *mode == popup::CommitInputMode::Normal
            && *focus == popup::CommitInputFocus::Body)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE))
        .unwrap();
    for c in "alpha beta".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();

    assert!(
        matches!(&gui.popup, PopupState::CommitInput { mode, body_state, focus, .. }
        if *mode == popup::CommitInputMode::Normal
            && *focus == popup::CommitInputFocus::Body
            && body_state.raw() == "pha beta"
            && body_state.cursor == 0)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput { focus, .. }
        if *focus == popup::CommitInputFocus::Summary));
}

#[test]
fn commit_editor_supports_vim_operators_undo_open_line_and_quote_object() {
    let repo = TempRepo::new("commit-vim-operators");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    // A fresh commit starts in Insert; one Normal-mode `u` reverts
    // that whole first Insert session.
    for c in "subject".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { summary_textarea, .. }
        if summary_textarea.lines().join("").is_empty())
    );

    // Put text in the body and exercise dw/dd/o.
    let wrap_width = gui.commit_body_wrap_width();
    if let PopupState::CommitInput {
        focus,
        summary_textarea,
        body_textarea,
        body_state,
        ..
    } = &mut gui.popup
    {
        body_state.set_text("alpha beta");
        body_state.move_to_start();
        body_state.render_into(body_textarea, wrap_width);
        popup::set_commit_input_focus(
            focus,
            summary_textarea,
            body_textarea,
            popup::CommitInputFocus::Body,
        );
    }

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.raw() == "beta")
    );
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.raw() == "alpha beta")
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.raw().is_empty())
    );
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .unwrap();

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { mode, body_state, .. }
        if *mode == popup::CommitInputMode::Insert && body_state.raw() == "alpha beta\n")
    );
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();

    // vi" selects only the quoted contents.
    if let PopupState::CommitInput {
        body_textarea,
        body_state,
        ..
    } = &mut gui.popup
    {
        body_state.set_text("say \"hello world\" now");
        body_state.set_cursor_index(8);
        body_state.render_into(body_textarea, wrap_width);
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_textarea, body_state, mode, .. }
        if mode.is_visual()
            && body_textarea.selection_range().is_some_and(|(start, end)|
                body_state.selected_visual_text(start, end, wrap_width) == "hello world"))
    );
}

#[test]
fn commit_editor_supports_counts_backspace_and_linewise_visual() {
    let repo = TempRepo::new("commit-vim-counts");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    let wrap_width = gui.commit_body_wrap_width();
    if let PopupState::CommitInput {
        focus,
        summary_textarea,
        body_textarea,
        body_state,
        mode,
        ..
    } = &mut gui.popup
    {
        *mode = popup::CommitInputMode::Normal;
        body_state.set_text("zero\none\ntwo\nthree");
        body_state.move_to_start();
        body_state.render_into(body_textarea, wrap_width);
        popup::set_commit_input_focus(
            focus,
            summary_textarea,
            body_textarea,
            popup::CommitInputFocus::Body,
        );
    }

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.visual_cursor(wrap_width).0 == 2)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.visual_cursor(wrap_width).0 == 0)
    );

    if let PopupState::CommitInput {
        body_state,
        body_textarea,
        ..
    } = &mut gui.popup
    {
        body_state.set_cursor_index(3);
        body_state.apply_cursor_into(body_textarea, wrap_width);
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.cursor_index() == 2 && body_state.raw() == "zero\none\ntwo\nthree")
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        mode, body_state, body_textarea, visual_line_anchor: Some(_), ..
    } if *mode == popup::CommitInputMode::VisualLine
        && body_textarea.selection_range().is_some_and(|(start, end)|
            body_state.selected_visual_text(start, end, wrap_width) == "zero")));

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        mode, body_state, body_textarea, visual_line_anchor: Some(_), ..
    } if *mode == popup::CommitInputMode::VisualLine
        && body_textarea.selection_range().is_some_and(|(start, end)|
            body_state.selected_visual_text(start, end, wrap_width) == "zero\none")));
}

#[test]
fn commit_editor_supports_gg_g_in_normal_and_visual_modes() {
    let repo = TempRepo::new("commit-vim-gg-g");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    let wrap_width = gui.commit_body_wrap_width();
    if let PopupState::CommitInput {
        focus,
        summary_textarea,
        body_textarea,
        body_state,
        mode,
        ..
    } = &mut gui.popup
    {
        *mode = popup::CommitInputMode::Normal;
        body_state.set_text("zero\none\ntwo\nthree");
        body_state.set_cursor_index(6); // `one`, column 1
        body_state.render_into(body_textarea, wrap_width);
        popup::set_commit_input_focus(
            focus,
            summary_textarea,
            body_textarea,
            popup::CommitInputFocus::Body,
        );
    }

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.visual_cursor(wrap_width) == (3, 0))
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.visual_cursor(wrap_width) == (0, 0))
    );

    if let PopupState::CommitInput {
        body_state,
        body_textarea,
        ..
    } = &mut gui.popup
    {
        body_state.set_cursor_index(6);
        body_state.apply_cursor_into(body_textarea, wrap_width);
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { mode, body_state, body_textarea, .. }
        if *mode == popup::CommitInputMode::Visual
            && body_state.visual_cursor(wrap_width) == (3, 0)
            && body_textarea.selection_range().is_some())
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    if let PopupState::CommitInput {
        body_state,
        body_textarea,
        ..
    } = &mut gui.popup
    {
        body_state.set_cursor_index(5); // start of `one`
        body_state.apply_cursor_into(body_textarea, wrap_width);
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        mode, body_state, body_textarea, visual_line_anchor: Some(_), ..
    } if *mode == popup::CommitInputMode::VisualLine
        && body_textarea.selection_range().is_some_and(|(start, end)|
            body_state.selected_visual_text(start, end, wrap_width) == "one\ntwo\nthree")));

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        mode, body_state, body_textarea, visual_line_anchor: Some(_), ..
    } if *mode == popup::CommitInputMode::VisualLine
        && body_textarea.selection_range().is_some_and(|(start, end)|
            body_state.selected_visual_text(start, end, wrap_width) == "zero\none")));
}

#[test]
fn commit_editor_supports_find_char_and_character_word_motions() {
    let repo = TempRepo::new("commit-vim-find-char");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    let wrap_width = gui.commit_body_wrap_width();
    if let PopupState::CommitInput {
        focus,
        summary_textarea,
        body_textarea,
        body_state,
        mode,
        ..
    } = &mut gui.popup
    {
        *mode = popup::CommitInputMode::Normal;
        body_state.set_text("alpha   beta alpha");
        body_state.move_to_start();
        body_state.render_into(body_textarea, wrap_width);
        popup::set_commit_input_focus(
            focus,
            summary_textarea,
            body_textarea,
            popup::CommitInputFocus::Body,
        );
    }

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.cursor_index() == 8)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.cursor_index() == 17)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.cursor_index() == 13)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_state, .. }
        if body_state.cursor_index() == 4)
    );
}

#[test]
fn commit_editor_question_mark_uses_command_palette_and_restores_editor() {
    let repo = TempRepo::new("commit-vim-help");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    // `?` is a Normal-mode command, and opens the same searchable
    // CommandPalette state used by the rest of lazygitrs.
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommandPalette { sections, .. }
        if sections.iter().any(|section| section.title.contains("NORMAL")))
    );
    assert!(matches!(
        &gui.pending_commit_popup,
        Some(PopupState::CommitInput { .. })
    ));

    gui.handle_command_palette_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput { mode, .. }
        if *mode == popup::CommitInputMode::Normal));
    assert!(gui.pending_commit_popup.is_none());
}

#[test]
fn patch_picker_ctrl_j_accepts_extra_modifiers_and_moves_files_immediately() {
    let repo = TempRepo::new("patch-picker-ctrl-j");
    let picker_dir = repo.path().join("patches");
    std::fs::create_dir_all(&picker_dir).unwrap();
    for index in 0..30 {
        std::fs::write(
            picker_dir.join(format!("{index:02}.patch")),
            format!("diff --git a/a b/a\n+line {index}\n"),
        )
        .unwrap();
    }

    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    gui.layout.update_size(120, 40);
    let state = popup::PatchFilePickerState::new(repo.path().to_path_buf(), picker_dir).unwrap();
    gui.popup = PopupState::PatchFilePicker {
        state,
        on_confirm: Box::new(|_, _| Ok(())),
    };

    gui.handle_popup_key(KeyEvent::new(
        KeyCode::Char('J'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ))
    .unwrap();

    assert!(matches!(
        &gui.popup,
        PopupState::PatchFilePicker { state, .. }
            if state.selected == 1 && state.picker_scroll_active()
    ));
}

#[test]
fn patch_picker_supports_vim_counts_and_top_bottom_jumps() {
    let repo = TempRepo::new("patch-picker-counts");
    let picker_dir = repo.path().join("patches");
    std::fs::create_dir_all(&picker_dir).unwrap();
    for index in 0..40 {
        std::fs::write(
            picker_dir.join(format!("{index:02}.patch")),
            format!("diff --git a/a b/a\n+line {index}\n"),
        )
        .unwrap();
    }

    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    gui.layout.update_size(120, 40);
    let state = popup::PatchFilePickerState::new(repo.path().to_path_buf(), picker_dir).unwrap();
    gui.popup = PopupState::PatchFilePicker {
        state,
        on_confirm: Box::new(|_, _| Ok(())),
    };

    for ch in ['2', '0', 'j'] {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .unwrap();
    }
    assert!(
        matches!(&gui.popup, PopupState::PatchFilePicker { state, .. } if state.selected == 20)
    );

    for ch in ['2', '0', 'k'] {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
            .unwrap();
    }
    assert!(matches!(&gui.popup, PopupState::PatchFilePicker { state, .. } if state.selected == 0));

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::PatchFilePicker { state, .. } if state.selected == 39)
    );

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::PatchFilePicker { state, .. } if state.selected == 0));
}

#[test]
fn commit_editor_ctrl_j_k_animate_cursor_and_viewport_but_keep_short_text_fixed() {
    let repo = TempRepo::new("commit-vim-scroll");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.layout.update_size(120, 40);
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    let wrap_width = gui.commit_body_wrap_width();
    if let PopupState::CommitInput {
        focus,
        summary_textarea,
        body_textarea,
        body_state,
        ..
    } = &mut gui.popup
    {
        body_state.set_text(
            (0..60)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        body_state.move_to_start();
        body_state.render_into(body_textarea, wrap_width);
        popup::set_commit_input_focus(
            focus,
            summary_textarea,
            body_textarea,
            popup::CommitInputFocus::Body,
        );
    }

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .unwrap();
    let queued = match &gui.popup {
        PopupState::CommitInput {
            body_scroll_remaining,
            ..
        } => *body_scroll_remaining,
        _ => 0,
    };
    assert_eq!(queued, 10);
    gui.tick_commit_input_scroll_animation();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        body_scroll_remaining, body_scroll_top, body_state, body_textarea, ..
    } if *body_scroll_remaining == 9
        && *body_scroll_top == 1
        && body_state.visual_cursor(wrap_width) == (1, 0)
        && body_state.visual_cursor(wrap_width) == body_textarea.cursor()));

    // Complete the half-page motion: long content must visibly advance
    // both the cursor and the explicit viewport by the full target amount.
    for _ in 0..9 {
        gui.tick_commit_input_scroll_animation();
    }
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        body_scroll_remaining, body_scroll_top, body_state, body_textarea, ..
    } if *body_scroll_remaining == 0
        && *body_scroll_top == 10
        && body_state.visual_cursor(wrap_width) == (10, 0)
        && body_state.visual_cursor(wrap_width) == body_textarea.cursor()));

    // A new opposite-direction command must reverse the exact same motion.
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_scroll_remaining, .. }
        if *body_scroll_remaining == -10)
    );
    for _ in 0..10 {
        gui.tick_commit_input_scroll_animation();
    }
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        body_scroll_remaining, body_scroll_top, body_state, body_textarea, ..
    } if *body_scroll_remaining == 0
        && *body_scroll_top == 0
        && body_state.visual_cursor(wrap_width) == (0, 0)
        && body_state.visual_cursor(wrap_width) == body_textarea.cursor()));

    // Short descriptions are the important regression case: Ctrl-J/K must
    // still animate the cursor even though the viewport itself cannot and
    // should not scroll because every line already fits on screen.
    if let PopupState::CommitInput {
        body_state,
        body_textarea,
        body_scroll_top,
        ..
    } = &mut gui.popup
    {
        body_state.set_text("one\ntwo\nthree");
        body_state.move_to_start();
        body_state.render_into(body_textarea, wrap_width);
        *body_scroll_top = 0;
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(
        matches!(&gui.popup, PopupState::CommitInput { body_scroll_remaining, .. }
        if *body_scroll_remaining == 10)
    );
    gui.tick_commit_input_scroll_animation();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        body_scroll_remaining, body_scroll_top, body_state, body_textarea, ..
    } if *body_scroll_remaining == 9
        && *body_scroll_top == 0
        && body_state.visual_cursor(wrap_width) == (1, 0)
        && body_state.visual_cursor(wrap_width) == body_textarea.cursor()));

    // Let the short-buffer animation finish.  It must stop on the final
    // real row without ever moving the viewport away from zero.
    for _ in 0..16 {
        gui.tick_commit_input_scroll_animation();
    }
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        body_scroll_remaining, body_scroll_top, body_state, body_textarea, ..
    } if *body_scroll_remaining == 0
        && *body_scroll_top == 0
        && body_state.visual_cursor(wrap_width) == (2, 0)
        && body_state.visual_cursor(wrap_width) == body_textarea.cursor()));

    // And Ctrl-K must animate back upward even though there was never any
    // viewport room to scroll.
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
        .unwrap();
    gui.tick_commit_input_scroll_animation();
    assert!(matches!(&gui.popup, PopupState::CommitInput {
        body_scroll_remaining, body_scroll_top, body_state, ..
    } if *body_scroll_remaining == -9
        && *body_scroll_top == 0
        && body_state.visual_cursor(wrap_width) == (1, 0)));
}

#[test]
fn commit_editor_normal_escape_stays_open_and_q_saves_draft() {
    let repo = TempRepo::new("commit-vim-q");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::CommitInput { mode, .. }
        if *mode == popup::CommitInputMode::Normal));

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(&gui.popup, PopupState::None));
    assert!(matches!(
        &gui.saved_commit_popup,
        Some(PopupState::CommitInput { .. })
    ));
}

#[test]
fn q_quits_patch_email_outside_recipient_editor() {
    let repo = TempRepo::new("patch-email-q");
    repo.commit("file", "base");
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    gui.popup = PopupState::EmailPatch {
        title: "Email patch test".to_string(),
        to_recipients: vec!["to@example.com".to_string()],
        cc_recipients: Vec::new(),
        focus: popup::EmailPatchFocus::To,
        to_selected: 0,
        cc_selected: 0,
        recipient_editor: None,
        recipient_undo: Vec::new(),
        dry_run: false,
        reviewing: true,
        preview_from: String::new(),
        preview_subject: String::new(),
        preview_date: String::new(),
        preview_body: "patch".to_string(),
        preview_scroll: 0,
        preview_scroll_from: 0.0,
        preview_scroll_target: 0.0,
        preview_scroll_started_at: None,
        confirming_send: false,
        resolving_recipients: false,
        resolution_request_id: 1,
        on_confirm: Box::new(|_, _, _, _| Ok(())),
    };

    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(gui.popup, PopupState::None));
}

#[test]
fn reopening_saved_commit_draft_resets_view_to_top_left() {
    let repo = TempRepo::new("commit-draft-view");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    for c in "subject".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        .unwrap();
    for c in "description".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    // Escape only returns to Normal; `q` is the explicit draft/quit key.
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    assert!(matches!(&gui.popup, PopupState::CommitInput {
        summary_textarea, body_textarea, body_state, focus, ..
    } if summary_textarea.cursor() == (0, 0)
        && body_textarea.cursor() == (0, 0)
        && body_state.cursor == 0
        && *focus == popup::CommitInputFocus::Summary));
}

#[test]
fn ctrl_m_in_commit_editor_is_ignored() {
    let repo = TempRepo::new("ctrl-m-editor");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();

    for c in "subject".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    gui.handle_popup_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL))
        .unwrap();
    for c in "body".chars() {
        gui.handle_popup_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }

    assert!(matches!(&gui.popup, PopupState::CommitInput {
        summary_textarea, body_state, focus, ..
    } if summary_textarea.lines().len() == 1
        && summary_textarea.lines()[0] == "subjectbody"
        && *focus == popup::CommitInputFocus::Summary
        && body_state.raw().is_empty()));
}

#[test]
fn ctrl_g_in_commit_editor_still_generates_message() {
    let repo = TempRepo::new("ctrl-g-editor");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    repo.git(&["add", "file"]);
    let mut config = crate::config::AppConfig::default();
    config.user_config.git.commit.generate_command.clear();
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(config, git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(gui.popup, PopupState::CommitInput { .. }));
    gui.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(gui.context_mgr.active(), ContextId::Files);
    assert!(matches!(
        &gui.popup,
        PopupState::Message { title, .. } if title == "AI generation unavailable"
    ));
}

#[test]
fn files_ai_commit_shortcut_can_be_remapped() {
    let repo = TempRepo::new("remapped-ai-commit");
    repo.commit("file", "base");
    std::fs::write(repo.path().join("file"), "changed").unwrap();
    let mut config = crate::config::AppConfig::default();
    config.user_config.keybinding.files.generate_ai_commit = "<c-t>".into();
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(config, git, None, false).unwrap();
    *gui.model.lock().unwrap() = gui.git.load_model().unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(matches!(gui.popup, PopupState::None));
    assert_eq!(gui.context_mgr.active(), ContextId::Files);
    gui.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(matches!(
        &gui.popup,
        PopupState::Confirm { title, .. } if title == "No files staged"
    ));
}

#[test]
fn ctrl_g_no_longer_switches_panels() {
    let repo = TempRepo::new("ctrl-g-no-toggle");
    repo.commit("file", "base");
    for context in [
        ContextId::Files,
        ContextId::Commits,
        ContextId::CommitFiles,
        ContextId::Reflog,
    ] {
        for diff_focused in [false, true] {
            let git = crate::git::GitCommands::new(repo.path()).unwrap();
            let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
            gui.context_mgr.set_active(context);
            gui.diff_focused = diff_focused;
            gui.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
                .unwrap();
            assert_eq!(gui.context_mgr.active(), context);
            assert_eq!(gui.diff_focused, diff_focused);
        }
    }
}

#[test]
fn start_in_commits_initializes_context_to_commits_head() {
    let repo = TempRepo::new("start-in-commits");
    let config = crate::config::AppConfig::default();
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let gui = Gui::new(config, git, None, true).unwrap();

    assert_eq!(gui.context_mgr.active(), ContextId::Commits);
    assert_eq!(gui.context_mgr.selected(ContextId::Commits), 0);
}

#[test]
fn head_focus_selects_checked_out_hash_not_another_branch_tip() {
    let repo = TempRepo::new("head-not-first");
    let head = repo.commit("file", "main commit");
    repo.git(&["checkout", "-b", "ahead"]);
    let other = repo.commit("file", "other branch commit");
    repo.git(&["checkout", "main"]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    // The normal commits panel is intentionally HEAD-relative now. Build an
    // explicit multi-branch list here so this regression still verifies that
    // focusing HEAD selects the checked-out hash rather than another branch tip.
    let commits = git
        .load_commits_for_branches(&["main".to_string(), "ahead".to_string()], 300)
        .unwrap();
    assert_eq!(commits[0].hash, other);
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    gui.model.lock().unwrap().set_commits(commits);
    gui.focus_head_commit().unwrap();
    assert_eq!(gui.context_mgr.active(), ContextId::Commits);
    let index = gui.context_mgr.selected(ContextId::Commits);
    assert_eq!(gui.model.lock().unwrap().commits[index].hash, head);
    assert_eq!(gui.context_mgr.scroll_offset(ContextId::Commits), index);
}

#[test]
fn head_focus_preserves_filters_and_uses_separate_head_history() {
    let repo = TempRepo::new("filtered-head");
    let older = repo.commit("filtered", "older");
    let head = repo.commit("other", "head");
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();

    for filter in [
        crate::git::commit::CommitFilter {
            path: Some("filtered".into()),
            ..Default::default()
        },
        crate::git::commit::CommitFilter {
            authors: vec!["nobody@example.com".into()],
            ..Default::default()
        },
        crate::git::commit::CommitFilter {
            branches: vec![older.clone()],
            ..Default::default()
        },
    ] {
        let commits = gui.git.load_filtered_commits_page(&filter, 300, 0).unwrap();
        let hashes: Vec<_> = commits.iter().map(|c| c.hash.clone()).collect();
        assert!(!hashes.contains(&head));
        gui.commit_path_filter = filter.path.clone();
        gui.commit_author_filter = filter.authors.clone();
        gui.commit_branch_filter = filter.branches.clone();
        gui.model.lock().unwrap().set_commits(commits);
        gui.context_mgr.set_active(ContextId::Files);
        gui.context_mgr.set_selection(2);
        gui.diff_focused = true;
        gui.focus_head_commit().unwrap();
        assert_eq!(gui.context_mgr.active(), ContextId::BranchCommits);
        assert_eq!(gui.model.lock().unwrap().sub_commits[0].hash, head);
        assert_eq!(gui.commit_path_filter, filter.path);
        assert_eq!(gui.commit_author_filter, filter.authors);
        assert_eq!(gui.commit_branch_filter, filter.branches);
        assert_eq!(
            gui.model
                .lock()
                .unwrap()
                .commits
                .iter()
                .map(|c| c.hash.clone())
                .collect::<Vec<_>>(),
            hashes
        );

        // Streaming model refreshes do not retain sub-history themselves.
        gui.model.lock().unwrap().clear_sub_commits();
        gui.after_model_refresh().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            gui.receive_ui_jobs();
            if !gui.model.lock().unwrap().sub_commits.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "async branch history refresh did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(gui.model.lock().unwrap().sub_commits[0].hash, head);
        gui.context_mgr.set_active(ContextId::BranchCommitFiles);
        gui.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(gui.context_mgr.active(), ContextId::Files);
        assert_eq!(gui.context_mgr.selected(ContextId::Files), 2);
        assert!(!gui.diff_focused);
    }
}

#[test]
fn head_focus_handles_head_outside_the_loaded_page_and_detached_head() {
    let repo = TempRepo::new("paged-head");
    let older = repo.commit("file", "older");
    let head = repo.commit("file", "head");
    repo.git(&["checkout", "--detach", &head]);
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let commits = git.load_commits_for_branch(&older, 1).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, false).unwrap();
    gui.model.lock().unwrap().set_commits(commits);
    gui.focus_head_commit().unwrap();
    assert_eq!(gui.context_mgr.active(), ContextId::BranchCommits);
    assert_eq!(gui.model.lock().unwrap().sub_commits[0].hash, head);
    assert_eq!(gui.model.lock().unwrap().commits[0].hash, older);
}

#[test]
fn commits_startup_selects_actual_head_and_preserves_path_filter() {
    let repo = TempRepo::new("startup-head");
    repo.commit("filtered", "older");
    let head = repo.commit("other", "head");
    repo.git(&["checkout", "-b", "ahead"]);
    repo.commit("other", "ahead");
    repo.git(&["checkout", "main"]);
    for path in [None, Some(PathBuf::from("filtered"))] {
        let git = crate::git::GitCommands::new(repo.path()).unwrap();
        let mut gui =
            Gui::new(crate::config::AppConfig::default(), git, path.clone(), true).unwrap();
        let commits = gui
            .git
            .load_filtered_commits_page(&gui.commit_filter_for_load().unwrap_or_default(), 300, 0)
            .unwrap();
        gui.model.lock().unwrap().set_commits(commits);
        gui.select_startup_head().unwrap();
        assert!(!gui.pending_startup_head);
        let model = gui.model.lock().unwrap();
        let selected = gui.context_mgr.selected_active();
        let commit = if path.is_some() {
            &model.sub_commits[selected]
        } else {
            &model.commits[selected]
        };
        assert_eq!(commit.hash, head);
        assert_eq!(
            gui.commit_path_filter,
            path.map(|p| p.to_string_lossy().to_string())
        );
    }
}

#[test]
fn startup_head_does_not_override_navigation() {
    let repo = TempRepo::new("startup-navigation");
    let git = crate::git::GitCommands::new(repo.path()).unwrap();
    let mut gui = Gui::new(crate::config::AppConfig::default(), git, None, true).unwrap();
    gui.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(gui.context_mgr.active(), ContextId::Files);
    gui.select_startup_head().unwrap();
    assert_eq!(gui.context_mgr.active(), ContextId::Files);
}
