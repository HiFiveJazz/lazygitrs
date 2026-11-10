use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use crossterm::event::KeyEvent;
use ratatui::style::Style;
use tui_textarea::{CursorMove, TextArea};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::Gui;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn delimiter_pair(delimiter: char) -> Option<(char, char)> {
    match delimiter {
        '"' | '\'' | '`' => Some((delimiter, delimiter)),
        '(' | ')' => Some(('(', ')')),
        '[' | ']' => Some(('[', ']')),
        '{' | '}' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        _ => None,
    }
}

fn inside_delimiter_range(text: &str, cursor: usize, delimiter: char) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let (open, close) = delimiter_pair(delimiter)?;

    if open == close {
        // Symmetric delimiters form adjacent pairs. Accept the insertion point
        // on either delimiter or immediately after the closing delimiter, but
        // do not accidentally bridge two separate quoted strings.
        let positions: Vec<usize> = chars
            .iter()
            .enumerate()
            .filter_map(|(idx, ch)| (*ch == open).then_some(idx))
            .collect();
        for pair in positions.chunks_exact(2) {
            let left = pair[0];
            let right = pair[1];
            if left <= cursor && cursor <= right + 1 {
                return Some((left + 1, right));
            }
        }
        return None;
    }

    // Paired delimiters can nest. Walk the whole line with a stack and keep
    // the tightest completed pair that surrounds the insertion-point cursor.
    let mut stack = Vec::new();
    let mut best: Option<(usize, usize)> = None;
    for (idx, ch) in chars.iter().copied().enumerate() {
        if ch == open {
            stack.push(idx);
        } else if ch == close {
            let Some(left) = stack.pop() else {
                continue;
            };
            if left <= cursor && cursor <= idx + 1 {
                let candidate = (left + 1, idx);
                if best
                    .map(|(start, end)| candidate.1 - candidate.0 < end - start)
                    .unwrap_or(true)
                {
                    best = Some(candidate);
                }
            }
        }
    }
    best
}

/// Synchronize a free-entry row and keep it selected. Used where the typed
/// value is valid on its own and suggestions are optional completions.
///
/// An empty `free_entry_category` opts out of the synthetic row (e.g. the
/// diff-grep dialog only confirms real matches): no row is inserted or
/// stripped — real items with empty categories are left alone — and the
/// selection is just kept on a matching item.
pub fn sync_list_picker_prefer_free_entry(core: &mut ListPickerCore, free_entry_category: &str) {
    sync_list_picker_free_entry(core, free_entry_category);
    if !free_entry_category.is_empty() && !core.search_textarea.lines().join("").trim().is_empty() {
        core.selected = 0;
    }
}

/// Normalize an externally-sourced commit body for the wrapped editor.
///
/// During editing, wrapping is visual (`BodySoftWrap` / `WrapLayout`) so
/// logical newlines from AI output, clipboard pastes, and history are preserved.
/// On submit, prose is hard-wrapped into the stored commit message.
/// Joining consecutive lines used to collapse bullet lists into one line
/// (`- a\n- b` → `- a - b`); we only normalize `\r\n` / trailing `\r`.
pub fn unwrap_commit_body(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Preferred dimensions for the two-field commit editor.  The inner width is
/// deliberately large enough to show a conventional 72-column commit body
/// without visually wrapping it a second time.
pub fn commit_popup_width(terminal_width: u16) -> u16 {
    terminal_width
        .saturating_sub(4)
        .min(84)
        .max(40)
        .min(terminal_width)
}

pub fn commit_popup_height(terminal_height: u16) -> u16 {
    terminal_height
        .saturating_sub(4)
        .min(28)
        .max(16)
        .min(terminal_height)
}

/// Width reserved for Vim-style relative line numbers in the commit body.
pub const COMMIT_BODY_GUTTER_WIDTH: u16 = 4;

/// Effective text width of the commit description after popup chrome and the
/// relative-number gutter are accounted for.
pub fn commit_body_text_width(terminal_width: u16, configured_width: usize) -> usize {
    let inner = commit_popup_width(terminal_width)
        .saturating_sub(4)
        .saturating_sub(COMMIT_BODY_GUTTER_WIDTH) as usize;
    if configured_width > 0 {
        inner.min(configured_width)
    } else {
        inner
    }
    .max(1)
}

fn looks_like_commit_trailer(line: &str) -> bool {
    let Some((token, _)) = line.split_once(':') else {
        return false;
    };
    !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn wrap_commit_line(line: &str, width: usize) -> Vec<String> {
    if width == 0
        || line.is_empty()
        || line.chars().next().is_some_and(char::is_whitespace)
        || line.starts_with('>')
        || line.starts_with('|')
        || line == "---"
        || looks_like_commit_trailer(line)
        || UnicodeWidthStr::width(line) <= width
    {
        return vec![line.to_string()];
    }

    let (first_prefix, continuation_prefix, content) = if let Some(rest) = line.strip_prefix("- ") {
        ("- ", "  ", rest)
    } else if let Some(rest) = line.strip_prefix("* ") {
        ("* ", "  ", rest)
    } else if let Some(rest) = line.strip_prefix("+ ") {
        ("+ ", "  ", rest)
    } else {
        let numbered = line
            .find(". ")
            .filter(|dot| *dot > 0 && line[..*dot].chars().all(|c| c.is_ascii_digit()));
        if let Some(dot) = numbered {
            let prefix = &line[..dot + 2];
            let rest = &line[dot + 2..];
            // The continuation indent only has to line up with the prose, so
            // leaking this tiny string is unnecessary; build it below.
            return wrap_commit_words(
                prefix,
                &" ".repeat(UnicodeWidthStr::width(prefix)),
                rest,
                width,
            );
        }
        ("", "", line)
    };

    wrap_commit_words(first_prefix, continuation_prefix, content, width)
}

fn wrap_commit_words(
    prefix: &str,
    continuation_prefix: &str,
    content: &str,
    width: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = prefix.to_string();
    let mut has_word = false;

    for word in content.split_whitespace() {
        let separator = if has_word { " " } else { "" };
        let candidate_width = UnicodeWidthStr::width(current.as_str())
            + UnicodeWidthStr::width(separator)
            + UnicodeWidthStr::width(word);

        if has_word && candidate_width > width {
            out.push(current);
            current = continuation_prefix.to_string();
            current.push_str(word);
        } else {
            current.push_str(separator);
            current.push_str(word);
        }
        has_word = true;
    }

    if has_word || !prefix.is_empty() {
        out.push(current);
    } else {
        out.push(String::new());
    }
    out
}

/// Turn the commit-body editor's visual wrapping into real commit-message
/// line breaks.  Existing explicit newlines remain hard boundaries; prose on
/// each logical line is wrapped independently.  Machine-readable trailers and
/// indented/preformatted lines are deliberately left alone.
pub fn hard_wrap_commit_body(text: &str, width: usize) -> String {
    unwrap_commit_body(text)
        .split('\n')
        .flat_map(|line| wrap_commit_line(line, width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Source-of-truth for the commit body while editing. The body textarea is a
/// soft-wrapped *display* of this raw text; user-pressed newlines remain hard
/// boundaries and the final submit step converts visual prose wrapping into
/// real commit-message line breaks.
///
/// Cursor is a char index into `raw` (not bytes) so multi-byte input is safe.
#[derive(Debug, Default, Clone)]
pub struct BodySoftWrap {
    pub raw: String,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
struct WrapLine {
    text: String,
    /// Char index in raw where this visual line starts.
    raw_start: usize,
    /// Number of raw chars covered by this line (excluding any space/newline
    /// consumed by the wrap break that follows).
    char_len: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct WrapLayout {
    lines: Vec<WrapLine>,
}

impl WrapLayout {
    fn build(raw: &str, wrap_width: usize) -> Self {
        let mut lines = Vec::new();
        let mut para_start = 0usize;
        let paragraphs: Vec<&str> = raw.split('\n').collect();
        let total_paragraphs = paragraphs.len();
        let wrap_width = wrap_width.max(1);
        for (p_idx, para) in paragraphs.iter().enumerate() {
            let chars: Vec<char> = para.chars().collect();
            if chars.is_empty() {
                lines.push(WrapLine {
                    text: String::new(),
                    raw_start: para_start,
                    char_len: 0,
                });
            } else {
                let mut start = 0usize;
                while start < chars.len() {
                    // Grow by display width (crabcode-style), not char count,
                    // so wide glyphs don't force horizontal scroll.
                    let mut end = start;
                    let mut width = 0usize;
                    let mut last_space: Option<usize> = None;
                    while end < chars.len() {
                        let ch = chars[end];
                        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if end > start && width + ch_width > wrap_width {
                            break;
                        }
                        if ch == ' ' {
                            last_space = Some(end);
                        }
                        width += ch_width;
                        end += 1;
                        if width >= wrap_width {
                            break;
                        }
                    }
                    if end == start {
                        // Pathological zero-width / over-wide glyph: advance one char.
                        end = (start + 1).min(chars.len());
                    }

                    let at_end = end >= chars.len();
                    let (line_end, consumed) = if at_end {
                        (end, 0)
                    } else {
                        match last_space {
                            Some(i) if i > start => (i, 1),
                            _ => (end, 0),
                        }
                    };
                    let text: String = chars[start..line_end].iter().collect();
                    let len = line_end - start;
                    lines.push(WrapLine {
                        text,
                        raw_start: para_start + start,
                        char_len: len,
                    });
                    start = line_end + consumed;
                }
            }
            // Advance past this paragraph's chars + the \n separator (except after the last).
            para_start += chars.len();
            if p_idx + 1 < total_paragraphs {
                para_start += 1;
            }
        }
        if lines.is_empty() {
            lines.push(WrapLine {
                text: String::new(),
                raw_start: 0,
                char_len: 0,
            });
        }
        WrapLayout { lines }
    }

    fn cursor_to_visual(&self, cursor: usize) -> (usize, usize) {
        for (i, line) in self.lines.iter().enumerate() {
            let line_end = line.raw_start + line.char_len;
            // Cursor falls inside this line (raw_start..=line_end). The
            // end-of-line position belongs to THIS line, not the next — that
            // way `move_visual_up` can land here and stay (otherwise it would
            // bounce forward to the next row, getting stuck).
            if cursor >= line.raw_start && cursor <= line_end {
                return (i, cursor - line.raw_start);
            }
            // Cursor is in the gap between this line's end and the next line's
            // start (a space or \n consumed by the wrap). Snap to start of next.
            if i + 1 < self.lines.len() && cursor < self.lines[i + 1].raw_start {
                return (i + 1, 0);
            }
        }
        let last = self.lines.len() - 1;
        let line = &self.lines[last];
        (
            last,
            cursor.saturating_sub(line.raw_start).min(line.char_len),
        )
    }

    fn visual_to_cursor(&self, row: usize, col: usize) -> usize {
        let line = self
            .lines
            .get(row)
            .or_else(|| self.lines.last())
            .expect("wrap layout always has at least one line");
        line.raw_start + col.min(line.char_len)
    }

    #[cfg(test)]
    pub fn as_textarea_text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

fn parse_command_key(key: &str) -> Option<KeyEvent> {
    let normalized = match key {
        "Enter" => "<enter>",
        "Tab" => "<tab>",
        "esc" | "Esc" => "<esc>",
        "Alt+↑" => "<a-k>",
        "Alt+↓" => "<a-j>",
        _ => key,
    };
    crate::config::keybindings::parse_key(normalized)
}

impl BodySoftWrap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: impl Into<String>) -> Self {
        let raw = text.into();
        let cursor = raw.chars().count();
        Self { raw, cursor }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    fn cursor_byte(&self) -> usize {
        self.raw
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len())
    }

    fn char_count(&self) -> usize {
        self.raw.chars().count()
    }

    pub fn cursor_index(&self) -> usize {
        self.cursor
    }

    pub fn set_cursor_index(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.char_count());
    }

    fn replace_char_range(&mut self, start: usize, end: usize, replacement: &str) {
        let start = start.min(self.char_count());
        let end = end.min(self.char_count()).max(start);
        let start_byte = self
            .raw
            .char_indices()
            .nth(start)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        let end_byte = self
            .raw
            .char_indices()
            .nth(end)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        self.raw.replace_range(start_byte..end_byte, replacement);
        self.cursor = start + replacement.chars().count();
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        let raw = text.into();
        self.cursor = raw.chars().count();
        self.raw = raw;
    }

    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move to the first character of the final logical line, matching Vim's
    /// `G` motion. A single trailing newline terminates the final line rather
    /// than creating a phantom extra line; additional newlines still represent
    /// real blank logical lines.
    pub fn move_to_last_logical_line_start(&mut self) {
        let chars: Vec<char> = self.raw.chars().collect();
        if chars.is_empty() {
            self.cursor = 0;
            return;
        }
        let probe = if chars.last() == Some(&'\n') {
            chars.len().saturating_sub(1)
        } else {
            chars.len()
        };
        self.cursor = self.logical_line_bounds_at(probe).0;
    }

    /// Append a commit trailer at the bottom while keeping the conventional
    /// blank line between prose and the trailer block. Returns false when the
    /// exact trailer is already present.
    pub fn append_trailer(&mut self, trailer: &str) -> bool {
        let trailer = trailer.trim();
        if trailer.is_empty() || self.raw.lines().any(|line| line.trim() == trailer) {
            return false;
        }

        let trimmed = self.raw.trim_end().to_string();
        let separator = if trimmed.is_empty() {
            ""
        } else if trimmed
            .lines()
            .next_back()
            .is_some_and(|line| looks_like_commit_trailer(line.trim()))
        {
            "\n"
        } else {
            "\n\n"
        };
        self.raw = format!("{trimmed}{separator}{trailer}");
        self.cursor = self.raw.chars().count();
        true
    }

    /// Remove an exact commit trailer line and tidy the separator around the
    /// remaining trailer block. Returns false when the trailer is not present.
    pub fn remove_trailer(&mut self, trailer: &str) -> bool {
        let trailer = trailer.trim();
        if trailer.is_empty() {
            return false;
        }

        let mut lines: Vec<&str> = self.raw.lines().collect();
        let Some(index) = lines.iter().position(|line| line.trim() == trailer) else {
            return false;
        };
        lines.remove(index);

        // Removing the first trailer from a trailer block can leave the blank
        // separator directly before another trailer, which is correct. Removing
        // the last/only trailer can leave trailing blank lines, which are not.
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }

        self.raw = lines.join("\n");
        self.cursor = self.cursor.min(self.raw.chars().count());
        true
    }

    pub fn insert_char(&mut self, c: char) {
        let b = self.cursor_byte();
        self.raw.insert(b, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let b = self.cursor_byte();
        self.raw.insert_str(b, s);
        self.cursor += s.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self
            .raw
            .char_indices()
            .nth(self.cursor - 1)
            .map(|(b, _)| b)
            .unwrap();
        self.raw.remove(prev);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let b = self.cursor_byte();
        self.raw.remove(b);
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    /// Vim `a`: move one character to the right only when that stays on the
    /// current visual line. At an end-of-line insertion point, remain there
    /// instead of crossing the newline/soft-wrap boundary.
    pub fn move_append(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        let line = &layout.lines[row];
        let line_end = line.raw_start + line.char_len;
        if self.cursor < line_end {
            self.cursor += 1;
        }
    }

    /// Vim-style `dw`: delete from the cursor through the current word and
    /// following horizontal whitespace, stopping before a hard newline.
    pub fn delete_word_forward(&mut self) -> bool {
        let chars: Vec<char> = self.raw.chars().collect();
        if self.cursor >= chars.len() || chars[self.cursor] == '\n' {
            return false;
        }
        let start = self.cursor;
        let mut end = start;

        if is_word_char(chars[end]) {
            while end < chars.len() && is_word_char(chars[end]) {
                end += 1;
            }
        } else {
            while end < chars.len()
                && chars[end] != '\n'
                && !is_word_char(chars[end])
                && !chars[end].is_whitespace()
            {
                end += 1;
            }
        }
        while end < chars.len() && chars[end] != '\n' && chars[end].is_whitespace() {
            end += 1;
        }
        if end == start {
            return false;
        }
        self.replace_char_range(start, end, "");
        true
    }

    /// Vim-style `dd`: delete the current *logical* line. Soft-wrap rows are
    /// display-only and therefore do not count as separate lines.
    pub fn delete_current_line(&mut self) -> bool {
        if self.raw.is_empty() {
            return false;
        }
        let chars: Vec<char> = self.raw.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && chars[start - 1] != '\n' {
            start -= 1;
        }
        let mut end = self.cursor.min(chars.len());
        while end < chars.len() && chars[end] != '\n' {
            end += 1;
        }

        if end < chars.len() {
            // Include the newline after this line.
            self.replace_char_range(start, end + 1, "");
            self.cursor = start.min(self.char_count());
        } else if start > 0 {
            // Last line: consume the preceding newline so we do not leave a
            // dangling blank logical line behind.
            self.replace_char_range(start - 1, end, "");
            self.cursor = (start - 1).min(self.char_count());
        } else {
            self.raw.clear();
            self.cursor = 0;
        }
        true
    }

    /// Vim `o`: create an empty logical line immediately below the current
    /// one and leave the cursor at the start of the new line.
    pub fn open_line_below(&mut self) {
        let chars: Vec<char> = self.raw.chars().collect();
        let mut end = self.cursor.min(chars.len());
        while end < chars.len() && chars[end] != '\n' {
            end += 1;
        }
        if end < chars.len() {
            self.replace_char_range(end + 1, end + 1, "\n");
            self.cursor = end + 1;
        } else {
            self.replace_char_range(end, end, "\n");
            self.cursor = end + 1;
        }
    }

    /// Move cursor to the start of the previous word (emacs/readline-style:
    /// skip preceding non-word chars, then skip word chars).
    pub fn move_word_left(&mut self) {
        let chars: Vec<char> = self.raw.chars().collect();
        let mut i = self.cursor;
        while i > 0 && !is_word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move cursor past the end of the next word for Insert-mode/readline
    /// bindings such as Alt+Right / Alt+F.
    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.raw.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && !is_word_char(chars[i]) {
            i += 1;
        }
        while i < n && is_word_char(chars[i]) {
            i += 1;
        }
        self.cursor = i;
    }

    /// Vim-style `w`: move to the first character of the next word/token,
    /// never to the whitespace after the current word.
    pub fn move_word_right_vim(&mut self) {
        let chars: Vec<char> = self.raw.chars().collect();
        let n = chars.len();
        if n == 0 {
            return;
        }
        if self.cursor >= n {
            self.cursor = n - 1;
            return;
        }

        let mut i = self.cursor;
        let class = |c: char| {
            if is_word_char(c) {
                0u8
            } else if c.is_whitespace() {
                1u8
            } else {
                2u8
            }
        };
        let current_class = class(chars[i]);
        if current_class != 1 {
            while i < n && class(chars[i]) == current_class {
                i += 1;
            }
        }
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i < n {
            self.cursor = i;
        } else if let Some(last) = chars.iter().rposition(|c| !c.is_whitespace()) {
            // No next token: behave like Neovim at the end of the buffer and
            // settle on the final real character of the final word/token.
            self.cursor = last;
        }
    }

    /// Delete from cursor back to the start of the previous word.
    pub fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        let start = self.cursor;
        if start == end {
            return;
        }
        let start_byte = self
            .raw
            .char_indices()
            .nth(start)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        let end_byte = self
            .raw
            .char_indices()
            .nth(end)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        self.raw.replace_range(start_byte..end_byte, "");
    }

    /// Cmd+Left equivalent: jump to the start of the current visual row,
    /// respecting soft-wrap boundaries (not just paragraph boundaries).
    pub fn move_visual_line_start(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        self.cursor = layout.lines[row].raw_start;
    }

    /// Cmd+Right / Insert-mode `A`: jump to the insertion point at the end
    /// of the current visual row.
    pub fn move_visual_line_end(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        let line = &layout.lines[row];
        self.cursor = line.raw_start + line.char_len;
    }

    /// Vim Normal/Visual-mode end-of-line motion: land on the final character
    /// rather than the insertion cell after it.
    pub fn move_visual_line_last_char(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        let line = &layout.lines[row];
        self.cursor = if line.char_len == 0 {
            line.raw_start
        } else {
            line.raw_start + line.char_len - 1
        };
    }

    /// Leave Insert mode with the cursor on a character, as Vim does. At the
    /// start of a visual row there is no character to the left, so stay put.
    pub fn leave_insert_mode(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (_, col) = layout.cursor_to_visual(self.cursor);
        if col > 0 {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    /// Vim Normal-mode `l`: move right without stepping into the insertion
    /// cell after the last character of the visual row.
    pub fn move_visual_right_normal(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, col) = layout.cursor_to_visual(self.cursor);
        let line = &layout.lines[row];
        if line.char_len == 0 {
            self.cursor = line.raw_start;
        } else if col >= line.char_len {
            self.cursor = line.raw_start + line.char_len - 1;
        } else if col + 1 < line.char_len {
            self.cursor += 1;
        }
    }

    /// Ensure a Normal/Visual-mode cursor is sitting on a character rather
    /// than the insertion cell immediately after a visual row.
    pub fn normalize_normal_cursor(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, col) = layout.cursor_to_visual(self.cursor);
        let line = &layout.lines[row];
        if line.char_len > 0 && col >= line.char_len {
            self.cursor = line.raw_start + line.char_len - 1;
        }
    }

    pub fn move_visual_up_normal(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, col) = layout.cursor_to_visual(self.cursor);
        if row == 0 {
            self.normalize_normal_cursor(wrap_width);
            return;
        }
        let target = &layout.lines[row - 1];
        let target_col = if target.char_len == 0 {
            0
        } else {
            col.min(target.char_len - 1)
        };
        self.cursor = target.raw_start + target_col;
    }

    pub fn move_visual_down_normal(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, col) = layout.cursor_to_visual(self.cursor);
        if row + 1 >= layout.line_count() {
            self.normalize_normal_cursor(wrap_width);
            return;
        }
        let target = &layout.lines[row + 1];
        let target_col = if target.char_len == 0 {
            0
        } else {
            col.min(target.char_len - 1)
        };
        self.cursor = target.raw_start + target_col;
    }

    /// Vim `f` / `F` on the current visual row. `count` selects the Nth
    /// occurrence in the requested direction.
    pub fn find_char_on_visual_line(
        &mut self,
        wrap_width: usize,
        target: char,
        forward: bool,
        count: usize,
    ) -> bool {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        let line = &layout.lines[row];
        let chars: Vec<char> = self.raw.chars().collect();
        let mut remaining = count.max(1);

        if forward {
            let start = self.cursor.saturating_add(1).max(line.raw_start);
            let end = line.raw_start + line.char_len;
            for idx in start..end {
                if chars.get(idx).copied() == Some(target) {
                    remaining -= 1;
                    if remaining == 0 {
                        self.cursor = idx;
                        return true;
                    }
                }
            }
        } else {
            let start = line.raw_start;
            let end = self.cursor.min(line.raw_start + line.char_len);
            for idx in (start..end).rev() {
                if chars.get(idx).copied() == Some(target) {
                    remaining -= 1;
                    if remaining == 0 {
                        self.cursor = idx;
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Cmd+Backspace equivalent: delete from cursor back to the start of the
    /// current visual row. Stops at the row boundary so a single chord doesn't
    /// nuke the whole paragraph.
    pub fn delete_to_visual_line_start(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        let start = layout.lines[row].raw_start;
        let end = self.cursor;
        if start >= end {
            return;
        }
        let start_byte = self
            .raw
            .char_indices()
            .nth(start)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        let end_byte = self
            .raw
            .char_indices()
            .nth(end)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        self.raw.replace_range(start_byte..end_byte, "");
        self.cursor = start;
    }

    pub fn move_visual_up(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width);
        let (row, col) = layout.cursor_to_visual(self.cursor);
        if row == 0 {
            // Arrow/animated vertical motion at the top edge is a no-op.  The
            // previous implementation jumped to raw cursor 0, which also
            // changed the column and made Ctrl-K animation mode-dependent.
            return;
        }
        let target = &layout.lines[row - 1];
        self.cursor = target.raw_start + col.min(target.char_len);
    }

    pub fn move_visual_down(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width);
        let (row, col) = layout.cursor_to_visual(self.cursor);
        if row + 1 >= layout.line_count() {
            // Stay on the final visual row.  Jumping to char_count() here puts
            // an Insert-mode cursor in the end-of-buffer insertion cell, so a
            // smooth-scroll animation could finish at a different column even
            // though there was nowhere left to move vertically.
            return;
        }
        let target = &layout.lines[row + 1];
        self.cursor = target.raw_start + col.min(target.char_len);
    }

    pub fn visual_cursor(&self, wrap_width: usize) -> (usize, usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        layout.cursor_to_visual(self.cursor)
    }

    /// Select the text inside the nearest surrounding delimiter pair, matching
    /// Vim text objects such as `vi"`, `vi'`, `vi(` / `vi)`, `vi[` and `vi{`.
    pub fn select_inside_delimiter(
        &mut self,
        textarea: &mut TextArea<'static>,
        wrap_width: usize,
        delimiter: char,
    ) -> bool {
        let Some((start, end)) = inside_delimiter_range(&self.raw, self.cursor, delimiter) else {
            return false;
        };
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let start_visual = layout.cursor_to_visual(start);
        let end_visual = layout.cursor_to_visual(end);
        self.cursor = start;
        textarea.cancel_selection();
        textarea.move_cursor(CursorMove::Jump(
            start_visual.0 as u16,
            start_visual.1 as u16,
        ));
        textarea.start_selection();
        self.cursor = end;
        textarea.move_cursor(CursorMove::Jump(end_visual.0 as u16, end_visual.1 as u16));
        true
    }

    fn logical_line_bounds_at(&self, cursor: usize) -> (usize, usize, usize) {
        let chars: Vec<char> = self.raw.chars().collect();
        let cursor = cursor.min(chars.len());
        let mut start = cursor;
        while start > 0 && chars[start - 1] != '\n' {
            start -= 1;
        }
        let mut end = cursor;
        while end < chars.len() && chars[end] != '\n' {
            end += 1;
        }
        let end_with_newline = if end < chars.len() { end + 1 } else { end };
        (start, end, end_with_newline)
    }

    /// Return the raw start cursor for the visual (soft-wrapped) row that
    /// contains the current cursor. Vim `V` in the commit editor intentionally
    /// operates on what the user sees on screen, so a wrapped paragraph starts
    /// as exactly one highlighted row instead of the entire logical paragraph.
    pub fn visual_line_start(&self, wrap_width: usize) -> usize {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        layout.lines[row].raw_start
    }

    /// Move the active linewise-Visual cursor by one displayed row.
    pub fn move_visual_linewise(&mut self, direction: isize, wrap_width: usize) -> bool {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, _) = layout.cursor_to_visual(self.cursor);
        let target = if direction > 0 {
            if row + 1 >= layout.lines.len() {
                return false;
            }
            row + 1
        } else {
            if row == 0 {
                return false;
            }
            row - 1
        };
        self.cursor = layout.lines[target].raw_start;
        true
    }

    /// Rebuild a linewise Visual selection from the anchor displayed row to
    /// the row containing the current cursor. This is deliberately based on
    /// soft-wrapped rows, not newline-delimited logical lines.
    pub fn select_visual_line_range(
        &mut self,
        textarea: &mut TextArea<'static>,
        wrap_width: usize,
        anchor_start: usize,
    ) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (anchor_row, _) = layout.cursor_to_visual(anchor_start);
        let (target_row, _) = layout.cursor_to_visual(self.cursor);

        textarea.cancel_selection();
        if target_row >= anchor_row {
            textarea.move_cursor(CursorMove::Jump(anchor_row as u16, 0));
            textarea.start_selection();
            let end_col = layout.lines[target_row].char_len;
            textarea.move_cursor(CursorMove::Jump(target_row as u16, end_col as u16));
        } else {
            let anchor_end = layout.lines[anchor_row].char_len;
            textarea.move_cursor(CursorMove::Jump(anchor_row as u16, anchor_end as u16));
            textarea.start_selection();
            textarea.move_cursor(CursorMove::Jump(target_row as u16, 0));
        }
    }

    /// Start `V` on exactly the displayed row under the cursor. Returns the
    /// raw row-start anchor used by subsequent `j`/`k`/`gg`/`G` motions.
    pub fn select_current_visual_line(
        &mut self,
        textarea: &mut TextArea<'static>,
        wrap_width: usize,
    ) -> usize {
        let anchor = self.visual_line_start(wrap_width);
        self.cursor = anchor;
        self.select_visual_line_range(textarea, wrap_width, anchor);
        anchor
    }

    pub fn move_to_last_visual_line_start(&mut self, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        self.cursor = layout.lines.last().map_or(0, |line| line.raw_start);
    }

    /// Return the raw text covered by a selection in the soft-wrapped display.
    pub fn selected_visual_text(
        &self,
        start: (usize, usize),
        end: (usize, usize),
        wrap_width: usize,
    ) -> String {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let start = layout.visual_to_cursor(start.0, start.1);
        let end = layout.visual_to_cursor(end.0, end.1);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.raw
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    /// Delete a selection expressed in visual soft-wrap coordinates.
    /// The projection-only wrap boundaries never become real newlines.
    pub fn delete_visual_selection(
        &mut self,
        start: (usize, usize),
        end: (usize, usize),
        wrap_width: usize,
    ) -> bool {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let start = layout.visual_to_cursor(start.0, start.1);
        let end = layout.visual_to_cursor(end.0, end.1);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        if start == end {
            return false;
        }
        let start_byte = self
            .raw
            .char_indices()
            .nth(start)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        let end_byte = self
            .raw
            .char_indices()
            .nth(end)
            .map(|(b, _)| b)
            .unwrap_or(self.raw.len());
        self.raw.replace_range(start_byte..end_byte, "");
        self.cursor = start;
        true
    }

    /// Place the visual cursor on an already-projected `textarea` without
    /// rebuilding it. Keeps the existing viewport so Up/Down only scroll when
    /// the cursor would leave the visible area (browser-textarea behavior).
    pub fn apply_cursor_into(&self, textarea: &mut TextArea<'static>, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let (row, col) = layout.cursor_to_visual(self.cursor);
        textarea.move_cursor(CursorMove::Jump(row as u16, col as u16));
    }

    /// Re-render `textarea` to display the current raw text soft-wrapped at
    /// `wrap_width`, and place the visual cursor where it logically belongs.
    ///
    /// We rebuild the textarea from scratch (rather than mutating in place)
    /// because tui_textarea's internal viewport/scroll state can get stuck
    /// past the end of content after a terminal resize. A fresh TextArea
    /// always starts with a clean viewport. Prefer [`Self::apply_cursor_into`]
    /// for pure cursor moves so scroll is preserved.
    pub fn render_into(&self, textarea: &mut TextArea<'static>, wrap_width: usize) {
        let layout = WrapLayout::build(&self.raw, wrap_width.max(1));
        let lines: Vec<String> = layout.lines.iter().map(|l| l.text.clone()).collect();
        let (row, col) = layout.cursor_to_visual(self.cursor);

        // Preserve existing visual styling so focus/cursor cues survive the rebuild.
        let cursor_style = textarea.cursor_style();
        let cursor_line_style = textarea.cursor_line_style();
        let placeholder_text = textarea.placeholder_text().to_string();
        let placeholder_style = textarea.placeholder_style();
        let style = textarea.style();

        let mut new_ta = TextArea::new(lines);
        new_ta.set_cursor_style(cursor_style);
        new_ta.set_cursor_line_style(cursor_line_style);
        new_ta.set_placeholder_text(placeholder_text);
        if let Some(s) = placeholder_style {
            new_ta.set_placeholder_style(s);
        }
        new_ta.set_style(style);
        new_ta.move_cursor(CursorMove::Jump(row as u16, col as u16));

        *textarea = new_ta;
    }
}

pub type ConfirmAction = Box<dyn FnOnce(&mut Gui) -> Result<()>>;
pub type InputAction = Box<dyn FnOnce(&mut Gui, &str) -> Result<()>>;
pub type MenuAction = Box<dyn Fn(&mut Gui) -> Result<()>>;
pub type EmailPatchAction = Box<dyn Fn(&mut Gui, String, String, bool) -> Result<()>>;

/// Result sent back from a menu item's background operation.
pub enum MenuAsyncResult {
    /// Copy the string to the clipboard.
    CopyToClipboard(String),
    /// Open the string as a URL/file.
    OpenUrl(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Error,
    Info,
}

/// Which field is focused in the commit input popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitInputFocus {
    Summary,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitInputMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

impl CommitInputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual | Self::VisualLine => "VISUAL",
        }
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Self::Visual | Self::VisualLine)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitPendingCommand {
    #[default]
    None,
    Delete,
    VisualInner,
    Count(u16),
    GoTop,
    FindForward(u16),
    FindBackward(u16),
}

impl CommitPendingCommand {
    pub fn count(self) -> usize {
        match self {
            Self::Count(count) | Self::FindForward(count) | Self::FindBackward(count) => {
                count.max(1) as usize
            }
            _ => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommitEditSnapshot {
    summary: String,
    summary_cursor: (usize, usize),
    body: String,
    body_cursor: usize,
    focus: CommitInputFocus,
}

impl CommitEditSnapshot {
    pub fn capture(
        summary_textarea: &TextArea<'static>,
        body_state: &BodySoftWrap,
        focus: CommitInputFocus,
    ) -> Self {
        Self {
            summary: summary_textarea.lines().join(""),
            summary_cursor: summary_textarea.cursor(),
            body: body_state.raw().to_string(),
            body_cursor: body_state.cursor_index(),
            focus,
        }
    }

    pub fn restore(
        self,
        summary_textarea: &mut TextArea<'static>,
        body_textarea: &mut TextArea<'static>,
        body_state: &mut BodySoftWrap,
        focus: &mut CommitInputFocus,
        wrap_width: usize,
    ) {
        set_commit_summary_text(summary_textarea, &self.summary);
        summary_textarea.move_cursor(CursorMove::Jump(
            self.summary_cursor.0 as u16,
            self.summary_cursor.1 as u16,
        ));
        body_state.set_text(self.body);
        body_state.set_cursor_index(self.body_cursor);
        body_state.render_into(body_textarea, wrap_width.max(1));
        set_commit_input_focus(focus, summary_textarea, body_textarea, self.focus);
    }
}

pub fn push_commit_undo_snapshot(
    undo_stack: &mut Vec<CommitEditSnapshot>,
    summary_textarea: &TextArea<'static>,
    body_state: &BodySoftWrap,
    focus: CommitInputFocus,
) {
    undo_stack.push(CommitEditSnapshot::capture(
        summary_textarea,
        body_state,
        focus,
    ));
    if undo_stack.len() > 100 {
        undo_stack.remove(0);
    }
}

pub fn delete_summary_word_forward(textarea: &mut TextArea<'static>) -> bool {
    let text = textarea.lines().join("");
    let chars: Vec<char> = text.chars().collect();
    let cursor = textarea.cursor().1.min(chars.len());
    if cursor >= chars.len() {
        return false;
    }
    let mut end = cursor;
    if is_word_char(chars[end]) {
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
    } else {
        while end < chars.len() && !is_word_char(chars[end]) && !chars[end].is_whitespace() {
            end += 1;
        }
    }
    while end < chars.len() && chars[end].is_whitespace() {
        end += 1;
    }
    if end == cursor {
        return false;
    }
    let new_text: String = chars[..cursor]
        .iter()
        .chain(chars[end..].iter())
        .copied()
        .collect();
    set_commit_summary_text(textarea, &new_text);
    textarea.move_cursor(CursorMove::Jump(0, cursor as u16));
    true
}

pub fn leave_summary_insert_mode(textarea: &mut TextArea<'static>) {
    if textarea.cursor().1 > 0 {
        textarea.move_cursor(CursorMove::Back);
    }
}

pub fn move_summary_word_forward_vim(textarea: &mut TextArea<'static>) {
    let text = textarea.lines().join("");
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let cursor = textarea.cursor().1.min(n);
    if n == 0 || cursor >= n {
        return;
    }
    let class = |c: char| {
        if is_word_char(c) {
            0u8
        } else if c.is_whitespace() {
            1u8
        } else {
            2u8
        }
    };
    let mut i = cursor;
    let current_class = class(chars[i]);
    if current_class != 1 {
        while i < n && class(chars[i]) == current_class {
            i += 1;
        }
    }
    while i < n && chars[i].is_whitespace() {
        i += 1;
    }
    if i < n {
        textarea.move_cursor(CursorMove::Jump(0, i as u16));
    } else if let Some(last) = chars.iter().rposition(|c| !c.is_whitespace()) {
        // Neovim-like terminal behavior: once `w` has no next token, pin the
        // Normal-mode cursor to the final real character instead of leaving it
        // somewhere in trailing whitespace or at an insertion cell.
        textarea.move_cursor(CursorMove::Jump(0, last as u16));
    }
}

/// Eyeliner-style candidate positions for the current visual line.
///
/// A primary candidate is a word character that occurs only once on the
/// searched side of the cursor, so `f{char}` / `F{char}` can jump to that word
/// directly. Words without such a character still get a secondary hint on
/// their first (forward) or last (backward) character.
fn eyeliner_targets(line: &str, cursor: usize, forward: bool) -> Vec<(usize, bool)> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let cursor = cursor.min(chars.len().saturating_sub(1));
    let in_direction = |idx: usize| {
        if forward { idx > cursor } else { idx < cursor }
    };

    let mut frequency = HashMap::<char, usize>::new();
    for (idx, ch) in chars.iter().copied().enumerate() {
        if in_direction(idx) && is_word_char(ch) {
            *frequency.entry(ch).or_default() += 1;
        }
    }

    let mut words = Vec::<(usize, usize)>::new();
    let mut idx = 0usize;
    while idx < chars.len() {
        if !is_word_char(chars[idx]) {
            idx += 1;
            continue;
        }
        let start = idx;
        while idx < chars.len() && is_word_char(chars[idx]) {
            idx += 1;
        }
        let end = idx;
        if (start..end).any(|idx| in_direction(idx)) {
            words.push((start, end));
        }
    }
    if !forward {
        words.reverse();
    }

    words
        .into_iter()
        .filter_map(|(start, end)| {
            let directional: Vec<usize> = if forward {
                (start..end).filter(|idx| in_direction(*idx)).collect()
            } else {
                (start..end)
                    .rev()
                    .filter(|idx| in_direction(*idx))
                    .collect()
            };
            if directional.is_empty() {
                return None;
            }
            if let Some(idx) = directional
                .iter()
                .copied()
                .find(|idx| frequency.get(&chars[*idx]).copied() == Some(1))
            {
                Some((idx, true))
            } else {
                Some((directional[0], false))
            }
        })
        .collect()
}

/// Highlight Eyeliner-style `f` / `F` targets on the textarea's current row.
pub fn apply_eyeliner_highlights(
    textarea: &mut TextArea<'static>,
    forward: bool,
    dim_style: Style,
    target_style: Style,
) {
    textarea.clear_custom_highlight();
    let (row, col) = textarea.cursor();
    let Some(line) = textarea.lines().get(row).cloned() else {
        return;
    };
    let line_len = line.chars().count();

    // Eyeliner dims the whole current line while find is pending, then paints
    // just the jump labels back in a high-contrast orange. No bold/underline:
    // the color contrast itself is the cue, matching the Neovim plugin feel.
    if line_len > 0 {
        textarea.custom_highlight(((row, 0), (row, line_len)), dim_style, 10);
    }
    for (target_col, _) in eyeliner_targets(&line, col, forward) {
        textarea.custom_highlight(((row, target_col), (row, target_col + 1)), target_style, 20);
    }
}

pub fn clear_eyeliner_highlights(textarea: &mut TextArea<'static>) {
    textarea.clear_custom_highlight();
}

pub fn move_summary_line_last_char(textarea: &mut TextArea<'static>) {
    let len = textarea.lines().join("").chars().count();
    let col = len.saturating_sub(1);
    textarea.move_cursor(CursorMove::Jump(0, col as u16));
}

pub fn move_summary_right_normal(textarea: &mut TextArea<'static>) {
    let len = textarea.lines().join("").chars().count();
    if len == 0 {
        textarea.move_cursor(CursorMove::Jump(0, 0));
        return;
    }
    let col = textarea.cursor().1.min(len - 1);
    textarea.move_cursor(CursorMove::Jump(0, (col + 1).min(len - 1) as u16));
}

pub fn find_summary_char(
    textarea: &mut TextArea<'static>,
    target: char,
    forward: bool,
    count: usize,
) -> bool {
    let text = textarea.lines().join("");
    let chars: Vec<char> = text.chars().collect();
    let cursor = textarea.cursor().1.min(chars.len());
    let mut remaining = count.max(1);
    if forward {
        for idx in cursor.saturating_add(1)..chars.len() {
            if chars[idx] == target {
                remaining -= 1;
                if remaining == 0 {
                    textarea.move_cursor(CursorMove::Jump(0, idx as u16));
                    return true;
                }
            }
        }
    } else {
        for idx in (0..cursor).rev() {
            if chars[idx] == target {
                remaining -= 1;
                if remaining == 0 {
                    textarea.move_cursor(CursorMove::Jump(0, idx as u16));
                    return true;
                }
            }
        }
    }
    false
}

pub fn select_summary_line(textarea: &mut TextArea<'static>) {
    let end = textarea.lines().join("").chars().count();
    textarea.cancel_selection();
    textarea.move_cursor(CursorMove::Jump(0, 0));
    textarea.start_selection();
    textarea.move_cursor(CursorMove::Jump(0, end as u16));
}

pub fn select_summary_inside_delimiter(textarea: &mut TextArea<'static>, delimiter: char) -> bool {
    let text = textarea.lines().join("");
    let cursor = textarea.cursor().1;
    let Some((start, end)) = inside_delimiter_range(&text, cursor, delimiter) else {
        return false;
    };
    textarea.cancel_selection();
    textarea.move_cursor(CursorMove::Jump(0, start as u16));
    textarea.start_selection();
    textarea.move_cursor(CursorMove::Jump(0, end as u16));
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitInputKind {
    Commit,
    Reword,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailPatchFocus {
    To,
    Cc,
}

impl EmailPatchFocus {
    pub fn next(self) -> Self {
        match self {
            Self::To => Self::Cc,
            Self::Cc => Self::To,
        }
    }

    pub fn previous(self) -> Self {
        self.next()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::To => "To",
            Self::Cc => "Cc",
        }
    }
}

pub struct EmailRecipientEditor {
    pub field: EmailPatchFocus,
    /// Existing recipient being edited. `None` means a new recipient.
    pub index: Option<usize>,
    pub textarea: TextArea<'static>,
}

#[derive(Debug, Clone)]
pub enum EmailRecipientUndo {
    Deleted {
        field: EmailPatchFocus,
        index: usize,
        value: String,
    },
    Inserted {
        field: EmailPatchFocus,
        index: usize,
        count: usize,
    },
    Replaced {
        field: EmailPatchFocus,
        index: usize,
        old_value: String,
        new_count: usize,
    },
}

pub const EMAIL_RECIPIENT_UNDO_LIMIT: usize = 32;

pub fn push_email_recipient_undo(stack: &mut Vec<EmailRecipientUndo>, entry: EmailRecipientUndo) {
    stack.push(entry);
    if stack.len() > EMAIL_RECIPIENT_UNDO_LIMIT {
        stack.remove(0);
    }
}

/// Return the normalized mailbox part of a visible recipient such as
/// `Name <user@example.com>`. This is intentionally a conservative validator,
/// not a full RFC 5322 parser: git-send-email's common human-facing forms are
/// accepted while obvious typos are rejected before sending.
pub fn email_address_key(recipient: &str) -> Option<String> {
    let value = recipient.trim();
    if value.is_empty() {
        return None;
    }

    let address = if let Some(open) = value.rfind('<') {
        let close = value.rfind('>')?;
        if close <= open || !value[close + 1..].trim().is_empty() {
            return None;
        }
        value[open + 1..close].trim()
    } else {
        value
    };

    if address
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return None;
    }

    let mut parts = address.split('@');
    let local = parts.next()?;
    let domain = parts.next()?;
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return None;
    }
    if local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || local.chars().any(|ch| {
            matches!(
                ch,
                '<' | '>' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '\\' | '"'
            )
        })
    {
        return None;
    }

    // Require a dotted domain for the mail UI. This intentionally catches
    // common mistakes like `user@example` while keeping validation simple.
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2
        || labels.iter().any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
    {
        return None;
    }

    Some(address.to_ascii_lowercase())
}

/// Remove duplicate mailboxes while preserving order. `To` wins over `Cc`
/// when the same address appears in both lists.
pub fn dedupe_email_recipients(to: &mut Vec<String>, cc: &mut Vec<String>) {
    let mut seen = HashSet::new();
    to.retain(|recipient| {
        let key =
            email_address_key(recipient).unwrap_or_else(|| recipient.trim().to_ascii_lowercase());
        seen.insert(key)
    });
    cc.retain(|recipient| {
        let key =
            email_address_key(recipient).unwrap_or_else(|| recipient.trim().to_ascii_lowercase());
        seen.insert(key)
    });
}

/// Validate a pending add/edit against both visible recipient lists. Editing
/// may keep its own mailbox (for example when only changing the display name),
/// but no other To/Cc entry may resolve to the same address.
pub fn validate_email_recipient_candidates(
    candidates: &[String],
    to: &[String],
    cc: &[String],
    editing: Option<(EmailPatchFocus, usize)>,
) -> std::result::Result<(), String> {
    let mut seen = HashSet::new();
    for (field, recipients) in [(EmailPatchFocus::To, to), (EmailPatchFocus::Cc, cc)] {
        for (index, recipient) in recipients.iter().enumerate() {
            if editing == Some((field, index)) {
                continue;
            }
            if let Some(key) = email_address_key(recipient) {
                seen.insert(key);
            }
        }
    }

    let mut pending = HashSet::new();
    for recipient in candidates {
        let Some(key) = email_address_key(recipient) else {
            return Err(format!("Invalid email address: {recipient}"));
        };
        if seen.contains(&key) || !pending.insert(key.clone()) {
            return Err(format!("Recipient already added: {key}"));
        }
    }
    Ok(())
}

/// Split a Git send-email recipient header into individual addresses while
/// preserving quoted display names such as `"Doe, Jane" <jane@example.com>`.
pub fn split_email_recipients(value: &str) -> Vec<String> {
    let mut recipients = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut angle_depth = 0usize;

    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if quoted => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            '<' if !quoted => {
                angle_depth = angle_depth.saturating_add(1);
                current.push(ch);
            }
            '>' if !quoted => {
                angle_depth = angle_depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if !quoted && angle_depth == 0 => {
                let recipient = current.trim();
                if !recipient.is_empty() {
                    recipients.push(recipient.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let recipient = current.trim();
    if !recipient.is_empty() {
        recipients.push(recipient.to_string());
    }

    recipients
}

pub fn join_email_recipients(recipients: &[String]) -> String {
    recipients
        .iter()
        .map(|recipient| recipient.trim())
        .filter(|recipient| !recipient.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the human-readable outgoing message shown in Email Review mode.
/// Recipients are taken from the current editable lists, while From/Subject/Date
/// and the body come from the git-send-email preflight/format-patch result.
#[cfg(test)]
pub fn build_email_patch_preview(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    date: &str,
    body: &str,
) -> String {
    let mut out = String::new();
    if !from.trim().is_empty() {
        out.push_str("From: ");
        out.push_str(from.trim());
        out.push('\n');
    }
    push_email_preview_recipient_header(&mut out, "To", to);
    if !cc.is_empty() {
        push_email_preview_recipient_header(&mut out, "Cc", cc);
    }
    if !subject.trim().is_empty() {
        out.push_str("Subject: ");
        out.push_str(subject.trim());
        out.push('\n');
    }
    if !date.trim().is_empty() {
        out.push_str("Date: ");
        out.push_str(date.trim());
        out.push('\n');
    }
    out.push('\n');
    out.push_str(body);
    out
}

#[cfg(test)]
fn push_email_preview_recipient_header(out: &mut String, name: &str, recipients: &[String]) {
    if recipients.is_empty() {
        out.push_str(name);
        out.push_str(": (none)\n");
        return;
    }

    out.push_str(name);
    out.push_str(": ");
    for (index, recipient) in recipients.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n    ");
        }
        out.push_str(recipient.trim());
    }
    out.push('\n');
}

impl CommitInputKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Commit => "Commit message",
            Self::Reword => "Reword commit",
        }
    }
}

pub enum PopupState {
    None,
    Confirm {
        title: String,
        message: String,
        on_confirm: ConfirmAction,
    },
    Input {
        title: String,
        textarea: TextArea<'static>,
        on_confirm: InputAction,
        /// When true, this is a commit message editor — enables AI generation via <c-g>.
        is_commit: bool,
        /// When true, focus is on the Confirm button instead of the textarea.
        confirm_focused: bool,
    },
    /// Compose recipients for a single-commit `git send-email` operation.
    EmailPatch {
        title: String,
        to_recipients: Vec<String>,
        cc_recipients: Vec<String>,
        focus: EmailPatchFocus,
        to_selected: usize,
        cc_selected: usize,
        recipient_editor: Option<EmailRecipientEditor>,
        recipient_undo: Vec<EmailRecipientUndo>,
        dry_run: bool,
        /// True while the outgoing message preview is being reviewed.
        reviewing: bool,
        /// Sender resolved by the initial git-send-email dry run.
        preview_from: String,
        /// Final subject resolved by the initial git-send-email dry run.
        preview_subject: String,
        /// Date header resolved by git-send-email/format-patch.
        preview_date: String,
        /// Formatted patch message body shown in Review mode.
        preview_body: String,
        /// Vertical scroll offset for the outgoing email preview body.
        preview_scroll: usize,
        /// Smooth email-preview scroll animation state.
        preview_scroll_from: f64,
        preview_scroll_target: f64,
        preview_scroll_started_at: Option<Instant>,
        /// True while the final send-confirmation overlay is visible.
        confirming_send: bool,
        /// True while the initial dry-run recipient resolution is running in the background.
        resolving_recipients: bool,
        /// Identifies the active async resolution so stale completions are ignored.
        resolution_request_id: u64,
        on_confirm: EmailPatchAction,
    },
    /// Two-field commit message editor (summary + body), like lazygit.
    CommitInput {
        kind: CommitInputKind,
        summary_textarea: TextArea<'static>,
        body_textarea: TextArea<'static>,
        /// Source-of-truth for body content. `body_textarea` is a soft-wrapped
        /// view of this string. All body edits flow through here; on submit,
        /// prose is hard-wrapped to the configured commit-message width.
        body_state: BodySoftWrap,
        focus: CommitInputFocus,
        mode: CommitInputMode,
        /// Multi-key Vim command currently waiting for its second key (`d…`, `vi…`).
        pending_command: CommitPendingCommand,
        /// Raw start offset of the logical line where `V` began. Linewise
        /// Visual motion rebuilds the selection from this anchor so `j`/`k`
        /// always extend whole logical lines, including across soft wraps.
        visual_line_anchor: Option<usize>,
        /// Commit-editor-local undo history. Each insert session or normal-mode
        /// mutation records one snapshot, so `u` never collides with Git undo.
        undo_stack: Vec<CommitEditSnapshot>,
        /// Whether the in-editor Vim/commit keybinding reference is visible.
        show_help: bool,
        /// Remaining rows for the neoscroll-style animated body viewport.
        body_scroll_remaining: isize,
        /// Tracked top visual row while the viewport is being scrolled independently
        /// of the editing cursor. This keeps smooth scrolling clamped and reversible.
        body_scroll_top: usize,
        on_confirm: InputAction,
    },
    Menu {
        title: String,
        items: Vec<MenuItem>,
        selected: usize,
        /// When set, this menu item index is running an async operation (shows inline spinner).
        loading_index: Option<usize>,
    },
    /// Checkout was refused because local changes would be overwritten.
    /// The existing branch remains checked out until an explicit choice runs.
    BranchSwitchConflict {
        source: String,
        target: String,
        paths: Vec<String>,
        selected: usize,
    },
    /// Guided reconciliation UI shown when the current branch and upstream have diverged.
    /// This is intentionally a dedicated large modal instead of the compact generic menu
    /// so the consequences of rebase/merge can be explained without truncation.
    PullDivergence {
        branch: String,
        upstream: String,
        ahead: usize,
        behind: usize,
        /// Primary selection: 0=rebase, 1=merge, 2=discard to upstream, 3=view.
        selected: usize,
    },
    /// Destructive reset requires the explicit 'y' key, not Enter.
    ConfirmResetToUpstream { branch: String, upstream: String },
    /// Informational or error message — dismissed by any key press.
    Message {
        title: String,
        message: String,
        kind: MessageKind,
    },
    /// Literal shell input: never soft-wrap or join away user newlines.
    ShellCommand {
        textarea: TextArea<'static>,
    },
    CommandOutput {
        title: String,
        message: String,
        kind: MessageKind,
        scroll: usize,
    },
    /// `git format-patch` preview with patch-aware coloring. Ctrl-J/K moves
    /// the cursor by 22 visual rows using the same short easing as commit-list
    /// navigation, while the viewport follows only as needed to keep it visible.
    PatchPreview {
        title: String,
        message: String,
        scroll: usize,
        cursor_line: usize,
        smooth_scroll_from: f64,
        smooth_scroll_target: f64,
        smooth_scroll_started_at: Option<Instant>,
        smooth_cursor_from: f64,
        smooth_cursor_target: f64,
        smooth_cursor_started_at: Option<Instant>,
    },
    /// Shown while a background operation (like AI commit generation) is running.
    Loading {
        title: String,
        message: String,
    },
    /// Multi-select checklist with search filter.
    Checklist {
        title: String,
        items: Vec<ChecklistItem>,
        selected: usize,
        search_textarea: TextArea<'static>,
        /// When present, non-empty search text is also a checkable custom item.
        free_entry_category: Option<String>,
        on_confirm: ChecklistAction,
    },
    /// Searchable command palette with keybinding hints.
    CommandPalette {
        sections: Vec<CommandSection>,
        selected: usize,
        search_textarea: TextArea<'static>,
        scroll_offset: usize,
    },
    /// Searchable ref picker (branches, tags, commits) with a callback.
    RefPicker {
        title: String,
        core: ListPickerCore,
        on_confirm: ListPickerAction,
    },
    /// Generic searchable list picker with free-text entry (path/author filters, etc.).
    /// Modeled after [`PopupState::RefPicker`] but with a configurable free-entry category.
    ListPicker {
        title: String,
        core: ListPickerCore,
        /// Category label for the synthetic free-entry row (e.g. `"[path]"`, `"[author]"`).
        free_entry_category: String,
        on_confirm: ListPickerAction,
    },
    /// Yazi-style three-pane filesystem browser used to choose an existing patch.
    PatchFilePicker {
        state: PatchFilePickerState,
        on_confirm: ListPickerAction,
    },
    /// Color theme picker with live preview and search.
    ThemePicker {
        core: ListPickerCore,
        /// The theme index before opening the picker (for cancel/revert).
        original_theme_index: usize,
    },
}

pub type ChecklistAction = Box<dyn FnOnce(&mut Gui, Vec<String>) -> Result<()>>;

pub struct ChecklistItem {
    pub label: String,
    pub checked: bool,
    pub is_free_entry: bool,
}

/// Keep a free-entry checklist row in sync with the current search text.
///
/// When `free_entry_category` is set and the search box is non-empty, a
/// synthetic item whose label is the typed text is inserted at the top so
/// users can multi-select arbitrary values (authors) just like known ones.
pub fn sync_checklist_free_entry(
    items: &mut Vec<ChecklistItem>,
    free_entry_category: Option<&str>,
    search: &str,
) {
    let previously_checked = items
        .iter()
        .find(|item| item.is_free_entry)
        .map(|item| (item.label.clone(), item.checked));
    items.retain(|item| !item.is_free_entry);
    if free_entry_category.is_none() {
        return;
    }
    let search = search.trim();
    if search.is_empty() {
        return;
    }
    if items.iter().any(|item| item.label == search) {
        return;
    }
    let checked = previously_checked
        .as_ref()
        .is_some_and(|(label, checked)| label == search && *checked);
    items.insert(
        0,
        ChecklistItem {
            label: search.to_string(),
            checked,
            is_free_entry: true,
        },
    );
}

impl PartialEq for PopupState {
    fn eq(&self, other: &Self) -> bool {
        matches!((self, other), (PopupState::None, PopupState::None))
    }
}

pub fn make_textarea(placeholder: &str) -> TextArea<'static> {
    use ratatui::style::{Color, Style};

    let mut ta = TextArea::default();
    ta.set_placeholder_text(placeholder);
    ta.set_cursor_line_style(Style::default());
    ta.set_placeholder_style(Style::default().fg(Color::DarkGray));
    ta
}

pub fn make_commit_summary_textarea() -> TextArea<'static> {
    let mut ta = make_textarea("Required");
    // CommitInput renders the real terminal cursor (bar/block) instead of a
    // reverse-video TextArea cell, so keep the widget's cursor paint neutral.
    ta.set_cursor_style(ratatui::style::Style::default());
    ta.set_cursor_line_style(ratatui::style::Style::default());
    ta
}

/// Replace the summary textarea contents with `text`, cursor at end.
///
/// Always rebuilds a fresh `TextArea` so horizontal `scroll_top` is reset.
/// Reusing select_all/cut/insert_str can leave a stale scroll past the new
/// end (tui-textarea's `next_scroll_top` then clamps to `cursor`, showing
/// empty space to the right of the text).
pub fn set_commit_summary_text(textarea: &mut TextArea<'static>, text: &str) {
    let mut fresh = make_commit_summary_textarea();
    // Preserve focus/cursor styling from the existing widget.
    fresh.set_cursor_style(textarea.cursor_style());
    fresh.set_cursor_line_style(textarea.cursor_line_style());
    fresh.set_style(textarea.style());
    if !text.is_empty() {
        fresh.insert_str(text);
    }
    *textarea = fresh;
}

pub fn make_commit_body_textarea() -> TextArea<'static> {
    let mut ta = make_textarea("Optional — hard-wrapped on commit");
    // Body starts unfocused — hide cursor
    ta.set_cursor_style(ratatui::style::Style::default());
    ta
}

pub fn set_commit_input_focus(
    focus: &mut CommitInputFocus,
    summary_textarea: &mut TextArea<'static>,
    body_textarea: &mut TextArea<'static>,
    new_focus: CommitInputFocus,
) {
    let visible = ratatui::style::Style::default();
    let hidden = ratatui::style::Style::default();
    // The active row background is painted by the popup renderer using the
    // same selected-row color as Files/Commits/Branches. Keep TextArea's own
    // cursor-line style neutral so it does not add bold/underline on top.
    let current_line = ratatui::style::Style::default();
    *focus = new_focus;
    match new_focus {
        CommitInputFocus::Summary => {
            summary_textarea.set_cursor_style(visible);
            summary_textarea.set_cursor_line_style(current_line);
            body_textarea.set_cursor_style(hidden);
            body_textarea.set_cursor_line_style(hidden);
        }
        CommitInputFocus::Body => {
            summary_textarea.set_cursor_style(hidden);
            summary_textarea.set_cursor_line_style(hidden);
            body_textarea.set_cursor_style(visible);
            body_textarea.set_cursor_line_style(current_line);
        }
    }
}

/// Reset a restored commit draft to a readable top-left viewport.  Draft
/// contents are preserved; only focus, cursor position, and textarea scroll
/// state are reset.
pub fn reset_commit_input_view(popup: &mut PopupState, wrap_width: usize) {
    let PopupState::CommitInput {
        summary_textarea,
        body_textarea,
        body_state,
        focus,
        mode,
        pending_command,
        visual_line_anchor,
        show_help,
        body_scroll_remaining,
        body_scroll_top,
        ..
    } = popup
    else {
        return;
    };

    let summary = summary_textarea.lines().join("");
    set_commit_summary_text(summary_textarea, &summary);
    summary_textarea.move_cursor(CursorMove::Jump(0, 0));

    body_state.move_to_start();
    *body_textarea = make_commit_body_textarea();
    body_state.render_into(body_textarea, wrap_width.max(1));

    let visible = ratatui::style::Style::default();
    let hidden = ratatui::style::Style::default();
    summary_textarea.cancel_selection();
    body_textarea.cancel_selection();
    summary_textarea.set_cursor_style(visible);
    body_textarea.set_cursor_style(hidden);
    *focus = CommitInputFocus::Summary;
    *mode = CommitInputMode::Normal;
    *pending_command = CommitPendingCommand::None;
    *visual_line_anchor = None;
    *show_help = false;
    *body_scroll_remaining = 0;
    *body_scroll_top = 0;
}

pub fn make_command_palette_search_textarea() -> TextArea<'static> {
    use ratatui::style::{Color, Style};

    let mut ta = make_textarea("Search commands or keybindings...");
    ta.set_style(Style::default().fg(Color::Yellow));
    ta.set_cursor_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(ratatui::style::Modifier::REVERSED),
    );
    ta
}

pub fn make_checklist_search_textarea() -> TextArea<'static> {
    use ratatui::style::{Color, Style};

    let mut ta = make_textarea("Filter...");
    ta.set_style(Style::default().fg(Color::Yellow));
    ta.set_cursor_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(ratatui::style::Modifier::REVERSED),
    );
    ta
}

pub struct MenuItem {
    pub label: String,
    pub description: String,
    pub key: Option<String>,
    pub action: Option<MenuAction>,
}

impl MenuItem {
    pub fn cancel() -> Self {
        Self {
            label: "Cancel".to_string(),
            description: String::new(),
            key: None,
            action: Some(Box::new(|_| Ok(()))),
        }
    }
}

pub struct CommandSection {
    pub title: String,
    pub entries: Vec<CommandEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Dispatch(KeyEvent),
    OpenThemePicker,
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub key: String,
    pub description: String,
    pub action: CommandAction,
}

impl CommandEntry {
    pub fn keybinding(key: String, description: String) -> Self {
        let action = parse_command_key(&key)
            .map(CommandAction::Dispatch)
            .unwrap_or(CommandAction::Unavailable);
        Self {
            key,
            description: description.into(),
            action,
        }
    }

    pub fn action(key: String, description: String, action: CommandAction) -> Self {
        Self {
            key,
            description,
            action,
        }
    }

    pub fn is_executable(&self) -> bool {
        self.action != CommandAction::Unavailable
    }
}

#[cfg(test)]
mod commit_body_wrap_tests {
    use super::{
        BodySoftWrap, WrapLayout, eyeliner_targets, hard_wrap_commit_body, unwrap_commit_body,
    };

    #[test]
    fn eyeliner_marks_unique_per_word_targets_and_fallbacks() {
        assert_eq!(
            eyeliner_targets("cat dog pig", 0, true),
            vec![(1, true), (4, true), (8, true)]
        );
        assert_eq!(
            eyeliner_targets("foo zoo", 0, true),
            vec![(1, false), (4, true)]
        );
    }

    #[test]
    fn vim_word_end_and_find_motions_land_on_characters() {
        let mut body = BodySoftWrap::from_text("alpha   beta gamma");
        body.set_cursor_index(0);
        body.move_word_right_vim();
        assert_eq!(body.cursor_index(), 8);

        body.move_visual_line_last_char(80);
        assert_eq!(body.cursor_index(), 17);

        body.set_cursor_index(0);
        assert!(body.find_char_on_visual_line(80, 'a', true, 1));
        assert_eq!(body.cursor_index(), 4);
        assert!(body.find_char_on_visual_line(80, 'a', true, 2));
        assert_eq!(body.cursor_index(), 14);
        assert!(body.find_char_on_visual_line(80, 'a', false, 1));
        assert_eq!(body.cursor_index(), 11);
    }

    #[test]
    fn vim_word_forward_at_last_word_pins_to_last_character() {
        let mut body = BodySoftWrap::from_text("alpha beta   ");
        body.set_cursor_index(6);
        body.move_word_right_vim();
        assert_eq!(body.cursor_index(), 9);

        let mut summary = super::make_commit_summary_textarea();
        summary.insert_str("alpha beta   ");
        summary.move_cursor(tui_textarea::CursorMove::Jump(0, 6));
        super::move_summary_word_forward_vim(&mut summary);
        assert_eq!(summary.cursor().1, 9);
    }

    #[test]
    fn unwrap_preserves_newlines_and_bullets() {
        let raw = "- something\n- something 2\n\nparagraph\n";
        assert_eq!(
            unwrap_commit_body(raw),
            "- something\n- something 2\n\nparagraph\n"
        );
    }

    #[test]
    fn unwrap_normalizes_crlf() {
        assert_eq!(unwrap_commit_body("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn wrap_layout_keeps_logical_newlines_as_rows() {
        let layout = WrapLayout::build("- something\n- something 2", 80);
        assert_eq!(layout.line_count(), 2);
        assert_eq!(layout.as_textarea_text(), "- something\n- something 2");
    }

    #[test]
    fn wrap_layout_breaks_on_display_width_not_char_count() {
        // Two wide chars (width 2 each) should wrap before a third at width 4.
        let layout = WrapLayout::build("あああ", 4);
        assert_eq!(layout.line_count(), 2);
        assert_eq!(layout.as_textarea_text(), "ああ\nあ");
    }

    #[test]
    fn soft_wrap_preserves_raw_newlines_while_rendering() {
        let mut state = BodySoftWrap::new();
        state.set_text("- something\n- something 2");
        let mut ta = super::make_commit_body_textarea();
        state.render_into(&mut ta, 80);
        assert_eq!(state.raw(), "- something\n- something 2");
        assert_eq!(ta.lines().join("\n"), "- something\n- something 2");
    }

    #[test]
    fn insert_vertical_motion_stays_put_at_visual_buffer_edges() {
        let mut state = BodySoftWrap::from_text("one\ntwo\nthree");

        // At the first visual row, moving up must not reset the column.
        state.set_cursor_index(2);
        state.move_visual_up(80);
        assert_eq!(state.visual_cursor(80), (0, 2));

        // At the final visual row, moving down must not jump to the
        // end-of-buffer insertion cell. This is important for Ctrl-J/K
        // neoscroll because fresh commit editors start in Insert mode.
        state.set_cursor_index(9); // `three`, column 1
        assert_eq!(state.visual_cursor(80), (2, 1));
        state.move_visual_down(80);
        assert_eq!(state.visual_cursor(80), (2, 1));
    }

    #[test]
    fn visual_line_selection_starts_with_only_the_current_wrapped_row() {
        let mut state = BodySoftWrap::from_text("alpha beta gamma delta");
        state.set_cursor_index(7); // inside the wrapped `beta` row
        let mut ta = super::make_commit_body_textarea();
        state.render_into(&mut ta, 6);

        let anchor = state.select_current_visual_line(&mut ta, 6);
        let selection = ta.selection_range().expect("V should select one row");
        assert_eq!(
            state.selected_visual_text(selection.0, selection.1, 6),
            "beta"
        );

        assert!(state.move_visual_linewise(1, 6));
        state.select_visual_line_range(&mut ta, 6, anchor);
        let selection = ta.selection_range().expect("Vj should extend one row");
        assert_eq!(
            state.selected_visual_text(selection.0, selection.1, 6),
            "beta gamma"
        );
    }

    #[test]
    fn visual_selection_maps_soft_wrap_back_to_raw_text() {
        let mut state = BodySoftWrap::from_text("alpha beta gamma");
        assert_eq!(state.selected_visual_text((0, 0), (1, 4), 6), "alpha beta");
        assert!(state.delete_visual_selection((0, 0), (1, 4), 6));
        assert_eq!(state.raw(), " gamma");
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn vim_append_stays_on_current_visual_line_at_eol() {
        let mut state = BodySoftWrap::from_text("abc\ndef");
        state.set_cursor_index(3);
        state.move_append(80);
        assert_eq!(state.cursor_index(), 3);

        state.set_cursor_index(1);
        state.move_append(80);
        assert_eq!(state.cursor_index(), 2);
    }

    #[test]
    fn vim_delete_word_and_line_operate_on_raw_logical_text() {
        let mut state = BodySoftWrap::from_text("alpha beta\nsecond line\nthird");
        state.move_to_start();
        assert!(state.delete_word_forward());
        assert_eq!(state.raw(), "beta\nsecond line\nthird");

        state.set_cursor_index(7);
        assert!(state.delete_current_line());
        assert_eq!(state.raw(), "beta\nthird");
    }

    #[test]
    fn vim_open_line_below_creates_a_real_newline() {
        let mut state = BodySoftWrap::from_text("one\ntwo");
        state.set_cursor_index(1);
        state.open_line_below();
        assert_eq!(state.raw(), "one\n\ntwo");
        assert_eq!(state.cursor_index(), 4);
    }

    #[test]
    fn vim_inner_delimiter_selection_excludes_delimiters() {
        for (text, cursor, delimiter, expected) in [
            ("say \"hello world\" now", 8, '"', "hello world"),
            ("say 'hello world' now", 8, '\'', "hello world"),
            ("call(foo bar) now", 7, '(', "foo bar"),
            ("call(foo bar) now", 7, ')', "foo bar"),
            ("list[foo bar] now", 7, '[', "foo bar"),
            ("map{foo bar} now", 6, '}', "foo bar"),
        ] {
            let mut state = BodySoftWrap::from_text(text);
            state.set_cursor_index(cursor);
            let mut textarea = super::make_commit_body_textarea();
            state.render_into(&mut textarea, 80);
            assert!(state.select_inside_delimiter(&mut textarea, 80, delimiter));
            let (start, end) = textarea.selection_range().expect("selection");
            assert_eq!(state.selected_visual_text(start, end, 80), expected);
        }
    }

    #[test]
    fn hard_wrap_commit_body_writes_real_prose_lines() {
        let body = "This is a deliberately long commit-message sentence that should become real lines instead of remaining one long logical line in the commit object.";
        let wrapped = hard_wrap_commit_body(body, 40);
        assert!(wrapped.lines().all(|line| line.chars().count() <= 40));
        assert!(wrapped.contains('\n'));
    }

    #[test]
    fn hard_wrap_preserves_trailers_and_preformatted_lines() {
        let trailer =
            "Signed-off-by: Example Person With A Very Long Name <example.person@example.com>";
        let code = "    this intentionally remains a long preformatted line even when the configured width is narrow";
        let wrapped = hard_wrap_commit_body(&format!("Short prose\n\n{trailer}\n{code}"), 32);
        assert!(wrapped.contains(trailer));
        assert!(wrapped.contains(code));
    }

    #[test]
    fn append_trailer_creates_one_trailer_block_and_deduplicates() {
        let mut state = BodySoftWrap::from_text("Explain why this changes behavior.");
        assert!(state.append_trailer("Signed-off-by: Test <test@example.com>"));
        assert_eq!(
            state.raw(),
            "Explain why this changes behavior.\n\nSigned-off-by: Test <test@example.com>"
        );
        assert!(!state.append_trailer("Signed-off-by: Test <test@example.com>"));
        assert!(state.append_trailer("Reviewed-by: Reviewer <r@example.com>"));
        assert!(state.raw().ends_with(
            "Signed-off-by: Test <test@example.com>\nReviewed-by: Reviewer <r@example.com>"
        ));
    }

    #[test]
    fn remove_trailer_cleans_up_separator_and_preserves_other_trailers() {
        let mut state = BodySoftWrap::from_text(
            "Explain why this changes behavior.\n\nSigned-off-by: Test <test@example.com>\nReviewed-by: Reviewer <r@example.com>",
        );
        assert!(state.remove_trailer("Signed-off-by: Test <test@example.com>"));
        assert_eq!(
            state.raw(),
            "Explain why this changes behavior.\n\nReviewed-by: Reviewer <r@example.com>"
        );
        assert!(state.remove_trailer("Reviewed-by: Reviewer <r@example.com>"));
        assert_eq!(state.raw(), "Explain why this changes behavior.");
        assert!(!state.remove_trailer("Signed-off-by: Test <test@example.com>"));
    }
}

#[cfg(test)]
mod command_entry_tests {
    use super::{CommandAction, CommandEntry};
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn single_key_binding_is_executable() {
        let entry = CommandEntry::keybinding("x".into(), "Delete".into());

        assert!(entry.is_executable());
        assert!(matches!(
            entry.action,
            CommandAction::Dispatch(key)
                if key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::NONE
        ));
    }

    #[test]
    fn compound_key_hint_is_not_executable() {
        let entry = CommandEntry::keybinding("j/k".into(), "Navigate".into());

        assert!(!entry.is_executable());
        assert_eq!(entry.action, CommandAction::Unavailable);
    }

    #[test]
    fn explicit_action_is_executable_without_a_keybinding() {
        let entry = CommandEntry::action(
            String::new(),
            "Color theme...".into(),
            CommandAction::OpenThemePicker,
        );

        assert!(entry.is_executable());
    }

    #[test]
    fn display_key_label_is_executable() {
        let entry = CommandEntry::keybinding("Tab".into(), "Next panel".into());

        assert!(matches!(
            entry.action,
            CommandAction::Dispatch(key) if key.code == KeyCode::Tab
        ));
    }
}

pub type ListPickerAction = Box<dyn FnOnce(&mut Gui, &str) -> Result<()>>;

/// Category used by [`PopupState::RefPicker`] for the synthetic free-entry row.
pub const REF_FREE_ENTRY_CATEGORY: &str = "[ref]";

#[derive(Debug, Clone)]
pub struct ListPickerItem {
    /// The value to pass to the callback (ref name, hash, theme id, etc.).
    pub value: String,
    /// Display label shown in the list.
    pub label: String,
    /// Section/category header (e.g. "Branches", "Tags"). Empty for flat lists.
    pub category: String,
    /// Optional right-aligned dimmed label (e.g. "light" / "dark" for themes).
    pub description: Option<String>,
}

/// Shared state for searchable list picker popups (RefPicker, ThemePicker, etc.).
pub struct ListPickerCore {
    pub items: Vec<ListPickerItem>,
    pub selected: usize,
    pub search_textarea: TextArea<'static>,
    pub scroll_offset: usize,
}

#[derive(Debug, Clone)]
pub struct PatchPickerEntry {
    pub path: std::path::PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub likely_patch: bool,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub enum PatchPickerPreview {
    Empty,
    Directory(Vec<PatchPickerEntry>),
    Text {
        lines: Vec<String>,
        truncated: bool,
        likely_patch: bool,
    },
    Binary {
        size: u64,
    },
    Error(String),
}

/// Filesystem state for the Apply Patch browser. This deliberately owns its
/// directory listings and preview instead of performing filesystem I/O during
/// rendering, which keeps navigation deterministic and cheap.
pub struct PatchFilePickerState {
    pub repo_root: std::path::PathBuf,
    pub current_dir: std::path::PathBuf,
    pub entries: Vec<PatchPickerEntry>,
    pub selected: usize,
    pub parent_entries: Vec<PatchPickerEntry>,
    pub parent_selected: usize,
    pub preview: PatchPickerPreview,
    /// Signed number of center-pane rows still to move during Ctrl-J/K smooth
    /// navigation. The selected file is the picker cursor; the renderer derives
    /// the visible slice from it, so moving this cursor also scrolls the pane.
    pub picker_scroll_remaining: isize,
    /// Last center-pane viewport height used when a half-page move was queued.
    picker_visible_rows: usize,
    /// Vim-style numeric prefix for center-pane motions (for example `20j`).
    pending_count: usize,
}

impl PatchFilePickerState {
    pub fn new(repo_root: std::path::PathBuf, current_dir: std::path::PathBuf) -> Result<Self> {
        let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
        let current_dir = current_dir.canonicalize().unwrap_or(current_dir);
        let mut state = Self {
            repo_root,
            current_dir,
            entries: Vec::new(),
            selected: 0,
            parent_entries: Vec::new(),
            parent_selected: 0,
            preview: PatchPickerPreview::Empty,
            picker_scroll_remaining: 0,
            picker_visible_rows: 1,
            pending_count: 0,
        };
        state.refresh()?;
        Ok(state)
    }

    pub fn selected_entry(&self) -> Option<&PatchPickerEntry> {
        self.entries.get(self.selected)
    }

    pub fn push_count_digit(&mut self, digit: u32) {
        let digit = digit as usize;
        // Match Vim's useful count behavior: a leading 0 is not a count, but
        // zeroes may extend an existing prefix (`2`, `0` -> `20`).
        if digit == 0 && self.pending_count == 0 {
            return;
        }
        self.pending_count = self
            .pending_count
            .saturating_mul(10)
            .saturating_add(digit)
            .min(99_999);
    }

    pub fn take_count(&mut self) -> usize {
        let count = self.pending_count.max(1);
        self.pending_count = 0;
        count
    }

    pub fn clear_count(&mut self) {
        self.pending_count = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.picker_scroll_remaining = 0;
        self.pending_count = 0;
        self.move_selection_step(delta);
    }

    fn move_selection_step(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            self.preview = PatchPickerPreview::Empty;
            return;
        }
        let last = self.entries.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.refresh_preview();
    }

    pub fn select_first(&mut self) {
        self.picker_scroll_remaining = 0;
        self.pending_count = 0;
        self.selected = 0;
        self.refresh_preview();
    }

    pub fn select_last(&mut self) {
        self.picker_scroll_remaining = 0;
        self.pending_count = 0;
        self.selected = self.entries.len().saturating_sub(1);
        self.refresh_preview();
    }

    pub fn picker_scroll_active(&self) -> bool {
        self.picker_scroll_remaining != 0
    }

    /// Queue a Yazi/Vim-style half-page move in the active Files pane. The
    /// selection advances one row per animation tick; because the visible slice
    /// is centered around `selected`, the file list scrolls smoothly with it.
    pub fn queue_picker_half_page_scroll(&mut self, direction: isize, visible_rows: usize) {
        if self.entries.is_empty() || direction == 0 {
            self.picker_scroll_remaining = 0;
            return;
        }

        self.picker_visible_rows = visible_rows.max(1);
        let amount = (self.picker_visible_rows / 2).max(1);
        let last = self.entries.len().saturating_sub(1);
        let current = self.selected.min(last);
        let target = if direction > 0 {
            current.saturating_add(amount).min(last)
        } else {
            current.saturating_sub(amount)
        };
        self.picker_scroll_remaining = target as isize - current as isize;

        // Advance the first row synchronously so the keypress always produces
        // visible feedback in the very next draw. Remaining rows use the normal
        // 8 ms animation cadence.
        if self.picker_scroll_remaining != 0 {
            self.tick_picker_scroll();
        }
    }

    pub fn tick_picker_scroll(&mut self) {
        if self.picker_scroll_remaining == 0 {
            return;
        }
        let step = self.picker_scroll_remaining.signum();
        let before = self.selected;
        self.move_selection_step(step);
        if self.selected == before {
            self.picker_scroll_remaining = 0;
            return;
        }
        self.picker_scroll_remaining -= step;
    }

    pub fn enter_selected(&mut self) -> Result<Option<std::path::PathBuf>> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(None);
        };
        if entry.is_dir {
            self.current_dir = entry.path;
            self.selected = 0;
            self.refresh()?;
            Ok(None)
        } else {
            Ok(Some(entry.path))
        }
    }

    pub fn go_parent(&mut self) -> Result<()> {
        let Some(parent) = self.current_dir.parent().map(std::path::Path::to_path_buf) else {
            return Ok(());
        };
        let previous = self.current_dir.clone();
        self.current_dir = parent;
        self.refresh()?;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.path.as_path() == previous.as_path())
        {
            self.selected = index;
            self.refresh_preview();
        }
        Ok(())
    }

    pub fn go_to(&mut self, directory: std::path::PathBuf) -> Result<()> {
        if !directory.is_dir() {
            anyhow::bail!("Not a directory: {}", directory.display());
        }
        self.current_dir = directory.canonicalize().unwrap_or(directory);
        self.selected = 0;
        self.refresh()
    }

    fn refresh(&mut self) -> Result<()> {
        self.picker_scroll_remaining = 0;
        self.pending_count = 0;
        self.entries = patch_picker_read_dir(&self.current_dir)?;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));

        self.parent_entries.clear();
        self.parent_selected = 0;
        if let Some(parent) = self.current_dir.parent() {
            self.parent_entries = patch_picker_read_dir(parent).unwrap_or_default();
            if let Some(index) = self
                .parent_entries
                .iter()
                .position(|entry| entry.path.as_path() == self.current_dir.as_path())
            {
                self.parent_selected = index;
            }
        }

        self.refresh_preview();
        Ok(())
    }

    fn refresh_preview(&mut self) {
        self.preview = match self.selected_entry().cloned() {
            None => PatchPickerPreview::Empty,
            Some(entry) if entry.is_dir => match patch_picker_read_dir(&entry.path) {
                Ok(entries) => PatchPickerPreview::Directory(entries),
                Err(error) => PatchPickerPreview::Error(error.to_string()),
            },
            Some(entry) => patch_picker_file_preview(&entry),
        };
    }
}

fn patch_picker_read_dir(directory: &std::path::Path) -> Result<Vec<PatchPickerEntry>> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(directory)? {
        let Ok(item) = item else { continue };
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let Ok(metadata) = item.metadata() else {
            continue;
        };
        let is_dir = metadata.is_dir();
        if !is_dir && !metadata.is_file() {
            continue;
        }
        let likely_patch = !is_dir && patch_picker_is_probably_patch(&path, &name);
        entries.push(PatchPickerEntry {
            path,
            name,
            is_dir,
            likely_patch,
            size: metadata.len(),
        });
    }
    entries.sort_by(|left, right| {
        let left_rank = if left.is_dir {
            0
        } else if left.likely_patch {
            1
        } else {
            2
        };
        let right_rank = if right.is_dir {
            0
        } else if right.likely_patch {
            1
        } else {
            2
        };
        left_rank.cmp(&right_rank).then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
    });
    Ok(entries)
}

fn patch_picker_is_probably_patch(path: &std::path::Path, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".patch")
        || lower.ends_with(".diff")
        || lower.ends_with(".mbox")
        || lower.ends_with(".eml")
        || lower.contains("patch")
    {
        return true;
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0u8; 8192];
    let Ok(read) = std::io::Read::read(&mut file, &mut buffer) else {
        return false;
    };
    if read == 0 || buffer[..read].contains(&0) {
        return false;
    }
    let prefix = String::from_utf8_lossy(&buffer[..read]);
    prefix.contains("diff --git ")
        || (prefix.contains("\n--- ") && prefix.contains("\n+++ "))
        || (prefix.starts_with("From ") && prefix.contains("\nSubject:"))
        || prefix.starts_with("Index: ")
}

fn patch_picker_file_preview(entry: &PatchPickerEntry) -> PatchPickerPreview {
    let Ok(mut file) = std::fs::File::open(&entry.path) else {
        return PatchPickerPreview::Error("Unable to open file".to_string());
    };
    const PREVIEW_BYTES: usize = 64 * 1024;
    let mut buffer = vec![0u8; PREVIEW_BYTES + 1];
    let read = match std::io::Read::read(&mut file, &mut buffer) {
        Ok(read) => read,
        Err(error) => return PatchPickerPreview::Error(error.to_string()),
    };
    buffer.truncate(read.min(PREVIEW_BYTES));
    if buffer.contains(&0) {
        return PatchPickerPreview::Binary { size: entry.size };
    }
    let text = String::from_utf8_lossy(&buffer);
    let lines = text.lines().take(500).map(str::to_string).collect();
    PatchPickerPreview::Text {
        lines,
        truncated: read > PREVIEW_BYTES,
        likely_patch: entry.likely_patch,
    }
}

/// True when index 0 is the synthetic free-entry row for `free_entry_category`.
pub fn is_free_entry_item(items: &[ListPickerItem], free_entry_category: &str) -> bool {
    !items.is_empty() && items[0].category == free_entry_category
}

/// Remove the synthetic free-entry row at index 0 if present.
pub fn remove_free_entry_item(items: &mut Vec<ListPickerItem>, free_entry_category: &str) {
    if is_free_entry_item(items, free_entry_category) {
        items.remove(0);
    }
}

/// After the search textarea changes, sync the free-entry synthetic item and
/// update selection to the first matching real item (or the free-entry row).
///
/// An empty `free_entry_category` disables the synthetic row entirely (used
/// by pickers like diff-grep that only confirm real matches): items are left
/// untouched and selection is clamped to matches. Callers must check the
/// selection is a real match before confirming (zero matches = no-op).
///
/// Scroll offset is left to the caller when matches exist (key vs paste differ);
/// when search is cleared, `scroll_offset` is reset to 0.
pub fn sync_list_picker_free_entry(core: &mut ListPickerCore, free_entry_category: &str) {
    let new_search = core.search_textarea.lines().join("");
    if free_entry_category.is_empty() {
        if new_search.trim().is_empty() {
            if !core.items.is_empty() {
                core.selected = 0;
            }
            core.scroll_offset = 0;
        } else {
            let matching = list_picker_matching_indices(&core.items, &new_search);
            if let Some(sel) = list_picker_clamp_selection_to_matches(&matching, core.selected) {
                core.selected = sel;
            }
        }
        return;
    }
    remove_free_entry_item(&mut core.items, free_entry_category);

    let new_lower = new_search.to_lowercase();
    if !new_lower.is_empty() {
        let trimmed = new_search.trim().to_string();
        core.items.insert(
            0,
            ListPickerItem {
                value: trimmed.clone(),
                label: trimmed,
                category: free_entry_category.to_string(),
                description: None,
            },
        );

        if let Some(idx) = core
            .items
            .iter()
            .skip(1)
            .position(|i| list_picker_item_matches(i, new_lower.trim()))
        {
            core.selected = idx + 1;
        } else {
            core.selected = 0;
        }
    } else {
        core.selected = 0;
        core.scroll_offset = 0;
    }
}

/// Whether a list-picker row matches the current search (empty search = all).
/// Multi-word queries are order-independent: every whitespace-separated token
/// must appear as a case-insensitive substring of label or value.
pub fn list_picker_item_matches(item: &ListPickerItem, search_lower: &str) -> bool {
    if search_lower.is_empty() {
        return true;
    }
    let label = item.label.to_lowercase();
    let value = item.value.to_lowercase();
    search_lower
        .split_whitespace()
        .all(|tok| label.contains(tok) || value.contains(tok))
}

/// Lowercased whitespace-separated tokens of a picker query, in order.
/// Empty / whitespace-only queries yield no tokens (match-all).
pub fn list_picker_search_tokens(search: &str) -> Vec<String> {
    search
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Whether a `?` command-palette entry matches pre-tokenized `tokens`.
/// Every token must appear in the key or the description (order-free),
/// mirroring the list-picker token-AND behavior. Empty tokens = match-all.
pub fn command_palette_entry_matches(key: &str, description: &str, tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let key_lower = key.to_lowercase();
    let desc_lower = description.to_lowercase();
    tokens
        .iter()
        .all(|tok| key_lower.contains(tok) || desc_lower.contains(tok))
}

/// Byte ranges in `label` covered by any of `tokens` (already lowercased),
/// matched case-insensitively. Sorted, non-overlapping (overlaps merged).
///
/// Ranges index the original `label` so the caller can slice it directly for
/// highlighting. A hit is only reported when the original slice lowercases
/// back to the token, which keeps byte indices valid for non-ASCII text
/// (slices that don't round-trip are skipped).
pub fn list_picker_highlight_ranges(label: &str, tokens: &[String]) -> Vec<(usize, usize)> {
    if tokens.is_empty() || label.is_empty() {
        return Vec::new();
    }
    let lower = label.to_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for tok in tokens {
        if tok.is_empty() {
            continue;
        }
        for (start, _) in lower.match_indices(tok.as_str()) {
            let end = start + tok.len();
            let Some(slice) = label.get(start..end) else {
                continue;
            };
            if slice.to_lowercase() != *tok {
                continue;
            }
            ranges.push((start, end));
        }
    }
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    merged
}

/// Indices into `items` that match `search` (trimmed, case-insensitive,
/// order-independent tokens). Lowercases the query once and each item once.
pub fn list_picker_matching_indices(items: &[ListPickerItem], search: &str) -> Vec<usize> {
    let search_lower = search.trim().to_lowercase();
    if search_lower.is_empty() {
        return (0..items.len()).collect();
    }
    let tokens: Vec<&str> = search_lower.split_whitespace().collect();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            let label = item.label.to_lowercase();
            let value = item.value.to_lowercase();
            tokens
                .iter()
                .all(|tok| label.contains(tok) || value.contains(tok))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Display-row index for `sel` among only the matching items (plus category headers).
pub fn list_picker_filtered_display_idx(
    items: &[ListPickerItem],
    matching: &[usize],
    sel: usize,
) -> usize {
    let mut di = 0usize;
    let mut last_cat = String::new();
    for &ei in matching {
        let Some(item) = items.get(ei) else {
            continue;
        };
        if !item.category.is_empty() && item.category != last_cat {
            di += 1;
            last_cat = item.category.clone();
        }
        if ei == sel {
            return di;
        }
        di += 1;
    }
    di
}

/// Next matching item index after `selected` (cycles within `matching`).
pub fn list_picker_next_match(matching: &[usize], selected: usize) -> Option<usize> {
    if matching.is_empty() {
        return None;
    }
    match matching.iter().position(|&i| i == selected) {
        Some(pos) => Some(matching[(pos + 1) % matching.len()]),
        None => matching
            .iter()
            .copied()
            .find(|&i| i > selected)
            .or_else(|| matching.first().copied()),
    }
}

/// Previous matching item index before `selected` (cycles within `matching`).
pub fn list_picker_prev_match(matching: &[usize], selected: usize) -> Option<usize> {
    if matching.is_empty() {
        return None;
    }
    match matching.iter().position(|&i| i == selected) {
        Some(0) => Some(matching[matching.len() - 1]),
        Some(pos) => Some(matching[pos - 1]),
        None => matching
            .iter()
            .rev()
            .copied()
            .find(|&i| i < selected)
            .or_else(|| matching.last().copied()),
    }
}

/// Keep `selected` on a matching row after the search string changes.
pub fn list_picker_clamp_selection_to_matches(
    matching: &[usize],
    selected: usize,
) -> Option<usize> {
    if matching.is_empty() {
        return None;
    }
    if matching.contains(&selected) {
        Some(selected)
    } else {
        matching.first().copied()
    }
}

/// Resolve the confirm value for a free-entry list picker: the selected item,
/// or the trimmed search text when nothing is selected but search is non-empty.
pub fn list_picker_confirm_value(core: &ListPickerCore) -> Option<String> {
    let search = core.search_textarea.lines().join("");
    if let Some(item) = core.items.get(core.selected) {
        Some(item.value.clone())
    } else if !search.trim().is_empty() {
        Some(search.trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod free_entry_tests {
    use super::*;

    fn core_with(items: Vec<ListPickerItem>, search: &str) -> ListPickerCore {
        let mut ta = make_command_palette_search_textarea();
        if !search.is_empty() {
            ta.insert_str(search);
        }
        ListPickerCore {
            items,
            selected: 0,
            search_textarea: ta,
            scroll_offset: 3,
        }
    }

    fn item(value: &str, category: &str) -> ListPickerItem {
        ListPickerItem {
            value: value.to_string(),
            label: value.to_string(),
            category: category.to_string(),
            description: None,
        }
    }

    #[test]
    fn sync_inserts_free_entry_and_selects_match() {
        let mut core = core_with(
            vec![
                item("main", "Branches"),
                item("feature/path-filter", "Branches"),
                item("v1.0", "Tags"),
            ],
            "path",
        );

        sync_list_picker_free_entry(&mut core, "[path]");

        assert_eq!(core.items.len(), 4);
        assert!(is_free_entry_item(&core.items, "[path]"));
        assert_eq!(core.items[0].value, "path");
        assert_eq!(core.items[0].category, "[path]");
        // First real match after the free-entry row
        assert_eq!(core.selected, 2);
        assert_eq!(core.items[core.selected].value, "feature/path-filter");
    }

    #[test]
    fn sync_selects_free_entry_when_no_match() {
        let mut core = core_with(vec![item("main", "Branches")], "orphan");

        sync_list_picker_free_entry(&mut core, "[author]");

        assert_eq!(core.items.len(), 2);
        assert_eq!(core.selected, 0);
        assert_eq!(core.items[0].value, "orphan");
        assert_eq!(core.items[0].category, "[author]");
    }

    #[test]
    fn preferred_free_entry_stays_selected_when_a_suggestion_matches() {
        let mut core = core_with(vec![item("src/config", "")], "src");

        sync_list_picker_prefer_free_entry(&mut core, "[path]");

        assert_eq!(core.selected, 0);
        assert_eq!(core.items[0].value, "src");
        assert_eq!(core.items[1].value, "src/config");
    }

    #[test]
    fn sync_replaces_previous_free_entry() {
        let mut core = core_with(vec![item("old", "[path]"), item("main", "Branches")], "new");

        sync_list_picker_free_entry(&mut core, "[path]");

        assert_eq!(core.items.len(), 2);
        assert_eq!(core.items[0].value, "new");
        assert!(core.items.iter().filter(|i| i.category == "[path]").count() == 1);
    }

    #[test]
    fn sync_clears_free_entry_and_resets_scroll_when_search_empty() {
        let mut core = core_with(vec![item("typed", "[path]"), item("main", "Branches")], "");
        core.selected = 1;

        sync_list_picker_free_entry(&mut core, "[path]");

        assert_eq!(core.items.len(), 1);
        assert_eq!(core.items[0].value, "main");
        assert_eq!(core.selected, 0);
        assert_eq!(core.scroll_offset, 0);
    }

    #[test]
    fn confirm_value_prefers_selected_item() {
        let mut core = core_with(vec![item("main", "Branches")], "mai");
        sync_list_picker_free_entry(&mut core, "[ref]");
        // selected should be the real match
        let value = list_picker_confirm_value(&core).unwrap();
        assert_eq!(value, "main");
    }

    #[test]
    fn confirm_value_falls_back_to_search_when_empty_list() {
        let core = core_with(vec![], "typed-value");
        assert_eq!(
            list_picker_confirm_value(&core).as_deref(),
            Some("typed-value")
        );
    }

    #[test]
    fn confirm_value_none_when_empty() {
        let core = core_with(vec![], "");
        assert!(list_picker_confirm_value(&core).is_none());
    }

    #[test]
    fn matching_indices_filters_case_insensitively() {
        let items = vec![
            item("src/main.rs", ""),
            item("README.md", ""),
            item("src/gui/mod.rs", ""),
        ];
        assert_eq!(list_picker_matching_indices(&items, "gui"), vec![2]);
        assert_eq!(list_picker_matching_indices(&items, "SRC"), vec![0, 2]);
        assert_eq!(list_picker_matching_indices(&items, ""), vec![0, 1, 2]);
        assert!(list_picker_matching_indices(&items, "zzz").is_empty());
    }

    #[test]
    fn matching_is_order_independent() {
        let items = vec![
            item("KeyCode Enter handling", ""),
            item("Enter KeyCode swapped", ""),
            item("unrelated line", ""),
        ];
        // Both orders match the same two rows.
        assert_eq!(
            list_picker_matching_indices(&items, "KeyCode Enter"),
            vec![0, 1]
        );
        assert_eq!(
            list_picker_matching_indices(&items, "Enter KeyCode"),
            vec![0, 1]
        );
        // Extra whitespace is ignored; all tokens must be present.
        assert_eq!(
            list_picker_matching_indices(&items, "  enter   keycode  "),
            vec![0, 1]
        );
        assert_eq!(
            list_picker_matching_indices(&items, "KeyCode unrelated"),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn highlight_ranges_cover_each_token_in_either_order() {
        let tokens = list_picker_search_tokens("KeyCode Enter");
        assert_eq!(tokens, vec!["keycode".to_string(), "enter".to_string()]);
        // Token order in the query must not matter for the ranges.
        let rev = list_picker_search_tokens("Enter KeyCode");
        for label in ["KeyCode Enter handling", "Enter KeyCode swapped"] {
            let a = list_picker_highlight_ranges(label, &tokens);
            let b = list_picker_highlight_ranges(label, &rev);
            assert_eq!(a, b);
            assert_eq!(a.len(), 2);
            // Ranges slice back to the original-cased text.
            let joined: String = a
                .iter()
                .map(|(s, e)| label[*s..*e].to_string())
                .collect::<Vec<_>>()
                .join("|")
                .to_lowercase();
            assert!(joined.contains("keycode"), "{label} -> {joined}");
            assert!(joined.contains("enter"), "{label} -> {joined}");
        }
        // Overlapping tokens merge into one range.
        let tokens = list_picker_search_tokens("enter ent");
        assert_eq!(
            list_picker_highlight_ranges("Enter here", &tokens),
            vec![(0, 5)]
        );
        // Empty query / no hits.
        assert!(list_picker_highlight_ranges("abc", &[]).is_empty());
        assert!(list_picker_highlight_ranges("abc", &list_picker_search_tokens("zzz")).is_empty());
    }

    #[test]
    fn command_palette_matching_is_order_independent() {
        let tokens = list_picker_search_tokens("diff grep");
        assert!(command_palette_entry_matches(
            "<c-f>",
            "Grep diff contents",
            &tokens
        ));
        let rev = list_picker_search_tokens("grep diff");
        assert!(command_palette_entry_matches(
            "<c-f>",
            "Grep diff contents",
            &rev
        ));
        // All tokens must be present; empty = match-all.
        assert!(!command_palette_entry_matches(
            "<c-f>",
            "Grep diff contents",
            &list_picker_search_tokens("diff zzz")
        ));
        assert!(command_palette_entry_matches(
            "<c-f>",
            "Grep diff contents",
            &[]
        ));
        // Tokens may split across key and description.
        assert!(command_palette_entry_matches(
            "<c-f>",
            "Grep diff contents",
            &list_picker_search_tokens("c-f grep")
        ));
    }

    #[test]
    fn empty_category_disables_synthetic_row_and_clamps_to_matches() {
        // Flat list with empty categories (like the diff-grep dialog): sync
        // must not insert or strip any row.
        let mut core = core_with(
            vec![item("src/main.rs", ""), item("src/gui/mod.rs", "")],
            "gui",
        );
        core.selected = 0;

        sync_list_picker_free_entry(&mut core, "");

        assert_eq!(core.items.len(), 2);
        assert_eq!(core.selected, 1);
        assert_eq!(core.items[core.selected].value, "src/gui/mod.rs");

        // Zero matches: items untouched, selection left for the Enter guard.
        let mut core = core_with(vec![item("src/main.rs", "")], "zzz");
        sync_list_picker_free_entry(&mut core, "");
        assert_eq!(core.items.len(), 1);
        assert_eq!(core.items[0].value, "src/main.rs");

        // Clearing the search resets to the top without touching items.
        let mut core = core_with(vec![item("a", ""), item("b", "")], "");
        core.selected = 1;
        sync_list_picker_free_entry(&mut core, "");
        assert_eq!(core.items.len(), 2);
        assert_eq!(core.selected, 0);
        assert_eq!(core.scroll_offset, 0);

        // The prefer- variant also skips its force-select for empty category.
        let mut core = core_with(vec![item("src/main.rs", "")], "zzz");
        sync_list_picker_prefer_free_entry(&mut core, "");
        assert_eq!(core.items.len(), 1);
    }

    #[test]
    fn next_prev_match_cycle_within_filtered_set() {
        let matching = vec![0usize, 3, 7];
        assert_eq!(list_picker_next_match(&matching, 0), Some(3));
        assert_eq!(list_picker_next_match(&matching, 7), Some(0));
        assert_eq!(list_picker_prev_match(&matching, 7), Some(3));
        assert_eq!(list_picker_prev_match(&matching, 0), Some(7));
        assert_eq!(
            list_picker_clamp_selection_to_matches(&matching, 5),
            Some(0)
        );
        assert_eq!(
            list_picker_clamp_selection_to_matches(&matching, 3),
            Some(3)
        );
    }
}

#[cfg(test)]
mod checklist_free_entry_tests {
    use super::{ChecklistItem, sync_checklist_free_entry};

    fn item(label: &str, checked: bool) -> ChecklistItem {
        ChecklistItem {
            label: label.to_string(),
            checked,
            is_free_entry: false,
        }
    }

    #[test]
    fn inserts_custom_author_at_top() {
        let mut items = vec![item("Alice <a@example.com>", false)];

        sync_checklist_free_entry(&mut items, Some("[author]"), "Bob <b@example.com>");

        assert_eq!(items.len(), 2);
        assert!(items[0].is_free_entry);
        assert_eq!(items[0].label, "Bob <b@example.com>");
        assert!(!items[0].checked);
    }

    #[test]
    fn preserves_checked_state_for_same_custom_value() {
        let mut items = vec![
            ChecklistItem {
                label: "typed".to_string(),
                checked: true,
                is_free_entry: true,
            },
            item("Alice <a@example.com>", false),
        ];

        sync_checklist_free_entry(&mut items, Some("[author]"), "typed");

        assert_eq!(items.len(), 2);
        assert!(items[0].is_free_entry);
        assert!(items[0].checked);
    }

    #[test]
    fn removes_free_entry_when_search_cleared() {
        let mut items = vec![
            ChecklistItem {
                label: "typed".to_string(),
                checked: true,
                is_free_entry: true,
            },
            item("Alice <a@example.com>", true),
        ];

        sync_checklist_free_entry(&mut items, Some("[author]"), "");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Alice <a@example.com>");
        assert!(items[0].checked);
    }
}

#[cfg(test)]
mod email_recipient_tests {
    use super::{
        EmailPatchFocus, build_email_patch_preview, dedupe_email_recipients, email_address_key,
        join_email_recipients, split_email_recipients, validate_email_recipient_candidates,
    };

    #[test]
    fn splits_recipient_list_without_breaking_quoted_names() {
        assert_eq!(
            split_email_recipients(
                "Jens Axboe <axboe@kernel.dk>, \"Doe, Jane\" <jane@example.com>, list@example.org"
            ),
            vec![
                "Jens Axboe <axboe@kernel.dk>",
                "\"Doe, Jane\" <jane@example.com>",
                "list@example.org",
            ]
        );
    }

    #[test]
    fn validates_and_normalizes_visible_email_addresses() {
        assert_eq!(
            email_address_key("Jens Axboe <Axboe@Kernel.DK>"),
            Some("axboe@kernel.dk".to_string())
        );
        assert_eq!(
            email_address_key("list@example.org"),
            Some("list@example.org".to_string())
        );
        assert_eq!(email_address_key("missing-at.example.org"), None);
        assert_eq!(email_address_key("user@example"), None);
        assert_eq!(email_address_key("user @example.org"), None);
        assert_eq!(email_address_key("user@.example.org"), None);
    }

    #[test]
    fn rejects_duplicate_recipients_across_to_and_cc() {
        let to = vec!["Jens Axboe <axboe@kernel.dk>".to_string()];
        let cc = vec!["list@example.org".to_string()];
        assert!(
            validate_email_recipient_candidates(&["AXBOE@kernel.dk".to_string()], &to, &cc, None,)
                .is_err()
        );
        assert!(
            validate_email_recipient_candidates(
                &["new@example.org".to_string(), "NEW@example.org".to_string()],
                &to,
                &cc,
                None,
            )
            .is_err()
        );
        assert!(
            validate_email_recipient_candidates(
                &["Jens <axboe@kernel.dk>".to_string()],
                &to,
                &cc,
                Some((EmailPatchFocus::To, 0)),
            )
            .is_ok()
        );
    }

    #[test]
    fn dedupes_resolved_recipients_with_to_taking_priority() {
        let mut to = vec![
            "Jens Axboe <axboe@kernel.dk>".to_string(),
            "AXBOE@kernel.dk".to_string(),
        ];
        let mut cc = vec![
            "axboe@kernel.dk".to_string(),
            "list@example.org".to_string(),
            "LIST@example.org".to_string(),
        ];
        dedupe_email_recipients(&mut to, &mut cc);
        assert_eq!(to, vec!["Jens Axboe <axboe@kernel.dk>"]);
        assert_eq!(cc, vec!["list@example.org"]);
    }

    #[test]
    fn builds_outgoing_email_preview_from_current_recipients() {
        let preview = build_email_patch_preview(
            "Jazz <jazz@example.com>",
            &["linux-kernel@example.org".to_string()],
            &[
                "Jens Axboe <axboe@kernel.dk>".to_string(),
                "reviewer@example.org".to_string(),
            ],
            "[PATCH] io_uring: test",
            "Thu, 30 Oct 2026 10:00:00 -0700",
            "Commit message\n\n---\n diff --git a/a b/a",
        );
        assert!(
            preview.starts_with("From: Jazz <jazz@example.com>\nTo: linux-kernel@example.org\n")
        );
        assert!(preview.contains("Cc: Jens Axboe <axboe@kernel.dk>,\n    reviewer@example.org\n"));
        assert!(preview.contains("Subject: [PATCH] io_uring: test\n"));
        assert!(preview.ends_with("Commit message\n\n---\n diff --git a/a b/a"));
    }
    #[test]
    fn joins_visible_recipients_for_send_email() {
        let recipients = vec![
            "Jens Axboe <axboe@kernel.dk>".to_string(),
            "list@example.org".to_string(),
        ];
        assert_eq!(
            join_email_recipients(&recipients),
            "Jens Axboe <axboe@kernel.dk>, list@example.org"
        );
    }
}

#[cfg(test)]
mod patch_file_picker_tests {
    use super::{PatchFilePickerState, PatchPickerPreview};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("lazygitrs-{label}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn patch_browser_sorts_directories_then_patch_candidates_then_other_files() {
        let root = temp_dir("patch-browser-sort");
        std::fs::create_dir(root.join("folder")).unwrap();
        std::fs::write(root.join("notes.txt"), "hello\n").unwrap();
        std::fs::write(root.join("fix.patch"), "diff --git a/a b/a\n").unwrap();
        std::fs::write(
            root.join("mailbox"),
            "From deadbeef Mon Sep 17 00:00:00 2001\nSubject: [PATCH] test\n",
        )
        .unwrap();

        let state = PatchFilePickerState::new(root.clone(), root.clone()).unwrap();
        let names: Vec<_> = state
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["folder", "fix.patch", "mailbox", "notes.txt"]);
        assert!(state.entries[1].likely_patch);
        assert!(state.entries[2].likely_patch);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_browser_half_page_scroll_animates_active_file_selection() {
        let root = temp_dir("patch-browser-picker-scroll");
        for index in 0..30 {
            std::fs::write(
                root.join(format!("{index:02}.patch")),
                format!("line {index}\n"),
            )
            .unwrap();
        }

        let mut state = PatchFilePickerState::new(root.clone(), root.clone()).unwrap();
        assert_eq!(state.selected, 0);

        state.queue_picker_half_page_scroll(1, 10);
        // First row advances synchronously; four rows remain for animation.
        assert_eq!(state.selected, 1);
        assert_eq!(state.picker_scroll_remaining, 4);
        for _ in 0..4 {
            state.tick_picker_scroll();
        }
        assert_eq!(state.selected, 5);
        assert!(!state.picker_scroll_active());

        state.queue_picker_half_page_scroll(-1, 10);
        assert_eq!(state.selected, 4);
        for _ in 0..4 {
            state.tick_picker_scroll();
        }
        assert_eq!(state.selected, 0);
        assert!(!state.picker_scroll_active());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_browser_half_page_scroll_clamps_to_available_files() {
        let root = temp_dir("patch-browser-short-picker-scroll");
        for index in 0..3 {
            std::fs::write(root.join(format!("{index}.patch")), "diff --git a/a b/a\n").unwrap();
        }

        let mut state = PatchFilePickerState::new(root.clone(), root.clone()).unwrap();
        state.queue_picker_half_page_scroll(1, 20);
        while state.picker_scroll_active() {
            state.tick_picker_scroll();
        }
        assert_eq!(state.selected, 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_browser_enters_directories_and_builds_live_text_preview() {
        let root = temp_dir("patch-browser-nav");
        let child = root.join("incoming");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("change.diff"), "diff --git a/a b/a\n+new\n").unwrap();

        let mut state = PatchFilePickerState::new(root.clone(), root.clone()).unwrap();
        assert!(state.entries[state.selected].is_dir);
        assert!(state.enter_selected().unwrap().is_none());
        assert_eq!(state.current_dir, child.canonicalize().unwrap());
        assert_eq!(state.entries.len(), 1);
        assert!(matches!(
            state.preview,
            PatchPickerPreview::Text {
                likely_patch: true,
                ..
            }
        ));

        std::fs::remove_dir_all(root).unwrap();
    }
}
