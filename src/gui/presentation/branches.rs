use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

use crate::config::Theme;
use crate::model::Model;

const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn render_branch_list<'a>(
    model: &Model,
    theme: &Theme,
    remote_op_label: Option<&str>,
    spinner_frame: usize,
    remote_op_success: bool,
) -> Vec<ListItem<'a>> {
    model
        .branches
        .iter()
        .map(|branch| {
            // `head_branch_name` is updated at checkout. A concurrent branch
            // metadata refresh can still carry the previous HEAD marker.
            let is_head = !model.head_branch_name.is_empty()
                && branch.name == model.head_branch_name;
            let name_style = if is_head {
                theme.branch_head
            } else {
                theme.branch_local
            };

            let head_marker = if is_head { "* " } else { "  " };

            let mut spans = vec![
                Span::styled(head_marker.to_string(), name_style),
                Span::styled(branch.name.clone(), name_style),
            ];

            if let Some(worktree) = model
                .worktrees
                .iter()
                .find(|worktree| !worktree.is_current && worktree.branch == branch.name)
            {
                spans.push(Span::styled(
                    format!(" ↪ {}", worktree.path),
                    Style::default().fg(theme.text_dimmed),
                ));
            }

            // Recency
            if !branch.recency.is_empty() {
                spans.insert(
                    0,
                    Span::styled(
                        format!("{:>3} ", branch.recency),
                        Style::default().fg(theme.text_dimmed),
                    ),
                );
            }

            // Remote operation indicator on head branch (e.g. "Pushing ✻" or "✓" on success)
            if is_head {
                if let Some(label) = remote_op_label {
                    let spinner = SPINNER_CHARS[(spinner_frame / 8) % SPINNER_CHARS.len()];
                    spans.push(Span::styled(
                        format!(" {} {}", label, spinner),
                        Style::default().fg(theme.accent_secondary),
                    ));
                } else if remote_op_success {
                    spans.push(Span::styled(
                        " ✓".to_string(),
                        Style::default().fg(theme.accent),
                    ));
                }
            }

            // Ahead/behind indicator (skip when remote op is active on head branch)
            if !(is_head && remote_op_label.is_some()) {
                if let Some((ahead, behind)) = branch.ahead_behind() {
                    let indicator = match (ahead > 0, behind > 0) {
                        (true, true) => format!(" ↑{}↓{}", ahead, behind),
                        (true, false) => format!(" ↑{}", ahead),
                        (false, true) => format!(" ↓{}", behind),
                        _ => String::new(),
                    };
                    if !indicator.is_empty() {
                        spans.push(Span::styled(
                            indicator,
                            Style::default().fg(theme.accent_secondary),
                        ));
                    }
                }
            }

            ListItem::new(Line::from(spans))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Branch;

    #[test]
    fn branch_marker_follows_head_name_even_if_cached_flag_is_stale() {
        let make_branch = |name: &str, head: bool| Branch {
            name: name.into(),
            hash: String::new(),
            recency: String::new(),
            commit_time: 0,
            pushables: String::new(),
            pullables: String::new(),
            upstream: None,
            head,
        };
        let model = Model {
            head_branch_name: "main".into(),
            branches: vec![make_branch("main", false), make_branch("list/neoscroll", true)],
            ..Default::default()
        };
        let rows = render_branch_list(&model, &Theme::default(), None, 0, false);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 3)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::List::new(rows),
                    ratatui::layout::Rect::new(0, 0, 40, 3),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "*");
        assert_eq!(buffer.cell((0, 1)).unwrap().symbol(), " ");
    }
}
