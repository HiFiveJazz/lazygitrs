//! Rendering for modal popup states.
//!
//! `views` re-exports `render_popup`, so this split is source organization
//! only and does not change the public rendering path.

use super::*;

pub(crate) fn render_popup(
    frame: &mut Frame,
    popup: &PopupState,
    area: Rect,
    spinner_frame: usize,
    theme: &Theme,
    ai_button_hovered: bool,
    ai_configured: bool,
) {
    // Bail out early on terminals too small to host any popup — better than
    // panicking inside a render with an out-of-bounds rect.
    if area.width < 4 || area.height < 4 {
        return;
    }
    let popup_width = (area.width * 60 / 100).min(60).max(30).min(area.width);
    let x = (area.width.saturating_sub(popup_width)) / 2;

    match popup {
        PopupState::ShellCommand { textarea } => {
            // Reuse input styling without introducing soft-wrap mutations.
            let input = PopupState::Input {
                title: "Execute Shell Command".into(),
                textarea: textarea.clone(),
                on_confirm: Box::new(|_, _| Ok(())),
                is_commit: false,
                confirm_focused: false,
            };
            render_popup(frame, &input, area, spinner_frame, theme, false, false);
        }
        PopupState::PatchPreview {
            title,
            message,
            scroll,
            cursor_line,
            ..
        } => {
            let (rect, rows) = command_output_geometry(area);
            let width = usize::from(rect.width.saturating_sub(2));
            let wrapped = wrap_patch_preview_lines(message, width, theme);
            let max = wrapped.len().saturating_sub(rows);
            let scroll = (*scroll).min(max);
            let cursor = (*cursor_line).min(wrapped.len().saturating_sub(1));

            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " {title} [{}/{}] ",
                    cursor + 1,
                    wrapped.len().max(1)
                ))
                .border_style(Style::default().fg(theme.accent_secondary));
            frame.render_widget(block, rect);
            let inner = rect.inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });

            let body_height = inner.height.saturating_sub(1);
            let mut lines = Vec::new();
            for (idx, line) in wrapped
                .iter()
                .enumerate()
                .skip(scroll)
                .take(usize::from(body_height))
            {
                let mut line = line.clone();
                if idx == cursor {
                    for span in &mut line.spans {
                        span.style = span.style.bg(theme.selected_bg);
                    }
                    if line.spans.is_empty() {
                        line.spans
                            .push(Span::styled(" ", Style::default().bg(theme.selected_bg)));
                    }
                }
                lines.push(line);
            }
            frame.render_widget(
                Paragraph::new(lines),
                Rect::new(inner.x, inner.y, inner.width, body_height),
            );
            if cursor >= scroll && cursor < scroll.saturating_add(usize::from(body_height)) {
                let y = inner.y + (cursor - scroll) as u16;
                frame.buffer_mut().set_style(
                    Rect::new(inner.x, y, inner.width, 1),
                    Style::default().bg(theme.selected_bg),
                );
            }

            let key_style = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
            let desc_style = Style::default().fg(theme.text_dimmed);
            let hint_line = Line::from(vec![
                Span::styled(" ctrl+j/k ", key_style),
                Span::styled("jump 22  ", desc_style),
                Span::styled("j/k ", key_style),
                Span::styled("line  ", desc_style),
                Span::styled("y ", key_style),
                Span::styled("copy  ", desc_style),
                Span::styled("esc ", key_style),
                Span::styled("close", desc_style),
            ]);
            frame.render_widget(
                Paragraph::new(hint_line),
                Rect::new(inner.x, inner.y + body_height, inner.width, 1),
            );
        }
        PopupState::CommandOutput {
            title,
            message,
            kind,
            scroll,
        } => {
            let (rect, rows) = command_output_geometry(area);
            let wrapped = wrap_popup_lines(message, usize::from(rect.width.saturating_sub(2)));
            let max = wrapped.len().saturating_sub(rows);
            let scroll = (*scroll).min(max);
            frame.render_widget(Clear, rect);
            let color = if *kind == crate::gui::popup::MessageKind::Error {
                Color::Red
            } else {
                theme.accent_secondary
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " {title} [{}/{}] ",
                    scroll + 1,
                    wrapped.len().max(1)
                ))
                .border_style(Style::default().fg(color));
            frame.render_widget(block, rect);
            let inner = rect.inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });
            let lines: Vec<Line> = wrapped
                .iter()
                .skip(scroll)
                .take(rows)
                .map(|line| Line::from(line.as_str()))
                .collect();
            frame.render_widget(
                Paragraph::new(lines),
                Rect::new(
                    inner.x,
                    inner.y,
                    inner.width,
                    inner.height.saturating_sub(1),
                ),
            );
            let key_style = Style::default()
                .fg(theme.text)
                .add_modifier(ratatui::style::Modifier::BOLD);
            let desc_style = Style::default().fg(theme.text_dimmed);
            let hint_line = Line::from(vec![
                Span::styled(" enter ", key_style),
                Span::styled("close  ", desc_style),
                Span::styled("esc ", key_style),
                Span::styled("close  ", desc_style),
                Span::styled("j/k ", key_style),
                Span::styled("scroll  ", desc_style),
                Span::styled("y ", key_style),
                Span::styled("copy  ", desc_style),
                Span::styled("pgup/pgdn ", key_style),
                Span::styled("page  ", desc_style),
                Span::styled("g/G ", key_style),
                Span::styled("top/bottom", desc_style),
            ]);
            frame.render_widget(
                Paragraph::new(hint_line),
                Rect::new(
                    inner.x,
                    inner.y + inner.height.saturating_sub(1),
                    inner.width,
                    inner.height.min(1),
                ),
            );
        }
        PopupState::Confirm { title, message, .. } => {
            let inner_width = popup_width.saturating_sub(4) as usize; // borders + padding
            let wrapped = wrap_popup_lines(message, inner_width);
            let wrapped = visible_popup_lines(&wrapped, area.height.saturating_sub(5) as usize);
            let confirm_height = clamped_popup_height(wrapped.len(), 5, area.height);
            let cy = (area.height.saturating_sub(confirm_height)) / 2;
            let popup_rect = Rect::new(x, cy, popup_width, confirm_height);
            frame.render_widget(Clear, popup_rect);
            let block = Block::default()
                .title(format!(" {} ", title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent_secondary));

            let mut text: Vec<Line> = Vec::new();
            text.push(Line::from(""));
            for line in &wrapped {
                text.push(Line::from(format!(" {}", line)));
            }
            text.push(Line::from(""));
            let key_style = Style::default().fg(theme.accent_secondary);
            let desc_style = Style::default().fg(theme.text_dimmed);
            text.push(Line::from(vec![
                Span::styled(" y", key_style),
                Span::styled(": yes  ", desc_style),
                Span::styled("n", key_style),
                Span::styled(": no", desc_style),
            ]));

            let widget = Paragraph::new(text).block(block);
            frame.render_widget(widget, popup_rect);
        }
        PopupState::Message {
            title,
            message,
            kind,
        } => {
            let is_error = *kind == crate::gui::popup::MessageKind::Error;
            let icon = if is_error { "⚠ " } else { "" };
            let inner_width = popup_width.saturating_sub(4) as usize; // borders + padding
            let wrapped = wrap_popup_lines(message, inner_width);
            let wrapped = visible_popup_lines(&wrapped, area.height.saturating_sub(4) as usize);
            let msg_height = clamped_popup_height(wrapped.len(), 4, area.height);
            let cy = (area.height.saturating_sub(msg_height)) / 2;
            let popup_rect = Rect::new(x, cy, popup_width, msg_height);
            frame.render_widget(Clear, popup_rect);
            let border_color = if is_error {
                Color::Red
            } else {
                theme.accent_secondary
            };
            let block = Block::default()
                .title(format!(" {}{} ", icon, title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            let mut text: Vec<Line> = Vec::new();
            text.push(Line::from(""));
            for line in &wrapped {
                text.push(Line::from(format!(" {}", line)));
            }
            text.push(Line::from(Span::styled(
                " Press any key to dismiss",
                Style::default().fg(theme.text_dimmed),
            )));

            let widget = Paragraph::new(text).block(block);
            frame.render_widget(widget, popup_rect);
        }
        PopupState::Input {
            title,
            textarea,
            is_commit,
            confirm_focused,
            ..
        } => {
            // Textarea popup: taller to allow multiline editing
            // Add extra row for commit dialogs to fit the confirm button row
            let ta_height = if *is_commit { 14u16 } else { 12u16 };
            let ta_height = ta_height.min(area.height);
            if ta_height < 3 || popup_width < 3 {
                return;
            }
            let ta_y = (area.height.saturating_sub(ta_height)) / 2;
            let ta_rect = Rect::new(x, ta_y, popup_width, ta_height);
            frame.render_widget(Clear, ta_rect);

            // Render a container block with title and hint
            let outer = Block::default()
                .title(format!(" {} ", title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.popup_border));
            frame.render_widget(outer, ta_rect);

            // Inner area for textarea + hint
            let inner = ta_rect.inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });

            if *is_commit {
                // Reserve 2 lines: one for hint, one for confirm button row
                if inner.height > 4 {
                    let ta_area = Rect::new(inner.x, inner.y, inner.width, inner.height - 2);
                    frame.render_widget(textarea, ta_area);

                    // Hint line (opencode-style: bold key, dim description)
                    let hint_area = Rect::new(inner.x, inner.y + inner.height - 2, inner.width, 1);
                    let key_style = Style::default()
                        .fg(theme.text)
                        .add_modifier(ratatui::style::Modifier::BOLD);
                    let desc_style = Style::default().fg(theme.text_dimmed);
                    let hint_line = Line::from(vec![
                        Span::styled(" ctrl+s ", key_style),
                        Span::styled("confirm  ", desc_style),
                        Span::styled("ctrl+o ", key_style),
                        Span::styled("menu  ", desc_style),
                        Span::styled("esc ", key_style),
                        Span::styled("cancel", desc_style),
                    ]);
                    frame.render_widget(Paragraph::new(hint_line), hint_area);

                    // Confirm button row (right-aligned)
                    let btn_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
                    let (btn_style, btn_text) = if *confirm_focused {
                        (
                            Style::default()
                                .fg(Color::Black)
                                .bg(theme.accent)
                                .add_modifier(ratatui::style::Modifier::BOLD),
                            " Confirm ",
                        )
                    } else {
                        (Style::default().fg(theme.accent), " Confirm ")
                    };
                    let btn_width = (btn_text.len() as u16).min(btn_area.width);
                    if btn_width > 0 {
                        let btn_x = btn_area.x + btn_area.width.saturating_sub(btn_width);
                        let btn_rect = Rect::new(btn_x, btn_area.y, btn_width, 1);
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(btn_text, btn_style))),
                            btn_rect,
                        );
                    }
                } else {
                    frame.render_widget(textarea, inner);
                }
            } else {
                // Non-commit: no button, just hint
                if inner.height > 2 {
                    let ta_area = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
                    frame.render_widget(textarea, ta_area);

                    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
                    let key_style = Style::default()
                        .fg(theme.text)
                        .add_modifier(ratatui::style::Modifier::BOLD);
                    let desc_style = Style::default().fg(theme.text_dimmed);
                    let hint_line = Line::from(vec![
                        Span::styled(" enter ", key_style),
                        Span::styled("confirm  ", desc_style),
                        Span::styled("esc ", key_style),
                        Span::styled("cancel", desc_style),
                    ]);
                    frame.render_widget(Paragraph::new(hint_line), hint_area);
                } else {
                    frame.render_widget(textarea, inner);
                }
            }
        }
        PopupState::EmailPatch {
            title,
            to_recipients,
            cc_recipients,
            focus,
            to_selected,
            cc_selected,
            recipient_editor,
            dry_run,
            reviewing,
            preview_from,
            preview_subject,
            preview_date,
            preview_body,
            preview_scroll,
            confirming_send,
            resolving_recipients,
            ..
        } => {
            let Some(rect) = email_popup_geometry(area) else {
                return;
            };
            frame.render_widget(Clear, rect);

            let mode_color = if *reviewing {
                theme.accent_secondary
            } else {
                theme.accent
            };
            let outer = Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(mode_color));
            frame.render_widget(outer, rect);

            let inner = rect.inner(ratatui::layout::Margin {
                horizontal: 2,
                vertical: 1,
            });
            if inner.height < 8 {
                return;
            }

            // Status badges and hints are anchored at the very bottom of the
            // modal. Compose/Review shares the footer row; Dry Run appears
            // directly above it only while enabled, so mode state is visible
            // without permanently spending space on an old-style checkbox.
            let footer_y = inner.y + inner.height.saturating_sub(1);
            let status_y = footer_y.saturating_sub(1);
            // Keep all status badges on one shared width so Compose, Review,
            // and Dry Run line up as a single right-edge status stack.
            let status_badge_width = 9u16.min(inner.width);
            let mode_text = if *reviewing { "Review" } else { "Compose" };
            let mode_width = status_badge_width;
            let mode_rect = Rect::new(
                inner.x + inner.width.saturating_sub(mode_width),
                footer_y,
                mode_width,
                1,
            );

            let key_style = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
            let desc_style = Style::default().fg(theme.text_dimmed);
            let footer = if *resolving_recipients {
                let spinner = SPINNER_CHARS[(spinner_frame / 8) % SPINNER_CHARS.len()];
                Line::from(vec![
                    Span::styled(format!(" {spinner} resolving recipients…  "), desc_style),
                    Span::styled("esc ", key_style),
                    Span::styled("cancel", desc_style),
                ])
            } else if *reviewing {
                Line::from(vec![
                    Span::styled("j/k ", key_style),
                    Span::styled("scroll  ", desc_style),
                    Span::styled("enter ", key_style),
                    Span::styled(if *dry_run { "check  " } else { "confirm  " }, desc_style),
                    Span::styled("ctrl+d ", key_style),
                    Span::styled("dry-run  ", desc_style),
                    Span::styled("esc ", key_style),
                    Span::styled("back  ", desc_style),
                    Span::styled("q ", key_style),
                    Span::styled("quit", desc_style),
                ])
            } else if recipient_editor.is_some() {
                // The editor has its own floating footer; keep the base footer
                // quiet underneath it.
                Line::from(Span::styled(" recipient editor open", desc_style))
            } else {
                Line::from(vec![
                    Span::styled("j/k ", key_style),
                    Span::styled("move  ", desc_style),
                    Span::styled("a ", key_style),
                    Span::styled("add  ", desc_style),
                    Span::styled("e ", key_style),
                    Span::styled("edit  ", desc_style),
                    Span::styled("d ", key_style),
                    Span::styled("delete  ", desc_style),
                    Span::styled("u ", key_style),
                    Span::styled("undo  ", desc_style),
                    Span::styled("tab ", key_style),
                    Span::styled("To/Cc  ", desc_style),
                    Span::styled("enter ", key_style),
                    Span::styled("review  ", desc_style),
                    Span::styled("q ", key_style),
                    Span::styled("quit", desc_style),
                ])
            };
            let footer_width = inner.width.saturating_sub(mode_width.saturating_add(1));
            if footer_width > 0 {
                frame.render_widget(
                    Paragraph::new(footer),
                    Rect::new(inner.x, footer_y, footer_width, 1),
                );
            }

            // Paint the entire fixed-width badge rectangle, then center the
            // label inside it. Styling only the text span makes labels with
            // different lengths (e.g. "Dry Run" vs "Review") appear
            // horizontally misaligned even when their Rects are identical.
            frame.render_widget(
                Block::default().style(Style::default().bg(mode_color)),
                mode_rect,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    mode_text,
                    Style::default()
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                )))
                .alignment(ratatui::layout::Alignment::Center),
                mode_rect,
            );

            if *dry_run {
                let dry_text = "Dry Run";
                let dry_width = status_badge_width;
                let dry_rect = Rect::new(
                    inner.x + inner.width.saturating_sub(dry_width),
                    status_y,
                    dry_width,
                    1,
                );
                // Pick Dry Run from the theme's graph palette, but never reuse
                // either Compose or Review's accent. This keeps the status
                // theme-native while making the three modes visually distinct.
                let dry_run_color = theme
                    .graph_colors
                    .iter()
                    .copied()
                    .find(|color| *color != theme.accent && *color != theme.accent_secondary)
                    .unwrap_or(theme.change_added);
                frame.render_widget(
                    Block::default().style(Style::default().bg(dry_run_color)),
                    dry_rect,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        dry_text,
                        Style::default()
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    )))
                    .alignment(ratatui::layout::Alignment::Center),
                    dry_rect,
                );
            }

            if *reviewing {
                if let Some((preview_inner, header_rows, body_rows)) = email_preview_body_layout(
                    preview_from,
                    to_recipients,
                    cc_recipients,
                    preview_subject,
                    preview_date,
                    area,
                ) {
                    let header_lines = email_preview_header_lines(
                        preview_from,
                        to_recipients,
                        cc_recipients,
                        preview_subject,
                        preview_date,
                        usize::from(preview_inner.width).max(1),
                    );
                    for (index, line) in header_lines.iter().take(header_rows).enumerate() {
                        frame.render_widget(
                            Paragraph::new(styled_email_header_line(line, theme)),
                            Rect::new(
                                preview_inner.x,
                                preview_inner.y + index as u16,
                                preview_inner.width,
                                1,
                            ),
                        );
                    }

                    let wrapped = wrap_patch_preview_lines(
                        preview_body,
                        usize::from(preview_inner.width).max(1),
                        theme,
                    );
                    let max_scroll = wrapped.len().saturating_sub(body_rows);
                    let scroll = (*preview_scroll).min(max_scroll);
                    let first = if wrapped.is_empty() { 0 } else { scroll + 1 };
                    let last = (scroll + body_rows).min(wrapped.len());

                    let divider_y = preview_inner.y + header_rows as u16;
                    let label = format!(" Patch [{first}-{last}/{}] ", wrapped.len().max(1));
                    let divider_width = usize::from(preview_inner.width);
                    let left = 2usize.min(divider_width);
                    let right = divider_width.saturating_sub(left + UnicodeWidthStr::width(label.as_str()));
                    let divider = format!(
                        "{}{}{}",
                        "─".repeat(left),
                        label,
                        "─".repeat(right),
                    );
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            divider,
                            Style::default().fg(theme.popup_border),
                        ))),
                        Rect::new(preview_inner.x, divider_y, preview_inner.width, 1),
                    );

                    let body_y = divider_y.saturating_add(1);
                    frame.render_widget(
                        Paragraph::new(wrapped)
                            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
                        Rect::new(
                            preview_inner.x,
                            body_y,
                            preview_inner.width,
                            body_rows.min(u16::MAX as usize) as u16,
                        ),
                    );
                }

                if *confirming_send {
                    render_email_send_confirmation(frame, rect, to_recipients, cc_recipients, theme);
                }
                return;
            }

            let focused_label = Style::default()
                .fg(theme.accent_secondary)
                .add_modifier(Modifier::BOLD);
            let idle_label = Style::default().fg(theme.text_dimmed);

            // Reserve rows above the bottom controls for the recipient lists.
            // Labels consume two rows and the gap between To/Cc consumes one.
            let content_start = if *reviewing {
                let action = if *dry_run { "Check" } else { "Send" };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("{action} this patch?"),
                        Style::default()
                            .fg(theme.text_strong)
                            .add_modifier(Modifier::BOLD),
                    ))),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                inner.y.saturating_add(2)
            } else {
                inner.y
            };
            let content_rows = status_y.saturating_sub(content_start) as usize;
            let list_budget = content_rows.saturating_sub(3).max(2);
            let (to_rows, cc_rows) = email_recipient_row_allocation(
                to_recipients.len(),
                cc_recipients.len(),
                list_budget,
            );

            let to_label = format!("To ({})", to_recipients.len());
            frame.render_widget(
                Paragraph::new(to_label).style(if !*reviewing && *focus == EmailPatchFocus::To {
                    focused_label
                } else {
                    idle_label
                }),
                Rect::new(inner.x, content_start, inner.width, 1),
            );
            let to_y = content_start.saturating_add(1);
            let to_lines = email_recipient_lines(
                to_recipients,
                *to_selected,
                !*reviewing && *focus == EmailPatchFocus::To,
                to_rows,
                !*reviewing,
                theme,
            );
            frame.render_widget(
                Paragraph::new(to_lines),
                Rect::new(inner.x, to_y, inner.width, to_rows as u16),
            );

            let cc_label_y = to_y.saturating_add(to_rows as u16).saturating_add(1);
            let cc_label = format!("Cc ({})", cc_recipients.len());
            frame.render_widget(
                Paragraph::new(cc_label).style(if !*reviewing && *focus == EmailPatchFocus::Cc {
                    focused_label
                } else {
                    idle_label
                }),
                Rect::new(inner.x, cc_label_y, inner.width, 1),
            );
            let cc_y = cc_label_y.saturating_add(1);
            let cc_lines = email_recipient_lines(
                cc_recipients,
                *cc_selected,
                !*reviewing && *focus == EmailPatchFocus::Cc,
                cc_rows,
                !*reviewing,
                theme,
            );
            frame.render_widget(
                Paragraph::new(cc_lines),
                Rect::new(inner.x, cc_y, inner.width, cc_rows as u16),
            );

            // Add/Edit is a real floating child dialog instead of consuming
            // rows below the recipient lists and moving the main footer.
            if !*reviewing {
                if let Some(editor) = recipient_editor {
                    let editor_width = (rect.width * 74 / 100)
                        .max(38)
                        .min(rect.width.saturating_sub(4));
                    let editor_height = 6u16.min(rect.height.saturating_sub(4));
                    if editor_width >= 20 && editor_height >= 5 {
                        let editor_rect = Rect::new(
                            rect.x + rect.width.saturating_sub(editor_width) / 2,
                            rect.y + rect.height.saturating_sub(editor_height) / 2,
                            editor_width,
                            editor_height,
                        );
                        frame.render_widget(Clear, editor_rect);
                        let verb = if editor.index.is_some() {
                            "Edit"
                        } else {
                            "Add"
                        };
                        let editor_block = Block::default()
                            .title(format!(" {verb} {} recipient ", editor.field.label()))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme.accent_secondary));
                        frame.render_widget(editor_block, editor_rect);

                        let editor_inner = editor_rect.inner(ratatui::layout::Margin {
                            horizontal: 1,
                            vertical: 1,
                        });
                        frame.render_widget(
                            &editor.textarea,
                            Rect::new(editor_inner.x, editor_inner.y, editor_inner.width, 1),
                        );
                        let editor_footer_y =
                            editor_inner.y + editor_inner.height.saturating_sub(1);
                        frame.render_widget(
                            Paragraph::new(Line::from(vec![
                                Span::styled(" enter ", key_style),
                                Span::styled("save  ", desc_style),
                                Span::styled("esc ", key_style),
                                Span::styled("cancel", desc_style),
                            ])),
                            Rect::new(editor_inner.x, editor_footer_y, editor_inner.width, 1),
                        );
                    }
                }
            }
        }

        PopupState::CommitInput {
            kind,
            summary_textarea,
            body_textarea,
            focus,
            mode,
            pending_command,
            body_scroll_top,
            ..
        } => {
            // Two-field commit editor: summary (1 line) + body (multi-line)
            // Layout: border, summary label, summary input, body label, body textarea, hint, border
            let popup_width = commit_popup_width(area.width);
            let x = (area.width.saturating_sub(popup_width)) / 2;
            let ta_height = commit_popup_height(area.height);
            if ta_height < 3 || popup_width < 3 {
                return;
            }
            let ta_y = (area.height.saturating_sub(ta_height)) / 2;
            let ta_rect = Rect::new(x, ta_y, popup_width, ta_height);
            frame.render_widget(Clear, ta_rect);

            let border_color = match focus {
                CommitInputFocus::Summary => theme.popup_border,
                CommitInputFocus::Body => theme.popup_border,
            };
            let outer = Block::default()
                .title(format!(" {} ", kind.title()))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            frame.render_widget(outer, ta_rect);

            let inner = ta_rect.inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });

            if inner.height > 7 {
                let focused_style = Style::default()
                    .fg(theme.accent_secondary)
                    .add_modifier(Modifier::BOLD);
                let unfocused_style = Style::default().fg(theme.text_dimmed);
                let (mode_color, mode_text_color) = match mode {
                    CommitInputMode::Normal => (Color::Blue, Color::Black),
                    CommitInputMode::Insert => (Color::LightGreen, Color::Black),
                    CommitInputMode::Visual | CommitInputMode::VisualLine => {
                        (Color::Rgb(170, 110, 255), Color::Black)
                    }
                };

                // Summary label
                let summary_label_area = Rect::new(inner.x, inner.y, inner.width, 1);
                let summary_label_style = if *focus == CommitInputFocus::Summary {
                    focused_style
                } else {
                    unfocused_style
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled("Summary", summary_label_style))),
                    summary_label_area,
                );

                // Summary input (1 line)
                let summary_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
                frame.render_widget(summary_textarea, summary_area);
                if *focus == CommitInputFocus::Summary && !mode.is_visual() {
                    frame.buffer_mut().set_style(
                        summary_area,
                        Style::default().bg(theme.selected_bg),
                    );
                }

                // Body label
                let body_label_area = Rect::new(inner.x, inner.y + 3, inner.width, 1);
                let body_label_style = if *focus == CommitInputFocus::Body {
                    focused_style
                } else {
                    unfocused_style
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled("Description", body_label_style))),
                    body_label_area,
                );

                // Body textarea with a Vim-style relative-number gutter. The body
                // itself remains a TextArea so its cursor/selection rendering and
                // viewport behavior stay intact.
                // Reserve only the final row for the compact mode badge / help hint.
                let body_height = inner.height.saturating_sub(5);
                let gutter_width = COMMIT_BODY_GUTTER_WIDTH.min(inner.width.saturating_sub(1));
                let gutter_area = Rect::new(inner.x, inner.y + 4, gutter_width, body_height);
                let body_area = Rect::new(
                    inner.x + gutter_width,
                    inner.y + 4,
                    inner.width.saturating_sub(gutter_width),
                    body_height,
                );
                // The commit editor owns its viewport explicitly. Rendering a
                // fresh display-only clone from `body_scroll_top` avoids fighting
                // tui-textarea's private cursor-following scroll state and makes
                // Ctrl-J/K neoscroll deterministic in both directions.
                let viewport_top = commit_body_viewport_top(
                    body_textarea,
                    *body_scroll_top,
                    body_height as usize,
                );
                let mut body_view = commit_body_viewport(body_textarea, viewport_top);
                if let super::popup::CommitPendingCommand::FindForward(_) = pending_command {
                    super::popup::apply_eyeliner_highlights(
                        &mut body_view,
                        true,
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::Rgb(255, 165, 0)),
                    );
                } else if let super::popup::CommitPendingCommand::FindBackward(_) = pending_command {
                    super::popup::apply_eyeliner_highlights(
                        &mut body_view,
                        false,
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::Rgb(255, 165, 0)),
                    );
                }
                frame.render_widget(&body_view, body_area);
                let body_for_render = &body_view;
                let body_cursor_position = body_view.rendered_cursor_position();

                if body_height > 0 && gutter_width > 0 {
                    let cursor_row = body_textarea.cursor().0;
                    let line_count = body_textarea.lines().len();
                    let number_width = gutter_width.saturating_sub(1) as usize;
                    let mut numbers = Vec::with_capacity(body_height as usize);
                    for visible_row in 0..body_height as usize {
                        let row = viewport_top + visible_row;
                        if row >= line_count {
                            numbers.push(Line::from(""));
                            continue;
                        }
                        let relative = row.abs_diff(cursor_row);
                        let style = if relative == 0 {
                            Style::default().fg(mode_color)
                        } else {
                            Style::default().fg(theme.text_dimmed)
                        };
                        let number = if relative == 0 { row + 1 } else { relative };
                        numbers.push(Line::from(Span::styled(
                            format!("{:>width$} ", number, width = number_width),
                            style,
                        )));
                    }
                    frame.render_widget(Paragraph::new(numbers), gutter_area);
                }

                // Match the selected-row treatment used by Files/Commits/Branches:
                // paint the entire active visual row with the theme's selection
                // background rather than bolding its text. `set_style` patches only
                // the background here, preserving text/selection/Eyeliner foregrounds.
                if *focus == CommitInputFocus::Body && !mode.is_visual() {
                    if let Some(position) = body_cursor_position {
                        if position.y >= body_area.y
                            && position.y < body_area.y.saturating_add(body_area.height)
                        {
                            frame.buffer_mut().set_style(
                                Rect::new(inner.x, position.y, inner.width, 1),
                                Style::default().bg(theme.selected_bg),
                            );
                        }
                    }
                }

                // Neovim Illuminate-style word-under-cursor background. Apply it
                // after the active-row background so matching words remain visible
                // on the selected line as well as elsewhere in the visible field.
                let eyeliner_pending = matches!(
                    pending_command,
                    super::popup::CommitPendingCommand::FindForward(_)
                        | super::popup::CommitPendingCommand::FindBackward(_)
                );
                if *mode == CommitInputMode::Normal && !eyeliner_pending {
                    // vim-illuminate is commonly themed as a subdued grey
                    // current-word background (for example Gruvbox Material's
                    // `grey background` mode). Derive ours from the active theme
                    // instead of reusing the orange diff-search highlight.
                    let illuminate_bg = mix_colors(theme.selected_bg, theme.text_dimmed, 72);
                    match focus {
                        CommitInputFocus::Summary => highlight_commit_word_occurrences(
                            frame,
                            summary_textarea,
                            summary_area,
                            illuminate_bg,
                        ),
                        CommitInputFocus::Body => highlight_commit_word_occurrences(
                            frame,
                            body_for_render,
                            body_area,
                            illuminate_bg,
                        ),
                    }
                }

                let footer_y = inner.y + inner.height.saturating_sub(1);
                let mode_label = format!(" {} ", mode.label());
                let mode_width = UnicodeWidthStr::width(mode_label.as_str()) as u16;
                let mode_width = mode_width.min(inner.width);
                let mode_rect = Rect::new(
                    inner.x + inner.width.saturating_sub(mode_width),
                    footer_y,
                    mode_width,
                    1,
                );

                frame.render_widget(
                    Block::default().style(Style::default().bg(mode_color)),
                    mode_rect,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        mode_label,
                        Style::default().fg(mode_text_color),
                    )))
                    .alignment(ratatui::layout::Alignment::Center),
                    mode_rect,
                );

                // Use Ratatui's real terminal cursor for the commit editor. The
                // render loop chooses a bar in INSERT and a block otherwise.
                // Calling set_cursor_position here also lets Ratatui own cursor
                // visibility, so every other screen remains cursor-hidden.
                let cursor_position = match focus {
                    CommitInputFocus::Summary => summary_textarea.rendered_cursor_position(),
                    CommitInputFocus::Body => body_cursor_position,
                };
                if let Some(position) = cursor_position {
                    frame.set_cursor_position((position.x, position.y));
                }

            } else {
                // Fallback: just render summary
                frame.render_widget(summary_textarea, inner);
            }
        }
        PopupState::PullDivergence {
            branch,
            upstream,
            ahead,
            behind,
            selected,
        } => {
            let Some(rect) = pull_divergence_popup_geometry(area) else {
                return;
            };
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .title(" Branches have diverged ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent));
            frame.render_widget(block, rect);

            let inner = rect.inner(ratatui::layout::Margin {
                horizontal: 2,
                vertical: 1,
            });
            let normal = Style::default().fg(theme.text);
            let dim = Style::default().fg(theme.text_dimmed);
            let key = Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD);
            let selected_style = Style::default()
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD);
            let branch_name = if branch.is_empty() { "HEAD" } else { branch.as_str() };

            let option = |index: usize, shortcut: &str, label: String| {
                let line = Line::from(vec![
                    Span::styled(format!(" {shortcut} "), key),
                    Span::styled(label, normal),
                ]);
                if *selected == index {
                    line.style(selected_style)
                } else {
                    line
                }
            };

            let lines = vec![
                Line::from(Span::styled(
                    format!("{branch_name} and {upstream} have diverged"),
                    normal.add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Local:   ", dim),
                    Span::styled(
                        format!("{ahead} commit{} ahead", if *ahead == 1 { "" } else { "s" }),
                        normal,
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Remote:  ", dim),
                    Span::styled(
                        format!("{behind} commit{} ahead", if *behind == 1 { "" } else { "s" }),
                        normal,
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "How should the histories be combined?",
                    normal.add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                option(0, "r", "Rebase local commits onto upstream".to_string()),
                Line::from(Span::styled(
                    format!(
                        "    Replays your {ahead} local commit{} after the {behind} upstream commit{}",
                        if *ahead == 1 { "" } else { "s" },
                        if *behind == 1 { "" } else { "s" }
                    ),
                    dim,
                )),
                Line::from(""),
                option(1, "m", "Merge upstream into local branch".to_string()),
                Line::from(Span::styled(
                    "    Keeps both histories and creates a merge commit",
                    dim,
                )),
                Line::from(""),
                option(2, "v", "View divergent commits".to_string()),
                Line::from(Span::styled(
                    format!("    Compare HEAD with {upstream} before choosing"),
                    dim,
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" R/M ", key),
                    Span::styled("choose rebase/merge and remember it for this repository", dim),
                ]),
                Line::from(vec![
                    Span::styled(" Esc ", key),
                    Span::styled("cancel", dim),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), inner);
        }
        PopupState::Menu {
            title,
            items,
            selected,
            loading_index,
        } => {
            let height = (items.len() as u16 + 2).min(area.height - 4);
            let my = (area.height.saturating_sub(height)) / 2;
            let popup_rect = Rect::new(x, my, popup_width, height);
            frame.render_widget(Clear, popup_rect);

            let block = Block::default()
                .title(format!(" {} ", title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent));

            let list_items: Vec<ListItem> = items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let disabled = item.action.is_none();
                    let is_loading = *loading_index == Some(i);

                    let label = if let Some(ref key) = item.key {
                        format!(" {} {}", key, item.label)
                    } else {
                        format!("   {}", item.label)
                    };

                    if disabled {
                        let text_style = Style::default()
                            .fg(theme.text_dimmed)
                            .add_modifier(Modifier::CROSSED_OUT);
                        ListItem::new(Line::from(Span::styled(label, text_style)))
                    } else {
                        let selected_style = if i == *selected {
                            Style::default()
                                .bg(theme.selected_bg)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        let line = if is_loading {
                            let spinner = SPINNER_CHARS[(spinner_frame / 8) % SPINNER_CHARS.len()];
                            Line::from(vec![
                                Span::styled(label, Style::default()),
                                Span::raw(" "),
                                Span::styled(
                                    format!("{}", spinner),
                                    Style::default().fg(theme.accent_secondary),
                                ),
                            ])
                        } else if !item.description.is_empty() {
                            Line::from(vec![
                                Span::styled(label, Style::default()),
                                Span::raw(" "),
                                Span::styled(
                                    &item.description,
                                    Style::default().fg(theme.accent_secondary),
                                ),
                            ])
                        } else {
                            Line::from(label)
                        };
                        ListItem::new(line).style(selected_style)
                    }
                })
                .collect();

            let list = List::new(list_items).block(block);
            frame.render_widget(list, popup_rect);

            // Show disabled item description only when the selected item is disabled
            if let Some(selected_item) = items.get(*selected) {
                if selected_item.action.is_none() && !selected_item.description.is_empty() {
                    let hint_y = popup_rect.y + popup_rect.height;
                    if hint_y < area.height {
                        let hint_text = format!("Disabled: {}", selected_item.description);
                        let hint_rect = Rect::new(popup_rect.x, hint_y, popup_rect.width, 1);
                        frame.render_widget(Clear, hint_rect);
                        frame.render_widget(
                            Paragraph::new(Span::styled(
                                hint_text,
                                Style::default().fg(theme.text_dimmed),
                            )),
                            hint_rect,
                        );
                    }
                }
            }
        }
        PopupState::Loading { title, message } => {
            render_loading_overlay(frame, area, spinner_frame, theme, title, message, None);
        }
        PopupState::Checklist {
            title,
            items,
            selected,
            search_textarea,
            free_entry_category,
            ..
        } => {
            let search = search_textarea.lines().join("");
            // Filter items by search query. Free-entry rows always remain
            // visible because they represent the current typed value.
            let visible: Vec<(usize, &super::popup::ChecklistItem)> = items
                .iter()
                .enumerate()
                .filter(|(_, it)| {
                    it.is_free_entry
                        || search.is_empty()
                        || it.label.to_lowercase().contains(&search.to_lowercase())
                })
                .collect();

            // Height: search bar (1) + blank (1) + items + blank (1) + hint (1) + borders (2)
            let content_lines = visible.len().max(1);
            let height = (content_lines as u16 + 6).min(area.height - 4).max(8);
            let cy = (area.height.saturating_sub(height)) / 2;
            let popup_rect = Rect::new(x, cy, popup_width, height);
            frame.render_widget(Clear, popup_rect);

            let block = Block::default()
                .title(format!(" {} ", title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent));
            frame.render_widget(block, popup_rect);

            let inner = popup_rect.inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });
            if inner.height < 3 {
                // Too small, skip
            } else {
                // Search bar row (TextArea for cursor + word/line edits)
                let search_area = Rect::new(inner.x, inner.y, inner.width, 1);
                frame.render_widget(search_textarea, search_area);

                // Separator line
                let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
                let sep = "─".repeat(inner.width as usize);
                frame.render_widget(
                    Paragraph::new(Span::styled(sep, Style::default().fg(theme.text_dimmed))),
                    sep_area,
                );

                // Checklist items
                let list_start = inner.y + 2;
                let list_height = inner.height.saturating_sub(3); // search + sep + hint
                let list_area = Rect::new(inner.x, list_start, inner.width, list_height);

                let list_items: Vec<ListItem> = visible
                    .iter()
                    .enumerate()
                    .map(|(vi, (_, item))| {
                        let check_sym = if item.checked { "◉" } else { "○" };
                        let check_color = if item.checked {
                            theme.accent
                        } else {
                            theme.text_dimmed
                        };
                        let is_selected = vi == *selected;
                        let category = if item.is_free_entry {
                            free_entry_category
                                .as_deref()
                                .map(|c| format!("  {c}"))
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };

                        let line = Line::from(vec![
                            Span::raw("  "),
                            Span::styled(check_sym, Style::default().fg(check_color)),
                            Span::raw("  "),
                            Span::styled(
                                format!("{}{}", item.label, category),
                                if is_selected {
                                    Style::default()
                                        .fg(theme.text_strong)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(theme.text_strong)
                                },
                            ),
                        ]);

                        if is_selected {
                            ListItem::new(line).style(Style::default().bg(theme.selected_bg))
                        } else {
                            ListItem::new(line)
                        }
                    })
                    .collect();

                let list = List::new(list_items);
                frame.render_widget(list, list_area);

                // Hint at bottom
                let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
                let any_checked = items.iter().any(|it| it.checked);
                let mut hint_spans = vec![
                    Span::styled(" space", Style::default().fg(theme.accent_secondary)),
                    Span::styled(": toggle  ", Style::default().fg(theme.text_dimmed)),
                ];
                if any_checked {
                    hint_spans.push(Span::styled(
                        "ctrl-a",
                        Style::default().fg(theme.accent_secondary),
                    ));
                    hint_spans.push(Span::styled(
                        ": clear  ",
                        Style::default().fg(theme.text_dimmed),
                    ));
                }
                hint_spans.push(Span::styled(
                    "enter",
                    Style::default().fg(theme.accent_secondary),
                ));
                hint_spans.push(Span::styled(
                    ": apply  ",
                    Style::default().fg(theme.text_dimmed),
                ));
                hint_spans.push(Span::styled(
                    "esc",
                    Style::default().fg(theme.accent_secondary),
                ));
                hint_spans.push(Span::styled(
                    ": cancel",
                    Style::default().fg(theme.text_dimmed),
                ));
                let hint = Line::from(hint_spans);
                frame.render_widget(Paragraph::new(hint), hint_area);
            }
        }
        PopupState::CommandPalette {
            sections,
            selected,
            search_textarea,
            scroll_offset,
        } => {
            // Collect all visible entries (filtered by search) as flat list with section headers
            let search = search_textarea.lines().join("");
            let tokens = super::popup::list_picker_search_tokens(&search);
            let has_search = !tokens.is_empty();

            // Build flat display list: (is_header, key, description, executable)
            let mut display: Vec<(bool, String, String, bool)> = Vec::new();
            for section in sections {
                let visible_entries: Vec<&super::popup::CommandEntry> = if has_search {
                    section
                        .entries
                        .iter()
                        .filter(|e| {
                            super::popup::command_palette_entry_matches(
                                &e.key,
                                &e.description,
                                &tokens,
                            )
                        })
                        .collect()
                } else {
                    section.entries.iter().collect()
                };

                if !visible_entries.is_empty() {
                    display.push((true, section.title.clone(), String::new(), false));
                    for entry in visible_entries {
                        let key = if entry.key.is_empty() && entry.is_executable() {
                            "▸".into()
                        } else {
                            entry.key.clone()
                        };
                        display.push((
                            false,
                            key,
                            entry.description.clone(),
                            entry.is_executable(),
                        ));
                    }
                }
            }

            // Sizing: use more of the screen for help
            let popup_width = (area.width * 70 / 100).min(72).max(36).min(area.width);
            let content_height = display.len().max(1);
            // search bar (1) + separator (1) + content + hint (1) + borders (2)
            let popup_height = (content_height as u16 + 5)
                .min(area.height.saturating_sub(4))
                .max(10);
            let x = (area.width.saturating_sub(popup_width)) / 2;
            let y = (area.height.saturating_sub(popup_height)) / 2;
            let popup_rect = Rect::new(x, y, popup_width, popup_height);
            frame.render_widget(Clear, popup_rect);

            let block = Block::default()
                .title(" Command Palette ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent));
            frame.render_widget(block, popup_rect);

            let inner = popup_rect.inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });
            if inner.height < 3 {
                return;
            }

            // Search bar row: " " prefix + textarea
            let prefix_width = 2u16; // "  "
            let prefix_rect = Rect::new(inner.x, inner.y, prefix_width, 1);
            let prefix_style = if search.is_empty() {
                Style::default().fg(theme.text_dimmed)
            } else {
                Style::default().fg(theme.accent_secondary)
            };
            frame.render_widget(
                Paragraph::new(Span::styled("  ", prefix_style)),
                prefix_rect,
            );
            let ta_width = inner.width.saturating_sub(prefix_width);
            let ta_rect = Rect::new(inner.x + prefix_width, inner.y, ta_width, 1);
            frame.render_widget(&*search_textarea, ta_rect);

            // Separator
            let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
            let sep = "─".repeat(inner.width as usize);
            frame.render_widget(
                Paragraph::new(Span::styled(sep, Style::default().fg(theme.text_dimmed))),
                sep_area,
            );

            // Content area
            let list_start = inner.y + 2;
            let list_height = inner.height.saturating_sub(3) as usize; // search + sep + hint
            let list_area = Rect::new(inner.x, list_start, inner.width, list_height as u16);

            // Use the stored scroll_offset, clamped to valid range
            let max_scroll = display.len().saturating_sub(list_height);
            let so = *scroll_offset;
            let effective_scroll = if so > max_scroll { max_scroll } else { so };

            let visible_display: Vec<&(bool, String, String, bool)> = display
                .iter()
                .skip(effective_scroll)
                .take(list_height)
                .collect();

            // Count non-header entries before scroll offset to track selection
            let mut entry_idx = 0usize;
            for (is_header, _, _, _) in display.iter().take(effective_scroll) {
                if !is_header {
                    entry_idx += 1;
                }
            }

            // Keep the description column aligned even when a context adds
            // longer Vim-style key labels (for example `h / l / backspace`).
            // The old fixed width of 14 let those entries spill into the
            // description column while shorter keys remained right-aligned.
            let max_key_width = display
                .iter()
                .filter(|(is_header, _, _, _)| !*is_header)
                .map(|(_, key, _, _)| UnicodeWidthStr::width(key.as_str()))
                .max()
                .unwrap_or(14);
            let key_col_width = max_key_width
                .max(14)
                .min((inner.width as usize).saturating_sub(12).max(14));

            let mut list_items: Vec<ListItem> = Vec::new();
            for (is_header, key_or_title, desc, executable) in visible_display {
                if *is_header {
                    let line = Line::from(vec![Span::styled(
                        format!(" {} ", key_or_title),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    )]);
                    list_items.push(ListItem::new(line));
                } else {
                    let is_selected = entry_idx == *selected;
                    entry_idx += 1;

                    let key_display = format!("  {:>width$}", key_or_title, width = key_col_width);
                    let desc_display = format!("  {}", desc);

                    let key_base_style = if is_selected {
                        Style::default()
                            .fg(theme.accent_secondary)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        // Keybinding hints stay consistently blue even when an
                        // entry is reference-only (for example `dw` or `vi{delim}`).
                        // Availability is still communicated by the description.
                        Style::default().fg(theme.accent)
                    };

                    let desc_base_style = if is_selected {
                        Style::default()
                            .fg(theme.text_strong)
                            .add_modifier(Modifier::BOLD)
                    } else if !executable {
                        Style::default().fg(theme.text_dimmed)
                    } else {
                        Style::default().fg(theme.text)
                    };

                    let highlight_style = Style::default()
                        .fg(theme.accent_secondary)
                        .add_modifier(Modifier::BOLD);

                    let build_spans = |text: &str, base: Style| -> Vec<Span<'static>> {
                        if !has_search {
                            return vec![Span::styled(text.to_string(), base)];
                        }
                        // Highlight every query token (order-free), mirroring
                        // the list-picker highlight.
                        let ranges = super::popup::list_picker_highlight_ranges(text, &tokens);
                        if ranges.is_empty() {
                            return vec![Span::styled(text.to_string(), base)];
                        }
                        let mut s = Vec::new();
                        let mut cursor = 0usize;
                        for (start, end) in ranges {
                            if start > cursor {
                                if let Some(chunk) = text.get(cursor..start) {
                                    if !chunk.is_empty() {
                                        s.push(Span::styled(chunk.to_string(), base));
                                    }
                                }
                            }
                            if let Some(chunk) = text.get(start..end) {
                                s.push(Span::styled(chunk.to_string(), highlight_style));
                            }
                            cursor = end;
                        }
                        if let Some(rest) = text.get(cursor..) {
                            if !rest.is_empty() {
                                s.push(Span::styled(rest.to_string(), base));
                            }
                        }
                        s
                    };

                    let mut spans = build_spans(&key_display, key_base_style);
                    spans.extend(build_spans(&desc_display, desc_base_style));
                    let line = Line::from(spans);

                    if is_selected {
                        list_items.push(
                            ListItem::new(line).style(Style::default().bg(theme.selected_bg)),
                        );
                    } else {
                        list_items.push(ListItem::new(line));
                    }
                }
            }

            let list = List::new(list_items);
            frame.render_widget(list, list_area);

            // Hint bar at bottom
            let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
            let hint = Line::from(vec![
                Span::styled(" ↑↓", Style::default().fg(theme.accent_secondary)),
                Span::styled(": navigate  ", Style::default().fg(theme.text_dimmed)),
                Span::styled("type", Style::default().fg(theme.accent_secondary)),
                Span::styled(": search  ", Style::default().fg(theme.text_dimmed)),
                Span::styled("enter", Style::default().fg(theme.accent_secondary)),
                Span::styled(": execute  ", Style::default().fg(theme.text_dimmed)),
                Span::styled("esc", Style::default().fg(theme.accent_secondary)),
                Span::styled(": close", Style::default().fg(theme.text_dimmed)),
            ]);
            frame.render_widget(Paragraph::new(hint), hint_area);
        }
        PopupState::RefPicker { title, core, .. } => {
            render_list_picker(
                frame,
                area,
                theme,
                core,
                title,
                70,
                72,
                36,
                &[
                    ("↑↓", "navigate"),
                    ("type", "jump to"),
                    ("enter", "select"),
                    ("esc", "cancel"),
                ],
            );
        }
        PopupState::ListPicker { title, core, .. } => {
            render_list_picker(
                frame,
                area,
                theme,
                core,
                title,
                70,
                72,
                36,
                &[
                    ("↑↓", "navigate"),
                    ("type", "filter / free entry"),
                    ("enter", "select"),
                    ("esc", "cancel"),
                ],
            );
        }
        PopupState::PatchFilePicker { state, .. } => {
            render_patch_file_picker(frame, area, theme, state);
        }
        PopupState::ThemePicker { core, .. } => {
            render_list_picker(
                frame,
                area,
                theme,
                core,
                "Color Theme",
                65,
                70,
                36,
                &[
                    ("↑↓", "preview"),
                    ("type", "filter"),
                    ("enter", "apply"),
                    ("esc", "cancel"),
                ],
            );
        }
        PopupState::None => {}
    }

    // Overlay the ✦ AI-generate button (and tooltip when hovered) inside the
    // commit-message popup, on the Summary label row.
    if let Some(btn_rect) = commit_ai_button_geometry(popup, area) {
        let buf = frame.buffer_mut();
        // No bg fill — ✦ has off-center bearings in most fonts, so any colored
        // block exposes the asymmetry. Color/weight change is the hover feedback.
        let glyph_style = if ai_configured {
            if ai_button_hovered {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.accent_secondary)
                    .add_modifier(Modifier::BOLD)
            }
        } else if ai_button_hovered {
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dimmed)
        };
        let glyph_col = btn_rect.x + 1;
        if let Some(cell) = buf.cell_mut((glyph_col, btn_rect.y)) {
            cell.set_char('\u{F0674}'); // Nerd Font: nf-md-creation (sparkle)
            cell.set_style(glyph_style);
        }

        if ai_button_hovered {
            let tip_style = Style::default().bg(theme.selected_bg).fg(theme.text_strong);
            let key_style = Style::default()
                .bg(theme.selected_bg)
                .fg(theme.accent_secondary)
                .add_modifier(Modifier::BOLD);
            let dim_style = Style::default().bg(theme.selected_bg).fg(theme.text_dimmed);

            let parts: Vec<(&str, Style)> = if ai_configured {
                vec![
                    (" ", tip_style),
                    ("c-g", key_style),
                    (" Generate w/ AI ", tip_style),
                ]
            } else {
                vec![
                    (" \u{F0674} Generate w/ AI ", tip_style),
                    ("(needs setup)", dim_style),
                    (" ", tip_style),
                ]
            };
            let tip_w: u16 = parts.iter().map(|(s, _)| s.chars().count() as u16).sum();
            let tip_rect = commit_ai_tooltip_rect(area, btn_rect, tip_w);

            let mut col = tip_rect.x;
            for (text, style) in &parts {
                for ch in text.chars() {
                    if col >= tip_rect.x + tip_rect.width {
                        break;
                    }
                    if col >= buf.area.x + buf.area.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((col, tip_rect.y)) {
                        cell.set_char(ch);
                        cell.set_style(*style);
                    }
                    col += 1;
                }
            }
        }
    }
}
