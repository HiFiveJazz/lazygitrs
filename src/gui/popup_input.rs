//! Keyboard handling for modal popups and searchable pickers.
//!
//! Kept separate from the main event loop so popup-specific state machines do
//! not dominate `gui::mod`, while remaining ordinary monomorphized `Gui`
//! methods with no runtime indirection.

use super::*;

impl Gui {
    pub(crate) fn handle_popup_key(&mut self, key: KeyEvent) -> Result<()> {
        // With enhanced keyboard reporting Ctrl+M can arrive as a distinct key
        // even though legacy terminals encode it as the same carriage return as
        // Enter. Lazygitrs intentionally assigns no popup/editor action to Ctrl+M.
        // Swallow it before any textarea or menu can reinterpret it.
        if key.code == KeyCode::Char('m') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }

        let was_help = matches!(self.popup, PopupState::CommandPalette { .. });
        let was_ref_picker = matches!(self.popup, PopupState::RefPicker { .. });
        let was_list_picker = matches!(self.popup, PopupState::ListPicker { .. });
        let was_patch_file_picker = matches!(self.popup, PopupState::PatchFilePicker { .. });
        let was_theme_picker = matches!(self.popup, PopupState::ThemePicker { .. });

        match &self.popup {
            PopupState::ConfirmResetToUpstream { branch, upstream } => {
                if key.code == KeyCode::Char('y') {
                    let branch = branch.clone();
                    let upstream = upstream.clone();
                    self.popup = PopupState::None;
                    self.start_reset_to_upstream(branch, upstream);
                } else if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n')) {
                    self.popup = PopupState::None;
                }
            }
            PopupState::Confirm { .. } => {
                if key.code == KeyCode::Char('y') || key.code == KeyCode::Enter {
                    let popup = std::mem::replace(&mut self.popup, PopupState::None);
                    if let PopupState::Confirm { on_confirm, .. } = popup {
                        if let Err(e) = on_confirm(self) {
                            self.popup = PopupState::Message {
                                title: "Error".to_string(),
                                message: format!("{}", e),
                                kind: MessageKind::Error,
                            };
                        }
                    }
                } else {
                    self.popup = PopupState::None;
                }
            }
            PopupState::ShellCommand { textarea } => {
                if key.code == KeyCode::Esc {
                    self.popup = PopupState::None;
                } else if key.code == KeyCode::Enter {
                    let command = textarea.lines().join("\n");
                    self.popup = PopupState::None;
                    controller::custom_commands::confirm_shell_prompt(self, &command)?;
                } else if let PopupState::ShellCommand { textarea } = &mut self.popup {
                    textarea_input(textarea, key);
                }
            }
            PopupState::PatchPreview { .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.popup = PopupState::None;
                } else if let PopupState::PatchPreview {
                    message,
                    scroll,
                    cursor_line,
                    smooth_scroll_from,
                    smooth_scroll_target,
                    smooth_scroll_started_at,
                    smooth_cursor_from,
                    smooth_cursor_target,
                    smooth_cursor_started_at,
                    ..
                } = &mut self.popup
                {
                    let area = Rect::new(0, 0, self.layout.width, self.layout.height);
                    let (rows, total) = views::patch_preview_metrics(message, area);
                    let last = total.saturating_sub(1);
                    let max_scroll = total.saturating_sub(rows);
                    let old_cursor = (*cursor_line).min(last);
                    let pending_cursor = if smooth_cursor_started_at.is_some() {
                        smooth_cursor_target.round() as usize
                    } else {
                        old_cursor
                    }
                    .min(last);
                    let (new_cursor, animate_cursor) = match (key.modifiers, key.code) {
                        (m, KeyCode::Char('j')) if m.contains(KeyModifiers::CONTROL) => {
                            (pending_cursor.saturating_add(22).min(last), true)
                        }
                        (m, KeyCode::Char('k')) if m.contains(KeyModifiers::CONTROL) => {
                            (pending_cursor.saturating_sub(22), true)
                        }
                        (_, KeyCode::Char('j') | KeyCode::Down) => {
                            (old_cursor.saturating_add(1).min(last), false)
                        }
                        (_, KeyCode::Char('k') | KeyCode::Up) => {
                            (old_cursor.saturating_sub(1), false)
                        }
                        (_, KeyCode::PageDown) => {
                            (pending_cursor.saturating_add(rows).min(last), true)
                        }
                        (_, KeyCode::PageUp) => (pending_cursor.saturating_sub(rows), true),
                        (_, KeyCode::Home | KeyCode::Char('g')) => (0, false),
                        (_, KeyCode::End | KeyCode::Char('G')) => (last, false),
                        (_, KeyCode::Char('y')) => {
                            let _ = Platform::copy_to_clipboard(message);
                            return Ok(());
                        }
                        _ => return Ok(()),
                    };

                    if animate_cursor && new_cursor != old_cursor {
                        *smooth_cursor_from = old_cursor as f64;
                        *smooth_cursor_target = new_cursor as f64;
                        *smooth_cursor_started_at = Some(Instant::now());
                    } else {
                        *cursor_line = new_cursor;
                        *smooth_cursor_from = new_cursor as f64;
                        *smooth_cursor_target = new_cursor as f64;
                        *smooth_cursor_started_at = None;
                    }

                    let viewport_start = (*scroll).min(max_scroll);
                    let viewport_end = viewport_start.saturating_add(rows.saturating_sub(1));
                    let target = if new_cursor < viewport_start {
                        new_cursor.min(max_scroll)
                    } else if new_cursor > viewport_end {
                        new_cursor
                            .saturating_sub(rows.saturating_sub(1))
                            .min(max_scroll)
                    } else {
                        viewport_start
                    };

                    if target != *scroll {
                        *smooth_scroll_from = *scroll as f64;
                        *smooth_scroll_target = target as f64;
                        *smooth_scroll_started_at = Some(Instant::now());
                    } else {
                        // A new navigation command can invalidate an older pending
                        // viewport destination even when the desired offset is the
                        // row currently on screen. Do not let that stale animation
                        // keep pulling the preview after the cursor was retargeted.
                        *smooth_scroll_from = *scroll as f64;
                        *smooth_scroll_target = *scroll as f64;
                        *smooth_scroll_started_at = None;
                    }
                }
            }
            PopupState::CommandOutput { .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.popup = PopupState::None;
                } else if let PopupState::CommandOutput {
                    message, scroll, ..
                } = &mut self.popup
                {
                    let max = views::command_output_max_scroll(
                        message,
                        Rect::new(0, 0, self.layout.width, self.layout.height),
                    );
                    let page = (usize::from(self.layout.height) * 4 / 5)
                        .saturating_sub(3)
                        .max(1);
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            *scroll = scroll.saturating_add(1).min(max)
                        }
                        KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
                        KeyCode::PageDown => *scroll = scroll.saturating_add(page).min(max),
                        KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
                        KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                        KeyCode::End | KeyCode::Char('G') => *scroll = max,
                        KeyCode::Char('y') => {
                            let _ = Platform::copy_to_clipboard(message);
                        }
                        _ => {}
                    }
                }
            }
            PopupState::Message { .. } => {
                // Any key dismisses the message
                self.popup = PopupState::None;
                self.deferred_notification_visible = false;
            }
            PopupState::EmailPatch {
                reviewing,
                resolving_recipients,
                ..
            } => {
                use crossterm::event::KeyModifiers;

                let reviewing = *reviewing;
                let resolving_recipients = *resolving_recipients;

                // Match the commit editor's Vim-style close key. `q` closes the
                // patch-email workflow whenever a recipient text editor is not
                // actively accepting literal input; Esc keeps its existing
                // review/back/cancel semantics.
                let recipient_editor_active = matches!(
                    &self.popup,
                    PopupState::EmailPatch {
                        recipient_editor: Some(_),
                        ..
                    }
                );
                if key.code == KeyCode::Char('q')
                    && key.modifiers.is_empty()
                    && !recipient_editor_active
                {
                    let popup = std::mem::replace(&mut self.popup, PopupState::None);
                    drop(popup);
                    trim_process_heap();
                    return Ok(());
                }

                if resolving_recipients {
                    if key.code == KeyCode::Esc {
                        let popup = std::mem::replace(&mut self.popup, PopupState::None);
                        drop(popup);
                        trim_process_heap();
                    }
                    return Ok(());
                }

                let toggle_dry_run =
                    key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL);
                let send_confirmation_open = matches!(
                    self.popup,
                    PopupState::EmailPatch {
                        confirming_send: true,
                        ..
                    }
                );
                if toggle_dry_run && !send_confirmation_open {
                    if let PopupState::EmailPatch { dry_run, .. } = &mut self.popup {
                        *dry_run = !*dry_run;
                    }
                    return Ok(());
                }

                if reviewing {
                    let confirming_send = matches!(
                        self.popup,
                        PopupState::EmailPatch {
                            confirming_send: true,
                            ..
                        }
                    );

                    if confirming_send {
                        match key.code {
                            KeyCode::Enter | KeyCode::Char('y') => {
                                self.run_email_patch_action();
                            }
                            KeyCode::Esc | KeyCode::Char('n') => {
                                if let PopupState::EmailPatch {
                                    confirming_send, ..
                                } = &mut self.popup
                                {
                                    *confirming_send = false;
                                }
                            }
                            _ => {}
                        }
                        return Ok(());
                    }

                    if key.code == KeyCode::Enter || key.code == KeyCode::Char('y') {
                        let dry_run =
                            matches!(self.popup, PopupState::EmailPatch { dry_run: true, .. });
                        if dry_run {
                            self.run_email_patch_action();
                        } else if let PopupState::EmailPatch {
                            confirming_send, ..
                        } = &mut self.popup
                        {
                            *confirming_send = true;
                        }
                    } else if key.code == KeyCode::Esc || key.code == KeyCode::Char('n') {
                        if let PopupState::EmailPatch {
                            reviewing,
                            focus,
                            preview_scroll,
                            preview_scroll_from,
                            preview_scroll_target,
                            preview_scroll_started_at,
                            confirming_send,
                            ..
                        } = &mut self.popup
                        {
                            *reviewing = false;
                            *focus = popup::EmailPatchFocus::To;
                            *preview_scroll = 0;
                            *preview_scroll_from = 0.0;
                            *preview_scroll_target = 0.0;
                            *preview_scroll_started_at = None;
                            *confirming_send = false;
                        }
                    } else {
                        let area = Rect::new(0, 0, self.layout.width, self.layout.height);
                        let max = if let PopupState::EmailPatch {
                            preview_from,
                            to_recipients,
                            cc_recipients,
                            preview_subject,
                            preview_date,
                            preview_body,
                            ..
                        } = &self.popup
                        {
                            views::email_preview_max_scroll(
                                preview_from,
                                to_recipients,
                                cc_recipients,
                                preview_subject,
                                preview_date,
                                preview_body,
                                area,
                            )
                        } else {
                            0
                        };
                        let page = (usize::from(self.layout.height) * 4 / 5)
                            .saturating_sub(8)
                            .max(1);

                        if let PopupState::EmailPatch {
                            preview_scroll,
                            preview_scroll_from,
                            preview_scroll_target,
                            preview_scroll_started_at,
                            ..
                        } = &mut self.popup
                        {
                            let current = (*preview_scroll).min(max);
                            let pending = if preview_scroll_started_at.is_some() {
                                preview_scroll_target.round() as usize
                            } else {
                                current
                            }
                            .min(max);
                            let target = match (key.modifiers, key.code) {
                                (m, KeyCode::Char('j')) if m.contains(KeyModifiers::CONTROL) => {
                                    pending.saturating_add(22).min(max)
                                }
                                (m, KeyCode::Char('k')) if m.contains(KeyModifiers::CONTROL) => {
                                    pending.saturating_sub(22)
                                }
                                (_, KeyCode::Char('j') | KeyCode::Down) => {
                                    pending.saturating_add(1).min(max)
                                }
                                (_, KeyCode::Char('k') | KeyCode::Up) => pending.saturating_sub(1),
                                (_, KeyCode::PageDown) => pending.saturating_add(page).min(max),
                                (_, KeyCode::PageUp) => pending.saturating_sub(page),
                                (_, KeyCode::Home | KeyCode::Char('g')) => 0,
                                (_, KeyCode::End | KeyCode::Char('G')) => max,
                                _ => pending,
                            };

                            if target != current || preview_scroll_started_at.is_some() {
                                let one_line_move = target.abs_diff(pending) == 1
                                    && matches!(
                                        key.code,
                                        KeyCode::Char('j')
                                            | KeyCode::Char('k')
                                            | KeyCode::Down
                                            | KeyCode::Up
                                    )
                                    && !key.modifiers.contains(KeyModifiers::CONTROL);

                                if one_line_move {
                                    // A terminal cannot render a fractional row. Animating a
                                    // one-line move only delays the visible update, which feels
                                    // like a stutter. Move single rows immediately and reserve
                                    // interpolation for multi-row jumps where it has frames to
                                    // show.
                                    *preview_scroll = target;
                                    *preview_scroll_from = target as f64;
                                    *preview_scroll_target = target as f64;
                                    *preview_scroll_started_at = None;
                                } else {
                                    *preview_scroll_from = current as f64;
                                    *preview_scroll_target = target as f64;
                                    *preview_scroll_started_at = Some(Instant::now());
                                }
                            }
                        }
                    }
                    return Ok(());
                }

                let editor_active = matches!(
                    &self.popup,
                    PopupState::EmailPatch {
                        recipient_editor: Some(_),
                        ..
                    }
                );
                if editor_active {
                    if key.code == KeyCode::Esc {
                        if let PopupState::EmailPatch {
                            recipient_editor, ..
                        } = &mut self.popup
                        {
                            *recipient_editor = None;
                        }
                        return Ok(());
                    }

                    if key.code == KeyCode::Enter {
                        let value = if let PopupState::EmailPatch {
                            recipient_editor: Some(editor),
                            ..
                        } = &self.popup
                        {
                            editor.textarea.lines().join(" ").trim().to_string()
                        } else {
                            String::new()
                        };
                        let parsed = popup::split_email_recipients(&value);
                        if parsed.is_empty() {
                            self.show_action_toast("Enter a recipient");
                            return Ok(());
                        }

                        let (editing_field, editing_index) = if let PopupState::EmailPatch {
                            recipient_editor: Some(editor),
                            ..
                        } = &self.popup
                        {
                            (editor.field, editor.index)
                        } else {
                            return Ok(());
                        };
                        let validation = if let PopupState::EmailPatch {
                            to_recipients,
                            cc_recipients,
                            ..
                        } = &self.popup
                        {
                            popup::validate_email_recipient_candidates(
                                &parsed,
                                to_recipients,
                                cc_recipients,
                                editing_index.map(|index| (editing_field, index)),
                            )
                        } else {
                            Ok(())
                        };
                        if let Err(message) = validation {
                            self.show_action_toast(message);
                            return Ok(());
                        }

                        if let PopupState::EmailPatch {
                            to_recipients,
                            cc_recipients,
                            to_selected,
                            cc_selected,
                            recipient_editor,
                            recipient_undo,
                            ..
                        } = &mut self.popup
                        {
                            let editor = recipient_editor.take().expect("email editor disappeared");
                            let (recipients, selected) = match editor.field {
                                popup::EmailPatchFocus::To => (to_recipients, to_selected),
                                popup::EmailPatchFocus::Cc => (cc_recipients, cc_selected),
                            };
                            if let Some(index) =
                                editor.index.filter(|index| *index < recipients.len())
                            {
                                let old_value = recipients[index].clone();
                                let inserted = parsed.len();
                                recipients.splice(index..=index, parsed);
                                popup::push_email_recipient_undo(
                                    recipient_undo,
                                    popup::EmailRecipientUndo::Replaced {
                                        field: editor.field,
                                        index,
                                        old_value,
                                        new_count: inserted,
                                    },
                                );
                                *selected = index.saturating_add(inserted.saturating_sub(1));
                            } else {
                                let index = recipients.len();
                                let inserted = parsed.len();
                                recipients.extend(parsed);
                                popup::push_email_recipient_undo(
                                    recipient_undo,
                                    popup::EmailRecipientUndo::Inserted {
                                        field: editor.field,
                                        index,
                                        count: inserted,
                                    },
                                );
                                *selected = recipients.len().saturating_sub(1);
                            }
                        }
                        return Ok(());
                    }

                    if let PopupState::EmailPatch {
                        recipient_editor: Some(editor),
                        ..
                    } = &mut self.popup
                    {
                        textarea_input(&mut editor.textarea, key);
                    }
                    return Ok(());
                }

                if key.code == KeyCode::Esc {
                    let popup = std::mem::replace(&mut self.popup, PopupState::None);
                    drop(popup);
                    trim_process_heap();
                } else if key.code == KeyCode::Enter {
                    let missing_to = if let PopupState::EmailPatch {
                        to_recipients,
                        reviewing,
                        focus,
                        preview_scroll,
                        preview_scroll_from,
                        preview_scroll_target,
                        preview_scroll_started_at,
                        confirming_send,
                        ..
                    } = &mut self.popup
                    {
                        if to_recipients.is_empty() {
                            *focus = popup::EmailPatchFocus::To;
                            true
                        } else {
                            *reviewing = true;
                            *preview_scroll = 0;
                            *preview_scroll_from = 0.0;
                            *preview_scroll_target = 0.0;
                            *preview_scroll_started_at = None;
                            *confirming_send = false;
                            false
                        }
                    } else {
                        false
                    };
                    if missing_to {
                        self.show_action_toast("Add a To recipient");
                    }
                } else if key.code == KeyCode::BackTab
                    || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
                {
                    if let PopupState::EmailPatch { focus, .. } = &mut self.popup {
                        *focus = focus.previous();
                    }
                } else if key.code == KeyCode::Tab {
                    if let PopupState::EmailPatch { focus, .. } = &mut self.popup {
                        *focus = focus.next();
                    }
                } else if matches!(key.code, KeyCode::Char('j') | KeyCode::Down) {
                    if let PopupState::EmailPatch {
                        focus,
                        to_recipients,
                        cc_recipients,
                        to_selected,
                        cc_selected,
                        ..
                    } = &mut self.popup
                    {
                        let (recipients, selected) = match *focus {
                            popup::EmailPatchFocus::To => (to_recipients, to_selected),
                            popup::EmailPatchFocus::Cc => (cc_recipients, cc_selected),
                        };
                        if !recipients.is_empty() {
                            *selected = selected.saturating_add(1).min(recipients.len() - 1);
                        }
                    }
                } else if matches!(key.code, KeyCode::Char('k') | KeyCode::Up) {
                    if let PopupState::EmailPatch {
                        focus,
                        to_selected,
                        cc_selected,
                        ..
                    } = &mut self.popup
                    {
                        let selected = match *focus {
                            popup::EmailPatchFocus::To => to_selected,
                            popup::EmailPatchFocus::Cc => cc_selected,
                        };
                        *selected = selected.saturating_sub(1);
                    }
                } else if matches!(key.code, KeyCode::Char('g') | KeyCode::Home) {
                    if let PopupState::EmailPatch {
                        focus,
                        to_selected,
                        cc_selected,
                        ..
                    } = &mut self.popup
                    {
                        match *focus {
                            popup::EmailPatchFocus::To => *to_selected = 0,
                            popup::EmailPatchFocus::Cc => *cc_selected = 0,
                        }
                    }
                } else if matches!(key.code, KeyCode::Char('G') | KeyCode::End) {
                    if let PopupState::EmailPatch {
                        focus,
                        to_recipients,
                        cc_recipients,
                        to_selected,
                        cc_selected,
                        ..
                    } = &mut self.popup
                    {
                        match *focus {
                            popup::EmailPatchFocus::To => {
                                *to_selected = to_recipients.len().saturating_sub(1)
                            }
                            popup::EmailPatchFocus::Cc => {
                                *cc_selected = cc_recipients.len().saturating_sub(1)
                            }
                        }
                    }
                } else if matches!(key.code, KeyCode::Char('d') | KeyCode::Delete) {
                    if let PopupState::EmailPatch {
                        focus,
                        to_recipients,
                        cc_recipients,
                        to_selected,
                        cc_selected,
                        recipient_undo,
                        ..
                    } = &mut self.popup
                    {
                        let field = *focus;
                        let (recipients, selected) = match field {
                            popup::EmailPatchFocus::To => (to_recipients, to_selected),
                            popup::EmailPatchFocus::Cc => (cc_recipients, cc_selected),
                        };
                        if !recipients.is_empty() {
                            let index = (*selected).min(recipients.len() - 1);
                            let value = recipients.remove(index);
                            popup::push_email_recipient_undo(
                                recipient_undo,
                                popup::EmailRecipientUndo::Deleted {
                                    field,
                                    index,
                                    value,
                                },
                            );
                            *selected = index.min(recipients.len().saturating_sub(1));
                        }
                    }
                } else if key.code == KeyCode::Char('u') && key.modifiers.is_empty() {
                    if let PopupState::EmailPatch {
                        focus,
                        to_recipients,
                        cc_recipients,
                        to_selected,
                        cc_selected,
                        recipient_undo,
                        ..
                    } = &mut self.popup
                    {
                        if let Some(entry) = recipient_undo.pop() {
                            let field = match &entry {
                                popup::EmailRecipientUndo::Deleted { field, .. }
                                | popup::EmailRecipientUndo::Inserted { field, .. }
                                | popup::EmailRecipientUndo::Replaced { field, .. } => *field,
                            };
                            *focus = field;
                            let (recipients, selected) = match field {
                                popup::EmailPatchFocus::To => (to_recipients, to_selected),
                                popup::EmailPatchFocus::Cc => (cc_recipients, cc_selected),
                            };
                            match entry {
                                popup::EmailRecipientUndo::Deleted { index, value, .. } => {
                                    let index = index.min(recipients.len());
                                    recipients.insert(index, value);
                                    *selected = index;
                                }
                                popup::EmailRecipientUndo::Inserted { index, count, .. } => {
                                    let start = index.min(recipients.len());
                                    let end = start.saturating_add(count).min(recipients.len());
                                    recipients.drain(start..end);
                                    *selected = start.min(recipients.len().saturating_sub(1));
                                }
                                popup::EmailRecipientUndo::Replaced {
                                    index,
                                    old_value,
                                    new_count,
                                    ..
                                } => {
                                    let start = index.min(recipients.len());
                                    let end = start.saturating_add(new_count).min(recipients.len());
                                    recipients.drain(start..end);
                                    let restore_at = start.min(recipients.len());
                                    recipients.insert(restore_at, old_value);
                                    *selected = restore_at;
                                }
                            }
                        }
                    }
                } else if key.code == KeyCode::Char('a') || key.code == KeyCode::Char('e') {
                    if let PopupState::EmailPatch {
                        focus,
                        to_recipients,
                        cc_recipients,
                        to_selected,
                        cc_selected,
                        recipient_editor,
                        ..
                    } = &mut self.popup
                    {
                        let field = *focus;
                        let (recipients, selected) = match field {
                            popup::EmailPatchFocus::To => (to_recipients, to_selected),
                            popup::EmailPatchFocus::Cc => (cc_recipients, cc_selected),
                        };
                        let editing = key.code == KeyCode::Char('e') && !recipients.is_empty();
                        let index = editing.then_some((*selected).min(recipients.len() - 1));
                        let mut textarea = popup::make_textarea("Name <email@example.com>");
                        if let Some(index) = index {
                            textarea.insert_str(&recipients[index]);
                        }
                        *recipient_editor = Some(popup::EmailRecipientEditor {
                            field,
                            index,
                            textarea,
                        });
                    }
                } else if let KeyCode::Char(digit @ '1'..='9') = key.code {
                    if key.modifiers.is_empty() {
                        let index = digit.to_digit(10).unwrap_or(1) as usize - 1;
                        if let PopupState::EmailPatch {
                            focus,
                            to_recipients,
                            cc_recipients,
                            to_selected,
                            cc_selected,
                            ..
                        } = &mut self.popup
                        {
                            match *focus {
                                popup::EmailPatchFocus::To if index < to_recipients.len() => {
                                    *to_selected = index
                                }
                                popup::EmailPatchFocus::Cc if index < cc_recipients.len() => {
                                    *cc_selected = index
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            PopupState::BranchSwitchConflict { source, target, paths, selected } => {
                let source = source.clone();
                let target = target.clone();
                let paths = paths.clone();
                let selected = *selected;
                let action = match key.code {
                    KeyCode::Esc => { self.popup = PopupState::None; return Ok(()); }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let PopupState::BranchSwitchConflict { selected, .. } = &mut self.popup {
                            *selected = (*selected + 1).min(4);
                        }
                        return Ok(());
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let PopupState::BranchSwitchConflict { selected, .. } = &mut self.popup {
                            *selected = selected.saturating_sub(1);
                        }
                        return Ok(());
                    }
                    KeyCode::Enter => Some(selected),
                    KeyCode::Char('c') => Some(0),
                    KeyCode::Char('s') => Some(1),
                    KeyCode::Char('m') => Some(2),
                    KeyCode::Char('d') => Some(3),
                    KeyCode::Char('v') => Some(4),
                    _ => None,
                };
                if let Some(action) = action {
                    self.popup = PopupState::None;
                    controller::branches::resolve_checkout_conflict(self, action, source, target, paths)?;
                }
            }
            PopupState::PullDivergence {
                upstream,
                ahead,
                behind,
                selected,
                ..
            } => {
                let upstream = upstream.clone();
                let ahead = *ahead;
                let behind = *behind;
                let selected = *selected;

                match key.code {
                    KeyCode::Esc => {
                        self.popup = PopupState::None;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let PopupState::PullDivergence { selected, .. } = &mut self.popup {
                            *selected = (*selected + 1).min(3);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let PopupState::PullDivergence { selected, .. } = &mut self.popup {
                            *selected = selected.saturating_sub(1);
                        }
                    }
                    KeyCode::Enter => match selected {
                        0 => {
                            self.popup = PopupState::None;
                            self.start_pull_integration(upstream, PullStrategy::Rebase, false);
                        }
                        1 => {
                            self.popup = PopupState::None;
                            self.start_pull_integration(upstream, PullStrategy::Merge, false);
                        }
                        2 => self.confirm_reset_to_upstream(upstream),
                        _ => self.start_pull_divergence_compare(upstream, ahead, behind),
                    },
                    KeyCode::Char('r') => {
                        self.popup = PopupState::None;
                        self.start_pull_integration(upstream, PullStrategy::Rebase, false);
                    }
                    KeyCode::Char('m') => {
                        self.popup = PopupState::None;
                        self.start_pull_integration(upstream, PullStrategy::Merge, false);
                    }
                    KeyCode::Char('d') => {
                        self.confirm_reset_to_upstream(upstream);
                    }
                    KeyCode::Char('v') => {
                        self.start_pull_divergence_compare(upstream, ahead, behind);
                    }
                    KeyCode::Char('R') => {
                        self.popup = PopupState::None;
                        self.start_pull_integration(upstream, PullStrategy::Rebase, true);
                    }
                    KeyCode::Char('M') => {
                        self.popup = PopupState::None;
                        self.start_pull_integration(upstream, PullStrategy::Merge, true);
                    }
                    _ => {}
                }
            }
            PopupState::Menu {
                items,
                selected: _,
                loading_index,
                ..
            } => {
                // Block all input while a menu item is loading (except Esc)
                if loading_index.is_some() && key.code != KeyCode::Esc {
                    return Ok(());
                }
                let _items_len = items.len();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let PopupState::Menu {
                            items, selected, ..
                        } = &mut self.popup
                        {
                            // Skip disabled items
                            let mut next = *selected + 1;
                            while next < items.len() && items[next].action.is_none() {
                                next += 1;
                            }
                            if next < items.len() {
                                *selected = next;
                            }
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let PopupState::Menu {
                            items, selected, ..
                        } = &mut self.popup
                        {
                            // Skip disabled items
                            if *selected > 0 {
                                let mut prev = *selected - 1;
                                while prev > 0 && items[prev].action.is_none() {
                                    prev -= 1;
                                }
                                if items[prev].action.is_some() {
                                    *selected = prev;
                                }
                            }
                        }
                    }
                    KeyCode::Enter => {
                        self.execute_menu_action(None);
                    }
                    KeyCode::Esc => {
                        if let Some(stashed) = self.pending_commit_popup.take() {
                            self.popup = stashed;
                        } else {
                            self.popup = PopupState::None;
                        }
                    }
                    KeyCode::Char(c) => {
                        // Check if the typed char matches a menu item shortcut key
                        let key_str = c.to_string();
                        let matched_idx = items
                            .iter()
                            .position(|item| item.key.as_deref() == Some(key_str.as_str()));
                        if let Some(idx) = matched_idx {
                            // Check if the item has an action (not disabled)
                            let has_action = items[idx].action.is_some();
                            if has_action {
                                self.execute_menu_action(Some(idx));
                            }
                            // If disabled, do nothing (stay on menu)
                        }
                        // If no match, ignore the key (stay on menu)
                    }
                    _ => {}
                }
            }
            PopupState::Input {
                is_commit,
                confirm_focused,
                ..
            } => {
                use crossterm::event::KeyModifiers;
                let is_commit = *is_commit;
                let confirm_focused = *confirm_focused;

                // Tab toggles focus between textarea and confirm button (commit only)
                if is_commit && key.code == KeyCode::Tab {
                    if let PopupState::Input {
                        confirm_focused, ..
                    } = &mut self.popup
                    {
                        *confirm_focused = !*confirm_focused;
                    }
                }
                // Confirm: Ctrl+S for commit, Enter on confirm button, Enter for non-commit
                else if (is_commit
                    && key.code == KeyCode::Char('s')
                    && key.modifiers.contains(KeyModifiers::CONTROL))
                    || (is_commit && confirm_focused && key.code == KeyCode::Enter)
                    || (!is_commit && key.code == KeyCode::Enter)
                {
                    let popup = std::mem::replace(&mut self.popup, PopupState::None);
                    if let PopupState::Input {
                        textarea,
                        on_confirm,
                        is_commit: was_commit,
                        ..
                    } = popup
                    {
                        // Commit messages preserve hard-wrapped newlines; single-line inputs
                        // strip soft-wrap newlines to recover the user's literal text.
                        let text = if was_commit {
                            textarea.lines().join("\n")
                        } else {
                            textarea.lines().join("")
                        };
                        // Save to commit history before calling on_confirm
                        if was_commit && !text.trim().is_empty() {
                            // Remove duplicate if it exists
                            self.commit_message_history.retain(|m| m != &text);
                            self.commit_message_history.insert(0, text.clone());
                            // Keep history bounded
                            self.commit_message_history.truncate(50);
                            self.save_commit_history();
                        }
                        self.commit_history_idx = None;
                        if let Err(e) = on_confirm(self, &text) {
                            self.popup = PopupState::Message {
                                title: "Error".to_string(),
                                message: format!("{}", e),
                                kind: MessageKind::Error,
                            };
                        }
                    }
                } else if key.code == KeyCode::Esc {
                    self.popup = PopupState::None;
                    self.commit_history_idx = None;
                } else if is_commit
                    && !confirm_focused
                    && (key.code == KeyCode::Up || key.code == KeyCode::Down)
                    && !self.commit_message_history.is_empty()
                {
                    // Cycle through commit message history with Up/Down
                    if let PopupState::Input { textarea, .. } = &mut self.popup {
                        // Only cycle if on first line (Up) or last line (Down)
                        let cursor_row = textarea.cursor().0;
                        let line_count = textarea.lines().len();
                        let should_cycle = match key.code {
                            KeyCode::Up => cursor_row == 0,
                            KeyCode::Down => cursor_row >= line_count.saturating_sub(1),
                            _ => false,
                        };

                        if should_cycle {
                            let history_len = self.commit_message_history.len();
                            match key.code {
                                KeyCode::Up => {
                                    let new_idx = match self.commit_history_idx {
                                        None => {
                                            // Save current draft
                                            self.commit_history_draft = textarea.lines().join("\n");
                                            0
                                        }
                                        Some(idx) => (idx + 1).min(history_len - 1),
                                    };
                                    self.commit_history_idx = Some(new_idx);
                                    let msg = &self.commit_message_history[new_idx];
                                    let mut new_ta =
                                        popup::make_textarea("Enter commit message...");
                                    new_ta.insert_str(msg);
                                    *textarea = new_ta;
                                }
                                KeyCode::Down => {
                                    match self.commit_history_idx {
                                        Some(0) => {
                                            // Go back to draft
                                            self.commit_history_idx = None;
                                            let draft = self.commit_history_draft.clone();
                                            let mut new_ta =
                                                popup::make_textarea("Enter commit message...");
                                            new_ta.insert_str(&draft);
                                            *textarea = new_ta;
                                        }
                                        Some(idx) => {
                                            let new_idx = idx - 1;
                                            self.commit_history_idx = Some(new_idx);
                                            let msg = &self.commit_message_history[new_idx];
                                            let mut new_ta =
                                                popup::make_textarea("Enter commit message...");
                                            new_ta.insert_str(msg);
                                            *textarea = new_ta;
                                        }
                                        None => {
                                            // Already at draft, do nothing
                                        }
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            // Not at boundary — forward to textarea for normal cursor movement
                            textarea_input(textarea, key);
                        }
                    }
                } else if is_commit
                    && !confirm_focused
                    && matches_key(
                        key,
                        &self
                            .config
                            .user_config
                            .keybinding
                            .commit_message
                            .commit_menu,
                    )
                {
                    // Commit message editor menu key (configurable)
                    self.show_commit_editor_menu()?;
                } else if !confirm_focused {
                    // Forward all other keys to the textarea (only when textarea is focused)
                    if let PopupState::Input {
                        textarea,
                        is_commit,
                        ..
                    } = &mut self.popup
                    {
                        textarea_input(textarea, key);
                        let popup_width = (self.layout.width * 60 / 100)
                            .min(60)
                            .max(30)
                            .min(self.layout.width);
                        let popup_inner = popup_width.saturating_sub(4) as usize;
                        if *is_commit {
                            // Hard-wrap: line breaks become part of the committed message
                            // (matches lazygit's 72-char convention).
                            let config_width = self.config.user_config.git.commit.auto_wrap_width;
                            let effective_width = if config_width > 0 {
                                popup_inner.min(config_width)
                            } else {
                                popup_inner
                            };
                            if effective_width > 0 {
                                auto_wrap_textarea(textarea, effective_width);
                            }
                        } else if popup_inner > 0 {
                            // Soft-wrap: visual only — newlines are stripped on submit so
                            // the original text (including spaces) round-trips exactly.
                            soft_wrap_textarea(textarea, popup_inner);
                        }
                    }
                }
            }
            PopupState::CommitInput {
                focus,
                mode,
                pending_command,
                ..
            } => {
                use crossterm::event::KeyModifiers;
                use tui_textarea::CursorMove;

                let focus = *focus;
                let mode = *mode;
                let pending_command = *pending_command;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                let cmd = has_command_modifier(key.modifiers);

                // Use the same searchable Command Palette as Files/Branches/Commits
                // instead of maintaining a one-off commit-editor help overlay.
                if mode == popup::CommitInputMode::Normal
                    && key.code == KeyCode::Char('?')
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                {
                    self.show_commit_command_palette();
                    return Ok(());
                }

                // Eyeliner hints exist only between f/F and the target key.
                // Clearing here also handles Tab, commit actions, Esc, etc.
                if matches!(
                    pending_command,
                    popup::CommitPendingCommand::FindForward(_)
                        | popup::CommitPendingCommand::FindBackward(_)
                ) {
                    if let PopupState::CommitInput {
                        summary_textarea,
                        body_textarea,
                        ..
                    } = &mut self.popup
                    {
                        popup::clear_eyeliner_highlights(summary_textarea);
                        popup::clear_eyeliner_highlights(body_textarea);
                    }
                }

                // Any direct key input interrupts an in-flight smooth scroll;
                // a new scroll command below can immediately replace it.
                if let PopupState::CommitInput {
                    body_scroll_remaining,
                    ..
                } = &mut self.popup
                {
                    *body_scroll_remaining = 0;
                }

                // Commit-editor actions remain available from every Vim mode.
                if matches_key(
                    key,
                    &self.config.user_config.keybinding.commit_message.sign_off,
                ) {
                    match self.git.signoff_trailer() {
                        Ok(trailer) => {
                            let wrap_width = self.commit_body_wrap_width();
                            let mut added = false;
                            let mut removed = false;
                            if let PopupState::CommitInput {
                                focus,
                                summary_textarea,
                                body_textarea,
                                body_state,
                                mode,
                                pending_command,
                                undo_stack,
                                ..
                            } = &mut self.popup
                            {
                                popup::push_commit_undo_snapshot(
                                    undo_stack,
                                    summary_textarea,
                                    body_state,
                                    *focus,
                                );
                                *pending_command = popup::CommitPendingCommand::None;
                                if mode.is_visual() {
                                    summary_textarea.cancel_selection();
                                    body_textarea.cancel_selection();
                                    *mode = popup::CommitInputMode::Normal;
                                }
                                removed = body_state.remove_trailer(&trailer);
                                if !removed {
                                    added = body_state.append_trailer(&trailer);
                                }
                                if added {
                                    popup::set_commit_input_focus(
                                        focus,
                                        summary_textarea,
                                        body_textarea,
                                        popup::CommitInputFocus::Body,
                                    );
                                }
                                if added || removed {
                                    body_state.render_into(body_textarea, wrap_width);
                                }
                            }
                            if removed {
                                self.show_action_toast("Removed Signed-off-by");
                            } else if added {
                                self.show_action_toast("Added Signed-off-by");
                            }
                        }
                        Err(err) => {
                            self.popup = PopupState::Message {
                                title: "Could not toggle sign-off".to_string(),
                                message: format!("{err}"),
                                kind: MessageKind::Error,
                            };
                        }
                    }
                } else if matches_key(
                    key,
                    &self
                        .config
                        .user_config
                        .keybinding
                        .commit_message
                        .commit_menu,
                ) {
                    self.show_commit_editor_menu()?;
                } else if matches_key(
                    key,
                    &self
                        .config
                        .user_config
                        .keybinding
                        .commit_message
                        .ai_generate,
                ) {
                    self.trigger_ai_commit_generation_from_editor();
                } else if ctrl
                    && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('k'))
                    && !mode.is_visual()
                {
                    // Match lazygitrs' existing patch-preview/list neoscroll: Ctrl-J/K
                    // animates the active visual row by 22 lines and the textarea
                    // viewport follows only when the cursor reaches an edge.
                    if let PopupState::CommitInput {
                        focus,
                        summary_textarea,
                        body_textarea,
                        pending_command,
                        ..
                    } = &mut self.popup
                    {
                        popup::set_commit_input_focus(
                            focus,
                            summary_textarea,
                            body_textarea,
                            popup::CommitInputFocus::Body,
                        );
                        *pending_command = popup::CommitPendingCommand::None;
                    }
                    let visible_rows = self.commit_body_visible_rows() as isize;
                    let amount = (visible_rows / 2).max(1);
                    let direction = if key.code == KeyCode::Char('k') {
                        -1
                    } else {
                        1
                    };
                    self.queue_commit_input_scroll(direction * amount);
                } else if key.code == KeyCode::Tab {
                    // Tab is the explicit cross-field motion in every mode. In
                    // Visual mode the selection is local to each field, so a
                    // field switch starts a fresh selection at the new cursor.
                    if let PopupState::CommitInput {
                        focus,
                        summary_textarea,
                        body_textarea,
                        mode,
                        pending_command,
                        ..
                    } = &mut self.popup
                    {
                        *pending_command = popup::CommitPendingCommand::None;
                        let new_focus = match *focus {
                            popup::CommitInputFocus::Summary => popup::CommitInputFocus::Body,
                            popup::CommitInputFocus::Body => popup::CommitInputFocus::Summary,
                        };
                        if mode.is_visual() {
                            summary_textarea.cancel_selection();
                            body_textarea.cancel_selection();
                        }
                        popup::set_commit_input_focus(
                            focus,
                            summary_textarea,
                            body_textarea,
                            new_focus,
                        );
                        if mode.is_visual() {
                            match new_focus {
                                popup::CommitInputFocus::Summary => {
                                    summary_textarea.start_selection()
                                }
                                popup::CommitInputFocus::Body => body_textarea.start_selection(),
                            }
                        }
                    }
                } else if ctrl && key.code == KeyCode::Enter {
                    self.submit_commit_input();
                } else if mode == popup::CommitInputMode::Normal
                    && key.modifiers.is_empty()
                    && matches!(
                        pending_command,
                        popup::CommitPendingCommand::None | popup::CommitPendingCommand::Count(_)
                    )
                    && (matches!(key.code, KeyCode::Char('1'..='9'))
                        || (matches!(key.code, KeyCode::Char('0'))
                            && matches!(pending_command, popup::CommitPendingCommand::Count(_))))
                {
                    if let KeyCode::Char(digit) = key.code {
                        let digit = digit.to_digit(10).unwrap_or(0) as u16;
                        if let PopupState::CommitInput {
                            pending_command, ..
                        } = &mut self.popup
                        {
                            let previous = match *pending_command {
                                popup::CommitPendingCommand::Count(count) => count,
                                _ => 0,
                            };
                            *pending_command = popup::CommitPendingCommand::Count(
                                previous.saturating_mul(10).saturating_add(digit).min(999),
                            );
                        }
                    }
                    return Ok(());
                } else if mode == popup::CommitInputMode::Normal
                    && matches!(
                        pending_command,
                        popup::CommitPendingCommand::FindForward(_)
                            | popup::CommitPendingCommand::FindBackward(_)
                    )
                {
                    let wrap_width = self.commit_body_wrap_width();
                    if let PopupState::CommitInput {
                        focus,
                        summary_textarea,
                        body_textarea,
                        body_state,
                        pending_command,
                        ..
                    } = &mut self.popup
                    {
                        let forward = matches!(
                            *pending_command,
                            popup::CommitPendingCommand::FindForward(_)
                        );
                        let count = (*pending_command).count();
                        // Eyeliner hints are transient: clear them as soon as the
                        // target key (or Esc/anything else) resolves the pending f/F.
                        popup::clear_eyeliner_highlights(summary_textarea);
                        popup::clear_eyeliner_highlights(body_textarea);
                        if let KeyCode::Char(target) = key.code {
                            match *focus {
                                popup::CommitInputFocus::Summary => {
                                    popup::find_summary_char(
                                        summary_textarea,
                                        target,
                                        forward,
                                        count,
                                    );
                                }
                                popup::CommitInputFocus::Body => {
                                    if body_state.find_char_on_visual_line(
                                        wrap_width, target, forward, count,
                                    ) {
                                        body_state.apply_cursor_into(body_textarea, wrap_width);
                                    }
                                }
                            }
                        }
                        *pending_command = popup::CommitPendingCommand::None;
                    }
                    return Ok(());
                } else if mode == popup::CommitInputMode::Normal
                    && !ctrl
                    && !alt
                    && !cmd
                    && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
                {
                    let dimmed =
                        ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray);
                    let target = ratatui::style::Style::default()
                        .fg(ratatui::style::Color::Rgb(255, 165, 0));
                    if let PopupState::CommitInput {
                        focus,
                        summary_textarea,
                        body_textarea,
                        pending_command,
                        ..
                    } = &mut self.popup
                    {
                        let count = (*pending_command).count().min(u16::MAX as usize) as u16;
                        let forward = key.code == KeyCode::Char('f');
                        *pending_command = if forward {
                            popup::CommitPendingCommand::FindForward(count)
                        } else {
                            popup::CommitPendingCommand::FindBackward(count)
                        };
                        popup::clear_eyeliner_highlights(summary_textarea);
                        popup::clear_eyeliner_highlights(body_textarea);
                        match *focus {
                            popup::CommitInputFocus::Summary => popup::apply_eyeliner_highlights(
                                summary_textarea,
                                forward,
                                dimmed,
                                target,
                            ),
                            popup::CommitInputFocus::Body => popup::apply_eyeliner_highlights(
                                body_textarea,
                                forward,
                                dimmed,
                                target,
                            ),
                        }
                    }
                    return Ok(());
                } else if mode == popup::CommitInputMode::Normal
                    && pending_command == popup::CommitPendingCommand::Delete
                {
                    let wrap_width = self.commit_body_wrap_width();
                    if let PopupState::CommitInput {
                        focus,
                        summary_textarea,
                        body_textarea,
                        body_state,
                        pending_command,
                        undo_stack,
                        ..
                    } = &mut self.popup
                    {
                        let before_len = undo_stack.len();
                        popup::push_commit_undo_snapshot(
                            undo_stack,
                            summary_textarea,
                            body_state,
                            *focus,
                        );
                        let changed = match (*focus, key.code) {
                            (popup::CommitInputFocus::Summary, KeyCode::Char('d')) => {
                                if summary_textarea.lines().join("").is_empty() {
                                    false
                                } else {
                                    popup::set_commit_summary_text(summary_textarea, "");
                                    true
                                }
                            }
                            (popup::CommitInputFocus::Summary, KeyCode::Char('w')) => {
                                popup::delete_summary_word_forward(summary_textarea)
                            }
                            (popup::CommitInputFocus::Body, KeyCode::Char('d')) => {
                                let changed = body_state.delete_current_line();
                                if changed {
                                    body_state.render_into(body_textarea, wrap_width);
                                }
                                changed
                            }
                            (popup::CommitInputFocus::Body, KeyCode::Char('w')) => {
                                let changed = body_state.delete_word_forward();
                                if changed {
                                    body_state.render_into(body_textarea, wrap_width);
                                }
                                changed
                            }
                            _ => false,
                        };
                        if !changed && undo_stack.len() > before_len {
                            undo_stack.pop();
                        }
                        *pending_command = popup::CommitPendingCommand::None;
                    }
                } else if mode.is_visual()
                    && pending_command == popup::CommitPendingCommand::VisualInner
                {
                    let wrap_width = self.commit_body_wrap_width();
                    if let PopupState::CommitInput {
                        focus,
                        mode,
                        summary_textarea,
                        body_textarea,
                        body_state,
                        pending_command,
                        ..
                    } = &mut self.popup
                    {
                        if let KeyCode::Char(delimiter) = key.code
                            && matches!(
                                delimiter,
                                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
                            )
                        {
                            match *focus {
                                popup::CommitInputFocus::Summary => {
                                    popup::select_summary_inside_delimiter(
                                        summary_textarea,
                                        delimiter,
                                    );
                                }
                                popup::CommitInputFocus::Body => {
                                    body_state.select_inside_delimiter(
                                        body_textarea,
                                        wrap_width,
                                        delimiter,
                                    );
                                }
                            }
                        } else if key.code == KeyCode::Esc {
                            summary_textarea.cancel_selection();
                            body_textarea.cancel_selection();
                            *mode = popup::CommitInputMode::Normal;
                        }
                        *pending_command = popup::CommitPendingCommand::None;
                    }
                } else {
                    match mode {
                        popup::CommitInputMode::Insert => {
                            // Fresh commit prompts open directly in Insert mode, so
                            // there is no preceding `i/a` keypress to seed the local
                            // undo stack. Capture the pre-edit state on the first
                            // Insert-mode action; subsequent insert sessions already
                            // push their snapshot when Normal mode enters Insert.
                            if key.code != KeyCode::Esc {
                                if let PopupState::CommitInput {
                                    focus,
                                    summary_textarea,
                                    body_state,
                                    undo_stack,
                                    ..
                                } = &mut self.popup
                                {
                                    if undo_stack.is_empty() {
                                        popup::push_commit_undo_snapshot(
                                            undo_stack,
                                            summary_textarea,
                                            body_state,
                                            *focus,
                                        );
                                    }
                                }
                            }
                            if key.code == KeyCode::Esc {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    ..
                                } = &mut self.popup
                                {
                                    summary_textarea.cancel_selection();
                                    body_textarea.cancel_selection();
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            popup::leave_summary_insert_mode(summary_textarea);
                                        }
                                        popup::CommitInputFocus::Body => {
                                            body_state.leave_insert_mode(wrap_width);
                                            body_state.apply_cursor_into(body_textarea, wrap_width);
                                        }
                                    }
                                    *mode = popup::CommitInputMode::Normal;
                                }
                            } else if key.code == KeyCode::Enter {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    ..
                                } = &mut self.popup
                                {
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            popup::set_commit_input_focus(
                                                focus,
                                                summary_textarea,
                                                body_textarea,
                                                popup::CommitInputFocus::Body,
                                            );
                                        }
                                        popup::CommitInputFocus::Body => {
                                            body_state.insert_char('\n');
                                            body_state.render_into(body_textarea, wrap_width);
                                        }
                                    }
                                }
                            } else {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    focus,
                                    ..
                                } = &mut self.popup
                                {
                                    match focus {
                                        popup::CommitInputFocus::Summary => {
                                            textarea_input(summary_textarea, key);
                                        }
                                        popup::CommitInputFocus::Body => {
                                            let mut content_changed = false;
                                            let mut cursor_moved = false;
                                            match key.code {
                                                KeyCode::Char(c) if !ctrl && !alt && !cmd => {
                                                    body_state.insert_char(c);
                                                    content_changed = true;
                                                }
                                                KeyCode::Backspace if cmd => {
                                                    body_state
                                                        .delete_to_visual_line_start(wrap_width);
                                                    content_changed = true;
                                                }
                                                KeyCode::Char('u') if ctrl => {
                                                    body_state
                                                        .delete_to_visual_line_start(wrap_width);
                                                    content_changed = true;
                                                }
                                                KeyCode::Backspace if alt => {
                                                    body_state.delete_word_left();
                                                    content_changed = true;
                                                }
                                                KeyCode::Char('w') if ctrl => {
                                                    body_state.delete_word_left();
                                                    content_changed = true;
                                                }
                                                KeyCode::Backspace => {
                                                    body_state.backspace();
                                                    content_changed = true;
                                                }
                                                KeyCode::Delete => {
                                                    body_state.delete();
                                                    content_changed = true;
                                                }
                                                KeyCode::Left if cmd => {
                                                    body_state.move_visual_line_start(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Right if cmd => {
                                                    body_state.move_visual_line_end(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Char('a') if ctrl => {
                                                    body_state.move_visual_line_start(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Char('e') if ctrl => {
                                                    body_state.move_visual_line_end(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Left if alt => {
                                                    body_state.move_word_left();
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Right if alt => {
                                                    body_state.move_word_right();
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Char('b') if alt => {
                                                    body_state.move_word_left();
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Char('f') if alt => {
                                                    body_state.move_word_right();
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Left => {
                                                    body_state.move_left();
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Right => {
                                                    body_state.move_right();
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Up => {
                                                    body_state.move_visual_up(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Down => {
                                                    body_state.move_visual_down(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::Home => {
                                                    body_state.move_visual_line_start(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                KeyCode::End => {
                                                    body_state.move_visual_line_end(wrap_width);
                                                    cursor_moved = true;
                                                }
                                                _ => {}
                                            }
                                            if content_changed {
                                                body_state.render_into(body_textarea, wrap_width);
                                            } else if cursor_moved {
                                                body_state
                                                    .apply_cursor_into(body_textarea, wrap_width);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        popup::CommitInputMode::Visual | popup::CommitInputMode::VisualLine => {
                            if key.code == KeyCode::Esc || key.code == KeyCode::Char('v') {
                                if let PopupState::CommitInput {
                                    mode,
                                    summary_textarea,
                                    body_textarea,
                                    pending_command,
                                    visual_line_anchor,
                                    ..
                                } = &mut self.popup
                                {
                                    summary_textarea.cancel_selection();
                                    body_textarea.cancel_selection();
                                    *pending_command = popup::CommitPendingCommand::None;
                                    *visual_line_anchor = None;
                                    *mode = popup::CommitInputMode::Normal;
                                }
                            } else if key.code == KeyCode::Char('i') && key.modifiers.is_empty() {
                                if let PopupState::CommitInput {
                                    pending_command, ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::VisualInner;
                                }
                            } else if key.code == KeyCode::Char('g') && !ctrl && !alt && !cmd {
                                let go_top = matches!(
                                    &self.popup,
                                    PopupState::CommitInput {
                                        pending_command: popup::CommitPendingCommand::GoTop,
                                        ..
                                    }
                                );
                                if go_top {
                                    self.move_commit_input_description_boundary(false);
                                } else if let PopupState::CommitInput {
                                    pending_command, ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::GoTop;
                                }
                            } else if key.code == KeyCode::Char('G') && !ctrl && !alt && !cmd {
                                self.move_commit_input_description_boundary(true);
                            } else if matches!(
                                key.code,
                                KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Char('c')
                            ) {
                                let wrap_width = self.commit_body_wrap_width();
                                let change = key.code == KeyCode::Char('c');
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    pending_command,
                                    visual_line_anchor,
                                    undo_stack,
                                    ..
                                } = &mut self.popup
                                {
                                    popup::push_commit_undo_snapshot(
                                        undo_stack,
                                        summary_textarea,
                                        body_state,
                                        *focus,
                                    );
                                    *pending_command = popup::CommitPendingCommand::None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            if summary_textarea
                                                .selection_range()
                                                .is_some_and(|(start, end)| start != end)
                                            {
                                                summary_textarea.cut();
                                            } else {
                                                summary_textarea.delete_next_char();
                                            }
                                            summary_textarea.cancel_selection();
                                        }
                                        popup::CommitInputFocus::Body => {
                                            let deleted = body_textarea
                                                .selection_range()
                                                .is_some_and(|(start, end)| {
                                                    body_state.delete_visual_selection(
                                                        start, end, wrap_width,
                                                    )
                                                });
                                            if !deleted {
                                                body_state.delete();
                                            }
                                            body_state.render_into(body_textarea, wrap_width);
                                            body_textarea.cancel_selection();
                                        }
                                    }
                                    *mode = if change {
                                        popup::CommitInputMode::Insert
                                    } else {
                                        popup::CommitInputMode::Normal
                                    };
                                    *visual_line_anchor = None;
                                }
                            } else if key.code == KeyCode::Char('y') {
                                let wrap_width = self.commit_body_wrap_width();
                                let mut selected = String::new();
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    visual_line_anchor,
                                    pending_command,
                                    ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            if let Some(((row, start), (_, end))) =
                                                summary_textarea.selection_range()
                                            {
                                                if let Some(line) =
                                                    summary_textarea.lines().get(row)
                                                {
                                                    selected = line
                                                        .chars()
                                                        .skip(start)
                                                        .take(end.saturating_sub(start))
                                                        .collect();
                                                }
                                            }
                                        }
                                        popup::CommitInputFocus::Body => {
                                            if let Some((start, end)) =
                                                body_textarea.selection_range()
                                            {
                                                selected = body_state
                                                    .selected_visual_text(start, end, wrap_width);
                                            }
                                        }
                                    }
                                    summary_textarea.cancel_selection();
                                    body_textarea.cancel_selection();
                                    *visual_line_anchor = None;
                                    *mode = popup::CommitInputMode::Normal;
                                }
                                if !selected.is_empty() {
                                    Platform::copy_to_clipboard(&selected)?;
                                    self.show_action_toast("Copied selection");
                                }
                            } else {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    visual_line_anchor,
                                    pending_command,
                                    ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => match key.code {
                                            KeyCode::Char('h') | KeyCode::Left => {
                                                summary_textarea.move_cursor(CursorMove::Back)
                                            }
                                            KeyCode::Char('l') | KeyCode::Right => {
                                                popup::move_summary_right_normal(summary_textarea)
                                            }
                                            KeyCode::Char('H')
                                            | KeyCode::Home
                                            | KeyCode::Char('0') => {
                                                summary_textarea.move_cursor(CursorMove::Head)
                                            }
                                            KeyCode::Char('L')
                                            | KeyCode::End
                                            | KeyCode::Char('$') => {
                                                popup::move_summary_line_last_char(summary_textarea)
                                            }
                                            KeyCode::Char('w') => {
                                                popup::move_summary_word_forward_vim(
                                                    summary_textarea,
                                                )
                                            }
                                            KeyCode::Char('b') => {
                                                summary_textarea.move_cursor(CursorMove::WordBack)
                                            }
                                            _ => {}
                                        },
                                        popup::CommitInputFocus::Body => {
                                            if *mode == popup::CommitInputMode::VisualLine
                                                && matches!(
                                                    key.code,
                                                    KeyCode::Char('j')
                                                        | KeyCode::Down
                                                        | KeyCode::Char('k')
                                                        | KeyCode::Up
                                                )
                                            {
                                                let direction = if matches!(
                                                    key.code,
                                                    KeyCode::Char('j') | KeyCode::Down
                                                ) {
                                                    1
                                                } else {
                                                    -1
                                                };
                                                if body_state
                                                    .move_visual_linewise(direction, wrap_width)
                                                {
                                                    if let Some(anchor) = *visual_line_anchor {
                                                        body_state.select_visual_line_range(
                                                            body_textarea,
                                                            wrap_width,
                                                            anchor,
                                                        );
                                                    }
                                                }
                                            } else {
                                                let mut moved = true;
                                                match key.code {
                                                    KeyCode::Char('h') | KeyCode::Left => {
                                                        body_state.move_left()
                                                    }
                                                    KeyCode::Char('l') | KeyCode::Right => {
                                                        body_state
                                                            .move_visual_right_normal(wrap_width)
                                                    }
                                                    KeyCode::Char('H')
                                                    | KeyCode::Home
                                                    | KeyCode::Char('0') => body_state
                                                        .move_visual_line_start(wrap_width),
                                                    KeyCode::Char('L')
                                                    | KeyCode::End
                                                    | KeyCode::Char('$') => body_state
                                                        .move_visual_line_last_char(wrap_width),
                                                    KeyCode::Char('w') => {
                                                        body_state.move_word_right_vim()
                                                    }
                                                    KeyCode::Char('b') => {
                                                        body_state.move_word_left()
                                                    }
                                                    KeyCode::Char('j') | KeyCode::Down => {
                                                        body_state
                                                            .move_visual_down_normal(wrap_width)
                                                    }
                                                    KeyCode::Char('k') | KeyCode::Up => {
                                                        body_state.move_visual_up_normal(wrap_width)
                                                    }
                                                    _ => moved = false,
                                                }
                                                if moved {
                                                    body_state.apply_cursor_into(
                                                        body_textarea,
                                                        wrap_width,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        popup::CommitInputMode::Normal => {
                            if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
                                let stashed = std::mem::replace(&mut self.popup, PopupState::None);
                                self.saved_commit_popup = Some(stashed);
                                self.commit_history_idx = None;
                            } else if key.code == KeyCode::Esc {
                                // Escape is a mode/cancel key in Vim, not a quit key.
                                // With no pending operator/text object there is simply
                                // nothing to cancel, so remain in Normal mode.
                                if let PopupState::CommitInput {
                                    pending_command, ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::None;
                                }
                            } else if key.code == KeyCode::Char('g') && !ctrl && !alt && !cmd {
                                let go_top = matches!(
                                    &self.popup,
                                    PopupState::CommitInput {
                                        pending_command: popup::CommitPendingCommand::GoTop,
                                        ..
                                    }
                                );
                                if go_top {
                                    self.move_commit_input_description_boundary(false);
                                } else if let PopupState::CommitInput {
                                    pending_command, ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::GoTop;
                                }
                            } else if key.code == KeyCode::Char('G') && !ctrl && !alt && !cmd {
                                self.move_commit_input_description_boundary(true);
                            } else if key.code == KeyCode::Enter {
                                self.submit_commit_input();
                            } else if key.code == KeyCode::Char('u') && key.modifiers.is_empty() {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    pending_command,
                                    undo_stack,
                                    body_scroll_remaining,
                                    ..
                                } = &mut self.popup
                                {
                                    if let Some(snapshot) = undo_stack.pop() {
                                        snapshot.restore(
                                            summary_textarea,
                                            body_textarea,
                                            body_state,
                                            focus,
                                            wrap_width,
                                        );
                                    }
                                    *pending_command = popup::CommitPendingCommand::None;
                                    *body_scroll_remaining = 0;
                                }
                            } else if key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
                                if let PopupState::CommitInput {
                                    pending_command, ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::Delete;
                                }
                            } else if matches!(
                                key.code,
                                KeyCode::Char('i')
                                    | KeyCode::Char('a')
                                    | KeyCode::Char('I')
                                    | KeyCode::Char('A')
                            ) {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    pending_command,
                                    undo_stack,
                                    ..
                                } = &mut self.popup
                                {
                                    popup::push_commit_undo_snapshot(
                                        undo_stack,
                                        summary_textarea,
                                        body_state,
                                        *focus,
                                    );
                                    *pending_command = popup::CommitPendingCommand::None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => match key.code {
                                            KeyCode::Char('a') => {
                                                summary_textarea.move_cursor(CursorMove::Forward)
                                            }
                                            KeyCode::Char('I') => {
                                                summary_textarea.move_cursor(CursorMove::Head)
                                            }
                                            KeyCode::Char('A') => {
                                                summary_textarea.move_cursor(CursorMove::End)
                                            }
                                            _ => {}
                                        },
                                        popup::CommitInputFocus::Body => {
                                            match key.code {
                                                KeyCode::Char('a') => {
                                                    body_state.move_append(wrap_width)
                                                }
                                                KeyCode::Char('I') => {
                                                    body_state.move_visual_line_start(wrap_width)
                                                }
                                                KeyCode::Char('A') => {
                                                    body_state.move_visual_line_end(wrap_width)
                                                }
                                                _ => {}
                                            }
                                            body_state.apply_cursor_into(body_textarea, wrap_width);
                                        }
                                    }
                                    *mode = popup::CommitInputMode::Insert;
                                }
                            } else if key.code == KeyCode::Char('o') && key.modifiers.is_empty() {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    pending_command,
                                    undo_stack,
                                    ..
                                } = &mut self.popup
                                {
                                    popup::push_commit_undo_snapshot(
                                        undo_stack,
                                        summary_textarea,
                                        body_state,
                                        *focus,
                                    );
                                    *pending_command = popup::CommitPendingCommand::None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            body_state.move_to_start();
                                            if !body_state.raw().is_empty() {
                                                body_state.insert_char('\n');
                                                body_state.move_to_start();
                                            }
                                            popup::set_commit_input_focus(
                                                focus,
                                                summary_textarea,
                                                body_textarea,
                                                popup::CommitInputFocus::Body,
                                            );
                                        }
                                        popup::CommitInputFocus::Body => {
                                            body_state.open_line_below();
                                        }
                                    }
                                    body_state.render_into(body_textarea, wrap_width);
                                    *mode = popup::CommitInputMode::Insert;
                                }
                            } else if key.code == KeyCode::Char('v') {
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    pending_command,
                                    visual_line_anchor,
                                    ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::None;
                                    *visual_line_anchor = None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            summary_textarea.start_selection()
                                        }
                                        popup::CommitInputFocus::Body => {
                                            body_textarea.start_selection()
                                        }
                                    }
                                    *mode = popup::CommitInputMode::Visual;
                                }
                            } else if key.code == KeyCode::Char('V') {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    mode,
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    pending_command,
                                    visual_line_anchor,
                                    ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::None;
                                    match *focus {
                                        popup::CommitInputFocus::Summary => {
                                            popup::select_summary_line(summary_textarea);
                                            *visual_line_anchor = None;
                                        }
                                        popup::CommitInputFocus::Body => {
                                            *visual_line_anchor =
                                                Some(body_state.select_current_visual_line(
                                                    body_textarea,
                                                    wrap_width,
                                                ));
                                        }
                                    }
                                    *mode = popup::CommitInputMode::VisualLine;
                                }
                            } else if focus == popup::CommitInputFocus::Body
                                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                            {
                                let area = Rect::new(0, 0, self.layout.width, self.layout.height);
                                let visible_rows =
                                    views::commit_description_textarea_geometry(&self.popup, area)
                                        .map(|rect| rect.height as isize)
                                        .unwrap_or(1)
                                        .max(1);
                                let amount = visible_rows;
                                let direction = if key.code == KeyCode::PageUp { -1 } else { 1 };
                                self.queue_commit_input_scroll(direction * amount);
                            } else if focus == popup::CommitInputFocus::Summary
                                && (key.code == KeyCode::Up || key.code == KeyCode::Down)
                                && !self.commit_message_history.is_empty()
                            {
                                let wrap_width = self.commit_body_wrap_width();
                                if let PopupState::CommitInput {
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    ..
                                } = &mut self.popup
                                {
                                    let history_len = self.commit_message_history.len();
                                    let load_msg =
                                        |summary_textarea: &mut tui_textarea::TextArea<'static>,
                                         body_textarea: &mut tui_textarea::TextArea<'static>,
                                         body_state: &mut popup::BodySoftWrap,
                                         msg: &str| {
                                            let (summary, body) = split_commit_message(msg);
                                            popup::set_commit_summary_text(
                                                summary_textarea,
                                                &summary,
                                            );
                                            *body_textarea = popup::make_commit_body_textarea();
                                            body_state.set_text(popup::unwrap_commit_body(&body));
                                            body_state.render_into(body_textarea, wrap_width);
                                        };
                                    match key.code {
                                        KeyCode::Up => {
                                            let new_idx = match self.commit_history_idx {
                                                None => {
                                                    let s = summary_textarea.lines().join("");
                                                    let b = body_state.raw().to_string();
                                                    self.commit_history_draft =
                                                        if b.trim().is_empty() {
                                                            s
                                                        } else {
                                                            format!("{}\n\n{}", s, b)
                                                        };
                                                    0
                                                }
                                                Some(idx) => (idx + 1).min(history_len - 1),
                                            };
                                            self.commit_history_idx = Some(new_idx);
                                            let msg = self.commit_message_history[new_idx].clone();
                                            load_msg(
                                                summary_textarea,
                                                body_textarea,
                                                body_state,
                                                &msg,
                                            );
                                        }
                                        KeyCode::Down => match self.commit_history_idx {
                                            Some(0) => {
                                                self.commit_history_idx = None;
                                                let draft = self.commit_history_draft.clone();
                                                load_msg(
                                                    summary_textarea,
                                                    body_textarea,
                                                    body_state,
                                                    &draft,
                                                );
                                            }
                                            Some(idx) => {
                                                let new_idx = idx - 1;
                                                self.commit_history_idx = Some(new_idx);
                                                let msg =
                                                    self.commit_message_history[new_idx].clone();
                                                load_msg(
                                                    summary_textarea,
                                                    body_textarea,
                                                    body_state,
                                                    &msg,
                                                );
                                            }
                                            None => {}
                                        },
                                        _ => {}
                                    }
                                }
                            } else {
                                let wrap_width = self.commit_body_wrap_width();
                                let repeat_count = pending_command.count();
                                if let PopupState::CommitInput {
                                    focus,
                                    summary_textarea,
                                    body_textarea,
                                    body_state,
                                    pending_command,
                                    undo_stack,
                                    ..
                                } = &mut self.popup
                                {
                                    *pending_command = popup::CommitPendingCommand::None;
                                    if matches!(key.code, KeyCode::Char('x') | KeyCode::Delete) {
                                        popup::push_commit_undo_snapshot(
                                            undo_stack,
                                            summary_textarea,
                                            body_state,
                                            *focus,
                                        );
                                    }
                                    match *focus {
                                        popup::CommitInputFocus::Summary => match key.code {
                                            KeyCode::Char('j') | KeyCode::Down => {
                                                popup::set_commit_input_focus(
                                                    focus,
                                                    summary_textarea,
                                                    body_textarea,
                                                    popup::CommitInputFocus::Body,
                                                );
                                                body_state.normalize_normal_cursor(wrap_width);
                                                for _ in 1..repeat_count {
                                                    body_state.move_visual_down_normal(wrap_width);
                                                }
                                                body_state
                                                    .apply_cursor_into(body_textarea, wrap_width);
                                            }
                                            KeyCode::Char('h')
                                            | KeyCode::Left
                                            | KeyCode::Backspace => {
                                                for _ in 0..repeat_count {
                                                    summary_textarea.move_cursor(CursorMove::Back);
                                                }
                                            }
                                            KeyCode::Char('l') | KeyCode::Right => {
                                                for _ in 0..repeat_count {
                                                    popup::move_summary_right_normal(
                                                        summary_textarea,
                                                    );
                                                }
                                            }
                                            KeyCode::Char('H')
                                            | KeyCode::Home
                                            | KeyCode::Char('0') => {
                                                summary_textarea.move_cursor(CursorMove::Head)
                                            }
                                            KeyCode::Char('L')
                                            | KeyCode::End
                                            | KeyCode::Char('$') => {
                                                popup::move_summary_line_last_char(summary_textarea)
                                            }
                                            KeyCode::Char('w') => {
                                                for _ in 0..repeat_count {
                                                    popup::move_summary_word_forward_vim(
                                                        summary_textarea,
                                                    );
                                                }
                                            }
                                            KeyCode::Char('b') => {
                                                for _ in 0..repeat_count {
                                                    summary_textarea
                                                        .move_cursor(CursorMove::WordBack);
                                                }
                                            }
                                            KeyCode::Char('x') | KeyCode::Delete => {
                                                for _ in 0..repeat_count {
                                                    summary_textarea.delete_next_char();
                                                }
                                            }
                                            _ => {}
                                        },
                                        popup::CommitInputFocus::Body => {
                                            let mut moved = true;
                                            let mut content_changed = false;
                                            match key.code {
                                                KeyCode::Char('k') | KeyCode::Up => {
                                                    for _ in 0..repeat_count {
                                                        let (row, _) =
                                                            body_state.visual_cursor(wrap_width);
                                                        if row == 0 {
                                                            popup::set_commit_input_focus(
                                                                focus,
                                                                summary_textarea,
                                                                body_textarea,
                                                                popup::CommitInputFocus::Summary,
                                                            );
                                                            moved = false;
                                                            break;
                                                        }
                                                        body_state
                                                            .move_visual_up_normal(wrap_width);
                                                    }
                                                }
                                                KeyCode::Char('j') | KeyCode::Down => {
                                                    for _ in 0..repeat_count {
                                                        body_state
                                                            .move_visual_down_normal(wrap_width);
                                                    }
                                                }
                                                KeyCode::Char('h')
                                                | KeyCode::Left
                                                | KeyCode::Backspace => {
                                                    for _ in 0..repeat_count {
                                                        body_state.move_left();
                                                    }
                                                }
                                                KeyCode::Char('l') | KeyCode::Right => {
                                                    for _ in 0..repeat_count {
                                                        body_state
                                                            .move_visual_right_normal(wrap_width);
                                                    }
                                                }
                                                KeyCode::Char('H')
                                                | KeyCode::Home
                                                | KeyCode::Char('0') => {
                                                    body_state.move_visual_line_start(wrap_width)
                                                }
                                                KeyCode::Char('L')
                                                | KeyCode::End
                                                | KeyCode::Char('$') => body_state
                                                    .move_visual_line_last_char(wrap_width),
                                                KeyCode::Char('w') => {
                                                    for _ in 0..repeat_count {
                                                        body_state.move_word_right_vim();
                                                    }
                                                }
                                                KeyCode::Char('b') => {
                                                    for _ in 0..repeat_count {
                                                        body_state.move_word_left();
                                                    }
                                                }
                                                KeyCode::Char('x') | KeyCode::Delete => {
                                                    for _ in 0..repeat_count {
                                                        body_state.delete();
                                                    }
                                                    moved = false;
                                                    content_changed = true;
                                                }
                                                _ => moved = false,
                                            }
                                            if content_changed {
                                                body_state.render_into(body_textarea, wrap_width);
                                            } else if moved {
                                                body_state
                                                    .apply_cursor_into(body_textarea, wrap_width);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            PopupState::Checklist {
                items,
                selected: _,
                search_textarea,
                ..
            } => {
                let search = search_textarea.lines().join("");
                let visible_count = items
                    .iter()
                    .filter(|it| {
                        it.is_free_entry
                            || search.is_empty()
                            || it.label.to_lowercase().contains(&search.to_lowercase())
                    })
                    .count();
                match key.code {
                    // Arrow keys only for navigation — j/k must type into the search filter
                    // (same pattern as ListPicker / CommandPalette / ThemePicker).
                    KeyCode::Down if key.modifiers.is_empty() => {
                        if let PopupState::Checklist { selected, .. } = &mut self.popup {
                            if visible_count > 0 {
                                *selected = (*selected + 1).min(visible_count - 1);
                            }
                        }
                    }
                    KeyCode::Up if key.modifiers.is_empty() => {
                        if let PopupState::Checklist { selected, .. } = &mut self.popup {
                            *selected = selected.saturating_sub(1);
                        }
                    }
                    KeyCode::Char(' ') if key.modifiers.is_empty() => {
                        // Toggle checked state on the visible item at `selected`
                        if let PopupState::Checklist {
                            items,
                            selected,
                            search_textarea,
                            ..
                        } = &mut self.popup
                        {
                            let search = search_textarea.lines().join("");
                            let visible_indices: Vec<usize> = items
                                .iter()
                                .enumerate()
                                .filter(|(_, it)| {
                                    it.is_free_entry
                                        || search.is_empty()
                                        || it.label.to_lowercase().contains(&search.to_lowercase())
                                })
                                .map(|(i, _)| i)
                                .collect();
                            if let Some(&real_idx) = visible_indices.get(*selected) {
                                items[real_idx].checked = !items[real_idx].checked;
                            }
                        }
                    }
                    KeyCode::Enter => {
                        let popup = std::mem::replace(&mut self.popup, PopupState::None);
                        if let PopupState::Checklist {
                            items, on_confirm, ..
                        } = popup
                        {
                            let checked: Vec<String> = items
                                .into_iter()
                                .filter(|it| it.checked)
                                .map(|it| it.label)
                                .collect();
                            if let Err(e) = on_confirm(self, checked) {
                                self.popup = PopupState::Message {
                                    title: "Error".to_string(),
                                    message: format!("{}", e),
                                    kind: MessageKind::Error,
                                };
                            }
                        }
                    }
                    KeyCode::Esc => {
                        self.popup = PopupState::None;
                    }
                    _ => {
                        // Search input: chars, backspace, Option/Cmd word/line edits.
                        if let PopupState::Checklist {
                            items,
                            selected,
                            search_textarea,
                            free_entry_category,
                            ..
                        } = &mut self.popup
                        {
                            let before = search_textarea.lines().join("");
                            textarea_input(search_textarea, key);
                            let after = search_textarea.lines().join("");
                            if after != before {
                                crate::gui::popup::sync_checklist_free_entry(
                                    items,
                                    free_entry_category.as_deref(),
                                    &after,
                                );
                                *selected = 0;
                            }
                        }
                    }
                }
            }
            PopupState::Loading { .. } => {
                // Block all input while loading — user must wait
            }
            PopupState::CommandPalette { .. } => {}
            PopupState::RefPicker { .. } => {}
            PopupState::ListPicker { .. } => {}
            PopupState::PatchFilePicker { .. } => {}
            PopupState::ThemePicker { .. } => {}
            PopupState::None => {}
        }

        // These are handled separately to avoid borrow conflicts.
        // Use else-if so that a handler that transitions to another popup
        // (e.g. Help → ThemePicker on Enter) does not also fire the new
        // popup's handler with the same key event.
        if was_help && matches!(self.popup, PopupState::CommandPalette { .. }) {
            self.handle_command_palette_key(key)?;
        } else if was_ref_picker && matches!(self.popup, PopupState::RefPicker { .. }) {
            self.handle_ref_picker_key(key)?;
        } else if was_list_picker && matches!(self.popup, PopupState::ListPicker { .. }) {
            self.handle_list_picker_key(key)?;
        } else if was_patch_file_picker && matches!(self.popup, PopupState::PatchFilePicker { .. })
        {
            self.handle_patch_file_picker_key(key)?;
        } else if was_theme_picker && matches!(self.popup, PopupState::ThemePicker { .. }) {
            self.handle_theme_picker_key(key);
        }

        Ok(())
    }

    pub(super) fn handle_command_palette_key(&mut self, key: KeyEvent) -> Result<()> {
        // Helper: compute display index for a given entry selection
        fn find_display_idx(sections: &[CommandSection], sel: usize, search: &str) -> usize {
            use crate::gui::popup::{command_palette_entry_matches, list_picker_search_tokens};
            let tokens = list_picker_search_tokens(search);
            let has_search = !tokens.is_empty();
            let mut ei = 0usize;
            let mut di = 0usize;
            for section in sections {
                let mut section_has_visible = false;
                for entry in &section.entries {
                    let matches = !has_search
                        || command_palette_entry_matches(&entry.key, &entry.description, &tokens);
                    if matches {
                        if !section_has_visible {
                            section_has_visible = true;
                            di += 1; // header row
                        }
                        if ei == sel {
                            return di;
                        }
                        ei += 1;
                        di += 1;
                    }
                }
            }
            di
        }

        fn count_visible(sections: &[CommandSection], search: &str) -> usize {
            use crate::gui::popup::{command_palette_entry_matches, list_picker_search_tokens};
            let tokens = list_picker_search_tokens(search);
            let has_search = !tokens.is_empty();
            sections
                .iter()
                .map(|s| {
                    if has_search {
                        s.entries
                            .iter()
                            .filter(|e| {
                                command_palette_entry_matches(&e.key, &e.description, &tokens)
                            })
                            .count()
                    } else {
                        s.entries.len()
                    }
                })
                .sum()
        }

        let mut selected_action = None;

        if let PopupState::CommandPalette {
            sections,
            selected,
            search_textarea,
            scroll_offset,
        } = &mut self.popup
        {
            use crossterm::event::KeyModifiers;
            let search = search_textarea.lines().join("");
            let search_lower = search.to_lowercase();
            // `search_lower` kept for scroll math below; filtering uses tokens.

            // Estimate list viewport height from terminal
            let popup_height = (self.layout.height as usize).saturating_sub(4).min(50);
            let list_height = popup_height.saturating_sub(5); // borders + search + sep + hint

            match key.code {
                KeyCode::Esc | KeyCode::Char('?')
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.popup = self.pending_commit_popup.take().unwrap_or(PopupState::None);
                    return Ok(());
                }
                KeyCode::Enter => {
                    use crate::gui::popup::{
                        command_palette_entry_matches, list_picker_search_tokens,
                    };
                    let tokens = list_picker_search_tokens(&search);
                    let has_search = !tokens.is_empty();
                    let mut ei = 0usize;
                    'outer: for section in sections.iter() {
                        for entry in &section.entries {
                            let vis = !has_search
                                || command_palette_entry_matches(
                                    &entry.key,
                                    &entry.description,
                                    &tokens,
                                );
                            if vis {
                                if ei == *selected {
                                    selected_action = Some(entry.action.clone());
                                    break 'outer;
                                }
                                ei += 1;
                            }
                        }
                    }
                }
                KeyCode::Down => {
                    let total = count_visible(sections, &search_lower);
                    if total > 0 {
                        *selected = (*selected + 1).min(total.saturating_sub(1));
                    }
                    let sdi = find_display_idx(sections, *selected, &search_lower);
                    if sdi >= *scroll_offset + list_height {
                        *scroll_offset = sdi.saturating_sub(list_height - 1);
                    }
                }
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    if *selected == 0 {
                        // First item: always scroll to top so the section header is visible
                        *scroll_offset = 0;
                    } else {
                        let sdi = find_display_idx(sections, *selected, &search_lower);
                        if sdi <= *scroll_offset {
                            // Scroll up to show the section header too when possible
                            *scroll_offset = sdi.saturating_sub(1);
                        }
                    }
                }
                _ => {
                    textarea_input(search_textarea, key);
                    let new_search = search_textarea.lines().join("");
                    if new_search != search {
                        *selected = 0;
                        *scroll_offset = 0;
                    }
                }
            }
        }

        if let Some(action) = selected_action {
            match action {
                CommandAction::Dispatch(key) => {
                    // A commit-editor palette temporarily stashes the editor in
                    // pending_commit_popup. Restore it before dispatch so selecting
                    // a palette command behaves exactly like typing that key in the
                    // editor; normal palettes still dispatch against PopupState::None.
                    self.popup = self.pending_commit_popup.take().unwrap_or(PopupState::None);
                    self.handle_key(key)?;
                }
                CommandAction::OpenThemePicker => {
                    if let Some(stashed) = self.pending_commit_popup.take() {
                        self.saved_commit_popup = Some(stashed);
                    }
                    self.popup = PopupState::None;
                    self.show_theme_picker();
                }
                CommandAction::Unavailable => {}
            }
        }

        Ok(())
    }

    fn handle_ref_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        use crate::gui::popup::{
            REF_FREE_ENTRY_CATEGORY, list_picker_confirm_value, sync_list_picker_free_entry,
        };

        if let PopupState::RefPicker { core, .. } = &mut self.popup {
            let search = core.search_textarea.lines().join("");
            let matching = list_picker_matching_indices(&core.items, &search);

            let h = self.layout.height as usize;
            let list_height = list_picker_visible_height(h);

            match key.code {
                KeyCode::Esc => {
                    self.popup = PopupState::None;
                    return Ok(());
                }
                KeyCode::Enter => {
                    let Some(value) = list_picker_confirm_value(core) else {
                        return Ok(());
                    };
                    let popup = std::mem::replace(&mut self.popup, PopupState::None);
                    if let PopupState::RefPicker { on_confirm, .. } = popup {
                        if let Err(e) = on_confirm(self, &value) {
                            self.popup = PopupState::Message {
                                title: "Error".to_string(),
                                message: format!("{}", e),
                                kind: MessageKind::Error,
                            };
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(next) = list_picker_next_match(&matching, core.selected) {
                        core.selected = next;
                    }
                    list_picker_scroll_after_nav(core, &matching, list_height, true);
                }
                KeyCode::Up => {
                    if let Some(prev) = list_picker_prev_match(&matching, core.selected) {
                        core.selected = prev;
                    }
                    list_picker_scroll_after_nav(core, &matching, list_height, false);
                }
                _ => {
                    textarea_input(&mut core.search_textarea, key);
                    let new_search = core.search_textarea.lines().join("");
                    if new_search != search {
                        sync_list_picker_free_entry(core, REF_FREE_ENTRY_CATEGORY);
                        let matching = list_picker_matching_indices(&core.items, &new_search);
                        if let Some(sel) =
                            list_picker_clamp_selection_to_matches(&matching, core.selected)
                        {
                            core.selected = sel;
                        }
                        if !new_search.is_empty() {
                            let sdi = list_picker_filtered_display_idx(
                                &core.items,
                                &matching,
                                core.selected,
                            );
                            core.scroll_offset = sdi.saturating_sub(list_height / 2);
                        } else {
                            core.scroll_offset = 0;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle keys for the generic free-entry [`PopupState::ListPicker`].
    /// Same navigation/search semantics as RefPicker, with a configurable free-entry category.
    fn handle_list_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        use crate::gui::popup::{list_picker_confirm_value, sync_list_picker_prefer_free_entry};

        if let PopupState::ListPicker {
            core,
            free_entry_category,
            ..
        } = &mut self.popup
        {
            let search = core.search_textarea.lines().join("");
            let matching = list_picker_matching_indices(&core.items, &search);
            let free_cat = free_entry_category.clone();

            let h = self.layout.height as usize;
            let list_height = list_picker_visible_height(h);

            match key.code {
                KeyCode::Esc => {
                    self.popup = PopupState::None;
                    return Ok(());
                }
                KeyCode::Enter => {
                    // Empty free-entry category (e.g. diff-grep) confirms real
                    // matches only: zero matches = no-op instead of jumping to
                    // a stale selection.
                    if free_cat.is_empty() && !matching.contains(&core.selected) {
                        return Ok(());
                    }
                    let Some(value) = list_picker_confirm_value(core) else {
                        return Ok(());
                    };
                    let popup = std::mem::replace(&mut self.popup, PopupState::None);
                    if let PopupState::ListPicker { on_confirm, .. } = popup {
                        if let Err(e) = on_confirm(self, &value) {
                            self.popup = PopupState::Message {
                                title: "Error".to_string(),
                                message: format!("{}", e),
                                kind: MessageKind::Error,
                            };
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(next) = list_picker_next_match(&matching, core.selected) {
                        core.selected = next;
                    }
                    list_picker_scroll_after_nav(core, &matching, list_height, true);
                }
                KeyCode::Up => {
                    if let Some(prev) = list_picker_prev_match(&matching, core.selected) {
                        core.selected = prev;
                    }
                    list_picker_scroll_after_nav(core, &matching, list_height, false);
                }
                _ => {
                    textarea_input(&mut core.search_textarea, key);
                    let new_search = core.search_textarea.lines().join("");
                    if new_search != search {
                        sync_list_picker_prefer_free_entry(core, &free_cat);
                        let matching = list_picker_matching_indices(&core.items, &new_search);
                        if let Some(sel) =
                            list_picker_clamp_selection_to_matches(&matching, core.selected)
                        {
                            core.selected = sel;
                        }
                        if !new_search.is_empty() {
                            let sdi = list_picker_filtered_display_idx(
                                &core.items,
                                &matching,
                                core.selected,
                            );
                            core.scroll_offset = sdi.saturating_sub(list_height / 2);
                        } else {
                            core.scroll_offset = 0;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Open a reusable free-entry list picker (path/author filters, etc.).
    ///
    /// Callers supply candidate items and a confirm callback. Typing always
    /// inserts a synthetic free-entry row under `free_entry_category` so the
    /// user can confirm arbitrary text even when it does not match a candidate.
    pub fn show_list_picker(
        &mut self,
        title: impl Into<String>,
        items: Vec<crate::gui::popup::ListPickerItem>,
        free_entry_category: impl Into<String>,
        on_confirm: crate::gui::popup::ListPickerAction,
    ) {
        use crate::gui::popup::{ListPickerCore, make_command_palette_search_textarea};

        self.popup = PopupState::ListPicker {
            title: title.into(),
            core: ListPickerCore {
                items,
                selected: 0,
                search_textarea: make_command_palette_search_textarea(),
                scroll_offset: 0,
            },
            free_entry_category: free_entry_category.into(),
            on_confirm,
        };
    }

    pub(super) fn patch_file_picker_animation_active(&self) -> bool {
        matches!(
            &self.popup,
            PopupState::PatchFilePicker { state, .. } if state.picker_scroll_active()
        )
    }

    pub(super) fn tick_patch_file_picker_animation(&mut self) {
        if let PopupState::PatchFilePicker { state, .. } = &mut self.popup {
            state.tick_picker_scroll();
        }
    }

    fn handle_patch_file_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        let mut confirm_path: Option<PathBuf> = None;
        let picker_rows = crate::gui::views::patch_file_picker_preview_height(self.layout.height);

        if let PopupState::PatchFilePicker { state, .. } = &mut self.popup {
            let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
            // Do not require an exact modifier bit-set: enhanced keyboard
            // protocols may carry additional modifier information alongside Ctrl.
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') if plain => {
                    self.popup = PopupState::None;
                    return Ok(());
                }
                KeyCode::Char('j') | KeyCode::Char('J') if control => {
                    state.clear_count();
                    state.queue_picker_half_page_scroll(1, picker_rows);
                }
                KeyCode::Char('k') | KeyCode::Char('K') if control => {
                    state.clear_count();
                    state.queue_picker_half_page_scroll(-1, picker_rows);
                }
                KeyCode::Char(ch) if plain && ch.is_ascii_digit() => {
                    state.push_count_digit(ch.to_digit(10).unwrap_or(0));
                }
                KeyCode::Down | KeyCode::Char('j') if plain => {
                    let count = state.take_count();
                    state.move_selection(count.min(isize::MAX as usize) as isize);
                }
                KeyCode::Up | KeyCode::Char('k') if plain => {
                    let count = state.take_count();
                    state.move_selection(-(count.min(isize::MAX as usize) as isize));
                }
                KeyCode::Char('g') if plain => state.select_first(),
                KeyCode::Char('G') if plain => state.select_last(),
                KeyCode::PageDown if plain => state.move_selection(10),
                KeyCode::PageUp if plain => state.move_selection(-10),
                KeyCode::Home if plain => state.select_first(),
                KeyCode::End if plain => state.select_last(),
                KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') if plain => {
                    state.go_parent()?;
                }
                KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') if plain => {
                    confirm_path = state.enter_selected()?;
                }
                KeyCode::Char('r') if plain => {
                    let repo_root = state.repo_root.clone();
                    state.go_to(repo_root)?;
                }
                KeyCode::Char('~') if plain => {
                    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                        state.go_to(home)?;
                    }
                }
                KeyCode::Char('d') if plain => {
                    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                        let downloads = home.join("Downloads");
                        if downloads.is_dir() {
                            state.go_to(downloads)?;
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(path) = confirm_path {
            let value = path.to_string_lossy().into_owned();
            let popup = std::mem::replace(&mut self.popup, PopupState::None);
            if let PopupState::PatchFilePicker { on_confirm, .. } = popup {
                if let Err(error) = on_confirm(self, &value) {
                    self.popup = PopupState::Message {
                        title: "Error".to_string(),
                        message: error.to_string(),
                        kind: MessageKind::Error,
                    };
                }
            }
        }
        Ok(())
    }

    fn handle_theme_picker_key(&mut self, key: KeyEvent) {
        if let PopupState::ThemePicker {
            core,
            original_theme_index,
        } = &mut self.popup
        {
            let search = core.search_textarea.lines().join("");
            let matching = list_picker_matching_indices(&core.items, &search);

            let h = self.layout.height as usize;
            let list_height = list_picker_visible_height(h);

            match key.code {
                KeyCode::Esc => {
                    self.current_theme_index = *original_theme_index;
                    self.popup = PopupState::None;
                    return;
                }
                KeyCode::Enter => {
                    let idx = core.selected;
                    self.popup = PopupState::None;
                    self.current_theme_index = idx;
                    if let Some(ct) = crate::config::COLOR_THEMES.get(idx) {
                        let mut state = self.config.app_state.clone();
                        state.color_theme = Some(ct.id.to_string());
                        let _ = state.save(&self.config.state_path);
                    }
                    return;
                }
                KeyCode::Down => {
                    if let Some(next) = list_picker_next_match(&matching, core.selected) {
                        core.selected = next;
                    }
                    self.current_theme_index = core.selected;
                    list_picker_scroll_after_nav(core, &matching, list_height, true);
                }
                KeyCode::Up => {
                    if let Some(prev) = list_picker_prev_match(&matching, core.selected) {
                        core.selected = prev;
                    }
                    self.current_theme_index = core.selected;
                    list_picker_scroll_after_nav(core, &matching, list_height, false);
                }
                _ => {
                    // Search/filter — keep selection within matching themes
                    textarea_input(&mut core.search_textarea, key);
                    let new_search = core.search_textarea.lines().join("");
                    if new_search != search {
                        let matching = list_picker_matching_indices(&core.items, &new_search);
                        if !new_search.trim().is_empty() {
                            if let Some(sel) =
                                list_picker_clamp_selection_to_matches(&matching, core.selected)
                            {
                                core.selected = sel;
                                self.current_theme_index = sel;
                                let sdi = list_picker_filtered_display_idx(
                                    &core.items,
                                    &matching,
                                    core.selected,
                                );
                                core.scroll_offset = sdi.saturating_sub(list_height / 2);
                            }
                        } else {
                            core.selected = *original_theme_index;
                            self.current_theme_index = *original_theme_index;
                            core.scroll_offset =
                                original_theme_index.saturating_sub(list_height / 2);
                        }
                    }
                }
            }
        }
    }

    fn show_theme_picker(&mut self) {
        use crate::gui::popup::{
            ListPickerCore, ListPickerItem, make_command_palette_search_textarea,
        };

        let original = self.current_theme_index;
        let items: Vec<ListPickerItem> = crate::config::COLOR_THEMES
            .iter()
            .map(|ct| ListPickerItem {
                value: ct.id.to_string(),
                label: ct.name.to_string(),
                category: String::new(),
                description: Some(ct.appearance.as_str().to_string()),
            })
            .collect();

        self.popup = PopupState::ThemePicker {
            core: ListPickerCore {
                items,
                selected: original,
                search_textarea: make_command_palette_search_textarea(),
                scroll_offset: 0,
            },
            original_theme_index: original,
        };
    }

    fn show_reset_rebase_picker(
        &mut self,
        title: &str,
        skip_head: bool,
        on_confirm: crate::gui::popup::ListPickerAction,
    ) {
        use crate::gui::popup::{ListPickerCore, make_command_palette_search_textarea};

        self.popup = PopupState::RefPicker {
            title: title.to_string(),
            core: ListPickerCore {
                items: self.collect_reset_rebase_picker_items(skip_head),
                selected: 0,
                search_textarea: make_command_palette_search_textarea(),
                scroll_offset: 0,
            },
            on_confirm,
        };
    }

    pub fn show_interactive_rebase_picker(&mut self) {
        self.show_reset_rebase_picker(
            "Interactive rebase current branch onto",
            true,
            Box::new(|gui, ref_name| {
                controller::branches::enter_interactive_rebase_onto(gui, ref_name)
            }),
        );
    }

    /// Global reset picker (default `Z`): choose a branch/commit/tag, then
    /// soft/mixed/hard reset options (lazygit `universal.viewResetOptions` /
    /// `CreateGitResetMenu`). Reuses the same searchable ref list as the
    /// interactive rebase picker, then the shared reset-options menu used by
    /// contextual lowercase `g`.
    pub fn show_reset_picker(&mut self) {
        self.show_reset_rebase_picker(
            "Reset to:",
            false,
            Box::new(|gui, ref_name| controller::commits::show_reset_menu_for_ref(gui, ref_name)),
        );
    }

    /// Shared branch/remote/tag/commit items for the interactive rebase and reset pickers.
    /// When `skip_head` is true, the current HEAD branch is omitted (rebase);
    /// when false it is included (reset). HEAD itself is always omitted from
    /// the commits section because operating on the tip is a no-op.
    fn collect_reset_rebase_picker_items(
        &self,
        skip_head: bool,
    ) -> Vec<crate::gui::popup::ListPickerItem> {
        use crate::gui::popup::ListPickerItem;

        let model = self.model.lock().unwrap();
        let mut items = Vec::new();

        controller::append_named_ref_picker_items(&mut items, &model, skip_head);

        for commit in model.commits.iter().skip(1) {
            items.push(ListPickerItem {
                value: commit.hash.clone(),
                label: format!(
                    "{} {}",
                    commit.short_hash(),
                    presentation::text::plain_text(&commit.name)
                ),
                category: "Commits".to_string(),
                description: None,
            });
        }

        items
    }
}
