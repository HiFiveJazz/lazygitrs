use std::collections::HashSet;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::Theme;
use crate::model::Model;
use crate::model::file_tree::FileTreeNode;

/// Render file list as a flat list (no tree structure).
///
/// Filename is shown first in the strong style, followed by the directory
/// path in a dimmed style — Zed-style.
pub fn render_file_list<'a>(model: &Model, theme: &Theme, width: usize) -> Vec<ListItem<'a>> {
    model
        .files
        .iter()
        .map(|file| {
            let name_style = file_name_style(file, theme);
            let dim_style = Style::default().fg(theme.text_dimmed);

            if file.rename_paths().is_some() {
                let mut spans = vec![Span::raw(" ")];
                spans.extend(file_status_spans(file, theme));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(file.display_name.clone(), name_style));
                return ListItem::new(Line::from(append_file_stats(
                    spans,
                    file.hunk_count,
                    file.additions,
                    file.deletions,
                    theme,
                    width,
                )));
            }

            let path = file.display_name.as_str();
            let (dir, name) = match path.rfind('/') {
                Some(idx) => (&path[..=idx], &path[idx + 1..]),
                None => ("", path),
            };

            let mut spans = vec![Span::raw(" ")];
            spans.extend(file_status_spans(file, theme));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(name.to_string(), name_style));
            if !dir.is_empty() {
                spans.push(Span::styled(format!(" {}", dir), dim_style));
            }

            ListItem::new(Line::from(append_file_stats(
                spans,
                file.hunk_count,
                file.additions,
                file.deletions,
                theme,
                width,
            )))
        })
        .collect()
}

pub(crate) fn append_file_stats<'a>(
    spans: Vec<Span<'a>>,
    hunk_count: usize,
    additions: usize,
    deletions: usize,
    theme: &Theme,
    width: usize,
) -> Vec<Span<'a>> {
    let mut stats = Vec::new();
    if hunk_count > 0 {
        stats.push(Span::styled(
            format!("*{}", hunk_count),
            Style::default().fg(theme.accent_secondary),
        ));
    }
    if additions > 0 {
        stats.push(Span::styled(format!(" +{}", additions), theme.diff_add));
    }
    if deletions > 0 {
        stats.push(Span::styled(format!(" -{}", deletions), theme.diff_remove));
    }
    if stats.is_empty() {
        return spans;
    }

    let stats_width: usize = stats.iter().map(|span| span.content.width()).sum();
    let content_width = width.saturating_sub(stats_width + 1);
    let mut fitted = truncate_spans(spans, content_width);
    let fitted_width: usize = fitted.iter().map(|span| span.content.width()).sum();
    fitted.push(Span::raw(
        " ".repeat(width.saturating_sub(fitted_width + stats_width)),
    ));
    fitted.extend(stats);
    fitted
}

fn truncate_spans<'a>(spans: Vec<Span<'a>>, max_width: usize) -> Vec<Span<'a>> {
    let mut remaining = max_width;
    let mut fitted = Vec::new();

    for span in spans {
        if remaining == 0 {
            break;
        }
        let span_width = span.content.width();
        if span_width <= remaining {
            remaining -= span_width;
            fitted.push(span);
            continue;
        }

        let mut text = String::new();
        for ch in span.content.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if ch_width > remaining {
                break;
            }
            text.push(ch);
            remaining -= ch_width;
        }
        fitted.push(Span::styled(text, span.style));
        break;
    }

    fitted
}

/// Render the cached file tree nodes into list items.
pub fn render_file_tree<'a>(
    model: &Model,
    theme: &Theme,
    nodes: &[FileTreeNode],
    collapsed_dirs: &HashSet<String>,
    width: usize,
) -> Vec<ListItem<'a>> {
    nodes
        .iter()
        .filter_map(|node| {
            let indent = "  ".repeat(node.depth);

            if node.is_dir {
                let is_collapsed = collapsed_dirs.contains(&node.path);
                let icon = if is_collapsed { "▶ " } else { "▼ " };

                // Directory is green if ALL child files are fully staged
                let all_staged = !node.child_file_indices.is_empty()
                    && node.child_file_indices.iter().all(|&idx| {
                        model
                            .files
                            .get(idx)
                            .is_some_and(|f| f.has_staged_changes && !f.has_unstaged_changes)
                    });

                let dir_style = if all_staged {
                    theme.file_staged
                } else {
                    Style::default().fg(theme.text_dimmed)
                };

                let is_root = node.path == ".";
                let line = if is_root {
                    Line::from(Span::styled(format!(" {} /", icon.trim_end()), dir_style))
                } else {
                    Line::from(vec![
                        Span::styled(format!(" {}{}", indent, icon), dir_style),
                        Span::styled(node.name.clone(), dir_style),
                    ])
                };
                Some(ListItem::new(line))
            } else if let Some(file_idx) = node.file_index {
                // The file list and cached tree are normally rebuilt together.
                // Async model refreshes can briefly make a cached node stale,
                // though, so never index directly into `model.files` here.
                let file = model.files.get(file_idx)?;
                let name_style = file_name_style(file, theme);

                let mut spans = vec![Span::raw(format!(" {}", indent))];
                spans.extend(file_status_spans(file, theme));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(node.name.clone(), name_style));
                let line = Line::from(append_file_stats(
                    spans,
                    file.hunk_count,
                    file.additions,
                    file.deletions,
                    theme,
                    width,
                ));
                Some(ListItem::new(line))
            } else {
                Some(ListItem::new(Line::raw("")))
            }
        })
        .collect()
}

/// File name color: green when fully staged, white otherwise.
fn file_name_style(file: &crate::model::File, theme: &Theme) -> Style {
    if file.has_staged_changes && !file.has_unstaged_changes {
        theme.file_staged
    } else {
        Style::default().fg(theme.text_strong)
    }
}

/// Per-char status colors: XY where X = index (staged) and Y = worktree
/// (unstaged). Fully-staged (`M `) is green, unstaged (` M`) is yellow, and
/// partially-staged (`MM`, `AM`, …) splits — left char green, right yellow.
fn file_status_spans(file: &crate::model::File, theme: &Theme) -> Vec<Span<'static>> {
    let status_style = if file.has_merge_conflicts {
        theme.file_conflicted
    } else if file.has_staged_changes && !file.has_unstaged_changes {
        theme.file_staged
    } else if !file.tracked {
        theme.file_untracked
    } else {
        theme.file_unstaged
    };

    let status_icon: &str = match file.short_status.as_str() {
        "??" => "??",
        "M " => "M ",
        " M" => " M",
        "MM" => "MM",
        "A " => "A ",
        "AM" => "AM",
        "D " => "D ",
        " D" => " D",
        "R " => "R ",
        "RM" => "RM",
        "C " => "C ",
        "CM" => "CM",
        "UU" => "UU",
        "AA" => "AA",
        "DD" => "DD",
        "AU" => "AU",
        "UA" => "UA",
        "DU" => "DU",
        "UD" => "UD",
        _ => "  ",
    };

    let mut chars = status_icon.chars();
    let first = chars.next().unwrap_or(' ').to_string();
    let second = chars.next().unwrap_or(' ').to_string();
    if file.has_staged_changes && file.has_unstaged_changes {
        vec![
            Span::styled(first, theme.file_staged),
            Span::styled(second, theme.file_unstaged),
        ]
    } else {
        vec![
            Span::styled(first, status_style),
            Span::styled(second, status_style),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{File, FileStatus};

    #[test]
    fn stale_file_tree_node_is_ignored_instead_of_panicking() {
        let mut model = Model::default();
        model.files.push(File {
            name: "file.txt".into(),
            display_name: "file.txt".into(),
            status: FileStatus::Modified,
            has_staged_changes: true,
            has_unstaged_changes: false,
            tracked: true,
            added: false,
            deleted: false,
            has_merge_conflicts: false,
            short_status: "M ".into(),
            hunk_count: 1,
            additions: 1,
            deletions: 0,
        });
        let nodes = crate::model::file_tree::build_file_tree(&model.files, &HashSet::new());

        // Simulate an async model refresh arriving one frame before the cached
        // file tree is rebuilt. This used to index model.files[0] and panic.
        model.files.clear();

        let rendered = render_file_tree(&model, &Theme::default(), &nodes, &HashSet::new(), 80);
        assert!(
            rendered.is_empty(),
            "the stale file node is skipped instead of indexing an empty file list"
        );
    }

    #[test]
    fn file_stats_are_right_aligned_and_preserved_when_name_is_truncated() {
        let file = File {
            name: "very-long-file-name.rs".into(),
            display_name: "very-long-file-name.rs".into(),
            status: FileStatus::Modified,
            has_staged_changes: false,
            has_unstaged_changes: true,
            tracked: true,
            added: false,
            deleted: false,
            has_merge_conflicts: false,
            short_status: " M".into(),
            hunk_count: 2,
            additions: 143,
            deletions: 71,
        };
        let theme = Theme::default();
        let width = 24;

        let spans = append_file_stats(
            vec![Span::raw(" M very-long-file-name.rs")],
            file.hunk_count,
            file.additions,
            file.deletions,
            &theme,
            width,
        );
        let rendered: String = spans.iter().map(|span| span.content.as_ref()).collect();

        assert_eq!(rendered.width(), width);
        assert!(rendered.ends_with("*2 +143 -71"));
    }
}
