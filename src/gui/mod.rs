mod command_palette;
pub mod context;
pub mod controller;
pub mod input;
mod keyboard;
pub mod layout;
pub mod modes;
mod mouse;
pub mod popup;
mod popup_input;
pub mod presentation;
mod refresh;
mod runtime;
pub mod scroll;
pub mod views;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{Command, cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::config::keybindings::parse_key;
use crate::config::{AppConfig, AppState};
use crate::git::remote::{PullOutcome, PullStrategy};
use crate::git::{DEFAULT_COMMIT_LIMIT, GitCommands, HeadState, MODEL_PART_COUNT, ModelPart};
use crate::model::Model;
use crate::model::file_tree::{CommitFileTreeNode, FileTreeNode, build_file_tree};
use crate::os::platform::Platform;
use crate::pager::side_by_side::{
    DiffPanel, DiffPanelLayout, DiffViewLayout, DiffViewState, TextSelection, is_rename_only_diff,
};

use self::context::{ContextId, ContextManager, SideWindow};
use self::input::InputReader;
use self::layout::LayoutState;
use self::modes::diff_mode::DiffModeState;
use self::modes::patch_building::PatchBuildingState;
use self::modes::rebase_mode::{EntryStatus, RebaseModeState, RebasePhase};
use self::popup::{CommandAction, CommandEntry, CommandSection};
use self::popup::{
    MessageKind, PopupState, list_picker_clamp_selection_to_matches,
    list_picker_filtered_display_idx, list_picker_matching_indices, list_picker_next_match,
    list_picker_prev_match,
};

/// Compute the visible list height for a list picker popup, given terminal height.
/// Must match the rendering formula: popup 60% height, minus borders (2), search bar + sep + hint (3).
const COMMIT_DETAILS_CACHE_ENTRIES: usize = 512;
const ACTION_TOAST_DURATION: Duration = Duration::from_millis(1800);

fn bounded_cache_insert<T>(
    cache: &mut HashMap<String, T>,
    order: &mut VecDeque<String>,
    key: String,
    value: T,
) {
    if cache.contains_key(&key) {
        cache.insert(key.clone(), value);
        if let Some(pos) = order.iter().position(|existing| existing == &key) {
            order.remove(pos);
        }
        order.push_back(key);
        return;
    }

    while cache.len() >= COMMIT_DETAILS_CACHE_ENTRIES {
        let Some(oldest) = order.pop_front() else {
            break;
        };
        cache.remove(&oldest);
    }
    cache.insert(key.clone(), value);
    order.push_back(key);
}

fn context_claims_global_repo_key(
    active: ContextId,
    key: KeyEvent,
    kb: &crate::config::KeybindingConfig,
) -> bool {
    matches!(active, ContextId::Tags) && key.code == KeyCode::Char('P')
        || matches_key(key, &kb.universal.pull_files)
            && !matches!(active, ContextId::Status | ContextId::Branches | ContextId::Remotes)
}

/// Return whether a streamed model update can change the preview for the
/// currently active context. Metadata-only updates must not invalidate an
/// otherwise identical diff: doing so restarts the mutable Files preview even
/// though its source did not change.
fn streamed_model_part_affects_diff(active: ContextId, part: &ModelPart) -> bool {
    match active {
        ContextId::Files => matches!(part, ModelPart::Files(_)),
        ContextId::Commits => matches!(part, ModelPart::Commits(_)),
        ContextId::Reflog => matches!(part, ModelPart::Reflog(_)),
        ContextId::Stash => matches!(part, ModelPart::Stash(_)),
        // Branch-commit and commit-file subviews are refreshed after the model
        // stream completes. Those async refreshes set needs_diff_refresh when
        // their actual backing data arrives, so generic model parts do not need
        // to invalidate them here. Other sidebar contexts do not render a diff.
        _ => false,
    }
}

fn list_picker_visible_height(terminal_height: usize) -> usize {
    let popup_h = (terminal_height * 60 / 100)
        .max(10)
        .min(terminal_height.saturating_sub(4));
    popup_h.saturating_sub(2).saturating_sub(3)
}

/// Keep the selection in view after Up/Down (including wrap last↔first).
fn list_picker_scroll_after_nav(
    core: &mut crate::gui::popup::ListPickerCore,
    matching: &[usize],
    list_height: usize,
    moved_down: bool,
) {
    let sdi = list_picker_filtered_display_idx(&core.items, matching, core.selected);
    if moved_down {
        if matching.first() == Some(&core.selected) {
            core.scroll_offset = 0;
        } else if sdi >= core.scroll_offset + list_height {
            core.scroll_offset = sdi.saturating_sub(list_height - 1);
        }
    } else if matching.last() == Some(&core.selected) {
        core.scroll_offset = sdi.saturating_sub(list_height.saturating_sub(1));
    } else if matching.first() == Some(&core.selected) {
        core.scroll_offset = 0;
    } else if sdi <= core.scroll_offset {
        core.scroll_offset = sdi.saturating_sub(1);
    }
}

/// Shared mouse scroll/click handling for free-entry list pickers (RefPicker, ListPicker).
fn handle_list_picker_mouse(
    core: &mut crate::gui::popup::ListPickerCore,
    mouse: crossterm::event::MouseEvent,
    layout_width: u16,
    layout_height: u16,
) {
    use crossterm::event::{MouseButton, MouseEventKind};

    let search = core.search_textarea.lines().join("");
    let matching = list_picker_matching_indices(&core.items, &search);
    let h = layout_height as usize;
    let lh = list_picker_visible_height(h);
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(prev) = list_picker_prev_match(&matching, core.selected) {
                core.selected = prev;
            }
            list_picker_scroll_after_nav(core, &matching, lh, false);
        }
        MouseEventKind::ScrollDown => {
            if let Some(next) = list_picker_next_match(&matching, core.selected) {
                core.selected = next;
            }
            list_picker_scroll_after_nav(core, &matching, lh, true);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Click to select an item in the list picker
            let area = ratatui::layout::Rect::new(0, 0, layout_width, layout_height);
            let popup_width = (area.width * 60 / 100).min(60).max(30);
            let max_popup = (area.height * 60 / 100).max(10);
            let popup_height = max_popup.min(area.height.saturating_sub(4));
            let x = (area.width.saturating_sub(popup_width)) / 2;
            let y = (area.height.saturating_sub(popup_height)) / 2;
            let inner_y = y + 1;
            let list_start = inner_y + 2;
            let inner_height = popup_height.saturating_sub(2);
            let list_height = inner_height.saturating_sub(3) as usize;

            if mouse.row >= list_start
                && mouse.row < list_start + list_height as u16
                && mouse.column >= x
                && mouse.column < x + popup_width
            {
                let row_in_list = (mouse.row - list_start) as usize;
                // Build filtered display rows (headers + matching items)
                let has_categories = matching
                    .iter()
                    .any(|&i| core.items.get(i).is_some_and(|it| !it.category.is_empty()));
                let mut display: Vec<(bool, Option<usize>)> = Vec::new();
                if has_categories {
                    let mut last_cat = String::new();
                    for &ei in &matching {
                        let Some(item) = core.items.get(ei) else {
                            continue;
                        };
                        if !item.category.is_empty() && item.category != last_cat {
                            display.push((true, None));
                            last_cat = item.category.clone();
                        }
                        display.push((false, Some(ei)));
                    }
                } else {
                    for &ei in &matching {
                        display.push((false, Some(ei)));
                    }
                }
                let max_scroll = display.len().saturating_sub(list_height);
                let effective_scroll = core.scroll_offset.min(max_scroll);
                let display_idx = effective_scroll + row_in_list;
                if let Some((is_header, item_idx)) = display.get(display_idx) {
                    if !*is_header {
                        if let Some(ei) = item_idx {
                            core.selected = *ei;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

pub type Term = Terminal<CrosstermBackend<crate::os::tty::TuiOutput>>;
const COMMIT_DETAILS_DEBOUNCE: Duration = Duration::from_millis(120);
const MAX_CONCURRENT_DIFF_JOBS: usize = 2;
const DIFF_PREVIEW_CACHE_ENTRIES: usize = 24;
const DIFF_PREVIEW_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_DIFF_BYTES: usize = 8 * 1024 * 1024;
/// How many diffs below/above the selection to warm in the preview cache.
const CHECKOUT_MODEL_PART_COUNT: usize = MODEL_PART_COUNT - 4;
const HISTORY_MODEL_PART_COUNT: usize = MODEL_PART_COUNT - 4;
const DIFF_PREFETCH_AHEAD: usize = 3;
const DIFF_PREFETCH_BEHIND: usize = 1;
/// At most this many prefetch loads queued or running at once.
const DIFF_PREFETCH_INFLIGHT_MAX: usize = 2;
const DIFF_PREFETCH_WORKERS: usize = 2;
/// Warm commit shortstats ahead of the cursor so the details panel does not
/// reveal the async metadata load during normal j/k navigation.
const COMMIT_STAT_PREFETCH_AHEAD: usize = 6;
const COMMIT_STAT_PREFETCH_BEHIND: usize = 2;
const COMMIT_STAT_PREFETCH_INFLIGHT_MAX: usize = 4;
const COMMIT_STAT_PREFETCH_WORKERS: usize = 2;
const COMMIT_STAT_PREFETCH_BATCH_MAX: usize = 4;
const COMMIT_STAT_PREFETCH_BATCH_WAIT: Duration = Duration::from_millis(2);
const FETCH_MODEL_PART_COUNT: usize = 5;
const PUSH_MODEL_PART_COUNT: usize = 3;
const PERIODIC_MODEL_PART_COUNT: usize = 2;
/// Keep speculative auto-fetch out of the way of interactive work.
const AUTO_FETCH_IDLE_GRACE: Duration = Duration::from_secs(2);
/// After an allocation-heavy interaction burst, return unused mimalloc pages
/// to the OS once the UI and its background workers have been quiet for a while.
const MEMORY_COLLECT_IDLE_GRACE: Duration = Duration::from_secs(10);

fn plain_char_key(key: KeyEvent, expected: char) -> bool {
    let modifiers = if expected.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    key.code == KeyCode::Char(expected) && key.modifiers == modifiers
}

fn has_command_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::META)
}

fn commit_stat_prefetch_indices(len: usize, selected: usize) -> Vec<usize> {
    if len == 0 || selected >= len {
        return Vec::new();
    }
    let mut indices = Vec::with_capacity(COMMIT_STAT_PREFETCH_AHEAD + COMMIT_STAT_PREFETCH_BEHIND);
    for step in 1..=COMMIT_STAT_PREFETCH_AHEAD {
        let index = selected.saturating_add(step);
        if index >= len {
            break;
        }
        indices.push(index);
    }
    for step in 1..=COMMIT_STAT_PREFETCH_BEHIND {
        let Some(index) = selected.checked_sub(step) else {
            break;
        };
        indices.push(index);
    }
    indices
}

pub(crate) fn textarea_input(
    textarea: &mut tui_textarea::TextArea<'static>,
    key: KeyEvent,
) -> bool {
    use tui_textarea::CursorMove;

    let cmd = has_command_modifier(key.modifiers);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Cmd+Left/Right (and Ctrl+Left/Right): line head/end.
        // Many macOS terminals remap Cmd+arrows to Home/End, so handle those too.
        KeyCode::Left if cmd || ctrl => textarea.move_cursor(CursorMove::Head),
        KeyCode::Right if cmd || ctrl => textarea.move_cursor(CursorMove::End),
        KeyCode::Home => textarea.move_cursor(CursorMove::Head),
        KeyCode::End => textarea.move_cursor(CursorMove::End),
        // Cmd/Ctrl+Backspace: delete to start of line
        KeyCode::Backspace if cmd => {
            textarea.delete_line_by_head();
        }
        // Option/Alt+Left/Right: move by word
        KeyCode::Left if alt => textarea.move_cursor(CursorMove::WordBack),
        KeyCode::Right if alt => textarea.move_cursor(CursorMove::WordForward),
        // Option/Alt+Backspace: delete previous word
        KeyCode::Backspace if alt => {
            let (row, col) = textarea.cursor();
            textarea.move_cursor(CursorMove::WordBack);
            let (new_row, new_col) = textarea.cursor();
            if new_row == row {
                for _ in new_col..col {
                    textarea.delete_next_char();
                }
            } else {
                textarea.move_cursor(CursorMove::Jump(row as u16, col as u16));
                for _ in 0..=col {
                    textarea.delete_char();
                }
            }
        }
        KeyCode::Char(_) if cmd => return false,
        KeyCode::Char('a') if ctrl => textarea.move_cursor(CursorMove::Head),
        KeyCode::Char('e') if ctrl => textarea.move_cursor(CursorMove::End),
        KeyCode::Char('u') if ctrl => {
            textarea.delete_line_by_head();
        }
        // Fall through: tui-textarea handles Alt+b/f/h/l and plain chars
        _ => return textarea.input(key),
    };
    true
}

/// A completed diff result from the background thread.
pub(crate) struct DiffResult {
    /// Generation counter to discard stale results.
    pub generation: u64,
    /// The diff key this result corresponds to.
    pub diff_key: String,
    /// The computed diff data: (filename, old_content, new_content) or None for empty.
    pub payload: DiffPayload,
    /// True for speculative neighbor loads: applied if the user is already
    /// waiting on this key, cached for later otherwise. Never generation-gated.
    pub is_prefetch: bool,
}

pub(crate) enum DiffPayload {
    /// Pre-parsed diff ready to apply (parsing done on background thread).
    Parsed(crate::pager::side_by_side::ParsedDiff),
    /// No diff to show.
    Empty,
}

enum CompareLoadEvent {
    Files {
        generation: u64,
        ref_a: String,
        ref_b: String,
        result: Result<Vec<crate::model::CommitFile>>,
    },
    Commits {
        generation: u64,
        ref_a: String,
        ref_b: String,
        result: Result<Vec<crate::model::Commit>>,
    },
}

/// Return large temporary allocations to glibc after workflows that are known
/// to allocate substantial short-lived buffers. Other allocators/platforms
/// already reclaim/reuse this memory appropriately, so this is a no-op there.
#[inline]
pub(crate) fn trim_process_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        // SAFETY: malloc_trim takes no pointers and only asks glibc to release
        // unused arena pages back to the OS.
        libc::malloc_trim(0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteOpKind {
    Push,
    Pull,
    PullIntegrate,
    Fetch,
    FetchAndCheckout,
    /// A single newly-created commit appended to the current HEAD.
    ///
    /// Unlike amend/reword/rebase, this can update the loaded commit
    /// model incrementally without rebuilding the current HEAD history.
    LocalAppend,
    /// HEAD was rewritten in place (amend or reword).
    LocalRewriteHead,
    LocalHistory,
    ResetToUpstream,
    /// `git send-email` is network-adjacent work that should share the
    /// serialized remote lane, but it does not mutate repository refs/model.
    Email(bool),
    Other,
}

enum RemoteOpResult {
    Done,
    Pull(PullOutcome),
    Backup(String),
}

type RemoteOpFn = Box<dyn FnOnce(&GitCommands) -> Result<RemoteOpResult> + Send>;

struct PendingRemoteOp {
    kind: RemoteOpKind,
    title: String,
    op: RemoteOpFn,
}

struct RemoteOpCompletion {
    backup_branch: Option<String>,
    head: Option<HeadState>,
    pull_outcome: Option<PullOutcome>,
    /// Some(false) proves that the network operation moved no repository refs.
    /// None means the operation does not use ref-change short-circuiting or
    /// the pre-operation snapshot could not be captured.
    refs_changed: Option<bool>,
}

struct DiffJob {
    generation: u64,
    diff_key: String,
    load: Box<dyn FnOnce() -> DiffPayload + Send>,
}

enum DiffSchedulerEvent {
    Job(DiffJob),
    Complete,
}

struct CachedDiffPreview {
    key: String,
    view: DiffViewState,
    estimated_bytes: usize,
}

#[derive(Default)]
struct DiffPreviewCache {
    entries: VecDeque<CachedDiffPreview>,
    estimated_bytes: usize,
}

impl DiffPreviewCache {
    fn insert(&mut self, key: String, mut view: DiffViewState) {
        for image in view.inline_images.values_mut() {
            image.unload_for_cache();
        }
        self.remove(&key);
        let estimated_bytes = estimate_diff_view_bytes(&view);
        if estimated_bytes > MAX_CACHED_DIFF_BYTES {
            return;
        }

        while self.entries.len() >= DIFF_PREVIEW_CACHE_ENTRIES
            || self.estimated_bytes.saturating_add(estimated_bytes) > DIFF_PREVIEW_CACHE_BYTES
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.estimated_bytes = self.estimated_bytes.saturating_sub(evicted.estimated_bytes);
        }

        self.estimated_bytes = self.estimated_bytes.saturating_add(estimated_bytes);
        self.entries.push_back(CachedDiffPreview {
            key,
            view,
            estimated_bytes,
        });
    }

    fn take(&mut self, key: &str) -> Option<DiffViewState> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.entries.remove(index)?;
        self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
        Some(entry.view)
    }

    fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|entry| entry.key == key)
    }

    /// Drop entries whose content can go stale on refresh (working-tree
    /// diffs), keeping hash-keyed commit/stash diffs that never change.
    fn retain_immutable(&mut self) {
        self.entries
            .retain(|entry| diff_key_is_immutable(&entry.key));
        self.estimated_bytes = self.entries.iter().map(|e| e.estimated_bytes).sum();
    }

    fn remove(&mut self, key: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            if let Some(entry) = self.entries.remove(index) {
                self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.estimated_bytes = 0;
    }

    /// Drop cached previews whose keys share a mutable-source prefix.
    ///
    /// Files previews are keyed by path for fast revisits, but their contents
    /// can change without the path changing (stage/unstage, hunk edits, etc.).
    /// Removing them when an authoritative file-status snapshot arrives keeps
    /// an old working-tree preview from being restored after the index changed.
    fn remove_prefix(&mut self, prefix: &str) {
        self.entries.retain(|entry| !entry.key.starts_with(prefix));
        self.estimated_bytes = self.entries.iter().map(|entry| entry.estimated_bytes).sum();
    }
}

fn estimate_diff_view_bytes(view: &DiffViewState) -> usize {
    let line_bytes = view.lines.iter().fold(0usize, |total, line| {
        let text_bytes = line
            .old_line
            .as_ref()
            .map(|(_, text)| text.len())
            .unwrap_or(0)
            .saturating_add(
                line.new_line
                    .as_ref()
                    .map(|(_, text)| text.len())
                    .unwrap_or(0),
            );
        let segment_bytes = line
            .old_segments
            .iter()
            .chain(line.new_segments.iter())
            .flatten()
            .map(|segment| segment.text.len())
            .sum::<usize>();
        total
            .saturating_add(text_bytes)
            .saturating_add(segment_bytes)
    });

    view.filename
        .len()
        .saturating_add(view.old_content.len())
        .saturating_add(view.new_content.len())
        .saturating_add(line_bytes)
        .saturating_mul(2)
        .saturating_add(
            view.inline_images
                .values()
                .map(|image| image.estimated_bytes())
                .sum::<usize>(),
        )
        .saturating_add(
            view.image_preview
                .as_ref()
                .map_or(0, |p| p.estimated_bytes()),
        )
}

type BackgroundJob = Box<dyn FnOnce() + Send>;
pub(crate) type UiJob = Box<dyn FnOnce(&mut Gui) + Send>;

fn spawn_index_mutation_worker(
    rx: mpsc::Receiver<IndexMutationCommand>,
    tx: mpsc::Sender<IndexMutationResult>,
    busy: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            busy.store(true, Ordering::Release);
            let mut mutation_error = None;
            let mut generation = first.generation;
            let mut current_git = first.git;
            let mut current = Some(first.mutation);

            loop {
                let mutation = current.take().expect("index mutation present");
                let result = match mutation {
                    IndexMutation::Paths { paths, stage } => {
                        if stage {
                            current_git.stage_files(&paths)
                        } else {
                            current_git.unstage_files(&paths)
                        }
                    }
                    IndexMutation::All { stage } => {
                        if stage {
                            current_git.stage_all()
                        } else {
                            current_git.unstage_all()
                        }
                    }
                };
                if let Err(err) = result
                    && mutation_error.is_none()
                {
                    mutation_error = Some(format!("{err:#}"));
                }

                match rx.try_recv() {
                    Ok(next) => {
                        generation = next.generation;
                        current_git = next.git;
                        current = Some(next.mutation);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let files = current_git.refresh_files_status_only();
            let totals = files.as_ref().ok().and_then(|f| current_git.worktree_diff_totals(f).ok());
            busy.store(false, Ordering::Release);
            if tx
                .send(IndexMutationResult {
                    generation,
                    files,
                    totals,
                    mutation_error,
                })
                .is_err()
            {
                break;
            }
        }
        busy.store(false, Ordering::Release);
    });
}

fn spawn_diff_scheduler(
    rx: mpsc::Receiver<DiffSchedulerEvent>,
    scheduler_tx: mpsc::Sender<DiffSchedulerEvent>,
    result_tx: mpsc::Sender<DiffResult>,
    generation: Arc<AtomicU64>,
) {
    std::thread::spawn(move || {
        let mut active_jobs = 0usize;
        let mut pending_job: Option<DiffJob> = None;

        while let Ok(event) = rx.recv() {
            match event {
                DiffSchedulerEvent::Job(job) => {
                    if generation.load(Ordering::Relaxed) != job.generation {
                        continue;
                    }
                    if active_jobs < MAX_CONCURRENT_DIFF_JOBS {
                        active_jobs += 1;
                        spawn_diff_job(
                            job,
                            result_tx.clone(),
                            scheduler_tx.clone(),
                            Arc::clone(&generation),
                        );
                    } else {
                        pending_job = Some(job);
                    }
                }
                DiffSchedulerEvent::Complete => {
                    active_jobs = active_jobs.saturating_sub(1);
                    if let Some(job) = pending_job.take() {
                        if generation.load(Ordering::Relaxed) == job.generation {
                            active_jobs += 1;
                            spawn_diff_job(
                                job,
                                result_tx.clone(),
                                scheduler_tx.clone(),
                                Arc::clone(&generation),
                            );
                        }
                    }
                }
            }
        }
    });
}

fn spawn_diff_job(
    job: DiffJob,
    result_tx: mpsc::Sender<DiffResult>,
    scheduler_tx: mpsc::Sender<DiffSchedulerEvent>,
    generation: Arc<AtomicU64>,
) {
    std::thread::spawn(move || {
        if generation.load(Ordering::Relaxed) == job.generation {
            let payload = (job.load)();
            if generation.load(Ordering::Relaxed) == job.generation {
                let _ = result_tx.send(DiffResult {
                    generation: job.generation,
                    diff_key: job.diff_key,
                    payload,
                    is_prefetch: false,
                });
            }
        }
        let _ = scheduler_tx.send(DiffSchedulerEvent::Complete);
    });
}

/// Diff keys derived from a commit or stash hash: the content can never
/// change, so caches keyed this way survive refreshes and never need a
/// same-key reload.
fn diff_key_is_immutable(key: &str) -> bool {
    key.starts_with("Commits:")
        || key.starts_with("Reflog:")
        || key.starts_with("BranchCommits:")
        || key.starts_with("Stash:")
}

struct DiffPrefetchJob {
    diff_key: String,
    load: Box<dyn FnOnce() -> DiffPayload + Send>,
}

/// Low-priority lane that warms the preview cache with neighbor diffs.
/// Every job MUST produce a result: `begin_diff_request` skips spawning an
/// interactive job when a prefetch for the same key is in flight, and waits
/// for this result instead.
fn spawn_diff_prefetch_workers(
    rx: mpsc::Receiver<DiffPrefetchJob>,
    result_tx: mpsc::Sender<DiffResult>,
) {
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..DIFF_PREFETCH_WORKERS {
        let rx = Arc::clone(&rx);
        let result_tx = result_tx.clone();
        std::thread::spawn(move || {
            loop {
                let job = match rx.lock() {
                    Ok(guard) => guard.recv(),
                    Err(_) => return,
                };
                let Ok(job) = job else { return };
                let payload = (job.load)();
                let _ = result_tx.send(DiffResult {
                    generation: 0,
                    diff_key: job.diff_key,
                    payload,
                    is_prefetch: true,
                });
            }
        });
    }
}

struct CommitStatPrefetchJob {
    generation: u64,
    hash: String,
}

struct CommitStatPrefetchResult {
    generation: u64,
    hash: String,
    result: Result<crate::model::commit::CommitStat>,
}

fn spawn_commit_stat_prefetch_workers(
    rx: mpsc::Receiver<CommitStatPrefetchJob>,
    result_tx: mpsc::Sender<CommitStatPrefetchResult>,
    git: Arc<GitCommands>,
) {
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..COMMIT_STAT_PREFETCH_WORKERS {
        let rx = Arc::clone(&rx);
        let result_tx = result_tx.clone();
        let git = Arc::clone(&git);
        std::thread::spawn(move || {
            loop {
                let jobs = match rx.lock() {
                    Ok(guard) => {
                        let Ok(first) = guard.recv() else { return };
                        let mut jobs = vec![first];
                        // The UI enqueues neighboring hashes back-to-back. Give
                        // that burst a tiny coalescing window so a worker that
                        // wakes on the first send does not immediately launch a
                        // one-hash Git process before the rest reach the queue.
                        if jobs.len() < COMMIT_STAT_PREFETCH_BATCH_MAX {
                            match guard.recv_timeout(COMMIT_STAT_PREFETCH_BATCH_WAIT) {
                                Ok(job) => jobs.push(job),
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => {}
                            }
                        }
                        while jobs.len() < COMMIT_STAT_PREFETCH_BATCH_MAX {
                            match guard.try_recv() {
                                Ok(job) => jobs.push(job),
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => break,
                            }
                        }
                        jobs
                    }
                    Err(_) => return,
                };

                let hashes = jobs.iter().map(|job| job.hash.clone()).collect::<Vec<_>>();
                let batch = git.commit_stats(&hashes);

                for job in jobs {
                    let result = match &batch {
                        Ok(stats) => Ok(stats.get(&job.hash).copied().unwrap_or_default()),
                        // Preserve the old behavior if the batched command could
                        // not even be launched/read for some unusual reason.
                        Err(_) => git.commit_stat(&job.hash),
                    };
                    let _ = result_tx.send(CommitStatPrefetchResult {
                        generation: job.generation,
                        hash: job.hash,
                        result,
                    });
                }
            }
        });
    }
}

/// Load + parse one commit's diff — shared by interactive loads and prefetch
/// so both produce byte-identical payloads for the same key.
fn commit_diff_payload(git: &GitCommands, hash: &str, label_prefix: &str) -> DiffPayload {
    if let Ok(diff) = git.diff_commit(hash) {
        let filename = format!("{}:{}", label_prefix, &hash[..7.min(hash.len())]);
        DiffPayload::Parsed(DiffViewState::parse_diff_output(&filename, &diff, 4, false))
    } else {
        DiffPayload::Empty
    }
}

/// Interactive commit preview loader with a cancellation checkpoint between
/// Git I/O and expensive parsing. Rapid j/k navigation can make a running Git
/// command stale while it is producing stdout; in that case there is no value
/// in turning a potentially huge diff into DiffLines/highlighters just for
/// `spawn_diff_job` to discard it afterwards.
fn commit_diff_payload_for_generation(
    git: &GitCommands,
    hash: &str,
    label_prefix: &str,
    generation_counter: &AtomicU64,
    expected_generation: u64,
) -> DiffPayload {
    let Ok(diff) = git.diff_commit(hash) else {
        return DiffPayload::Empty;
    };
    if generation_counter.load(Ordering::Relaxed) != expected_generation {
        return DiffPayload::Empty;
    }

    let filename = format!("{}:{}", label_prefix, &hash[..7.min(hash.len())]);
    DiffPayload::Parsed(DiffViewState::parse_diff_output(&filename, &diff, 4, false))
}

fn stash_diff_payload(git: &GitCommands, index: usize) -> DiffPayload {
    match git.stash_diff(index) {
        Ok(diff) if diff.is_empty() => DiffPayload::Empty,
        Ok(diff) => {
            let filename = format!("stash@{{{}}}", index);
            let exists = git.repo_path().join(&filename).exists();
            DiffPayload::Parsed(DiffViewState::parse_diff_output(
                &filename, &diff, 4, exists,
            ))
        }
        Err(_) => DiffPayload::Empty,
    }
}

fn spawn_latest_background_worker(rx: mpsc::Receiver<BackgroundJob>) {
    std::thread::spawn(move || {
        while let Ok(mut job) = rx.recv() {
            loop {
                match rx.recv_timeout(COMMIT_DETAILS_DEBOUNCE) {
                    Ok(newer_job) => job = newer_job,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            job();
        }
    });
}

struct AiCommitJob {
    generation: u64,
    cancel: Arc<AtomicBool>,
    cancel_armed_at: Option<Instant>,
}

struct AiCommitResult {
    generation: u64,
    result: Result<Option<String>>,
}

#[derive(Debug, Clone)]
enum AiCommitSource {
    Staged,
    Commit(String),
}

enum IndexMutation {
    Paths { paths: Vec<String>, stage: bool },
    All { stage: bool },
}

struct IndexMutationCommand {
    generation: u64,
    git: Arc<GitCommands>,
    mutation: IndexMutation,
}

struct IndexMutationResult {
    generation: u64,
    files: Result<Vec<crate::model::File>>,
    totals: Option<(usize, usize)>,
    mutation_error: Option<String>,
}

struct CommitPageResult {
    generation: u64,
    /// When true, replace the current window. Otherwise append/prepend.
    replace: bool,
    prepend: bool,
    skip: usize,
    limit: usize,
    result: Result<Vec<crate::model::Commit>>,
}

const COMMIT_PAGE_PREFETCH_THRESHOLD: usize = 100;
const MAX_LOADED_COMMITS: usize = 3000;
const PULL_MODEL_PART_COUNT: usize = MODEL_PART_COUNT - 2;

pub struct Gui {
    pub config: Arc<AppConfig>,
    pub git: Arc<GitCommands>,
    pub model: Arc<Mutex<Model>>,
    pub context_mgr: ContextManager,
    pub layout: LayoutState,
    pub popup: PopupState,
    shell_command_job: Option<controller::custom_commands::RunningCommand>,
    /// Refresh notifications delayed by a shell modal, in arrival order.
    deferred_notifications: VecDeque<PopupState>,
    deferred_notification_visible: bool,
    pub diff_view: DiffViewState,
    /// Cached graph layouts used to render only the visible commit rows.
    commit_list_cache: presentation::commits::CommitListCache,
    pub command_log: crate::os::cmd::CommandLog,
    pub show_command_log: bool,
    pub should_quit: bool,
    pub needs_refresh: bool,
    pub needs_files_refresh: bool,
    pub needs_diff_refresh: bool,
    pub search_query: String,
    /// Whether search input mode is active (typing into search bar).
    pub search_active: bool,
    /// Indices of items matching the current search in the active panel.
    pub search_matches: Vec<usize>,
    /// Current position within search_matches.
    pub search_match_idx: usize,
    pub screen_mode: ScreenMode,
    /// True while the user is dragging the sidebar divider with the mouse.
    sidebar_resizing: bool,
    /// Portrait-only: add this to the mouse row when mapping to side height so
    /// grabs on the expanded panel bottom (above trailing collapsed rows) and
    /// the main/diff top border share one continuous drag.
    sidebar_resize_row_offset: u16,
    pub show_file_tree: bool,
    /// Cached file tree nodes — rebuilt on refresh when tree view is active.
    pub file_tree_nodes: Vec<FileTreeNode>,
    /// Set of collapsed directory paths in the file tree.
    pub collapsed_dirs: HashSet<String>,
    /// Whether the diff/main panel is focused (entered via Enter on a file).
    pub diff_focused: bool,
    /// Whether a diff is currently being loaded on a background thread.
    pub diff_loading: bool,
    /// When the current diff load started (for delayed "Loading..." display).
    pub(crate) diff_loading_since: Option<Instant>,
    /// Track what we last loaded a diff for, to avoid reloading on every frame.
    last_diff_key: String,
    /// Generation counter — incremented on each diff request, used to discard stale results.
    pub(crate) diff_generation: Arc<AtomicU64>,
    /// Sender for background diff loading.
    diff_rx: mpsc::Receiver<DiffResult>,
    /// Bounded scheduler: starts immediately, caps parallelism, and retains
    /// only the newest overflow request while navigation is rapid.
    diff_scheduler_tx: mpsc::Sender<DiffSchedulerEvent>,
    /// Recently completed parsed previews, moved in and out for instant revisits.
    diff_preview_cache: DiffPreviewCache,
    /// The diff key whose content `diff_view` currently shows. Stays on the
    /// outgoing key while a newer selection loads (stale content is kept
    /// visible instead of blanking the pane), and is how the outgoing view
    /// finds its slot in the preview cache when the replacement arrives.
    displayed_diff_key: String,
    /// Sender for the speculative neighbor-diff lane.
    diff_prefetch_tx: mpsc::Sender<DiffPrefetchJob>,
    /// Keys with a prefetch queued or running. An interactive request for one
    /// of these waits for the prefetch result instead of duplicating the work.
    diff_prefetch_inflight: HashSet<String>,
    /// Low-priority shortstat prefetch lane for nearby commits.
    commit_stat_prefetch_tx: mpsc::Sender<CommitStatPrefetchJob>,
    commit_stat_prefetch_rx: mpsc::Receiver<CommitStatPrefetchResult>,
    commit_stat_prefetch_inflight: HashSet<String>,
    /// Invalidates prefetch results when switching repositories.
    commit_stat_prefetch_generation: u64,
    /// Receiver for AI commit message generation results.
    ai_commit_rx: mpsc::Receiver<AiCommitResult>,
    /// Sender cloned into background threads for AI commit generation.
    ai_commit_tx: mpsc::Sender<AiCommitResult>,
    /// Receiver for incremental commit pages loaded after the first capped page.
    commit_page_rx: mpsc::Receiver<CommitPageResult>,
    /// Sender cloned into background threads for incremental commit loading.
    commit_page_tx: mpsc::Sender<CommitPageResult>,
    /// True while a background commit page is in flight.
    commit_page_loading: bool,
    /// True when the last older-history page reached the end of history.
    commit_history_complete: bool,
    /// Absolute skip offset represented by `model.commits[0]`. The commits
    /// panel is a sliding window so huge histories stay fully navigable without
    /// retaining every page visited for the lifetime of the TUI.
    commit_window_start: usize,
    /// Generation counter used to discard stale commit-page results after refresh.
    commit_page_generation: u64,
    /// Active AI commit generation job, if one is running.
    ai_commit_job: Option<AiCommitJob>,
    /// Generation counter used to discard stale AI results after cancellation.
    ai_commit_generation: u64,
    /// Diff source for the next AI commit message generation.
    ai_commit_source: AiCommitSource,
    /// Receiver for background remote operations (push, pull, fetch).
    remote_op_rx: mpsc::Receiver<(RemoteOpKind, Result<RemoteOpCompletion>)>,
    /// Sender cloned into background threads for remote operations.
    remote_op_tx: mpsc::Sender<(RemoteOpKind, Result<RemoteOpCompletion>)>,
    /// A manual network operation requested while silent auto-fetch owns the
    /// remote lane. Only one user operation is queued; repeated keys while it
    /// waits are intentionally ignored.
    pending_remote_op: Option<PendingRemoteOp>,
    /// Async light files refresh (status-only) for non-index refreshes.
    files_refresh_rx: Option<mpsc::Receiver<Result<(Vec<crate::model::File>, Option<(usize, usize)>)>>>,
    files_refresh_in_progress: bool,
    /// True when starting a checkout invalidated an in-flight model/files
    /// refresh. If checkout fails, queue a fresh refresh because the old
    /// HEAD-relative stream may have intentionally stopped mid-flight.
    checkout_invalidated_refresh: bool,
    /// One long-lived worker owns all index mutations so rapid staging cannot
    /// race on `.git/index.lock`. The worker drains bursts in order and emits
    /// one status snapshot after each burst.
    index_mutation_tx: mpsc::Sender<IndexMutationCommand>,
    index_mutation_rx: mpsc::Receiver<IndexMutationResult>,
    index_mutation_busy: Arc<AtomicBool>,
    index_mutation_generation: u64,
    /// Background `git ls-files` for the path-filter picker (ctrl-s → path).
    filter_paths_rx: Option<mpsc::Receiver<Result<Vec<String>>>>,
    /// Receiver for automatic fetch results. Kept separate from remote_op so
    /// startup/periodic fetch failures stay non-modal and cannot clobber a
    /// user-initiated push/pull.
    auto_fetch_rx: mpsc::Receiver<Result<bool>>,
    /// Sender cloned into background threads for automatic fetches.
    auto_fetch_tx: mpsc::Sender<Result<bool>>,
    /// When the last automatic fetch started. The visible startup fetch resets
    /// this clock so periodic auto-fetch waits a full configured interval.
    last_auto_fetch_at: Option<Instant>,
    /// Last terminal interaction. Periodic auto-fetch only starts after a short
    /// idle grace period so a user navigating toward Push/Pull/Checkout wins.
    last_user_activity_at: Instant,
    /// True while an automatic fetch is in flight, so startup/periodic fetches
    /// and manual network operations share one serialized lane.
    auto_fetch_in_flight: bool,
    /// True only for the initial automatic fetch. Its "Fetching" overlay is
    /// cleared as soon as the network operation finishes; periodic fetches stay
    /// silent as before.
    startup_fetch_in_flight: bool,
    /// Rearmed by terminal activity and cleared after one idle allocator collect.
    /// Keeps `mi_collect(true)` off the hot path while still letting RSS fall
    /// after bursty commit/diff navigation.
    memory_collect_armed: bool,
    /// Receiver for background menu item operations (e.g. fetching PR URLs).
    menu_async_rx: mpsc::Receiver<Result<popup::MenuAsyncResult>>,
    /// Sender cloned into background threads for menu async operations.
    pub(crate) menu_async_tx: mpsc::Sender<Result<popup::MenuAsyncResult>>,
    /// Small completion jobs produced by background work that must mutate GUI state.
    ui_job_rx: mpsc::Receiver<UiJob>,
    pub(crate) ui_job_tx: mpsc::Sender<UiJob>,
    /// Undo stack: stores reflog hashes for undo/redo.
    undo_reflog_idx: usize,
    /// Patch building mode state.
    pub patch_building: PatchBuildingState,
    /// Diff/compare mode state.
    pub diff_mode: DiffModeState,
    /// Initial compare-mode lists are loaded off the UI thread so large histories
    /// (for example Linux kernel branches with tens of thousands of divergent commits)
    /// never block input/rendering.
    compare_load_rx: Option<mpsc::Receiver<CompareLoadEvent>>,
    compare_load_generation: u64,
    /// Interactive rebase mode state.
    pub rebase_mode: RebaseModeState,
    /// Stashed commit editor popup while commit menu or AI generation is shown.
    pending_commit_popup: Option<PopupState>,
    /// Persists the commit editor across Esc so re-opening doesn't lose typed text.
    /// Cleared on successful commit or explicit Clear from the commit menu.
    pub(crate) saved_commit_popup: Option<PopupState>,
    /// Temporarily holds a menu popup during action execution so async actions can restore it.
    pending_menu_popup: Option<PopupState>,
    /// Search bar textarea (1-line editor for search input).
    search_textarea: Option<tui_textarea::TextArea<'static>>,
    /// Last time a refresh occurred (for 10s background auto-refresh interval).
    last_refresh_at: Instant,
    /// Active branch filter for commits panel. When non-empty, only commits from these branches are shown.
    pub commit_branch_filter: Vec<String>,
    /// Optional path used to filter the main commits panel.
    pub commit_path_filter: Option<String>,
    /// Optional author identity used to filter the main commits panel.
    pub commit_author_filter: Vec<String>,
    /// Hash of the commit whose files are being viewed in CommitFiles context.
    pub commit_files_hash: String,
    /// First line of the commit message for the commit being viewed.
    pub commit_files_message: String,
    /// Cached commit file tree nodes for the CommitFiles view.
    pub commit_file_tree_nodes: Vec<CommitFileTreeNode>,
    /// Set of collapsed directory paths in the commit file tree.
    pub commit_files_collapsed_dirs: HashSet<String>,
    /// Whether to show tree view for commit files (mirrors show_file_tree).
    pub show_commit_file_tree: bool,
    /// Name of the branch/tag whose commits are being viewed in BranchCommits context.
    pub branch_commits_name: String,
    /// Select HEAD once startup history arrives, unless the user navigates first.
    pending_startup_head: bool,
    /// Name of the remote whose branches are being viewed in RemoteBranches context.
    pub remote_branches_name: String,
    /// Parent context to return to when pressing Esc from BranchCommits.
    pub sub_commits_parent_context: context::ContextId,
    /// Parent context to return to when pressing Esc from CommitFiles.
    pub commit_files_parent_context: Option<context::ContextId>,
    /// Receiver for streamed model parts during initial load or background
    /// refresh. Each git data type arrives independently so the UI can
    /// waterfall-display results. Set to `None` once all parts received.
    initial_load_rx: Option<mpsc::Receiver<ModelPart>>,
    /// How many model parts have arrived so far (out of MODEL_PART_COUNT).
    initial_load_received: usize,
    initial_load_expected: usize,
    /// True while a background `load_model_streaming` refresh is in flight.
    /// Prevents stacking concurrent full refreshes on the UI thread.
    refresh_in_progress: bool,
    /// Invalidates selection-dependent background reloads started after a
    /// model refresh (branch commits / commit files).
    subview_refresh_generation: u64,
    /// Frame counter for the loading spinner animation.
    spinner_frame: usize,
    /// Label shown on the head branch during a remote operation (e.g. "Pushing", "Pulling").
    remote_op_label: Option<String>,
    /// One local mutation (stash/submodule/worktree/etc.) may run in the
    /// background at a time so potentially slow Git commands never block input.
    background_mutation_label: Option<String>,
    /// Timestamp when the last remote operation succeeded (for showing a temporary ✓).
    remote_op_success_at: Option<Instant>,
    /// Short non-modal confirmation for copy/save/open actions.
    action_toast: Option<(String, Instant)>,
    /// Branch name from checkout-by-name; used to offer create-on-miss when checkout fails.
    pub(crate) pending_checkout_by_name: Option<String>,
    /// True for a user-selected recovery strategy, which must not retry the
    /// same conflict popup after an unsuccessful or partially applied checkout.
    pub(crate) pending_checkout_assisted: bool,
    /// Commit-and-Switch moves the source tip, invalidating cached ahead counts.
    pub(crate) pending_checkout_committed: bool,
    /// Distinguish a checkout refresh from other 12-part model refreshes.
    checkout_refresh_active: bool,
    /// Chosen from the checkout-conflicts picker; focus it after the async
    /// status refresh, since the cached Files list can be stale at failure time.
    pub(crate) pending_conflict_inspect_file: Option<String>,
    /// Editor to run after suspending the TUI (hx/nvim/vim).
    pending_editor_launch: Option<crate::config::user_config::EditorLaunch>,
    /// Copied commit hashes for cherry-pick paste (newest first).
    pub cherry_pick_clipboard: Vec<String>,
    /// Anchor index for range selection in commits list (None = not in range mode).
    pub range_select_anchor: Option<usize>,
    /// History of previously submitted commit messages (most recent first).
    pub commit_message_history: Vec<String>,
    /// Current index into commit_message_history when cycling (None = not cycling).
    pub commit_history_idx: Option<usize>,
    /// Stashed current draft when cycling through history.
    commit_history_draft: String,
    /// Current color theme index into COLOR_THEMES.
    pub current_theme_index: usize,
    /// Cache of shortstat summaries per commit hash.  Populated asynchronously
    /// by background threads so the render path never blocks on git.
    pub commit_stats_cache:
        std::sync::Arc<std::sync::Mutex<HashMap<String, crate::model::commit::CommitStat>>>,
    commit_stats_cache_order: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    /// Cache of full commit messages (subject + body) per hash, fetched
    /// asynchronously so the details panel can render the full description.
    pub commit_messages_cache: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    commit_messages_cache_order: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    /// Latest-only queue for commit metadata shown below the commit list.
    commit_details_job_tx: mpsc::Sender<BackgroundJob>,
    /// Invalidates commit-detail jobs when selection changes while one is running.
    commit_details_generation: Arc<AtomicU64>,
    /// Commit hash most recently considered for details loading.
    last_commit_details_key: String,
    /// Vertical scroll offset (rows) for the commit-details box.  Reset
    /// whenever the selected commit hash changes.
    pub commit_details_scroll: u16,
    /// Hash the current `commit_details_scroll` value corresponds to.  When
    /// render sees a different hash, it resets the scroll.
    pub commit_details_scroll_hash: String,
    /// Whether commit details are visible. `t` in commit lists; `T` in file subviews.
    pub show_commit_details: bool,
    /// First character of the configurable two-key Go to top binding.
    pending_goto_top: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenMode {
    Normal,
    Half,
    Full,
}

/// True when a `git checkout <name>` failure means the ref does not exist.
pub(crate) fn is_checkout_ref_not_found(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("did not match any file(s) known to git")
        || lower.contains("unknown revision or path not in the working tree")
        || lower.contains("invalid reference:")
}

/// Pathspec for a tree-node path: root (".") => empty (whole tree), dirs get a
/// trailing slash so git matches the directory contents.
fn pathspec_for_tree_path(path: &str) -> Option<String> {
    if path.is_empty() || path == "." {
        return None; // whole tree / no path filter
    }
    if path.ends_with('/') {
        Some(path.to_string())
    } else {
        Some(format!("{}/", path))
    }
}

/// Synthesize a unified diff for a new (untracked) file from its raw content.
/// This allows untracked files to be included in combined multi-file diffs.
fn synthesize_new_file_diff(filename: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let count = lines.len();
    let mut diff = String::new();
    diff.push_str(&format!("diff --git a/{f} b/{f}\n", f = filename));
    diff.push_str("new file mode 100644\n");
    diff.push_str(&format!("--- /dev/null\n"));
    diff.push_str(&format!("+++ b/{}\n", filename));
    diff.push_str(&format!("@@ -0,0 +1,{} @@\n", count));
    for line in &lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

/// Synthetic binary diff so the pager renders its unavailable-preview placeholder.
fn synthesize_binary_file_diff(filename: &str) -> String {
    format!(
        "diff --git a/{f} b/{f}\n\
         new file mode 100644\n\
         index 0000000..1111111\n\
         Binary files /dev/null and b/{f} differ\n",
        f = filename
    )
}

/// Pure renames have no hunks; show file content instead of only path lines.
fn parse_file_diff_payload(
    git: &GitCommands,
    name: &str,
    current_path: &str,
    diff: &str,
    exists: bool,
    prefer_staged: bool,
) -> DiffPayload {
    if is_rename_only_diff(diff) {
        let content = if prefer_staged {
            git.file_content_staged(current_path)
                .or_else(|_| git.file_content(current_path))
        } else {
            git.file_content(current_path)
                .or_else(|_| git.file_content_staged(current_path))
        };
        if let Ok(content) = content {
            if !content.is_empty() {
                return DiffPayload::Parsed(DiffViewState::parse_content(
                    current_path,
                    &content,
                    &content,
                    4,
                    exists,
                ));
            }
        }
    }
    DiffPayload::Parsed(DiffViewState::parse_diff_output(name, diff, 4, exists))
}

fn parse_commit_file_diff_payload(
    git: &GitCommands,
    hash: &str,
    name: &str,
    current_path: &str,
    diff: &str,
) -> DiffPayload {
    let paths = crate::git::diff::diff_paths_for_label(name);
    let parent = format!("{hash}^1");
    if let Some(parsed) = image_diff_payload(
        git,
        name,
        diff,
        false,
        crate::pager::image_preview::ImageSource::Revision {
            revision: &parent,
            path: paths[0],
        },
        crate::pager::image_preview::ImageSource::Revision {
            revision: hash,
            path: current_path,
        },
    ) {
        return DiffPayload::Parsed(parsed);
    }
    if is_rename_only_diff(diff) {
        if let Ok(content) = git.file_content_at_commit(hash, current_path) {
            if !content.is_empty() {
                return DiffPayload::Parsed(DiffViewState::parse_content(
                    current_path,
                    &content,
                    &content,
                    4,
                    false,
                ));
            }
        }
    }
    DiffPayload::Parsed(DiffViewState::parse_diff_output(name, diff, 4, false))
}

/// Only single raster-file selections are previewed; multi-file buffers retain
/// their ordinary binary placeholders. Decoding validates the actual bytes.
fn image_diff_payload(
    git: &GitCommands,
    name: &str,
    diff: &str,
    exists: bool,
    old: crate::pager::image_preview::ImageSource<'_>,
    new: crate::pager::image_preview::ImageSource<'_>,
) -> Option<crate::pager::side_by_side::ParsedDiff> {
    let paths = crate::git::diff::diff_paths_for_label(name);
    if !paths
        .iter()
        .any(|p| crate::pager::image_preview::is_image_path(p))
    {
        return None;
    }
    let preview = crate::pager::image_preview::load(git, old, new)?;
    // A rename-only image has no binary marker, but must still be a nonempty
    // diff view without text hunks (image content cannot be staged by hunk).
    let placeholder = synthesize_binary_file_diff(name);
    let mut parsed = DiffViewState::parse_diff_output(
        name,
        if diff.is_empty() || is_rename_only_diff(diff) {
            &placeholder
        } else {
            diff
        },
        4,
        exists,
    );
    parsed.image_preview = Some(preview);
    Some(parsed)
}

impl Gui {
    fn show_error(&mut self, title: &str, err: anyhow::Error) {
        self.show_notification(PopupState::Message {
            title: title.to_string(),
            message: format!("{:#}", err),
            kind: MessageKind::Error,
        });
    }

    pub(crate) fn show_action_toast(&mut self, message: impl Into<String>) {
        self.action_toast = Some((message.into(), Instant::now()));
    }

    fn periodic_refresh_due(&self) -> bool {
        let refresh_interval = self.config.user_config.refresher.refresh_interval;

        self.config.user_config.git.auto_refresh
            && refresh_interval > 0
            && !self.refresh_in_progress
            && self.initial_load_rx.is_none()
            && !(self.pending_checkout_by_name.is_some() && self.remote_op_label.is_some())
            && self.last_refresh_at.elapsed().as_secs() >= refresh_interval
    }

    fn show_notification(&mut self, notification: PopupState) {
        if self.shell_command_ui_active() || self.deferred_notification_ui_active() {
            // Auto-refresh can report the same failure repeatedly during a
            // five-minute command. Keep one adjacent copy, not an error flood.
            let duplicate = matches!(
                (self.deferred_notifications.back(), &notification),
                (Some(PopupState::Message { title: a, message: b, kind: c }),
                 PopupState::Message { title: x, message: y, kind: z })
                    if a == x && b == y && c == z
            );
            if !duplicate {
                self.deferred_notifications.push_back(notification);
            }
        } else {
            self.popup = notification;
        }
    }

    fn deferred_notification_ui_active(&self) -> bool {
        self.deferred_notification_visible || !self.deferred_notifications.is_empty()
    }

    fn receive_deferred_notifications(&mut self) {
        // Never replace another editor, result, or notification. Dispatch one
        // per dismissal and keep AI/remote results queued until it is seen.
        if self.popup == PopupState::None && !self.shell_command_ui_active() {
            self.deferred_notification_visible = false;
            if let Some(notification) = self.deferred_notifications.pop_front() {
                self.popup = notification;
                self.deferred_notification_visible = true;
            }
        }
    }

    pub fn new(
        config: AppConfig,
        git: GitCommands,
        filter_path: Option<PathBuf>,
        start_in_commits: bool,
    ) -> Result<Self> {
        let (diff_tx, diff_rx) = mpsc::channel();
        let (diff_scheduler_tx, diff_scheduler_rx) = mpsc::channel();
        let (diff_prefetch_tx, diff_prefetch_rx) = mpsc::channel();
        let (commit_stat_prefetch_tx, commit_stat_prefetch_job_rx) = mpsc::channel();
        let (commit_stat_prefetch_result_tx, commit_stat_prefetch_rx) = mpsc::channel();
        let (commit_details_job_tx, commit_details_job_rx) = mpsc::channel();
        let (ai_commit_tx, ai_commit_rx) = mpsc::channel();
        let (commit_page_tx, commit_page_rx) = mpsc::channel();
        let (remote_op_tx, remote_op_rx) = mpsc::channel();
        let (index_mutation_tx, index_mutation_cmd_rx) = mpsc::channel();
        let (index_mutation_result_tx, index_mutation_rx) = mpsc::channel();
        let index_mutation_busy = Arc::new(AtomicBool::new(false));
        let (auto_fetch_tx, auto_fetch_rx) = mpsc::channel();
        let (menu_async_tx, menu_async_rx) = mpsc::channel();
        let (ui_job_tx, ui_job_rx) = mpsc::channel();
        let show_file_tree = config
            .app_state
            .show_file_tree
            .unwrap_or(config.user_config.gui.show_file_tree);
        let show_command_log_default = config
            .app_state
            .show_command_log
            .unwrap_or(config.user_config.gui.show_command_log);
        let diff_line_wrap = config.app_state.diff_line_wrap.unwrap_or(false);
        let diff_view_layout = config
            .app_state
            .diff_view
            .as_deref()
            .and_then(DiffViewLayout::from_state_value)
            .unwrap_or_default();
        let show_commit_details = config.app_state.show_commit_details.unwrap_or(true);
        let command_log = crate::os::cmd::new_command_log();
        crate::os::cmd::set_thread_command_log(command_log.clone());

        // Start with an empty model — each piece of data loads in the
        // background and streams in as it becomes ready, so the UI can
        // paint immediately and waterfall-display results.
        let git = Arc::new(git);
        let diff_generation = Arc::new(AtomicU64::new(0));
        let commit_details_generation = Arc::new(AtomicU64::new(0));
        spawn_diff_scheduler(
            diff_scheduler_rx,
            diff_scheduler_tx.clone(),
            diff_tx.clone(),
            Arc::clone(&diff_generation),
        );
        spawn_index_mutation_worker(
            index_mutation_cmd_rx,
            index_mutation_result_tx,
            Arc::clone(&index_mutation_busy),
        );
        spawn_diff_prefetch_workers(diff_prefetch_rx, diff_tx.clone());
        spawn_commit_stat_prefetch_workers(
            commit_stat_prefetch_job_rx,
            commit_stat_prefetch_result_tx,
            Arc::clone(&git),
        );
        spawn_latest_background_worker(commit_details_job_rx);
        // Compile tree-sitter highlight queries off the critical path so the
        // first diff shown doesn't pay the ~40-60ms lazy-init cost.
        std::thread::spawn(crate::pager::highlight::warm_configs);
        let mut model = Model::default();
        model.repo_name = git.repo_name();
        model.head_hash = git.head_hash().unwrap_or_default();
        model.head_branch_name = git.current_branch_name().unwrap_or_default();

        // Normalize `-f` to a repo-relative path before kicking off the stream
        // so the first Commits part is already filtered (lazygit-style).
        let startup_path_filter = filter_path.and_then(|p| {
            let raw = p.to_string_lossy().trim().to_string();
            if raw.is_empty() {
                return None;
            }
            let relative = p
                .canonicalize()
                .ok()
                .and_then(|abs| {
                    abs.strip_prefix(git.repo_path())
                        .ok()
                        .map(|rel| rel.to_string_lossy().to_string())
                })
                .filter(|rel| !rel.is_empty())
                .unwrap_or(raw);
            Some(relative)
        });
        let startup_commit_filter =
            startup_path_filter
                .as_ref()
                .map(|path| crate::git::commit::CommitFilter {
                    path: Some(path.clone()),
                    ..Default::default()
                });

        let (initial_load_tx, initial_load_rx) = mpsc::channel();
        git.load_model_streaming(&initial_load_tx, startup_commit_filter, true);

        let commit_history = Self::load_commit_history(&config);

        // Resolve saved color theme
        let current_theme_index = config
            .app_state
            .color_theme
            .as_deref()
            .and_then(|id| crate::config::COLOR_THEMES.iter().position(|t| t.id == id))
            .unwrap_or(0);

        let mut context_mgr = ContextManager::new();
        if startup_path_filter.is_some() || start_in_commits {
            context_mgr.set_active(ContextId::Commits);
            context_mgr.set_selection_for(ContextId::Commits, 0);
        }

        Ok(Self {
            config: Arc::new(config),
            git,
            model: Arc::new(Mutex::new(model)),
            initial_load_rx: Some(initial_load_rx),
            initial_load_received: 0,
            initial_load_expected: MODEL_PART_COUNT,
            refresh_in_progress: false,
            subview_refresh_generation: 0,
            context_mgr,
            layout: LayoutState::default(),
            popup: PopupState::None,
            shell_command_job: None,
            deferred_notifications: VecDeque::new(),
            deferred_notification_visible: false,
            diff_view: {
                let mut dv = DiffViewState::new();
                dv.wrap = diff_line_wrap;
                dv.view_layout = diff_view_layout;
                dv
            },
            commit_list_cache: presentation::commits::CommitListCache::default(),
            command_log,
            show_command_log: show_command_log_default,
            should_quit: false,
            needs_refresh: false,
            needs_files_refresh: false,
            needs_diff_refresh: true,
            search_query: String::new(),
            search_active: false,
            search_matches: Vec::new(),
            search_match_idx: 0,
            screen_mode: ScreenMode::Normal,
            sidebar_resizing: false,
            sidebar_resize_row_offset: 0,
            show_file_tree,
            file_tree_nodes: Vec::new(),
            collapsed_dirs: HashSet::new(),
            diff_focused: false,
            diff_loading: false,
            diff_loading_since: None,
            last_diff_key: String::new(),
            diff_generation,
            diff_rx,
            diff_scheduler_tx,
            diff_preview_cache: DiffPreviewCache::default(),
            displayed_diff_key: String::new(),
            diff_prefetch_tx,
            diff_prefetch_inflight: HashSet::new(),
            commit_stat_prefetch_tx,
            commit_stat_prefetch_rx,
            commit_stat_prefetch_inflight: HashSet::new(),
            commit_stat_prefetch_generation: 0,
            ai_commit_rx,
            ai_commit_tx,
            commit_page_rx,
            commit_page_tx,
            commit_page_loading: false,
            commit_history_complete: false,
            commit_window_start: 0,
            commit_page_generation: 0,
            ai_commit_job: None,
            ai_commit_generation: 0,
            ai_commit_source: AiCommitSource::Staged,
            remote_op_rx,
            remote_op_tx,
            pending_remote_op: None,
            files_refresh_rx: None,
            files_refresh_in_progress: false,
            checkout_invalidated_refresh: false,
            index_mutation_tx,
            index_mutation_rx,
            index_mutation_busy,
            index_mutation_generation: 0,
            filter_paths_rx: None,
            auto_fetch_rx,
            auto_fetch_tx,
            last_auto_fetch_at: Some(Instant::now()),
            last_user_activity_at: Instant::now(),
            auto_fetch_in_flight: false,
            startup_fetch_in_flight: false,
            memory_collect_armed: false,
            menu_async_rx,
            menu_async_tx,
            ui_job_rx,
            ui_job_tx,
            undo_reflog_idx: 0,
            patch_building: PatchBuildingState::new(),
            diff_mode: DiffModeState::new(),
            compare_load_rx: None,
            compare_load_generation: 0,
            rebase_mode: RebaseModeState::new(),
            pending_commit_popup: None,
            saved_commit_popup: None,
            pending_menu_popup: None,
            search_textarea: None,
            last_refresh_at: Instant::now(),
            commit_branch_filter: Vec::new(),
            commit_path_filter: startup_path_filter.clone(),
            commit_author_filter: Vec::new(),
            commit_files_hash: String::new(),
            commit_files_message: String::new(),
            commit_file_tree_nodes: Vec::new(),
            commit_files_collapsed_dirs: HashSet::new(),
            show_commit_file_tree: show_file_tree,
            branch_commits_name: String::new(),
            pending_startup_head: start_in_commits,
            remote_branches_name: String::new(),
            sub_commits_parent_context: context::ContextId::Branches,
            commit_files_parent_context: None,
            spinner_frame: 0,
            remote_op_label: None,
            background_mutation_label: None,
            remote_op_success_at: None,
            action_toast: None,
            pending_checkout_by_name: None,
            pending_checkout_assisted: false,
            pending_checkout_committed: false,
            checkout_refresh_active: false,
            pending_conflict_inspect_file: None,
            pending_editor_launch: None,
            cherry_pick_clipboard: Vec::new(),
            range_select_anchor: None,
            commit_message_history: commit_history,
            commit_history_idx: None,
            commit_history_draft: String::new(),
            current_theme_index,
            commit_stats_cache: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            commit_stats_cache_order: std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new())),
            commit_messages_cache: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            commit_messages_cache_order: std::sync::Arc::new(
                std::sync::Mutex::new(VecDeque::new()),
            ),
            commit_details_job_tx,
            commit_details_generation,
            last_commit_details_key: String::new(),
            commit_details_scroll: 0,
            commit_details_scroll_hash: String::new(),
            show_commit_details,
            pending_goto_top: false,
        })
    }

    /// Get the currently active theme.
    pub fn active_theme(&self) -> crate::config::Theme {
        crate::config::COLOR_THEMES
            .get(self.current_theme_index)
            .map(|ct| ct.to_theme())
            .unwrap_or_default()
    }

    pub fn run(&mut self) -> Result<()> {
        let (mut terminal, keyboard_enhanced) = setup_terminal()?;
        // Continuous reader thread: reassembly needs reads between frames
        // (see `input` module). One event-per-frame is what leaked ↑ as 'A'.
        let input = InputReader::spawn();

        // Sync layout dimensions with actual terminal size so mouse handling
        // uses the correct geometry from the very first frame.
        let size = terminal.size()?;
        self.layout.update_size(size.width, size.height);

        // Match lazygit's startup behavior: immediately refresh remote-tracking
        // refs in the background so ahead/behind counts reflect the server, not
        // just the last local fetch. The operation is non-interactive and the
        // visible "Fetching" overlay disappears as soon as it completes.
        self.start_startup_fetch();

        let mut keyboard_enhanced = keyboard_enhanced;
        let result = self.main_loop(&mut terminal, &input, &mut keyboard_enhanced);

        self.shell_command_job.take(); // Drop cancels and reaps before leaving the TUI.
        self.diff_preview_cache.clear();
        self.diff_view.reset_keep_prefs();
        self.displayed_diff_key.clear();
        restore_terminal(&mut terminal, keyboard_enhanced)?;
        result
    }

    /// Queue an editor launch. Terminal editors (`suspend: true`) run after the
    /// current frame via [`Self::run_suspended_editor`]; GUI editors spawn now.
    pub(crate) fn launch_editor(
        &mut self,
        launch: crate::config::user_config::EditorLaunch,
    ) -> Result<()> {
        if launch.suspend {
            self.pending_editor_launch = Some(launch);
            Ok(())
        } else {
            crate::os::cmd::log_command(&launch.display_cmd());
            std::process::Command::new(&launch.program)
                .args(&launch.args)
                .spawn()?;
            Ok(())
        }
    }

    fn run_suspended_editor(
        &mut self,
        terminal: &mut Term,
        input: &InputReader,
        keyboard_enhanced: &mut bool,
        launch: crate::config::user_config::EditorLaunch,
    ) -> Result<()> {
        use crossterm::terminal::{Clear, ClearType};
        use std::io::Write as _;
        use std::process::Stdio;

        // Pause the input thread before leaving the alt screen — crossterm's
        // event reader is process-wide and would steal hx/nvim's keystrokes.
        input.pause();
        input.drain();
        // Dropping the cached protocols queues scoped deletes and ensures they
        // are freshly transmitted after returning from another alternate screen.
        self.diff_preview_cache.clear();
        self.diff_view.reset_keep_prefs();
        self.displayed_diff_key.clear();
        self.needs_diff_refresh = true;
        crate::pager::image_preview::flush_cleanup(terminal.backend_mut());
        restore_terminal(terminal, *keyboard_enhanced)?;

        // LeaveAlternateScreen restores the previous buffer; wipe the primary
        // screen so Helix doesn't paint over leftover lazygit frames.
        // Use the same TUI output handle (/dev/tty when stdout is piped).
        {
            let mut out = crate::os::tty::open_tui_output()
                .context("Failed to open terminal output for editor handoff")?;
            execute!(
                out,
                Clear(ClearType::All),
                Clear(ClearType::Purge),
                cursor::MoveTo(0, 0),
                cursor::Show
            )?;
            out.flush()?;
        }

        crate::os::cmd::log_command(&launch.display_cmd());
        let status = std::process::Command::new(&launch.program)
            .args(&launch.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();

        // Re-enter the TUI regardless of editor exit status.
        let (new_term, new_enhanced) = setup_terminal()?;
        *terminal = new_term;
        *keyboard_enhanced = new_enhanced;

        input.drain();
        input.resume();

        // Force a full redraw next frame.
        terminal.clear()?;
        let size = terminal.size()?;
        self.layout.update_size(size.width, size.height);

        match status {
            Ok(st) if st.success() => Ok(()),
            Ok(st) => {
                self.show_error(
                    "Editor failed",
                    anyhow::anyhow!("exited with status {}", st.code().unwrap_or(-1)),
                );
                Ok(())
            }
            Err(err) => {
                self.show_error("Failed to launch editor", err.into());
                Ok(())
            }
        }
    }

    fn patch_preview_animation_active(&self) -> bool {
        matches!(
            self.popup,
            PopupState::PatchPreview {
                smooth_scroll_started_at: Some(_),
                ..
            }
        ) || matches!(
            self.popup,
            PopupState::PatchPreview {
                smooth_cursor_started_at: Some(_),
                ..
            }
        )
    }

    fn tick_patch_preview_animation(&mut self) {
        let duration = scroll::LIST_SCROLL_DURATION;
        let PopupState::PatchPreview {
            scroll,
            cursor_line,
            smooth_scroll_from,
            smooth_scroll_target,
            smooth_scroll_started_at,
            smooth_cursor_from,
            smooth_cursor_target,
            smooth_cursor_started_at,
            message,
            ..
        } = &mut self.popup
        else {
            return;
        };

        let area = Rect::new(0, 0, self.layout.width, self.layout.height);
        let (rows, total) = views::patch_preview_metrics(message, area);
        let last = total.saturating_sub(1);
        let max_scroll = total.saturating_sub(rows);

        if let Some(started_at) = *smooth_cursor_started_at {
            let t = (started_at.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0);
            let target = smooth_cursor_target.min(last as f64);
            if t >= 1.0 {
                *cursor_line = target as usize;
                *smooth_cursor_started_at = None;
                *smooth_cursor_from = target;
                *smooth_cursor_target = target;
            } else {
                let value = *smooth_cursor_from + (target - *smooth_cursor_from) * t;
                *cursor_line = if target >= *smooth_cursor_from {
                    value.floor() as usize
                } else {
                    value.ceil() as usize
                };
            }
        }

        if let Some(started_at) = *smooth_scroll_started_at {
            let t = (started_at.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0);
            let target = smooth_scroll_target.min(max_scroll as f64);
            if t >= 1.0 {
                *scroll = target as usize;
                *smooth_scroll_started_at = None;
                *smooth_scroll_from = target;
                *smooth_scroll_target = target;
            } else {
                let value = *smooth_scroll_from + (target - *smooth_scroll_from) * t;
                *scroll = if target >= *smooth_scroll_from {
                    value.floor() as usize
                } else {
                    value.ceil() as usize
                };
            }
        }
    }

    fn run_email_patch_action(&mut self) {
        let popup = std::mem::replace(&mut self.popup, PopupState::None);
        let PopupState::EmailPatch {
            title,
            to_recipients,
            cc_recipients,
            to_selected,
            cc_selected,
            recipient_undo,
            dry_run,
            preview_from,
            preview_subject,
            preview_date,
            preview_body,
            preview_scroll,
            preview_scroll_from,
            preview_scroll_target,
            preview_scroll_started_at,
            resolution_request_id,
            on_confirm,
            ..
        } = popup
        else {
            return;
        };

        let to = popup::join_email_recipients(&to_recipients);
        let cc = popup::join_email_recipients(&cc_recipients);
        if let Err(e) = on_confirm(self, to, cc, dry_run) {
            self.popup = PopupState::Message {
                title: "Error".to_string(),
                message: format!("{}", e),
                kind: MessageKind::Error,
            };
        } else if dry_run {
            // A dry run is a check, not a navigation event. Keep the exact
            // reviewed message visible so the user can inspect it, see the
            // success toast, disable Dry Run, and then confirm the real send.
            self.popup = PopupState::EmailPatch {
                title,
                to_recipients,
                cc_recipients,
                focus: popup::EmailPatchFocus::To,
                to_selected,
                cc_selected,
                recipient_editor: None,
                recipient_undo,
                dry_run: true,
                reviewing: true,
                preview_from,
                preview_subject,
                preview_date,
                preview_body,
                preview_scroll,
                preview_scroll_from,
                preview_scroll_target,
                preview_scroll_started_at,
                confirming_send: false,
                resolving_recipients: false,
                resolution_request_id,
                on_confirm,
            };
        }
    }

    fn email_preview_animation_active(&self) -> bool {
        matches!(
            self.popup,
            PopupState::EmailPatch {
                reviewing: true,
                preview_scroll_started_at: Some(_),
                ..
            }
        )
    }

    fn tick_email_preview_animation(&mut self) {
        let duration = scroll::LIST_SCROLL_DURATION;
        let PopupState::EmailPatch {
            reviewing: true,
            preview_from,
            to_recipients,
            cc_recipients,
            preview_subject,
            preview_date,
            preview_body,
            preview_scroll,
            preview_scroll_from,
            preview_scroll_target,
            preview_scroll_started_at,
            ..
        } = &mut self.popup
        else {
            return;
        };

        let area = Rect::new(0, 0, self.layout.width, self.layout.height);
        let max = views::email_preview_max_scroll(
            preview_from,
            to_recipients,
            cc_recipients,
            preview_subject,
            preview_date,
            preview_body,
            area,
        );

        if let Some(started_at) = *preview_scroll_started_at {
            let t = (started_at.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0);
            let target = preview_scroll_target.min(max as f64);
            if t >= 1.0 {
                *preview_scroll = target as usize;
                *preview_scroll_started_at = None;
                *preview_scroll_from = target;
                *preview_scroll_target = target;
            } else {
                let value = *preview_scroll_from + (target - *preview_scroll_from) * t;
                *preview_scroll = if target >= *preview_scroll_from {
                    value.floor() as usize
                } else {
                    value.ceil() as usize
                };
            }
        }
    }

    fn shell_command_ui_active(&self) -> bool {
        self.shell_command_job.is_some()
            || matches!(
                self.popup,
                PopupState::ShellCommand { .. }
                    | PopupState::CommandOutput { .. }
                    | PopupState::PatchPreview { .. }
            )
    }
    fn input_wait_timeout(&self) -> Duration {
        if self.context_mgr.has_active_scroll_animation()
            || self.context_mgr.has_active_selection_animation()
            || self.diff_view.has_active_smooth_scroll()
            || self.patch_preview_animation_active()
            || self.email_preview_animation_active()
            || self.commit_input_scroll_animation_active()
            || self.patch_file_picker_animation_active()
        {
            Duration::from_millis(8)
        } else if self.shell_command_job.is_some()
            || self.ai_commit_generation_active()
            || self.remote_op_label.is_some()
            || self.background_mutation_label.is_some()
            || matches!(
                self.popup,
                PopupState::EmailPatch {
                    resolving_recipients: true,
                    ..
                }
            )
            || self.action_toast.is_some()
            || !self.commit_stat_prefetch_inflight.is_empty()
            || self.needs_refresh
            || self.diff_loading
            || self.initial_load_rx.is_some()
            || self.refresh_in_progress
        {
            Duration::from_millis(16)
        } else if self.config.user_config.git.auto_refresh {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(200)
        }
    }

    /// Return unused mimalloc pages after a burst of interactive work.
    ///
    /// Collection is deliberately conservative: it runs at most once per input
    /// burst, only after a long idle window, and never while the main diff/stat
    /// or repository refresh lanes are still active. The system-allocator build
    /// uses the same policy but `collect_unused_memory()` is a no-op.
    fn maybe_collect_idle_memory(&mut self) {
        if !self.memory_collect_armed
            || self.last_user_activity_at.elapsed() < MEMORY_COLLECT_IDLE_GRACE
        {
            return;
        }

        let background_work_active = self.diff_loading
            || !self.diff_prefetch_inflight.is_empty()
            || !self.commit_stat_prefetch_inflight.is_empty()
            || self.initial_load_rx.is_some()
            || self.refresh_in_progress
            || self.files_refresh_in_progress
            || self.commit_page_loading
            || self.auto_fetch_in_flight
            || self.remote_op_label.is_some()
            || self.background_mutation_label.is_some()
            || self.index_mutation_busy.load(Ordering::Relaxed)
            || self.ai_commit_job.is_some();

        if background_work_active {
            return;
        }

        crate::allocator::collect_unused_memory();
        self.memory_collect_armed = false;
    }

    /// Apply one batch of terminal events before the next paint.
    fn handle_event_batch(&mut self, events: Vec<Event>) {
        if !events.is_empty() {
            self.last_user_activity_at = Instant::now();
            self.memory_collect_armed = true;
        }
        for event in events {
            match event {
                Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                    if let Err(err) = self.handle_key(key) {
                        self.show_error("Command failed", err);
                    }
                }
                Event::Mouse(mouse) => self.handle_mouse(mouse),
                Event::Resize(w, h) => self.handle_resize(w, h),
                Event::FocusGained if self.config.user_config.git.auto_refresh => {
                    self.needs_refresh = true;
                }
                Event::Paste(data) => self.handle_paste(data),
                _ => {}
            }
            if self.should_quit {
                break;
            }
        }
    }

    fn handle_resize(&mut self, w: u16, h: u16) {
        self.layout.update_size(w, h);
        // Re-flow any active commit-message textarea to the new width so
        // wrapping stays consistent with what the user sees.
        let popup_width = (w * 60 / 100).min(60).max(30).min(w);
        let popup_inner = popup_width.saturating_sub(4) as usize;
        let config_width = self.config.user_config.git.commit.auto_wrap_width;
        let effective_width = if config_width > 0 {
            popup_inner.min(config_width)
        } else {
            popup_inner
        };
        let commit_effective_width = popup::commit_body_text_width(w, config_width);
        match &mut self.popup {
            PopupState::Input {
                textarea,
                is_commit: true,
                ..
            } => {
                if effective_width > 0 {
                    auto_wrap_textarea(textarea, effective_width);
                }
            }
            PopupState::Input {
                textarea,
                is_commit: false,
                ..
            } => {
                let raw: String = textarea.lines().join("");
                if popup_inner > 0 && !raw.is_empty() {
                    let mut new_ta = popup::make_textarea("");
                    new_ta.insert_str(&raw);
                    soft_wrap_textarea(&mut new_ta, popup_inner);
                    *textarea = new_ta;
                }
            }
            PopupState::CommitInput {
                summary_textarea,
                body_textarea,
                body_state,
                mode,
                body_scroll_remaining,
                ..
            } => {
                if mode.is_visual() {
                    summary_textarea.cancel_selection();
                    body_textarea.cancel_selection();
                    *mode = popup::CommitInputMode::Normal;
                }
                *body_scroll_remaining = 0;
                if commit_effective_width > 0 {
                    body_state.render_into(body_textarea, commit_effective_width);
                }
            }
            _ => {}
        }
    }

    /// Receive completed diff results from the background thread (non-blocking).
    fn receive_diff_results(&mut self) {
        // Drain all available results, keeping only the latest valid one
        let current_gen = self.diff_generation.load(Ordering::Relaxed);
        while let Ok(result) = self.diff_rx.try_recv() {
            if result.is_prefetch {
                self.diff_prefetch_inflight.remove(&result.diff_key);
                if result.diff_key == self.last_diff_key && self.diff_loading {
                    // The user navigated onto this key while the prefetch was
                    // in flight and is waiting on it — apply it directly.
                    self.apply_diff_payload(result.diff_key, result.payload);
                } else if result.diff_key != self.last_diff_key
                    && result.diff_key != self.displayed_diff_key
                {
                    if let DiffPayload::Parsed(parsed) = result.payload {
                        let mut view = DiffViewState::new();
                        view.wrap = self.diff_view.wrap;
                        view.view_layout = self.diff_view.view_layout;
                        view.apply_parsed(parsed);
                        self.diff_preview_cache.insert(result.diff_key, view);
                    }
                }
                continue;
            }
            // Discard stale results from older generations
            if result.generation != current_gen || result.diff_key != self.last_diff_key {
                continue;
            }
            self.apply_diff_payload(result.diff_key, result.payload);
        }
    }

    /// Swap a completed diff into the view, stashing the outgoing one for
    /// instant revisits.
    fn apply_diff_payload(&mut self, diff_key: String, payload: DiffPayload) {
        self.diff_loading = false;
        self.diff_loading_since = None;
        if self.displayed_diff_key != diff_key {
            // The view still shows the previous selection — cache it before
            // overwriting. This also resets scroll/search for the new content.
            self.stash_displayed_diff();
        }
        match payload {
            DiffPayload::Parsed(parsed) => {
                self.diff_view.apply_parsed(parsed);
                self.displayed_diff_key = diff_key;
            }
            DiffPayload::Empty => {
                self.diff_view.reset_keep_prefs();
                self.displayed_diff_key.clear();
            }
        }
    }

    fn current_diff_key(&self) -> String {
        if self.diff_mode.active {
            return self.diff_mode.diff_key();
        }

        let active = self.context_mgr.active();
        let selected = self.context_mgr.selected_active();
        let model = self.model.lock().unwrap();
        match active {
            ContextId::Files => {
                if self.show_file_tree {
                    self.file_tree_nodes
                        .get(selected)
                        .map(|node| {
                            node.file_index
                                .and_then(|index| model.files.get(index))
                                .map(|file| format!("Files:file:{}", file.name))
                                .unwrap_or_else(|| format!("Files:dir:{}", node.path))
                        })
                        .unwrap_or_else(|| "Files:none".to_string())
                } else {
                    model
                        .files
                        .get(selected)
                        .map(|file| format!("Files:file:{}", file.name))
                        .unwrap_or_else(|| "Files:none".to_string())
                }
            }
            ContextId::Commits => model
                .commits
                .get(selected)
                .map(|commit| format!("Commits:{}", commit.hash))
                .unwrap_or_else(|| "Commits:none".to_string()),
            ContextId::Reflog => model
                .reflog_commits
                .get(selected)
                .map(|commit| format!("Reflog:{}", commit.hash))
                .unwrap_or_else(|| "Reflog:none".to_string()),
            ContextId::Stash => model
                .stash_entries
                .get(selected)
                .map(|entry| format!("Stash:{}", entry.hash))
                .unwrap_or_else(|| "Stash:none".to_string()),
            ContextId::BranchCommits => model
                .sub_commits
                .get(selected)
                .map(|commit| format!("BranchCommits:{}", commit.hash))
                .unwrap_or_else(|| "BranchCommits:none".to_string()),
            ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                let prefix = format!("{:?}:{}", active, self.commit_files_hash);
                if self.show_commit_file_tree {
                    self.commit_file_tree_nodes
                        .get(selected)
                        .map(|node| {
                            node.file_index
                                .and_then(|index| model.commit_files.get(index))
                                .map(|file| format!("{}:file:{}", prefix, file.name))
                                .unwrap_or_else(|| format!("{}:dir:{}", prefix, node.path))
                        })
                        .unwrap_or_else(|| format!("{}:none", prefix))
                } else {
                    model
                        .commit_files
                        .get(selected)
                        .map(|file| format!("{}:file:{}", prefix, file.name))
                        .unwrap_or_else(|| format!("{}:none", prefix))
                }
            }
            _ => format!("{:?}:{}", active, selected),
        }
    }

    fn begin_diff_request(&mut self, diff_key: String) -> Option<u64> {
        if diff_key == self.last_diff_key && !self.needs_diff_refresh {
            return None;
        }

        let selection_changed = diff_key != self.last_diff_key;

        // Same-key refresh of a hash-keyed diff: the content cannot have
        // changed, so skip the reload when it's already on screen — and when
        // it's already loading, let the in-flight job finish instead of
        // bumping the generation (which would invalidate its result and
        // restart the race on every refresh tick).
        if !selection_changed
            && diff_key_is_immutable(&diff_key)
            && (self.diff_loading
                || (self.displayed_diff_key == diff_key && !self.diff_view.is_empty()))
        {
            self.needs_diff_refresh = false;
            return None;
        }

        self.last_diff_key = diff_key.clone();
        self.needs_diff_refresh = false;

        let generation = self.diff_generation.fetch_add(1, Ordering::Relaxed) + 1;
        if selection_changed {
            if let Some(mut cached) = self.diff_preview_cache.take(&diff_key) {
                cached.wrap = self.diff_view.wrap;
                cached.view_layout = self.diff_view.view_layout;
                self.stash_displayed_diff();
                self.diff_view = cached;
                self.displayed_diff_key = diff_key;
                self.diff_loading = false;
                self.diff_loading_since = None;
                return None;
            }
            // A prefetch for this key is already in flight — wait for its
            // result instead of spawning a duplicate load.
            if self.diff_prefetch_inflight.contains(&diff_key) {
                self.diff_loading = true;
                self.diff_loading_since = Some(Instant::now());
                return None;
            }
            // Cache miss: keep the outgoing diff on screen while the new one
            // loads. It moves into the revisit cache when the result arrives.
        }

        self.diff_loading = false;
        self.diff_loading_since = None;
        Some(generation)
    }

    /// Move the currently displayed diff into the revisit cache, leaving a
    /// fresh view (prefs preserved) in its place.
    fn stash_displayed_diff(&mut self) {
        if self.diff_view.is_empty() || self.displayed_diff_key.is_empty() {
            self.displayed_diff_key.clear();
            return;
        }

        let mut replacement = DiffViewState::new();
        replacement.wrap = self.diff_view.wrap;
        replacement.view_layout = self.diff_view.view_layout;
        let view = std::mem::replace(&mut self.diff_view, replacement);
        self.diff_preview_cache
            .insert(std::mem::take(&mut self.displayed_diff_key), view);
    }

    /// Blank the diff pane (nothing selected / context without a diff),
    /// preserving the outgoing view for instant revisits.
    pub(crate) fn clear_diff_view(&mut self) {
        self.stash_displayed_diff();
        self.diff_view.reset_keep_prefs();
    }

    /// Invalidate cached previews backed by the mutable Files/index state.
    ///
    /// Keep the current pixels visible until the replacement arrives, but clear
    /// their cache identity so `apply_diff_payload()` cannot stash that stale
    /// view after a stage/unstage refresh. This prevents an old `Unstaged`
    /// preview from being resurrected when revisiting a staged file.
    fn invalidate_files_diff_previews(&mut self) {
        self.diff_preview_cache.remove_prefix("Files:");
        if self.displayed_diff_key.starts_with("Files:") {
            self.displayed_diff_key.clear();
        }
    }

    /// Speculatively warm the preview cache with diffs the user is likely to
    /// view next: neighbors of the selection in commit-like panels, and the
    /// Commits selection while another panel is focused (so switching to
    /// Commits shows its diff instantly). Commit/stash diffs are immutable,
    /// so warmed entries never go stale.
    fn maybe_prefetch_diffs(&mut self) {
        if self.diff_mode.active || self.rebase_mode.active || self.patch_building.active {
            return;
        }
        if self.diff_prefetch_inflight.len() >= DIFF_PREFETCH_INFLIGHT_MAX {
            return;
        }

        let active = self.context_mgr.active();
        match active {
            ContextId::Commits
            | ContextId::Reflog
            | ContextId::BranchCommits
            | ContextId::Stash => {
                let selected = self.context_mgr.selected_active();
                for step in 1..=DIFF_PREFETCH_AHEAD {
                    self.maybe_prefetch_one(active, selected + step);
                }
                for step in 1..=DIFF_PREFETCH_BEHIND {
                    let Some(index) = selected.checked_sub(step) else {
                        break;
                    };
                    self.maybe_prefetch_one(active, index);
                }
            }
            _ => {
                self.maybe_prefetch_one(
                    ContextId::Commits,
                    self.context_mgr.selected(ContextId::Commits),
                );
            }
        }
    }

    fn maybe_prefetch_one(&mut self, context: ContextId, index: usize) {
        if self.diff_prefetch_inflight.len() >= DIFF_PREFETCH_INFLIGHT_MAX {
            return;
        }
        let (diff_key, load): (String, Box<dyn FnOnce() -> DiffPayload + Send>) = {
            let model = self.model.lock().unwrap();
            let git = Arc::clone(&self.git);
            match context {
                ContextId::Commits => {
                    let Some(commit) = model.commits.get(index) else {
                        return;
                    };
                    let hash = commit.hash.clone();
                    let key = format!("Commits:{}", hash);
                    (
                        key,
                        Box::new(move || commit_diff_payload(&git, &hash, "commit")),
                    )
                }
                ContextId::BranchCommits => {
                    let Some(commit) = model.sub_commits.get(index) else {
                        return;
                    };
                    let hash = commit.hash.clone();
                    let key = format!("BranchCommits:{}", hash);
                    (
                        key,
                        Box::new(move || commit_diff_payload(&git, &hash, "commit")),
                    )
                }
                ContextId::Reflog => {
                    let Some(commit) = model.reflog_commits.get(index) else {
                        return;
                    };
                    let hash = commit.hash.clone();
                    let key = format!("Reflog:{}", hash);
                    (
                        key,
                        Box::new(move || commit_diff_payload(&git, &hash, "reflog")),
                    )
                }
                ContextId::Stash => {
                    let Some(entry) = model.stash_entries.get(index) else {
                        return;
                    };
                    let key = format!("Stash:{}", entry.hash);
                    let stash_index = entry.index;
                    (key, Box::new(move || stash_diff_payload(&git, stash_index)))
                }
                _ => return,
            }
        };

        if diff_key == self.last_diff_key
            || diff_key == self.displayed_diff_key
            || self.diff_prefetch_inflight.contains(&diff_key)
            || self.diff_preview_cache.contains(&diff_key)
        {
            return;
        }
        self.diff_prefetch_inflight.insert(diff_key.clone());
        let _ = self
            .diff_prefetch_tx
            .send(DiffPrefetchJob { diff_key, load });
    }

    fn receive_commit_stat_prefetch_results(&mut self) {
        while let Ok(result) = self.commit_stat_prefetch_rx.try_recv() {
            if result.generation != self.commit_stat_prefetch_generation {
                continue;
            }
            self.commit_stat_prefetch_inflight.remove(&result.hash);
            let Ok(stat) = result.result else {
                continue;
            };
            if let (Ok(mut cache), Ok(mut order)) = (
                self.commit_stats_cache.lock(),
                self.commit_stats_cache_order.lock(),
            ) {
                bounded_cache_insert(&mut cache, &mut order, result.hash, stat);
            }
        }
    }

    /// Warm shortstats around the selected commit. The current commit remains on
    /// the debounced details path; only neighbors are speculative so holding j/k
    /// does not turn every transient selection into an immediate Git process.
    fn maybe_prefetch_commit_stats(&mut self) {
        if !self.show_commit_details
            || self.commit_stat_prefetch_inflight.len() >= COMMIT_STAT_PREFETCH_INFLIGHT_MAX
        {
            return;
        }

        let active = self.context_mgr.active();
        let selected = self.context_mgr.selected_active();
        let hashes = {
            let model = self.model.lock().unwrap();
            let commits = match active {
                ContextId::Commits => &model.commits,
                ContextId::BranchCommits => &model.sub_commits,
                ContextId::Reflog => &model.reflog_commits,
                _ => return,
            };
            commit_stat_prefetch_indices(commits.len(), selected)
                .into_iter()
                .filter_map(|index| commits.get(index).map(|commit| commit.hash.clone()))
                .collect::<Vec<_>>()
        };

        for hash in hashes {
            if self.commit_stat_prefetch_inflight.len() >= COMMIT_STAT_PREFETCH_INFLIGHT_MAX {
                break;
            }
            let cached = self
                .commit_stats_cache
                .lock()
                .map(|cache| cache.contains_key(&hash))
                .unwrap_or(false);
            if cached || self.commit_stat_prefetch_inflight.contains(&hash) {
                continue;
            }

            self.commit_stat_prefetch_inflight.insert(hash.clone());
            let job = CommitStatPrefetchJob {
                generation: self.commit_stat_prefetch_generation,
                hash: hash.clone(),
            };
            if self.commit_stat_prefetch_tx.send(job).is_err() {
                self.commit_stat_prefetch_inflight.remove(&hash);
                break;
            }
        }
    }

    fn clear_diff_preview_cache(&mut self) {
        self.diff_preview_cache.clear();
    }

    fn maybe_request_commit_details(&mut self) {
        if !self.show_commit_details {
            if !self.last_commit_details_key.is_empty() {
                self.last_commit_details_key.clear();
                self.commit_details_generation
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }

        let active = self.context_mgr.active();
        let selected = self.context_mgr.selected_active();
        let hash = {
            let model = self.model.lock().unwrap();
            match active {
                ContextId::Commits => model
                    .commits
                    .get(selected)
                    .map(|commit| commit.hash.clone()),
                ContextId::BranchCommits => model
                    .sub_commits
                    .get(selected)
                    .map(|commit| commit.hash.clone()),
                ContextId::Reflog => model
                    .reflog_commits
                    .get(selected)
                    .map(|commit| commit.hash.clone()),
                ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                    (!self.commit_files_hash.is_empty()).then(|| self.commit_files_hash.clone())
                }
                _ => None,
            }
        };
        let Some(hash) = hash else {
            if !self.last_commit_details_key.is_empty() {
                self.last_commit_details_key.clear();
                self.commit_details_generation
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        };

        let details_key = format!("{:?}:{}", active, hash);
        if details_key == self.last_commit_details_key {
            return;
        }
        self.last_commit_details_key = details_key;
        let generation = self
            .commit_details_generation
            .fetch_add(1, Ordering::Relaxed)
            + 1;

        let stat_cached = self
            .commit_stats_cache
            .lock()
            .map(|cache| cache.contains_key(&hash))
            .unwrap_or(false);
        let message_cached = self
            .commit_messages_cache
            .lock()
            .map(|cache| cache.contains_key(&hash))
            .unwrap_or(false);
        if stat_cached && message_cached {
            return;
        }

        let git = Arc::clone(&self.git);
        let stat_cache = Arc::clone(&self.commit_stats_cache);
        let stat_order = Arc::clone(&self.commit_stats_cache_order);
        let message_cache = Arc::clone(&self.commit_messages_cache);
        let message_order = Arc::clone(&self.commit_messages_cache_order);
        let generation_counter = Arc::clone(&self.commit_details_generation);
        let _ = self.commit_details_job_tx.send(Box::new(move || {
            if generation_counter.load(Ordering::Relaxed) != generation {
                return;
            }

            if !stat_cached && !message_cached {
                if let Ok((message, stat)) = git.commit_details(&hash) {
                    if let (Ok(mut cache), Ok(mut order)) = (stat_cache.lock(), stat_order.lock()) {
                        bounded_cache_insert(&mut cache, &mut order, hash.clone(), stat);
                    }
                    if let (Ok(mut cache), Ok(mut order)) =
                        (message_cache.lock(), message_order.lock())
                    {
                        bounded_cache_insert(&mut cache, &mut order, hash, message);
                    }
                }
                return;
            }

            if !stat_cached {
                if let Ok(stat) = git.commit_stat(&hash) {
                    if let (Ok(mut cache), Ok(mut order)) = (stat_cache.lock(), stat_order.lock()) {
                        bounded_cache_insert(&mut cache, &mut order, hash.clone(), stat);
                    }
                }
            }

            if generation_counter.load(Ordering::Relaxed) != generation {
                return;
            }

            if !message_cached {
                if let Ok(message) = git.commit_message_full(&hash) {
                    if let (Ok(mut cache), Ok(mut order)) =
                        (message_cache.lock(), message_order.lock())
                    {
                        bounded_cache_insert(&mut cache, &mut order, hash, message);
                    }
                }
            }
        }));
    }

    pub(crate) fn queue_diff_job<F>(&self, generation: u64, diff_key: String, load: F)
    where
        F: FnOnce() -> DiffPayload + Send + 'static,
    {
        let _ = self
            .diff_scheduler_tx
            .send(DiffSchedulerEvent::Job(DiffJob {
                generation,
                diff_key,
                load: Box::new(load),
            }));
    }

    /// Check for completed AI commit message generation results.
    fn receive_ai_commit_results(&mut self) {
        // Shell input/results own the modal until dismissed. Leave AI results
        // queued instead of replacing a typed command or restoring an editor
        // that shell completion would immediately discard.
        if self.shell_command_ui_active() || self.deferred_notification_ui_active() {
            return;
        }
        while let Ok(result) = self.ai_commit_rx.try_recv() {
            let active_generation = self.ai_commit_job.as_ref().map(|job| job.generation);
            if active_generation != Some(result.generation) {
                continue;
            }
            self.ai_commit_job = None;

            match result.result {
                Ok(Some(message)) => {
                    let wrap = popup::commit_body_text_width(
                        self.layout.width,
                        self.config.user_config.git.commit.auto_wrap_width,
                    );

                    // Split AI message into summary (first line) and body (rest).
                    // Preserve AI newlines as logical lines; BodySoftWrap soft-wraps
                    // for display only (avoids collapsing `- a\n- b` into one line).
                    let (summary, body) = match message.find('\n') {
                        Some(idx) => {
                            let s = message[..idx].to_string();
                            let raw_body = message[idx + 1..].trim_start_matches('\n').to_string();
                            (s, popup::unwrap_commit_body(&raw_body))
                        }
                        None => (message.clone(), String::new()),
                    };

                    // Helper to populate the two textareas
                    let fill_commit = |stashed: &mut PopupState| {
                        if let PopupState::CommitInput {
                            summary_textarea,
                            body_textarea,
                            body_state,
                            mode,
                            body_scroll_remaining,
                            ..
                        } = stashed
                        {
                            summary_textarea.cancel_selection();
                            body_textarea.cancel_selection();
                            *mode = popup::CommitInputMode::Normal;
                            *body_scroll_remaining = 0;
                            popup::set_commit_summary_text(summary_textarea, &summary);
                            body_state.set_text(body.clone());
                            body_state.render_into(body_textarea, wrap);
                        }
                    };

                    // Restore the stashed commit editor, replacing its textarea content.
                    // This intentionally steals focus when generation completes.
                    if let Some(mut stashed) = self.pending_commit_popup.take() {
                        fill_commit(&mut stashed);
                        self.popup = stashed;
                    } else {
                        let mut summary_ta = popup::make_commit_summary_textarea();
                        summary_ta.insert_str(&summary);
                        let mut body_ta = popup::make_commit_body_textarea();
                        let body_state = popup::BodySoftWrap::from_text(body.clone());
                        if !body.is_empty() {
                            body_state.render_into(&mut body_ta, wrap);
                        }
                        self.popup = PopupState::CommitInput {
                            kind: popup::CommitInputKind::Commit,
                            summary_textarea: summary_ta,
                            body_textarea: body_ta,
                            body_state,
                            focus: popup::CommitInputFocus::Summary,
                            mode: popup::CommitInputMode::Normal,
                            pending_command: Default::default(),
                            visual_line_anchor: None,
                            undo_stack: Vec::new(),
                            show_help: false,
                            body_scroll_remaining: 0,
                            body_scroll_top: 0,
                            on_confirm: Box::new(|gui, msg| {
                                if !msg.is_empty() {
                                    let message = msg.to_string();
                                    gui.start_remote_op(
                                        RemoteOpKind::LocalAppend,
                                        "Commit",
                                        "Creating commit...",
                                        move |git| {
                                            git.create_commit(&message, false)?;
                                            Ok(())
                                        },
                                    );
                                }
                                Ok(())
                            }),
                        };
                    }
                    self.ai_commit_source = AiCommitSource::Staged;
                }
                Ok(None) => {
                    if let Some(stashed) = self.pending_commit_popup.take() {
                        self.saved_commit_popup = Some(stashed);
                    }
                    self.ai_commit_source = AiCommitSource::Staged;
                }
                Err(e) => {
                    if let Some(stashed) = self.pending_commit_popup.take() {
                        self.saved_commit_popup = Some(stashed);
                    }
                    self.ai_commit_source = AiCommitSource::Staged;
                    self.popup = PopupState::Message {
                        title: "AI generation failed".to_string(),
                        message: format!(
                            "{}\n\nYour commit draft was saved. Open the commit prompt again to restore it.",
                            e
                        ),
                        kind: MessageKind::Error,
                    };
                }
            }
        }
    }

    fn receive_filter_paths(&mut self) {
        // A completed picker must not replace a shell input/output popup.
        if self.shell_command_ui_active() || self.deferred_notification_ui_active() {
            return;
        }
        let result = {
            let Some(rx) = self.filter_paths_rx.as_ref() else {
                return;
            };
            match rx.try_recv() {
                Ok(v) => Some(v),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => None,
            }
        };
        self.filter_paths_rx = None;
        match result {
            Some(Ok(paths)) => {
                let items = paths
                    .into_iter()
                    .map(|path| crate::gui::popup::ListPickerItem {
                        value: path.clone(),
                        label: path,
                        category: String::new(),
                        description: None,
                    })
                    .collect();
                self.show_list_picker(
                    "Filter by path",
                    items,
                    "Path",
                    Box::new(|gui, path| {
                        gui.commit_path_filter =
                            crate::gui::controller::commits::nonempty_for_filter(path);
                        crate::gui::controller::commits::apply_commit_filters_and_focus(gui)
                    }),
                );
            }
            Some(Err(e)) => {
                self.popup = PopupState::Message {
                    title: "Filter by path".to_string(),
                    message: format!("Could not list paths: {e}"),
                    kind: MessageKind::Error,
                };
            }
            None => {}
        }
    }

    fn receive_commit_page_results(&mut self) {
        while let Ok(result) = self.commit_page_rx.try_recv() {
            if result.generation != self.commit_page_generation {
                continue;
            }

            self.commit_page_loading = false;
            match result.result {
                Ok(commits) => {
                    let page_len = commits.len();
                    let mut model = self.model.lock().unwrap();
                    if result.replace {
                        model.set_commits(commits);
                        self.commit_window_start = result.skip;
                        drop(model);
                        self.context_mgr.set_selection_for(ContextId::Commits, 0);
                        self.context_mgr.set_scroll_offset(ContextId::Commits, 0);
                        self.range_select_anchor = None;
                        self.commit_history_complete = page_len < result.limit;
                    } else if result.prepend {
                        let existing: HashSet<String> =
                            model.commits.iter().map(|c| c.hash.clone()).collect();
                        let new_commits: Vec<_> = commits
                            .into_iter()
                            .filter(|c| !existing.contains(&c.hash))
                            .collect();
                        let added = new_commits.len();
                        model.prepend_commits(new_commits);
                        if model.commits.len() > MAX_LOADED_COMMITS {
                            model.truncate_commits(MAX_LOADED_COMMITS);
                            // The previous tail (possibly the end of history)
                            // was evicted, so scrolling down must be allowed to
                            // page it back in later.
                            self.commit_history_complete = false;
                        }
                        self.commit_window_start = result.skip;
                        let selected = self.context_mgr.selected(ContextId::Commits);
                        let scroll = self.context_mgr.scroll_offset(ContextId::Commits);
                        self.context_mgr
                            .set_selection_for(ContextId::Commits, selected.saturating_add(added));
                        self.context_mgr
                            .set_scroll_offset(ContextId::Commits, scroll.saturating_add(added));
                        if let Some(anchor) = self.range_select_anchor.as_mut() {
                            *anchor = anchor.saturating_add(added);
                        }
                        self.context_mgr.clamp_selections(&model);
                    } else {
                        let mut seen: HashSet<String> =
                            model.commits.iter().map(|c| c.hash.clone()).collect();
                        let new_commits: Vec<_> = commits
                            .into_iter()
                            .filter(|c| seen.insert(c.hash.clone()))
                            .collect();
                        model.extend_commits(new_commits);
                        self.commit_history_complete = page_len < result.limit;

                        if model.commits.len() > MAX_LOADED_COMMITS {
                            let excess = model.commits.len() - MAX_LOADED_COMMITS;
                            model.drain_commits_front(excess);
                            self.commit_window_start =
                                self.commit_window_start.saturating_add(excess);
                            let selected = self.context_mgr.selected(ContextId::Commits);
                            let scroll = self.context_mgr.scroll_offset(ContextId::Commits);
                            self.context_mgr.set_selection_for(
                                ContextId::Commits,
                                selected.saturating_sub(excess),
                            );
                            self.context_mgr.set_scroll_offset(
                                ContextId::Commits,
                                scroll.saturating_sub(excess),
                            );
                            if let Some(anchor) = self.range_select_anchor.as_mut() {
                                *anchor = anchor.saturating_sub(excess);
                            }
                        }
                        self.context_mgr.clamp_selections(&model);
                    }
                }
                Err(e) => {
                    if !result.prepend {
                        self.commit_history_complete = true;
                    }
                    if self.popup == PopupState::None
                        || self.shell_command_ui_active()
                        || self.deferred_notification_ui_active()
                    {
                        self.show_notification(PopupState::Message {
                            title: "Commits".to_string(),
                            message: format!("Could not load commits: {}", e),
                            kind: MessageKind::Error,
                        });
                    }
                }
            }
        }
    }

    /// Reload first page of commits for current filters (async).
    /// Used by ctrl-s so we don't block UI or refresh files/branches.
    pub(crate) fn reload_filtered_commits_async(&mut self) {
        self.reset_commit_pagination();
        if let Ok(mut model) = self.model.lock() {
            model.clear_commits();
        }
        self.context_mgr.set_selection_for(ContextId::Commits, 0);
        self.range_select_anchor = None;
        self.spawn_commit_page_load(DEFAULT_COMMIT_LIMIT, 0, true, false);
    }

    fn spawn_commit_page_load(&mut self, limit: usize, skip: usize, replace: bool, prepend: bool) {
        self.commit_page_loading = true;
        let generation = self.commit_page_generation;
        let git = Arc::clone(&self.git);
        let tx = self.commit_page_tx.clone();
        let filter = crate::git::commit::CommitFilter {
            branches: self.commit_branch_filter.clone(),
            path: self.commit_path_filter.clone(),
            authors: self.commit_author_filter.clone(),
        };

        std::thread::spawn(move || {
            let unpushed = git.unpushed_commit_hashes().unwrap_or_default();
            let result = git
                .load_filtered_commits_page(&filter, limit, skip)
                .map(|mut commits| {
                    crate::git::GitCommands::apply_unpushed_status(&mut commits, &unpushed);
                    commits
                });
            let _ = tx.send(CommitPageResult {
                generation,
                replace,
                prepend,
                skip,
                limit,
                result,
            });
        });
    }

    fn maybe_request_more_commits(&mut self) {
        if self.context_mgr.active() != ContextId::Commits || self.commit_page_loading {
            return;
        }

        let len = {
            let model = self.model.lock().unwrap();
            model.commits.len()
        };
        if len == 0 {
            return;
        }

        let selected = self.context_mgr.selected(ContextId::Commits);
        let scroll = self.context_mgr.scroll_offset(ContextId::Commits);

        // When the front of a bounded window was previously evicted, scrolling
        // back toward the top loads the preceding page and evicts from the tail.
        if self.commit_window_start > 0
            && (selected <= COMMIT_PAGE_PREFETCH_THRESHOLD
                || scroll <= COMMIT_PAGE_PREFETCH_THRESHOLD)
        {
            let limit = DEFAULT_COMMIT_LIMIT.min(self.commit_window_start);
            let skip = self.commit_window_start - limit;
            self.spawn_commit_page_load(limit, skip, false, true);
            return;
        }

        if self.commit_history_complete || len < DEFAULT_COMMIT_LIMIT {
            self.commit_history_complete = true;
            return;
        }

        let viewport_end = scroll.saturating_add(self.sidebar_visible_height());
        let near_loaded_tail = selected.saturating_add(COMMIT_PAGE_PREFETCH_THRESHOLD) >= len
            || viewport_end.saturating_add(COMMIT_PAGE_PREFETCH_THRESHOLD) >= len;
        if !near_loaded_tail {
            return;
        }

        self.spawn_commit_page_load(
            DEFAULT_COMMIT_LIMIT,
            self.commit_window_start.saturating_add(len),
            false,
            false,
        );
    }

    fn reset_commit_pagination(&mut self) {
        self.commit_page_generation = self.commit_page_generation.wrapping_add(1);
        self.commit_page_loading = false;
        self.commit_history_complete = false;
        self.commit_window_start = 0;
    }

    /// Start the initial visible fetch used to make branch divergence current
    /// with the remote as soon as lazygitrs opens. Respect the existing
    /// auto-fetch controls: users who disable automatic network access still get
    /// a completely local startup.
    fn start_startup_fetch(&mut self) {
        if !self.config.user_config.git.auto_fetch
            || self.config.user_config.refresher.fetch_interval == 0
            || self.auto_fetch_in_flight
            || self.remote_op_label.is_some()
        {
            return;
        }

        self.start_automatic_fetch(true);
    }

    /// Kick off a silent background `git fetch --all` if auto-fetch is enabled
    /// and the configured interval has elapsed since the last one. No popup,
    /// no status on the head branch — the user shouldn't be interrupted.
    fn maybe_start_auto_fetch(&mut self) {
        if !self.config.user_config.git.auto_fetch {
            return;
        }
        let interval = self.config.user_config.refresher.fetch_interval;
        if interval == 0 {
            return;
        }
        if self.auto_fetch_in_flight {
            return;
        }
        // Speculative network work should only start while the user is idle.
        // This sharply reduces the chance that a manual push/pull/checkout has
        // to queue behind a fetch that lazygitrs started on its own.
        if self.last_user_activity_at.elapsed() < AUTO_FETCH_IDLE_GRACE {
            return;
        }
        // Don't race a user-initiated push/pull/fetch (even with
        // --no-write-fetch-head, concurrent network ops are wasteful and can
        // still contend on packed-refs / remote-tracking updates).
        if self.remote_op_label.is_some() {
            return;
        }
        let due = match self.last_auto_fetch_at {
            None => false,
            Some(t) => t.elapsed().as_secs() >= interval,
        };
        if !due {
            return;
        }

        self.start_automatic_fetch(false);
    }

    /// Shared worker launcher for the visible startup fetch and silent periodic
    /// auto-fetch. Both use the same non-interactive network lane so a missing
    /// credential can never hang the TUI and manual push/pull operations queue
    /// behind at most one automatic fetch.
    fn start_automatic_fetch(&mut self, visible: bool) {
        self.last_auto_fetch_at = Some(Instant::now());
        self.auto_fetch_in_flight = true;
        self.startup_fetch_in_flight = visible;
        if visible {
            self.remote_op_label = Some("Fetching".to_string());
            self.remote_op_success_at = None;
        }

        let git = Arc::clone(&self.git);
        let tx = self.auto_fetch_tx.clone();
        let cmd_log = self.command_log.clone();
        std::thread::spawn(move || {
            crate::os::cmd::set_thread_command_log(cmd_log);
            let result = git.fetch_all_background();
            let _ = tx.send(result);
        });
    }

    /// Collect automatic fetch completions. When refs changed, reuse the
    /// targeted fetch refresh rather than reloading the entire model. Failures
    /// (offline, auth prompt suppressed, etc.) remain intentionally silent.
    fn receive_auto_fetch_results(&mut self) {
        while let Ok(result) = self.auto_fetch_rx.try_recv() {
            self.auto_fetch_in_flight = false;
            let startup_fetch = std::mem::take(&mut self.startup_fetch_in_flight);
            if startup_fetch && self.remote_op_label.as_deref() == Some("Fetching") {
                // The startup spinner is only for the network check itself. Let
                // the refreshed ahead/behind counts become visible immediately.
                self.remote_op_label = None;
            }

            // Manual network operations own the lane as soon as auto-fetch
            // releases it. A queued guided pull immediately fetches/classifies
            // the same upstream refs itself, so don't start a stale fetch-model
            // refresh immediately in front of it.
            let pending = if self.remote_op_label.is_none() {
                self.pending_remote_op.take()
            } else {
                None
            };
            let guided_pull_owns_refresh = matches!(
                pending.as_ref().map(|pending| pending.kind),
                Some(RemoteOpKind::Pull)
            );

            if matches!(result, Ok(true))
                && !guided_pull_owns_refresh
                && !self.start_background_fetch_refresh()
            {
                // Another model refresh is already in flight. Queue the normal
                // refresh rather than risk leaving stale remote state.
                self.needs_refresh = true;
            }

            if let Some(pending) = pending {
                let PendingRemoteOp { kind, title, op } = pending;
                self.launch_remote_op(kind, &title, op);
            }
        }
    }

    /// Check for completed background remote operations (push, pull, fetch).
    fn receive_remote_op_results(&mut self) {
        // In particular, a remote failure must not replace the shell's modal
        // and then be lost when the shell completes.
        if self.shell_command_ui_active() || self.deferred_notification_ui_active() {
            return;
        }
        if let Ok((kind, result)) = self.remote_op_rx.try_recv() {
            self.remote_op_label = None;
            match result {
                Ok(completion) => {
                    let mark_remote_success = !matches!(
                        completion.pull_outcome.as_ref(),
                        Some(
                            PullOutcome::Diverged { .. }
                                | PullOutcome::FastForwardOnlyBlocked { .. }
                                | PullOutcome::NeedsCleanWorktree { .. }
                                | PullOutcome::NoUpstream { .. }
                                | PullOutcome::Integrated {
                                    conflicted: true,
                                    ..
                                }
                        )
                    );
                    let was_checkout = self.pending_checkout_by_name.take().is_some();
                    self.pending_checkout_assisted = false;
                    let checkout_committed = std::mem::take(&mut self.pending_checkout_committed);
                    let confirmed_checkout_head = if was_checkout {
                        completion.head.clone()
                    } else {
                        None
                    };
                    if let Some(head) = completion.head.as_ref() {
                        let mut checked_out_branch_moved = false;
                        {
                            let mut model = self.model.lock().unwrap();
                            for branch in &mut model.branches {
                                branch.head = branch.name == head.branch_name;
                            }

                            // Match the exact ordering used by the eventual full branch
                            // refresh now, while we still have the cached branch metadata.
                            // This makes checkout one atomic visual reorder instead of moving
                            // HEAD first and reshuffling the remaining branches a moment later.
                            if !head.branch_name.is_empty()
                                && model
                                    .branches
                                    .iter()
                                    .any(|branch| branch.name == head.branch_name)
                            {
                                crate::model::sort_branches_for_display(&mut model.branches);
                                checked_out_branch_moved = true;
                            }

                            // Checkout cannot add/remove worktrees. Make the current
                            // worktree row reflect the confirmed HEAD immediately; the
                            // targeted refresh reuses this cached list instead of spawning
                            // `git worktree list --porcelain` again.
                            if let Some(worktree) = model
                                .worktrees
                                .iter_mut()
                                .find(|worktree| worktree.is_current)
                            {
                                worktree.hash.clone_from(&head.hash);
                                worktree.branch = if head.branch_name.is_empty() {
                                    "(detached)".to_string()
                                } else {
                                    head.branch_name.clone()
                                };
                            }

                            model.head_hash.clone_from(&head.hash);
                            model.head_branch_name.clone_from(&head.branch_name);
                        }

                        if checked_out_branch_moved {
                            self.context_mgr.snap_selection_to(ContextId::Branches, 0);
                        }
                        self.needs_diff_refresh = true;
                    }
                    match kind {
                        RemoteOpKind::Push => {
                            // Push cannot change local worktree contents or commit
                            // topology. Refresh only branch/upstream, remote refs,
                            // and pushed decorations, and do it off the UI thread.
                            if !self.start_background_push_refresh() {
                                // A pre-existing model refresh may have captured
                                // state from before the push. Queue a follow-up.
                                self.needs_refresh = true;
                            }
                        }
                        RemoteOpKind::Pull => {
                            let integrated = matches!(
                                completion.pull_outcome.as_ref(),
                                Some(
                                    PullOutcome::FastForwarded { .. }
                                        | PullOutcome::Integrated { .. }
                                )
                            );

                            if integrated {
                                if !self.start_background_pull_refresh() {
                                    self.needs_refresh = true;
                                }
                            } else if completion.refs_changed != Some(false)
                                && !self.start_background_fetch_refresh()
                            {
                                // Fetch may have advanced remote-tracking refs even
                                // when the workflow stops to ask how to reconcile.
                                self.needs_refresh = true;
                            }

                            if let Some(outcome) = completion.pull_outcome.clone() {
                                self.present_pull_outcome(outcome);
                            }
                        }
                        RemoteOpKind::PullIntegrate => {
                            // Merge/rebase can leave conflicts with unchanged refs,
                            // so never use the no-op ref shortcut for this phase.
                            if !self.start_background_pull_refresh() {
                                self.needs_refresh = true;
                            }
                            if let Some(outcome) = completion.pull_outcome.clone() {
                                self.present_pull_outcome(outcome);
                            }
                        }
                        RemoteOpKind::Fetch => {
                            // A fetch that changed no relevant refs changed none of
                            // the data shown by the targeted fetch refresh.
                            if completion.refs_changed != Some(false)
                                && !self.start_background_fetch_refresh()
                            {
                                self.needs_refresh = true;
                            }
                        }
                        RemoteOpKind::LocalAppend => {
                            self.request_append_refresh();
                        }
                        RemoteOpKind::LocalRewriteHead => {
                            self.request_head_rewrite_refresh();
                        }
                        RemoteOpKind::LocalHistory => {
                            self.request_history_refresh();
                        }
                        RemoteOpKind::ResetToUpstream => {
                            // Reset replaces HEAD, index and tracked worktree content.
                            // Discard old in-flight worktree snapshots and rebuild everything.
                            self.git.cancel_worktree_reads();
                            self.initial_load_rx = None;
                            self.refresh_in_progress = false;
                            self.files_refresh_rx = None;
                            self.files_refresh_in_progress = false;
                            self.diff_preview_cache.retain_immutable();
                            self.needs_refresh = true;
                            if let Some(backup) = completion.backup_branch {
                                self.popup = PopupState::Message {
                                    title: "Branch matched upstream".into(),
                                    message: format!("Previous HEAD saved as {backup}. Untracked files were preserved."),
                                    kind: MessageKind::Info,
                                };
                            }
                        }
                        RemoteOpKind::Email(dry_run) => {
                            self.show_action_toast(if dry_run {
                                "Email dry run passed"
                            } else {
                                "Patch emailed"
                            });
                            trim_process_heap();
                        }
                        RemoteOpKind::FetchAndCheckout | RemoteOpKind::Other => {
                            self.needs_refresh = true;
                        }
                    }
                    if mark_remote_success && !matches!(kind, RemoteOpKind::Email(_)) {
                        self.remote_op_success_at = Some(Instant::now());
                    }
                    if was_checkout {
                        // Close the generation around the mutation. This also
                        // invalidates any worktree read that happened to start
                        // while checkout itself was running; the targeted refresh
                        // below captures the new generation and new HEAD.
                        self.git.cancel_worktree_reads();

                        // A pre-checkout stream must not overwrite confirmed HEAD
                        // or files. Its workers can finish, but drop their results.
                        self.initial_load_rx = None;
                        self.refresh_in_progress = false;
                        self.files_refresh_rx = None;
                        self.files_refresh_in_progress = false;
                        self.needs_files_refresh = false;
                        self.checkout_invalidated_refresh = false;
                        // Dispatch before the next paint/input wait, not after an
                        // idle 50/200ms poll. HEAD is already visible above.
                        if !self.start_background_checkout_refresh(confirmed_checkout_head, checkout_committed) {
                            self.needs_refresh = true;
                        }
                    }
                }
                Err(e) => {
                    let target = self.pending_checkout_by_name.take();
                    let failed_checkout = target.is_some();
                    let assisted = std::mem::take(&mut self.pending_checkout_assisted);
                    self.pending_checkout_committed = false;
                    // AI generation may have restored the editor just before
                    // a queued remote error arrives. Preserve that draft.
                    if matches!(self.popup, PopupState::CommitInput { .. }) {
                        self.saved_commit_popup =
                            Some(std::mem::replace(&mut self.popup, PopupState::None));
                    }
                    let err = format!("{:#}", e);
                    if let Some(name) = target.as_ref().filter(|_| is_checkout_ref_not_found(&err)).cloned() {
                        self.popup = PopupState::Confirm {
                            title: "Branch not found".to_string(),
                            message: format!("Branch not found. Create a new branch named {}?", name),
                            on_confirm: Box::new(move |gui| {
                                gui.git.create_branch(&name)?;
                                gui.needs_refresh = true;
                                Ok(())
                            }),
                        };
                    } else if let Some(target) = target.as_ref().filter(|_| !assisted && controller::branches::is_checkout_worktree_conflict(&err)).cloned() {
                        let source = self.model.lock().unwrap().head_branch_name.clone();
                        self.needs_files_refresh = true;
                        self.popup = PopupState::BranchSwitchConflict {
                            source,
                            target,
                            paths: controller::branches::checkout_conflicting_paths(&err),
                            selected: 0,
                        };
                    } else {
                        self.popup = PopupState::Message {
                            title: "Error".to_string(),
                            message: err,
                            kind: MessageKind::Error,
                        };
                    }
                    if failed_checkout {
                        self.recover_refresh_after_failed_checkout();
                        // Assisted checkout may commit/stash/discard before an
                        // unsuccessful checkout, or --merge may move HEAD and
                        // leave conflicts. Always re-read the repository.
                        if assisted {
                            self.needs_refresh = true;
                        }
                    }
                    // Even a failed reset may have changed the index or worktree.
                    // A previously running refresh was cancelled before the attempt.
                    if matches!(kind, RemoteOpKind::PullIntegrate | RemoteOpKind::ResetToUpstream) {
                        self.needs_refresh = true;
                    }
                    if matches!(kind, RemoteOpKind::Email(_)) {
                        trim_process_heap();
                    }
                }
            }
        }
    }

    fn present_pull_outcome(&mut self, outcome: PullOutcome) {
        match outcome {
            PullOutcome::UpToDate { upstream } => {
                self.show_action_toast(format!("Already up to date with {upstream}"));
            }
            PullOutcome::AheadOnly { upstream, ahead } => {
                self.popup = PopupState::Message {
                    title: "Nothing to pull".to_string(),
                    message: format!(
                        "Your branch is {ahead} commit{} ahead of {upstream} and has no upstream commits to pull.\n\nPush to publish your local commits.",
                        if ahead == 1 { "" } else { "s" }
                    ),
                    kind: MessageKind::Info,
                };
            }
            PullOutcome::FastForwarded { upstream, behind } => {
                self.show_action_toast(format!(
                    "Fast-forwarded {behind} commit{} from {upstream}",
                    if behind == 1 { "" } else { "s" }
                ));
            }
            PullOutcome::FastForwardOnlyBlocked {
                upstream,
                ahead,
                behind,
            } => {
                self.popup = PopupState::Message {
                    title: "Fast-forward only pull".to_string(),
                    message: format!(
                        "Your branch has diverged from {upstream} ({ahead} local / {behind} upstream), but this repository is configured with pull.ff=only.\n\nChange that Git setting if you want lazygitrs to offer merge/rebase reconciliation."
                    ),
                    kind: MessageKind::Info,
                };
            }
            PullOutcome::NeedsCleanWorktree {
                upstream,
                ahead,
                behind,
            } => {
                self.popup = PopupState::Message {
                    title: "Local changes before pull".to_string(),
                    message: format!(
                        "Your branch is {ahead} commit{} ahead and {behind} commit{} behind {upstream}.\n\nTracked local changes are present. Commit, stash, or discard them before pulling so lazygitrs does not hide an integration conflict behind an automatic stash.",
                        if ahead == 1 { "" } else { "s" },
                        if behind == 1 { "" } else { "s" }
                    ),
                    kind: MessageKind::Info,
                };
            }
            PullOutcome::Diverged {
                upstream,
                ahead,
                behind,
            } => {
                self.show_pull_divergence_menu(upstream, ahead, behind);
            }
            PullOutcome::Integrated {
                upstream,
                strategy,
                conflicted,
            } => {
                if conflicted {
                    let operation = match strategy {
                        PullStrategy::Rebase => "rebase",
                        PullStrategy::Merge => "merge",
                    };
                    self.popup = PopupState::Message {
                        title: "Pull paused on conflicts".to_string(),
                        message: format!(
                            "The {operation} with {upstream} started but stopped on conflicts. Resolve the conflicted files, then use the existing continue/abort controls."
                        ),
                        kind: MessageKind::Info,
                    };
                } else {
                    let action = match strategy {
                        PullStrategy::Rebase => "Rebased onto",
                        PullStrategy::Merge => "Merged",
                    };
                    self.show_action_toast(format!("{action} {upstream}"));
                }
            }
            PullOutcome::NoUpstream { branch } => {
                let subject = if branch.is_empty() {
                    "The current detached HEAD".to_string()
                } else {
                    format!("Branch '{branch}'")
                };
                self.popup = PopupState::Message {
                    title: "No upstream configured".to_string(),
                    message: format!(
                        "{subject} has no upstream branch. Push it with the 'set upstream' option first, then pull."
                    ),
                    kind: MessageKind::Info,
                };
            }
        }
    }

    fn show_pull_divergence_menu(&mut self, upstream: String, ahead: usize, behind: usize) {
        let branch = self.model.lock().unwrap().head_branch_name.clone();
        self.popup = PopupState::PullDivergence {
            branch,
            upstream,
            ahead,
            behind,
            selected: 0,
        };
    }

    fn confirm_reset_to_upstream(&mut self, upstream: String) {
        let branch = self.model.lock().unwrap().head_branch_name.clone();
        self.popup = PopupState::ConfirmResetToUpstream { branch, upstream };
    }

    fn start_reset_to_upstream(&mut self, branch: String, upstream: String) {
        if self.remote_op_label.is_some() || self.pending_remote_op.is_some() {
            return;
        }
        // Invalidate in-flight status loads before the destructive operation.
        self.git.cancel_worktree_reads();
        self.initial_load_rx = None;
        self.refresh_in_progress = false;
        self.files_refresh_rx = None;
        self.files_refresh_in_progress = false;
        let op: RemoteOpFn = Box::new(move |git| {
            git.reset_to_upstream_with_backup(&branch, &upstream)
                .map(RemoteOpResult::Backup)
        });
        if self.auto_fetch_in_flight {
            self.pending_remote_op = Some(PendingRemoteOp {
                kind: RemoteOpKind::ResetToUpstream,
                title: "Resetting to upstream".to_string(),
                op,
            });
        } else {
            self.launch_remote_op(RemoteOpKind::ResetToUpstream, "Resetting to upstream", op);
        }
    }

    fn start_pull_divergence_compare(&mut self, upstream: String, ahead: usize, behind: usize) {
        self.popup = PopupState::None;
        self.diff_mode.enter(self.show_file_tree);
        self.diff_mode.ref_a = "HEAD".to_string();
        self.diff_mode.ref_a_display = "HEAD (local)".to_string();
        self.diff_mode.ref_b = upstream.clone();
        self.diff_mode.ref_b_display = upstream.clone();
        self.diff_mode.ahead_behind = Some((ahead, behind));
        self.diff_mode.comparison_loading = true;
        self.diff_mode
            .set_focus(crate::gui::modes::diff_mode::DiffModeFocus::Commits);
        self.clear_diff_view();
        self.start_compare_load_async("HEAD".to_string(), upstream);
    }

    pub(crate) fn cancel_compare_load(&mut self) {
        self.compare_load_generation = self.compare_load_generation.wrapping_add(1);
        self.compare_load_rx = None;
        self.diff_mode.comparison_loading = false;
    }

    fn start_compare_load_async(&mut self, ref_a: String, ref_b: String) {
        self.compare_load_generation = self.compare_load_generation.wrapping_add(1);
        let generation = self.compare_load_generation;
        let git = Arc::clone(&self.git);
        let (tx, rx) = mpsc::channel();
        self.compare_load_rx = Some(rx);
        self.diff_mode.comparison_loading = true;

        std::thread::spawn(move || {
            let files = git.diff_refs_files(&ref_a, &ref_b);
            let files_ok = files.is_ok();
            let _ = tx.send(CompareLoadEvent::Files {
                generation,
                ref_a: ref_a.clone(),
                ref_b: ref_b.clone(),
                result: files,
            });
            if !files_ok {
                return;
            }

            // This is the potentially expensive part on very divergent repositories.
            // Keep it off the UI thread and cap the first page just like normal Compare mode.
            let commits = git.load_compare_commits(&ref_a, &ref_b, DEFAULT_COMMIT_LIMIT, 0);
            let _ = tx.send(CompareLoadEvent::Commits {
                generation,
                ref_a,
                ref_b,
                result: commits,
            });
        });
    }

    fn receive_compare_load_results(&mut self) {
        let mut finished = false;
        loop {
            let event = match self.compare_load_rx.as_ref() {
                Some(rx) => match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                },
                None => break,
            };

            match event {
                CompareLoadEvent::Files {
                    generation,
                    ref_a,
                    ref_b,
                    result,
                } => {
                    if generation != self.compare_load_generation
                        || !self.diff_mode.active
                        || self.diff_mode.ref_a != ref_a
                        || self.diff_mode.ref_b != ref_b
                    {
                        continue;
                    }
                    match result {
                        Ok(files) => {
                            self.diff_mode.diff_files = files;
                            self.diff_mode.diff_files_selected = 0;
                            self.diff_mode.diff_files_scroll = 0;
                            self.diff_mode.viewport_manually_scrolled = false;
                            self.diff_mode.collapsed_dirs.clear();
                            self.diff_mode.clear_list_search();
                            controller::diff_mode::update_diff_mode_tree(self);
                            self.needs_diff_refresh = true;
                        }
                        Err(err) => {
                            self.diff_mode.comparison_loading = false;
                            finished = true;
                            self.show_error("Compare failed", err);
                        }
                    }
                }
                CompareLoadEvent::Commits {
                    generation,
                    ref_a,
                    ref_b,
                    result,
                } => {
                    finished = true;
                    if generation != self.compare_load_generation
                        || !self.diff_mode.active
                        || self.diff_mode.ref_a != ref_a
                        || self.diff_mode.ref_b != ref_b
                    {
                        continue;
                    }
                    self.diff_mode.comparison_loading = false;
                    match result {
                        Ok(commits) => {
                            self.diff_mode.commits = commits;
                            self.diff_mode.commits_selected = 0;
                            self.diff_mode.commits_scroll = 0;
                            self.diff_mode.commits_viewport_manually_scrolled = false;
                            self.diff_mode.commits_revision =
                                self.diff_mode.commits_revision.wrapping_add(1);
                            self.needs_diff_refresh = true;
                        }
                        Err(err) => self.show_error("Compare history failed", err),
                    }
                }
            }
        }

        if finished {
            self.compare_load_rx = None;
        }
    }

    /// Kick off a status-only files refresh on a background thread.
    fn start_files_refresh_async(&mut self) {
        if self.index_mutation_busy.load(Ordering::Acquire) {
            self.needs_files_refresh = true;
            return;
        }
        if self.files_refresh_in_progress {
            return;
        }
        self.needs_files_refresh = false;
        self.files_refresh_in_progress = true;
        let git = Arc::clone(&self.git);
        let (tx, rx) = mpsc::channel();
        self.files_refresh_rx = Some(rx);
        std::thread::spawn(move || {
            let result = git.refresh_files_status_only().map(|files| {
                let totals = git.worktree_diff_totals(&files).ok();
                (files, totals)
            });
            let _ = tx.send(result);
        });
    }

    /// Queue an index mutation on the single serialized index worker.
    /// The UI has already applied its optimistic state; the worker returns an
    /// authoritative status snapshot after draining the current burst.
    pub(crate) fn enqueue_stage_then_refresh(&mut self, paths: Vec<String>, stage: bool) {
        if paths.is_empty() {
            return;
        }
        self.index_mutation_generation = self.index_mutation_generation.wrapping_add(1);
        if self
            .index_mutation_tx
            .send(IndexMutationCommand {
                generation: self.index_mutation_generation,
                git: Arc::clone(&self.git),
                mutation: IndexMutation::Paths { paths, stage },
            })
            .is_err()
        {
            self.popup = PopupState::Message {
                title: "Stage failed".into(),
                message: "Index worker is unavailable".into(),
                kind: MessageKind::Error,
            };
        }
    }

    /// Queue stage/unstage-all through the same serialized index worker.
    pub(crate) fn enqueue_stage_all_then_refresh(&mut self, stage: bool) {
        self.index_mutation_generation = self.index_mutation_generation.wrapping_add(1);
        if self
            .index_mutation_tx
            .send(IndexMutationCommand {
                generation: self.index_mutation_generation,
                git: Arc::clone(&self.git),
                mutation: IndexMutation::All { stage },
            })
            .is_err()
        {
            self.popup = PopupState::Message {
                title: "Stage failed".into(),
                message: "Index worker is unavailable".into(),
                kind: MessageKind::Error,
            };
        }
    }

    fn receive_index_mutation_results(&mut self) {
        while let Ok(result) = self.index_mutation_rx.try_recv() {
            let is_latest = result.generation == self.index_mutation_generation;
            match result.files {
                Ok(files) if is_latest => {
                    let mut model = self.model.lock().unwrap();
                    model.set_files(files);
                    if let Some((added, deleted)) = result.totals {
                        model.total_additions = added;
                        model.total_deletions = deleted;
                    }
                    if self.show_file_tree {
                        self.file_tree_nodes = build_file_tree(&model.files, &self.collapsed_dirs);
                        self.context_mgr.files_list_len_override = Some(self.file_tree_nodes.len());
                    } else {
                        self.file_tree_nodes.clear();
                        self.context_mgr.files_list_len_override = None;
                    }
                    drop(model);
                    self.invalidate_files_diff_previews();
                    self.needs_diff_refresh = true;
                }
                Ok(_) => {}
                Err(err) if is_latest => self.show_error("Refresh failed", err),
                Err(_) => {}
            }
            if let Some(message) = result.mutation_error {
                self.show_notification(PopupState::Message {
                    title: "Stage failed".into(),
                    message,
                    kind: MessageKind::Error,
                });
            }
        }
        if self.needs_files_refresh && !self.index_mutation_busy.load(Ordering::Acquire) {
            self.start_files_refresh_async();
        }
    }

    fn focus_conflicting_file_after_refresh(&mut self) {
        let Some(path) = self.pending_conflict_inspect_file.take() else { return; };
        let idx = self.model.lock().unwrap().files.iter().position(|file| file.name == path);
        let Some(idx) = idx else {
            self.show_action_toast(format!("Cannot find {path} in Files"));
            return;
        };
        let row = if self.show_file_tree {
            self.file_tree_nodes
                .iter()
                .position(|node| node.file_index == Some(idx))
                .unwrap_or(idx)
        } else { idx };
        self.context_mgr.set_active(ContextId::Files);
        self.context_mgr.set_selection(row);
        self.needs_diff_refresh = true;
    }

    /// Apply a completed status-only files refresh.
    fn receive_files_refresh(&mut self) {
        let Some(rx) = self.files_refresh_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((files, totals))) => {
                self.files_refresh_rx = None;
                self.files_refresh_in_progress = false;
                {
                    let mut model = self.model.lock().unwrap();
                    model.set_files(files);
                    if let Some((added, deleted)) = totals {
                        model.total_additions = added;
                        model.total_deletions = deleted;
                    }
                    if self.show_file_tree {
                        self.file_tree_nodes = build_file_tree(&model.files, &self.collapsed_dirs);
                        self.context_mgr.files_list_len_override = Some(self.file_tree_nodes.len());
                    } else {
                        self.file_tree_nodes.clear();
                        self.context_mgr.files_list_len_override = None;
                    }
                }
                self.invalidate_files_diff_previews();
                // If more stage ops landed while we were refreshing, do another.
                if self.needs_files_refresh {
                    self.start_files_refresh_async();
                } else {
                    self.focus_conflicting_file_after_refresh();
                    self.needs_diff_refresh = true;
                }
            }
            Ok(Err(err)) => {
                self.files_refresh_rx = None;
                self.files_refresh_in_progress = false;
                self.show_error("Refresh failed", err);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.files_refresh_rx = None;
                self.files_refresh_in_progress = false;
            }
        }
    }

    /// Execute a menu item action. If `override_idx` is Some, use that index;
    /// otherwise use the currently selected index.
    fn execute_menu_action(&mut self, override_idx: Option<usize>) {
        let popup = std::mem::replace(&mut self.popup, PopupState::None);
        if let PopupState::Menu {
            ref items,
            selected,
            ..
        } = popup
        {
            let idx = override_idx.unwrap_or(selected);
            let has_action = items.get(idx).and_then(|i| i.action.as_ref()).is_some();
            if has_action {
                // Stash the menu so async actions can restore it via start_menu_async.
                self.pending_menu_popup = Some(popup);
                // Call the action from the stashed popup.
                let action_result = {
                    let menu = self.pending_menu_popup.as_ref().unwrap();
                    if let PopupState::Menu { items, .. } = menu {
                        let action = items[idx].action.as_ref().unwrap();
                        // SAFETY: We hold a shared ref to pending_menu_popup while calling
                        // action(self). The action may move the popup out of pending_menu_popup
                        // via start_menu_async (which calls .take()), but it won't invalidate
                        // the action pointer because the action is inside items which are moved
                        // as a whole. We use a raw pointer to avoid the borrow conflict.
                        let action_ptr = action as *const dyn Fn(&mut Gui) -> Result<()>;
                        unsafe { (*action_ptr)(self) }
                    } else {
                        Ok(())
                    }
                };
                match action_result {
                    Err(e) => {
                        self.pending_menu_popup = None;
                        self.popup = PopupState::Message {
                            title: "Error".to_string(),
                            message: format!("{}", e),
                            kind: MessageKind::Error,
                        };
                    }
                    Ok(()) => {
                        if self.pending_menu_popup.is_some() {
                            // Action didn't call start_menu_async — it was synchronous.
                            // Discard the stashed menu (popup stays None = menu closed).
                            self.pending_menu_popup = None;
                        }
                    }
                }
            }
        }
    }

    fn receive_ui_jobs(&mut self) {
        while let Ok(job) = self.ui_job_rx.try_recv() {
            job(self);
        }
    }

    /// Handle results from background menu item operations.
    fn receive_menu_async_results(&mut self) {
        if let Ok(result) = self.menu_async_rx.try_recv() {
            // Only process if the popup is still a menu with loading state.
            // If the user pressed Esc, the menu is already gone — discard the result.
            let is_menu_loading = matches!(
                &self.popup,
                PopupState::Menu {
                    loading_index: Some(_),
                    ..
                }
            );
            if !is_menu_loading {
                return;
            }
            match result {
                Ok(outcome) => {
                    // Close the menu
                    self.popup = PopupState::None;
                    match outcome {
                        popup::MenuAsyncResult::CopyToClipboard(url) => {
                            if let Err(e) = Platform::copy_to_clipboard(&url) {
                                self.popup = PopupState::Message {
                                    title: "Error".to_string(),
                                    message: format!("{}", e),
                                    kind: MessageKind::Error,
                                };
                            }
                        }
                        popup::MenuAsyncResult::OpenUrl(url) => {
                            if let Err(e) = Platform::open_file(&url) {
                                self.popup = PopupState::Message {
                                    title: "Error".to_string(),
                                    message: format!("{}", e),
                                    kind: MessageKind::Error,
                                };
                            }
                        }
                    }
                }
                Err(e) => {
                    self.popup = PopupState::Message {
                        title: "No PR found".to_string(),
                        message: format!("{}", e),
                        kind: MessageKind::Info,
                    };
                }
            }
        }
    }

    /// Start the guided pull workflow on the same serialized network lane used
    /// by fetch/push. The worker fetches and classifies HEAD vs upstream before
    /// deciding whether integration is safe or needs a UI choice.
    pub fn start_pull(&mut self) {
        if self.remote_op_label.is_some() || self.pending_remote_op.is_some() {
            return;
        }

        let op: RemoteOpFn = Box::new(|git| Ok(RemoteOpResult::Pull(git.pull()?)));
        if self.auto_fetch_in_flight {
            self.pending_remote_op = Some(PendingRemoteOp {
                kind: RemoteOpKind::Pull,
                title: "Pull".to_string(),
                op,
            });
            return;
        }

        self.launch_remote_op(RemoteOpKind::Pull, "Pull", op);
    }

    fn start_pull_integration(&mut self, upstream: String, strategy: PullStrategy, remember: bool) {
        if self.remote_op_label.is_some() || self.pending_remote_op.is_some() {
            return;
        }

        // The divergence popup may have been shown while a fetch-only model
        // refresh was still streaming. Integration changes HEAD/worktree, so
        // disconnect that old snapshot before mutating and let the completion
        // start one authoritative pull refresh.
        self.git.cancel_worktree_reads();
        self.initial_load_rx = None;
        self.initial_load_received = 0;
        self.refresh_in_progress = false;
        self.files_refresh_rx = None;
        self.files_refresh_in_progress = false;
        self.needs_files_refresh = false;

        let op_upstream = upstream.clone();
        let op: RemoteOpFn = Box::new(move |git| {
            if remember {
                git.remember_pull_strategy(strategy)?;
            }
            let conflicted = git.integrate_upstream(&op_upstream, strategy)?;
            Ok(RemoteOpResult::Pull(PullOutcome::Integrated {
                upstream: op_upstream,
                strategy,
                conflicted,
            }))
        });

        if self.auto_fetch_in_flight {
            self.pending_remote_op = Some(PendingRemoteOp {
                kind: RemoteOpKind::PullIntegrate,
                title: "Pull".to_string(),
                op,
            });
            return;
        }

        self.launch_remote_op(RemoteOpKind::PullIntegrate, "Pull", op);
    }

    /// Run a remote operation (push/pull/fetch) on the single network lane.
    /// If silent auto-fetch is already using the lane, remember the manual
    /// request and launch it as soon as auto-fetch completes.
    pub fn start_remote_op<F>(&mut self, kind: RemoteOpKind, title: &str, _message: &str, op: F)
    where
        F: FnOnce(&GitCommands) -> Result<()> + Send + 'static,
    {
        if self.remote_op_label.is_some() || self.pending_remote_op.is_some() {
            return;
        }

        let op: RemoteOpFn = Box::new(move |git| {
            op(git)?;
            Ok(RemoteOpResult::Done)
        });
        if self.auto_fetch_in_flight {
            self.pending_remote_op = Some(PendingRemoteOp {
                kind,
                title: title.to_string(),
                op,
            });
            return;
        }

        self.launch_remote_op(kind, title, op);
    }

    fn launch_remote_op(&mut self, kind: RemoteOpKind, title: &str, op: RemoteOpFn) {
        let is_checkout = self.pending_checkout_by_name.is_some();
        if is_checkout {
            // Do this at actual launch time, not when a checkout is merely queued
            // behind auto-fetch. Otherwise an unrelated refresh could be left
            // intentionally incomplete while the network lane is still busy.
            self.prepare_for_checkout();
        }

        // Show operation label on the head branch in the sidebar (e.g. "Pushing", "Pulling").
        let label = match title {
            "Push" => "Pushing",
            "Pull" => "Pulling",
            "Fetch" => "Fetching",
            other => other,
        };

        self.remote_op_label = Some(label.to_string());
        self.remote_op_success_at = None;

        let git = Arc::clone(&self.git);
        let tx = self.remote_op_tx.clone();

        std::thread::spawn(move || {
            // Capture refs before network operations so an up-to-date pull/fetch
            // can prove that the expensive global history refresh is unnecessary.
            let before_refs = if matches!(kind, RemoteOpKind::Pull | RemoteOpKind::Fetch) {
                git.ref_snapshot().ok()
            } else {
                None
            };

            let result = op(&git).map(|op_result| {
                let (pull_outcome, backup_branch) = match op_result {
                    RemoteOpResult::Done => (None, None),
                    RemoteOpResult::Pull(outcome) => (Some(outcome), None),
                    RemoteOpResult::Backup(backup) => (None, Some(backup)),
                };
                let head = if is_checkout {
                    // Read on the worker, never on the UI thread. If this read
                    // fails, checkout still succeeded; the refresh is the fallback.
                    git.head_state().ok()
                } else {
                    None
                };
                let refs_changed = if let Some(before) = before_refs.as_ref() {
                    match git.ref_snapshot() {
                        Ok(after) => Some(after != *before),
                        // Preserve the existing conservative behavior when the
                        // post-operation snapshot cannot be read.
                        Err(_) => Some(true),
                    }
                } else {
                    None
                };

                RemoteOpCompletion {
                    backup_branch,
                    head,
                    pull_outcome,
                    refs_changed,
                }
            });

            let _ = tx.send((kind, result));
        });
    }

    /// Run a stash mutation without falling back to the expensive full-model refresh.
    ///
    /// Stash creation/apply/pop can only change the worktree/index and stash reflog;
    /// drop/rename only change the stash reflog. Refresh those pieces directly and
    /// in parallel so stash actions do not reload branches, commits, remotes, tags,
    /// worktrees, reflog history, contributors, and other unrelated model state.
    pub(crate) fn start_background_stash_mutation<F>(
        &mut self,
        title: &str,
        refresh_worktree: bool,
        op: F,
    ) where
        F: FnOnce(&GitCommands) -> Result<()> + Send + 'static,
    {
        if self.background_mutation_label.is_some() {
            return;
        }

        self.background_mutation_label = Some(title.to_string());
        let git = Arc::clone(&self.git);
        let tx = self.ui_job_tx.clone();
        let title = title.to_string();

        std::thread::spawn(move || {
            let operation_result = op(&git);

            // Keep worktree-wide reads serialized. On large repositories, running
            // status and shortstat concurrently makes both slower by competing for
            // filesystem/cache bandwidth. The stash list itself is tiny.
            let stash_result = git.load_stash();
            let files_result = refresh_worktree.then(|| git.load_files_status_only());
            let diff_stats_result = files_result.as_ref().and_then(|result| result.as_ref().ok())
                .map(|files| git.worktree_diff_totals(files));

            let _ = tx.send(Box::new(move |gui: &mut Gui| {
                gui.background_mutation_label = None;

                let mut stash_changed = false;
                let mut files_changed = false;
                let mut refresh_error: Option<anyhow::Error> = None;

                {
                    let mut model = gui.model.lock().unwrap();

                    match stash_result {
                        Ok(stashes) => {
                            model.stash_entries = stashes;
                            stash_changed = true;
                        }
                        Err(err) => refresh_error = Some(err),
                    }

                    if let Some(files_result) = files_result {
                        match files_result {
                            Ok(files) => {
                                model.set_files(files);
                                files_changed = true;
                            }
                            Err(err) if refresh_error.is_none() => refresh_error = Some(err),
                            Err(_) => {}
                        }
                    }

                    if let Some(diff_stats_result) = diff_stats_result {
                        match diff_stats_result {
                            Ok((added, deleted)) => {
                                model.total_additions = added;
                                model.total_deletions = deleted;
                            }
                            Err(err) if refresh_error.is_none() => refresh_error = Some(err),
                            Err(_) => {}
                        }
                    }

                    if stash_changed {
                        gui.context_mgr.clamp_selections(&model);
                    }
                }

                if files_changed {
                    gui.rebuild_file_tree_from_model();
                }
                if files_changed || stash_changed {
                    gui.needs_diff_refresh = true;
                }

                if let Err(err) = operation_result {
                    // A failed stash apply/pop can still leave conflict markers or
                    // partial worktree changes. The targeted refresh above already
                    // reflects that state before the error is shown.
                    gui.show_error(&title, err);
                } else if let Some(err) = refresh_error {
                    gui.show_error("Stash refresh failed", err);
                }
            }) as UiJob);
        });
    }

    /// Run a potentially slow local Git mutation without blocking the TUI.
    /// A single lane is intentional: these commands often touch the index,
    /// worktree, or repository metadata and gain nothing from racing each other.
    pub fn start_background_mutation<F>(&mut self, title: &str, op: F)
    where
        F: FnOnce(&GitCommands) -> Result<()> + Send + 'static,
    {
        if self.background_mutation_label.is_some() {
            return;
        }
        self.background_mutation_label = Some(title.to_string());
        let git = Arc::clone(&self.git);
        let tx = self.ui_job_tx.clone();
        let title = title.to_string();
        std::thread::spawn(move || {
            let result = op(&git);
            let _ = tx.send(Box::new(move |gui: &mut Gui| {
                gui.background_mutation_label = None;
                // Even a failed apply/pop can leave conflict markers or partial
                // worktree changes. Always refresh from Git's actual state.
                gui.needs_refresh = true;
                gui.needs_diff_refresh = true;
                if let Err(err) = result {
                    gui.show_error(&title, err);
                }
            }) as UiJob);
        });
    }

    /// Start an async operation for a menu item. Restores the menu popup with a
    /// loading spinner on the item at `index` and spawns a background thread.
    pub fn start_menu_async<F>(&mut self, index: usize, op: F)
    where
        F: FnOnce(&crate::git::GitCommands) -> Result<popup::MenuAsyncResult> + Send + 'static,
    {
        // Restore the menu popup (stashed by execute_menu_action) with loading_index set.
        if let Some(menu) = self.pending_menu_popup.take() {
            if let PopupState::Menu {
                title,
                items,
                selected,
                ..
            } = menu
            {
                self.popup = PopupState::Menu {
                    title,
                    items,
                    selected,
                    loading_index: Some(index),
                };
            }
        }
        let git = Arc::clone(&self.git);
        let tx = self.menu_async_tx.clone();
        std::thread::spawn(move || {
            let result = op(&git);
            let _ = tx.send(result);
        });
    }

    pub(crate) fn ai_commit_generation_active(&self) -> bool {
        self.ai_commit_job.is_some()
    }

    /// Start AI commit message generation on a background thread.
    pub fn start_ai_commit_generation(&mut self) {
        if self.ai_commit_generation_active() {
            return;
        }

        let git = Arc::clone(&self.git);
        let tx = self.ai_commit_tx.clone();
        let cmd = self.config.user_config.git.commit.generate_command.clone();
        let source = self.ai_commit_source.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        self.ai_commit_generation = self.ai_commit_generation.wrapping_add(1);
        let generation = self.ai_commit_generation;
        self.ai_commit_job = Some(AiCommitJob {
            generation,
            cancel,
            cancel_armed_at: None,
        });

        std::thread::spawn(move || {
            let result = match source {
                AiCommitSource::Staged => {
                    crate::git::ai_commit::generate_commit_message_cancellable(
                        git.repo_path(),
                        &cmd,
                        worker_cancel,
                    )
                }
                AiCommitSource::Commit(hash) => git.commit_diff(&hash).and_then(|diff| {
                    crate::git::ai_commit::generate_commit_message_from_diff_cancellable(
                        git.repo_path(),
                        &diff,
                        &cmd,
                        worker_cancel,
                    )
                }),
            };
            let _ = tx.send(AiCommitResult { generation, result });
        });
    }

    fn begin_ai_commit_generation_ui(&mut self) {
        if self.ai_commit_generation_active() {
            return;
        }
        self.start_ai_commit_generation();
    }

    pub fn trigger_ai_commit_generation_from_editor(&mut self) {
        let generate_cmd = self.config.user_config.git.commit.generate_command.trim();
        if self.ai_commit_generation_active() {
            return;
        }
        if generate_cmd.is_empty() {
            self.popup = PopupState::Message {
                title: "AI generation unavailable".to_string(),
                message: "Set git.commit.generateCommand in your config first.".to_string(),
                kind: MessageKind::Error,
            };
            return;
        }

        self.ai_commit_source = match &self.popup {
            PopupState::CommitInput {
                kind: popup::CommitInputKind::Reword,
                ..
            } => {
                let selected = self.context_mgr.selected_active();
                self.model
                    .lock()
                    .unwrap()
                    .commits
                    .get(selected)
                    .map(|commit| AiCommitSource::Commit(commit.hash.clone()))
                    .unwrap_or(AiCommitSource::Staged)
            }
            _ => AiCommitSource::Staged,
        };
        let stashed = std::mem::replace(&mut self.popup, PopupState::None);
        self.pending_commit_popup = Some(stashed);
        self.begin_ai_commit_generation_ui();
    }

    fn handle_ai_commit_cancel_key(&mut self, key: KeyEvent) -> bool {
        if key.code != KeyCode::Esc {
            return false;
        }

        let Some(job) = &mut self.ai_commit_job else {
            return false;
        };

        let now = Instant::now();
        let armed = job
            .cancel_armed_at
            .map(|armed_at| now.duration_since(armed_at) <= Duration::from_millis(900))
            .unwrap_or(false);

        if armed {
            job.cancel.store(true, Ordering::Relaxed);
            self.ai_commit_job = None;
            if let Some(stashed) = self.pending_commit_popup.take() {
                self.saved_commit_popup = Some(stashed);
            }
            true
        } else {
            job.cancel_armed_at = Some(now);
            false
        }
    }

    /// Request diff loading on a background thread if selection changed.
    fn maybe_request_diff(&mut self) {
        // Rebase mode has no diff to load
        if self.rebase_mode.active {
            return;
        }

        // Diff mode has its own diff loading
        if self.diff_mode.active {
            let diff_key = self.current_diff_key();
            let Some(generation) = self.begin_diff_request(diff_key.clone()) else {
                return;
            };

            self.diff_loading = true;
            self.diff_loading_since = Some(Instant::now());

            controller::diff_mode::maybe_request_diff(self, generation, diff_key);
            return;
        }

        let active = self.context_mgr.active();
        let selected = self.context_mgr.selected_active();
        let diff_key = self.current_diff_key();
        let Some(generation) = self.begin_diff_request(diff_key.clone()) else {
            return;
        };

        let model = self.model.lock().unwrap();
        match active {
            ContextId::Files => {
                // Files panel: load and parse async on background thread
                let file_idx = if self.show_file_tree {
                    self.file_tree_nodes
                        .get(selected)
                        .and_then(|n| n.file_index)
                } else {
                    Some(selected)
                };
                if let Some(file) = file_idx.and_then(|i| model.files.get(i)) {
                    let name = file.name.clone();
                    let current_path = file.current_path().to_string();
                    let diff_paths: Vec<String> =
                        file.diff_paths().into_iter().map(str::to_string).collect();
                    let has_staged = file.has_staged_changes;
                    let has_unstaged = file.has_unstaged_changes;
                    let tracked = file.tracked;
                    let conflicted = file.has_merge_conflicts;
                    drop(model);

                    let git = Arc::clone(&self.git);

                    self.diff_loading = true;
                    self.diff_loading_since = Some(Instant::now());
                    self.queue_diff_job(generation, diff_key, move || {
                        let path_refs: Vec<&str> = diff_paths.iter().map(String::as_str).collect();
                        use crate::pager::image_preview::ImageSource;
                        let old_path = path_refs[0];
                        let old = if !tracked {
                            ImageSource::Missing
                        } else {
                            ImageSource::Revision {
                                revision: if has_staged { "HEAD" } else { "" },
                                path: old_path,
                            }
                        };
                        let new = if has_staged && !has_unstaged {
                            ImageSource::Revision {
                                revision: "",
                                path: &current_path,
                            }
                        } else {
                            ImageSource::Worktree(&current_path)
                        };
                        if let Some(parsed) = (!conflicted)
                            .then(|| {
                                image_diff_payload(
                                    &git,
                                    &name,
                                    "",
                                    git.repo_path().join(&current_path).exists(),
                                    old,
                                    new,
                                )
                            })
                            .flatten()
                        {
                            return DiffPayload::Parsed(parsed);
                        }
                        // Single HEAD buffer with coherent line numbers;
                        // hunks are dimmed/tinted staged vs unstaged via
                        // overlap with the unstaged diff. Falls back to the
                        // unstaged-only view when HEAD is unavailable
                        // (e.g. unborn HEAD).
                        if has_unstaged && has_staged {
                            let exists = git.repo_path().join(&current_path).exists();
                            let unstaged = git.diff_file_paths(&path_refs).unwrap_or_default();
                            match git.diff_paths_vs_head(&path_refs) {
                                Ok(head) if !head.is_empty() => {
                                    let parsed = DiffViewState::parse_head_with_staged(
                                        &name, &head, &unstaged, 4, exists,
                                    );
                                    if parsed.lines.is_empty() {
                                        DiffPayload::Empty
                                    } else {
                                        DiffPayload::Parsed(parsed)
                                    }
                                }
                                _ => {
                                    let mut payload = parse_file_diff_payload(
                                        &git,
                                        &name,
                                        &current_path,
                                        &unstaged,
                                        exists,
                                        false,
                                    );
                                    if let DiffPayload::Parsed(ref mut parsed) = payload {
                                        parsed.hunk_staged = vec![false; parsed.hunk_starts.len()];
                                    }
                                    payload
                                }
                            }
                        } else {
                            let diff_result = if has_unstaged {
                                git.diff_file_paths(&path_refs)
                            } else if has_staged {
                                git.diff_file_staged_paths(&path_refs)
                            } else {
                                Ok(String::new())
                            };

                            let exists = git.repo_path().join(&current_path).exists();
                            match diff_result {
                                Ok(diff) if diff.is_empty() && !tracked => {
                                    // Untracked file: everything is unstaged.
                                    let parsed = if git.is_binary_path(&current_path) {
                                        DiffViewState::parse_diff_output(
                                            &current_path,
                                            &synthesize_binary_file_diff(&current_path),
                                            4,
                                            exists,
                                        )
                                    } else {
                                        match git.file_content(&current_path) {
                                            Ok(content) if !content.is_empty() => {
                                                DiffViewState::parse_content(
                                                    &current_path,
                                                    "",
                                                    &content,
                                                    4,
                                                    exists,
                                                )
                                            }
                                            _ => return DiffPayload::Empty,
                                        }
                                    };
                                    let mut parsed = parsed;
                                    parsed.hunk_staged = vec![false; parsed.hunk_starts.len()];
                                    DiffPayload::Parsed(parsed)
                                }
                                Ok(diff) if diff.is_empty() => DiffPayload::Empty,
                                Ok(diff) => {
                                    // Single-side view: classify every hunk
                                    // so the title counts + dimming + menu
                                    // stay correct even at 0-staged or
                                    // all-staged.
                                    let staged_only = has_staged && !has_unstaged;
                                    let mut payload = parse_file_diff_payload(
                                        &git,
                                        &name,
                                        &current_path,
                                        &diff,
                                        exists,
                                        staged_only,
                                    );
                                    if let DiffPayload::Parsed(ref mut parsed) = payload {
                                        parsed.hunk_staged =
                                            vec![staged_only; parsed.hunk_starts.len()];
                                    }
                                    payload
                                }
                                Err(_) => DiffPayload::Empty,
                            }
                        }
                    });
                } else if self.show_file_tree {
                    // Directory node: show combined diff of all child files (async)
                    if let Some(node) = self.file_tree_nodes.get(selected) {
                        if node.is_dir && !node.child_file_indices.is_empty() {
                            // One `git diff HEAD -- dir/` for tracked files under the
                            // directory; only untracked children still need synthesize.
                            let untracked: Vec<String> = node
                                .child_file_indices
                                .iter()
                                .filter_map(|&i| model.files.get(i))
                                .filter(|f| !f.tracked)
                                .map(|f| f.current_path().to_string())
                                .collect();
                            let excluded: std::collections::HashSet<String> = node
                                .child_file_indices
                                .iter()
                                .filter_map(|&i| model.files.get(i))
                                .filter(|f| f.has_merge_conflicts)
                                .flat_map(|f| f.diff_paths().into_iter().map(str::to_string))
                                .collect();
                            let pathspec = pathspec_for_tree_path(&node.path);
                            let dir_name = node.name.clone();
                            drop(model);

                            let git = Arc::clone(&self.git);
                            let gen_counter = Arc::clone(&self.diff_generation);

                            self.diff_loading = true;
                            self.diff_loading_since = Some(Instant::now());
                            self.queue_diff_job(generation, diff_key, move || {
                                if gen_counter.load(Ordering::Relaxed) != generation {
                                    return DiffPayload::Empty;
                                }
                                let paths: Vec<&str> = match pathspec.as_deref() {
                                    Some(p) => vec![p],
                                    None => Vec::new(),
                                };
                                // Directory hover: single HEAD buffer for all
                                // tracked files under the directory, with
                                // hunks classified staged/unstaged via
                                // overlap with the unstaged diff. Untracked
                                // children only exist unstaged. Hunk actions
                                // stay Cancel-only here (no single file).
                                let mut combined_diff =
                                    git.diff_paths_vs_head(&paths).unwrap_or_default();
                                let mut unstaged_combined =
                                    git.diff_file_paths(&paths).unwrap_or_default();
                                for path in &untracked {
                                    if gen_counter.load(Ordering::Relaxed) != generation {
                                        return DiffPayload::Empty;
                                    }
                                    let synth = if git.is_binary_path(path) {
                                        synthesize_binary_file_diff(path)
                                    } else {
                                        let content = git.file_content(path).unwrap_or_default();
                                        if content.is_empty() {
                                            continue;
                                        }
                                        synthesize_new_file_diff(path, &content)
                                    };
                                    if !combined_diff.is_empty() {
                                        combined_diff.push('\n');
                                    }
                                    combined_diff.push_str(&synth);
                                    // Mirror the synthesized section so the
                                    // untracked file classifies as unstaged.
                                    if !unstaged_combined.is_empty() {
                                        unstaged_combined.push('\n');
                                    }
                                    unstaged_combined.push_str(&synth);
                                }

                                if combined_diff.trim().is_empty() {
                                    DiffPayload::Empty
                                } else {
                                    use crate::pager::side_by_side::{
                                        DiffViewState as DVS, head_block_staged_flags,
                                    };
                                    let mut parsed =
                                        DVS::parse_diff_output(&dir_name, &combined_diff, 4, true);
                                    parsed.hunk_staged = head_block_staged_flags(
                                        &combined_diff,
                                        &unstaged_combined,
                                        4,
                                    );
                                    if parsed.hunk_staged.len() != parsed.hunk_starts.len() {
                                        parsed.hunk_staged = vec![false; parsed.hunk_starts.len()];
                                    }
                                    crate::pager::image_preview::attach_inline_image_previews(
                                        &mut parsed,
                                        git.repo_path(),
                                        &combined_diff,
                                        "HEAD",
                                        None,
                                        &excluded,
                                    );
                                    if parsed.lines.is_empty() {
                                        DiffPayload::Empty
                                    } else {
                                        DiffPayload::Parsed(parsed)
                                    }
                                }
                            });
                        } else {
                            drop(model);
                            self.clear_diff_view();
                        }
                    } else {
                        drop(model);
                        self.clear_diff_view();
                    }
                } else {
                    drop(model);
                    self.clear_diff_view();
                }
            }
            ContextId::Commits => {
                // Commits: load and parse async on background thread
                if let Some(commit) = model.commits.get(selected) {
                    let hash = commit.hash.clone();
                    drop(model);

                    let git = Arc::clone(&self.git);
                    let generation_counter = Arc::clone(&self.diff_generation);

                    self.diff_loading = true;
                    self.diff_loading_since = Some(Instant::now());
                    self.queue_diff_job(generation, diff_key, move || {
                        commit_diff_payload_for_generation(
                            &git,
                            &hash,
                            "commit",
                            &generation_counter,
                            generation,
                        )
                    });
                } else {
                    drop(model);
                    self.clear_diff_view();
                }
            }
            ContextId::Reflog => {
                // Reflog: load and parse commit diff async
                if let Some(commit) = model.reflog_commits.get(selected) {
                    let hash = commit.hash.clone();
                    drop(model);

                    let git = Arc::clone(&self.git);
                    let generation_counter = Arc::clone(&self.diff_generation);

                    self.diff_loading = true;
                    self.diff_loading_since = Some(Instant::now());
                    self.queue_diff_job(generation, diff_key, move || {
                        commit_diff_payload_for_generation(
                            &git,
                            &hash,
                            "reflog",
                            &generation_counter,
                            generation,
                        )
                    });
                } else {
                    drop(model);
                    self.clear_diff_view();
                }
            }
            ContextId::Stash => {
                // Stash: load and parse async
                if let Some(entry) = model.stash_entries.get(selected) {
                    let index = entry.index;
                    drop(model);

                    let git = Arc::clone(&self.git);

                    self.diff_loading = true;
                    self.diff_loading_since = Some(Instant::now());
                    self.queue_diff_job(generation, diff_key, move || {
                        stash_diff_payload(&git, index)
                    });
                } else {
                    drop(model);
                    self.clear_diff_view();
                }
            }
            ContextId::BranchCommits => {
                // BranchCommits: load and parse commit diff async
                if let Some(commit) = model.sub_commits.get(selected) {
                    let hash = commit.hash.clone();
                    drop(model);

                    let git = Arc::clone(&self.git);
                    let generation_counter = Arc::clone(&self.diff_generation);

                    self.diff_loading = true;
                    self.diff_loading_since = Some(Instant::now());
                    self.queue_diff_job(generation, diff_key, move || {
                        commit_diff_payload_for_generation(
                            &git,
                            &hash,
                            "commit",
                            &generation_counter,
                            generation,
                        )
                    });
                } else {
                    drop(model);
                    self.clear_diff_view();
                }
            }
            ContextId::CommitFiles | ContextId::StashFiles | ContextId::BranchCommitFiles => {
                // CommitFiles/StashFiles/BranchCommitFiles: load and parse diff async
                let file_idx = if self.show_commit_file_tree {
                    self.commit_file_tree_nodes
                        .get(selected)
                        .and_then(|n| n.file_index)
                } else {
                    Some(selected)
                };
                if let Some(commit_file) = file_idx.and_then(|i| model.commit_files.get(i)) {
                    let name = commit_file.name.clone();
                    let current_path = commit_file.current_path().to_string();
                    let hash = self.commit_files_hash.clone();
                    drop(model);

                    let git = Arc::clone(&self.git);

                    self.diff_loading = true;
                    self.diff_loading_since = Some(Instant::now());
                    self.queue_diff_job(generation, diff_key, move || {
                        if let Ok(diff) = git.diff_commit_file(&hash, &name) {
                            if diff.is_empty() {
                                DiffPayload::Empty
                            } else {
                                parse_commit_file_diff_payload(
                                    &git,
                                    &hash,
                                    &name,
                                    &current_path,
                                    &diff,
                                )
                            }
                        } else {
                            DiffPayload::Empty
                        }
                    });
                } else if self.show_commit_file_tree {
                    // Directory node in tree view: show combined diff of all child files
                    if let Some(node) = self.commit_file_tree_nodes.get(selected) {
                        if node.is_dir && !node.child_file_indices.is_empty() {
                            // Single pathspec-filtered `git show`/`git diff` — not N× per file.
                            let pathspec = pathspec_for_tree_path(&node.path);
                            let dir_name = node.name.clone();
                            let hash = self.commit_files_hash.clone();
                            drop(model);

                            let git = Arc::clone(&self.git);
                            let gen_counter = Arc::clone(&self.diff_generation);

                            self.diff_loading = true;
                            self.diff_loading_since = Some(Instant::now());
                            self.queue_diff_job(generation, diff_key, move || {
                                if gen_counter.load(Ordering::Relaxed) != generation {
                                    return DiffPayload::Empty;
                                }
                                let paths: Vec<&str> = match pathspec.as_deref() {
                                    Some(p) => vec![p],
                                    None => Vec::new(),
                                };
                                let combined_diff =
                                    git.diff_commit_paths(&hash, &paths).unwrap_or_default();
                                if combined_diff.is_empty() {
                                    DiffPayload::Empty
                                } else {
                                    let mut parsed = DiffViewState::parse_diff_output(
                                        &dir_name,
                                        &combined_diff,
                                        4,
                                        true,
                                    );
                                    let parent = format!("{hash}^1");
                                    crate::pager::image_preview::attach_inline_image_previews(
                                        &mut parsed,
                                        git.repo_path(),
                                        &combined_diff,
                                        &parent,
                                        Some(&hash),
                                        &HashSet::new(),
                                    );
                                    DiffPayload::Parsed(parsed)
                                }
                            });
                        } else {
                            drop(model);
                            self.clear_diff_view();
                        }
                    } else {
                        drop(model);
                        self.clear_diff_view();
                    }
                } else {
                    // No file selected — clear diff
                    drop(model);
                    self.clear_diff_view();
                }
            }
            _ => {
                drop(model);
                self.clear_diff_view();
            }
        }
    }

    /// Repo-level keybindings that work regardless of which panel is focused
    /// (including the diff panel). Returns Ok(true) if the key was consumed.
    fn try_handle_global_repo_keys(&mut self, key: KeyEvent) -> Result<bool> {
        let kb = self.config.user_config.keybinding.clone();

        // Push is universal (except in Tags); Pull is local to Status,
        // Branches, and Remotes. In Commits, `p` belongs to patch actions.
        if context_claims_global_repo_key(self.context_mgr.active(), key, &kb) {
            return Ok(false);
        }

        if matches_key(key, &kb.universal.push_files) || matches_key(key, &kb.universal.pull_files)
        {
            controller::remotes::handle_key(self, key, &kb)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Split-resize shortcuts shared by the main UI and compare mode.
    fn try_handle_panel_resize_key(&mut self, key: KeyEvent) -> bool {
        let keybindings = &self.config.user_config.keybinding;
        // Side-panel resize: orientation-aware.
        // Portrait (vertical stack): side on top, diff on bottom.
        //   Alt+h/l → shrink/expand by step
        //   Alt+k → diff pane full (ratio 0.0), Alt+j → side pane full (ratio 1.0)
        // Landscape (horizontal split): side on left, diff on right.
        //   Alt+h/l → shrink/expand by step, Alt+k → side full, Alt+j → main full

        let portrait =
            layout::should_use_portrait(self.layout.width, self.layout.height, self.screen_mode);
        let shrink_key = matches_key(key, &keybindings.universal.shrink_side_panel);
        let expand_key = matches_key(key, &keybindings.universal.expand_side_panel);
        if shrink_key || expand_key {
            const STEP: f64 = 0.05;
            let delta = if shrink_key { -STEP } else { STEP };
            self.layout.side_panel_ratio = (self.layout.side_panel_ratio + delta).clamp(0.0, 1.0);
            return true;
        }

        if matches_key(key, &keybindings.universal.side_panel_full) {
            // Alt+k: diff full in portrait, side full in landscape
            self.layout.side_panel_ratio = if portrait { 0.0 } else { 1.0 };
            return true;
        }
        if matches_key(key, &keybindings.universal.main_panel_full) {
            // Alt+j: side full in portrait, main full in landscape
            self.layout.side_panel_ratio = if portrait { 1.0 } else { 0.0 };
            return true;
        }
        if matches_key(key, &keybindings.universal.reset_side_panel) {
            self.layout.side_panel_ratio = self.config.user_config.gui.side_panel_width;
            return true;
        }

        false
    }

    /// Compare layout shortcuts run only after its text-input handlers.
    pub fn selected_file_index(&self) -> Option<usize> {
        let selected = self.context_mgr.selected_active();
        if self.show_file_tree {
            self.file_tree_nodes
                .get(selected)
                .and_then(|node| node.file_index)
        } else {
            Some(selected)
        }
    }

    fn commit_history_path(config: &AppConfig) -> std::path::PathBuf {
        config.state_dir.join("commit_message_history")
    }

    fn persist_command_log_visibility(&self) {
        if let Ok(mut state) = AppState::load(&self.config.state_path) {
            state.show_command_log = Some(self.show_command_log);
            let _ = state.save(&self.config.state_path);
        }
    }

    pub fn persist_file_tree_visibility(&self) {
        if let Ok(mut state) = AppState::load(&self.config.state_path) {
            state.show_file_tree = Some(self.show_file_tree);
            let _ = state.save(&self.config.state_path);
        }
    }

    pub fn persist_commit_details_visibility(&self) {
        if let Ok(mut state) = AppState::load(&self.config.state_path) {
            state.show_commit_details = Some(self.show_commit_details);
            let _ = state.save(&self.config.state_path);
        }
    }

    pub fn persist_diff_line_wrap(&self) {
        if let Ok(mut state) = AppState::load(&self.config.state_path) {
            state.diff_line_wrap = Some(self.diff_view.wrap);
            let _ = state.save(&self.config.state_path);
        }
    }

    pub fn persist_diff_view_layout(&self) {
        if let Ok(mut state) = AppState::load(&self.config.state_path) {
            state.diff_view = Some(self.diff_view.view_layout.as_state_value().to_string());
            let _ = state.save(&self.config.state_path);
        }
    }

    fn load_commit_history(config: &AppConfig) -> Vec<String> {
        let path = Self::commit_history_path(config);
        match std::fs::read_to_string(&path) {
            Ok(contents) => contents
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Effective wrap width for the commit-body textarea, derived from popup
    /// geometry and the user's `git.commit.auto_wrap_width` config.
    fn commit_body_wrap_width(&self) -> usize {
        popup::commit_body_text_width(
            self.layout.width.max(1),
            self.config.user_config.git.commit.auto_wrap_width,
        )
    }

    fn submit_commit_input(&mut self) {
        let popup_state = std::mem::replace(&mut self.popup, PopupState::None);
        let PopupState::CommitInput {
            summary_textarea,
            body_state,
            on_confirm,
            ..
        } = popup_state
        else {
            self.popup = popup_state;
            return;
        };

        let summary = summary_textarea.lines().join("");
        let raw_body = body_state.raw().trim();
        let body = if raw_body.is_empty() {
            String::new()
        } else {
            let commit_config = &self.config.user_config.git.commit;
            if commit_config.auto_wrap_commit_message && commit_config.auto_wrap_width > 0 {
                popup::hard_wrap_commit_body(raw_body, commit_config.auto_wrap_width)
            } else {
                popup::unwrap_commit_body(raw_body)
            }
        };
        let text = if body.is_empty() {
            summary
        } else {
            format!("{}\n\n{}", summary, body)
        };

        if !text.trim().is_empty() {
            self.commit_message_history.retain(|m| m != &text);
            self.commit_message_history.insert(0, text.clone());
            self.commit_message_history.truncate(50);
            self.save_commit_history();
        }
        self.commit_history_idx = None;
        self.saved_commit_popup = None;
        if let Err(e) = on_confirm(self, &text) {
            self.popup = PopupState::Message {
                title: "Error".to_string(),
                message: format!("{}", e),
                kind: MessageKind::Error,
            };
        }
    }

    fn commit_input_scroll_animation_active(&self) -> bool {
        matches!(
            &self.popup,
            PopupState::CommitInput {
                body_scroll_remaining,
                ..
            } if *body_scroll_remaining != 0
        )
    }

    fn commit_body_visible_rows(&self) -> usize {
        // views.rs: popup border (2 rows) + Summary/Description chrome (5 rows).
        popup::commit_popup_height(self.layout.height)
            .saturating_sub(7)
            .max(1) as usize
    }

    fn commit_body_effective_scroll_top(
        body_textarea: &tui_textarea::TextArea<'static>,
        requested_top: usize,
        visible_rows: usize,
    ) -> usize {
        let visible_rows = visible_rows.max(1);
        let line_count = body_textarea.lines().len().max(1);
        let max_top = line_count.saturating_sub(visible_rows);
        let cursor_row = body_textarea.cursor().0.min(line_count.saturating_sub(1));
        let mut top = requested_top.min(max_top);
        if cursor_row < top {
            top = cursor_row;
        } else if cursor_row >= top.saturating_add(visible_rows) {
            top = cursor_row
                .saturating_add(1)
                .saturating_sub(visible_rows)
                .min(max_top);
        }
        top
    }

    fn move_commit_input_description_boundary(&mut self, to_bottom: bool) {
        let wrap_width = self.commit_body_wrap_width();
        let PopupState::CommitInput {
            focus,
            summary_textarea,
            body_textarea,
            body_state,
            mode,
            pending_command,
            visual_line_anchor,
            body_scroll_remaining,
            body_scroll_top,
            ..
        } = &mut self.popup
        else {
            return;
        };

        // `gg` / `G` are Description motions. From Normal mode the Summary
        // field crosses into Description; Visual selections deliberately stay
        // local to their field because TextArea cannot represent a selection
        // spanning the two independent editors.
        if *focus == popup::CommitInputFocus::Summary {
            if mode.is_visual() {
                *pending_command = popup::CommitPendingCommand::None;
                return;
            }
            popup::set_commit_input_focus(
                focus,
                summary_textarea,
                body_textarea,
                popup::CommitInputFocus::Body,
            );
        }

        if to_bottom {
            if *mode == popup::CommitInputMode::VisualLine {
                body_state.move_to_last_visual_line_start(wrap_width);
            } else {
                body_state.move_to_last_logical_line_start();
            }
        } else {
            body_state.move_to_start();
        }

        match *mode {
            popup::CommitInputMode::VisualLine => {
                if let Some(anchor) = *visual_line_anchor {
                    body_state.select_visual_line_range(body_textarea, wrap_width, anchor);
                }
            }
            popup::CommitInputMode::Visual | popup::CommitInputMode::Normal => {
                if *mode == popup::CommitInputMode::Normal {
                    body_state.normalize_normal_cursor(wrap_width);
                }
                body_state.apply_cursor_into(body_textarea, wrap_width);
            }
            popup::CommitInputMode::Insert => {}
        }

        // A direct Vim jump supersedes any in-flight smooth scroll. The render
        // path will pull this zero-based requested viewport to the new cursor
        // when the target lies off-screen.
        *body_scroll_remaining = 0;
        *body_scroll_top = 0;
        *pending_command = popup::CommitPendingCommand::None;
    }

    fn queue_commit_input_scroll(&mut self, rows: isize) {
        if rows == 0 {
            return;
        }
        let visible_rows = self.commit_body_visible_rows();
        let PopupState::CommitInput {
            body_textarea,
            body_scroll_remaining,
            body_scroll_top,
            ..
        } = &mut self.popup
        else {
            return;
        };

        // Ctrl-J/K is a cursor motion first and a viewport motion second, just
        // like Vim/Neovim Ctrl-D/U.  Do NOT disable it merely because every
        // line currently fits in the viewport: in that case the cursor should
        // still move while the viewport remains at row 0.  Earlier versions
        // returned here when max_top == 0, which made the binding look dead for
        // the common short-description case.
        *body_scroll_top =
            Self::commit_body_effective_scroll_top(body_textarea, *body_scroll_top, visible_rows);
        *body_scroll_remaining = rows.clamp(-999, 999);
    }

    fn tick_commit_input_scroll_animation(&mut self) {
        let wrap_width = self.commit_body_wrap_width();
        let visible_rows = self.commit_body_visible_rows();
        let PopupState::CommitInput {
            body_textarea,
            body_state,
            body_scroll_remaining,
            body_scroll_top,
            mode,
            ..
        } = &mut self.popup
        else {
            return;
        };
        if *body_scroll_remaining == 0 {
            return;
        }

        // Animate the Neovim Ctrl-D/U model directly: the editing cursor and
        // the visible viewport advance together by one visual row per frame.
        // Keeping those two pieces in lock-step is the key detail the earlier
        // attempts missed.  tui-textarea's private viewport is not involved at
        // all; BodySoftWrap owns the cursor and `body_scroll_top` owns exactly
        // which wrapped row the renderer starts from.
        let step: isize = if *body_scroll_remaining > 0 { 1 } else { -1 };
        let before_cursor = body_state.visual_cursor(wrap_width);
        if step > 0 {
            if *mode == popup::CommitInputMode::Insert {
                body_state.move_visual_down(wrap_width);
            } else {
                body_state.move_visual_down_normal(wrap_width);
            }
        } else if *mode == popup::CommitInputMode::Insert {
            body_state.move_visual_up(wrap_width);
        } else {
            body_state.move_visual_up_normal(wrap_width);
        }
        let after_cursor = body_state.visual_cursor(wrap_width);
        if after_cursor != before_cursor {
            body_state.apply_cursor_into(body_textarea, wrap_width);
        }

        let line_count = body_textarea.lines().len().max(1);
        let max_top = line_count.saturating_sub(visible_rows.max(1));
        let before_top = *body_scroll_top;
        *body_scroll_top = if step > 0 {
            (*body_scroll_top).saturating_add(1).min(max_top)
        } else {
            (*body_scroll_top).saturating_sub(1)
        };

        // If the entire description fits, max_top is zero: the cursor still
        // animates (matching Neovim), but the viewport correctly stays fixed.
        // At the opposite extreme, once the viewport reaches the file edge the
        // cursor may continue to its target without creating blank space.
        if after_cursor == before_cursor && *body_scroll_top == before_top {
            *body_scroll_remaining = 0;
            return;
        }

        // Clamp once more against the new cursor in case an edit/resize left a
        // stale top row before this command began.  This never cancels the
        // lock-step motion above because the cursor moved in the same direction.
        *body_scroll_top =
            Self::commit_body_effective_scroll_top(body_textarea, *body_scroll_top, visible_rows);
        *body_scroll_remaining -= step;
    }

    fn save_commit_history(&self) {
        let path = Self::commit_history_path(&self.config);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let contents = self.commit_message_history.join("\0");
        let _ = std::fs::write(&path, contents);
    }

    pub fn update_file_tree_state(&mut self) {
        if self.show_file_tree {
            let model = self.model.lock().unwrap();
            self.file_tree_nodes = build_file_tree(&model.files, &self.collapsed_dirs);
            self.context_mgr.files_list_len_override = Some(self.file_tree_nodes.len());
        } else {
            self.file_tree_nodes.clear();
            self.context_mgr.files_list_len_override = None;
        }
    }

    /// Exit sub-contexts (like CommitFiles) back to their parent context
    /// before navigating away to another window.
    fn exit_sub_contexts(&mut self) {
        self.range_select_anchor = None;
        if self.context_mgr.active() == ContextId::CommitFiles {
            self.context_mgr.set_active(ContextId::Commits);
        }
        if self.context_mgr.active() == ContextId::StashFiles {
            self.context_mgr.set_active(ContextId::Stash);
        }
        if self.context_mgr.active() == ContextId::BranchCommitFiles {
            self.context_mgr.set_active(ContextId::BranchCommits);
        }
        if self.context_mgr.active() == ContextId::BranchCommits {
            self.context_mgr.set_active(ContextId::Branches);
        }
        if self.context_mgr.active() == ContextId::RemoteBranches {
            self.context_mgr.set_active(ContextId::Remotes);
        }
    }

    fn select_startup_head(&mut self) -> Result<()> {
        if std::mem::take(&mut self.pending_startup_head)
            && self.context_mgr.active() == ContextId::Commits
        {
            self.focus_head_commit()?;
        }
        Ok(())
    }

    fn focus_head_commit(&mut self) -> Result<()> {
        // HEAD may not be the first all-ref commit, and may be absent from a
        // filtered or paginated list. Never change filters or inject a commit
        // that doesn't match them: reuse the separate revision-history view.
        let hash = self.git.head_hash().unwrap_or_default();
        let selected = self
            .model
            .lock()
            .unwrap()
            .commits
            .iter()
            .position(|commit| commit.hash == hash);
        let (context, index) = if let Some(index) = selected {
            (ContextId::Commits, index)
        } else if hash.is_empty() {
            // Unborn repository: an empty commit panel is still a valid target.
            (ContextId::Commits, 0)
        } else {
            let commits = self.git.load_commits_for_branch(&hash, 300)?;
            self.model.lock().unwrap().set_sub_commits(commits);
            self.branch_commits_name = "HEAD".to_string();
            self.sub_commits_parent_context = ContextId::Commits;
            (ContextId::BranchCommits, 0)
        };
        self.context_mgr.set_active(context);
        self.context_mgr.set_selection(index);
        self.context_mgr.set_scroll_offset(context, index);
        self.needs_diff_refresh = true;
        Ok(())
    }

    fn next_screen_mode(&mut self) {
        self.screen_mode = match self.screen_mode {
            ScreenMode::Normal => ScreenMode::Half,
            ScreenMode::Half => ScreenMode::Full,
            ScreenMode::Full => ScreenMode::Normal,
        };
    }

    fn prev_screen_mode(&mut self) {
        self.screen_mode = match self.screen_mode {
            ScreenMode::Normal => ScreenMode::Full,
            ScreenMode::Half => ScreenMode::Normal,
            ScreenMode::Full => ScreenMode::Half,
        };
    }
}

/// Split a commit message into (summary, body).
/// The summary is the first line; the body is everything after the first blank line separator.
fn split_commit_message(msg: &str) -> (String, String) {
    match msg.find('\n') {
        Some(idx) => {
            let summary = msg[..idx].to_string();
            let rest = msg[idx + 1..].trim_start_matches('\n').to_string();
            (summary, rest)
        }
        None => (msg.to_string(), String::new()),
    }
}

/// Auto-wrap all lines in a textarea so no line exceeds `wrap_width`.
/// Rebuilds the entire textarea content with hard line breaks at word boundaries.
/// Soft-wrap: like `auto_wrap_textarea` but preserves every character (including
/// spaces at line breaks). Inserts visual newlines only — callers join with `""`
/// at submit time to recover the original string. Used for single-line popup
/// inputs (branch name, tag name, etc.) that need browser-textarea-style visual
/// wrapping without polluting the value sent downstream.
fn soft_wrap_textarea(textarea: &mut tui_textarea::TextArea<'static>, wrap_width: usize) {
    if wrap_width == 0 {
        return;
    }

    let raw: String = textarea.lines().join("");
    if raw.is_empty() {
        return;
    }
    let chars: Vec<char> = raw.chars().collect();

    // Skip if already laid out correctly: every line ≤ wrap_width, and every
    // non-final line is exactly wrap_width chars.
    let lines = textarea.lines();
    let last = lines.len().saturating_sub(1);
    let already_ok = lines.iter().enumerate().all(|(i, l)| {
        let n = l.chars().count();
        if i < last {
            n == wrap_width
        } else {
            n <= wrap_width
        }
    });
    if already_ok {
        return;
    }

    // Track absolute char offset of cursor so we can restore it after rewrap.
    let (cursor_row, cursor_col) = textarea.cursor();
    let mut cursor_abs = 0usize;
    for (i, line) in textarea.lines().iter().enumerate() {
        let line_chars = line.chars().count();
        if i < cursor_row {
            cursor_abs += line_chars;
        } else {
            cursor_abs += cursor_col.min(line_chars);
            break;
        }
    }

    let mut wrapped: Vec<String> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + wrap_width).min(chars.len());
        wrapped.push(chars[start..end].iter().collect());
        start = end;
    }
    let new_text = wrapped.join("\n");

    // Map cursor back into the wrapped layout (each row is exactly wrap_width
    // chars except possibly the last).
    let new_row = cursor_abs / wrap_width;
    let new_col = cursor_abs % wrap_width;

    textarea.select_all();
    textarea.cut();
    textarea.insert_str(&new_text);
    textarea.move_cursor(tui_textarea::CursorMove::Top);
    textarea.move_cursor(tui_textarea::CursorMove::Head);
    for _ in 0..new_row {
        textarea.move_cursor(tui_textarea::CursorMove::Down);
    }
    for _ in 0..new_col {
        textarea.move_cursor(tui_textarea::CursorMove::Forward);
    }
}

fn auto_wrap_textarea(textarea: &mut tui_textarea::TextArea<'static>, wrap_width: usize) {
    if wrap_width == 0 {
        return;
    }

    let needs_wrap = textarea.lines().iter().any(|l| l.len() > wrap_width);
    if !needs_wrap {
        return;
    }

    // Compute cursor's absolute char offset in the original text
    let (cursor_row, cursor_col) = textarea.cursor();
    let original_lines: Vec<String> = textarea.lines().iter().map(|s| s.to_string()).collect();

    let mut cursor_abs = 0usize;
    for (i, line) in original_lines.iter().enumerate() {
        if i < cursor_row {
            cursor_abs += line.len() + 1;
        } else {
            cursor_abs += cursor_col.min(line.len());
            break;
        }
    }

    // Word-wrap all lines
    let mut wrapped: Vec<String> = Vec::new();
    for line in &original_lines {
        if line.len() <= wrap_width {
            wrapped.push(line.clone());
        } else {
            let mut remaining = line.as_str();
            while remaining.len() > wrap_width {
                let break_at = remaining[..wrap_width].rfind(' ').unwrap_or(wrap_width);
                let break_at = if break_at == 0 { wrap_width } else { break_at };
                wrapped.push(remaining[..break_at].to_string());
                remaining = remaining[break_at..].trim_start();
            }
            if !remaining.is_empty() {
                wrapped.push(remaining.to_string());
            }
        }
    }

    let new_text = wrapped.join("\n");

    // Map the absolute cursor offset into the new wrapped text
    // The wrapping only adds newlines (replacing spaces), so character content
    // is preserved. Walk the new text to find the right row/col.
    let mut abs = 0usize;
    let mut new_row = 0;
    let mut new_col = 0;
    for (i, wline) in wrapped.iter().enumerate() {
        if abs + wline.len() >= cursor_abs {
            new_row = i;
            new_col = (cursor_abs - abs).min(wline.len());
            break;
        }
        abs += wline.len() + 1; // +1 for newline
        new_row = i + 1;
        new_col = 0;
    }

    // Replace content and restore cursor
    textarea.select_all();
    textarea.cut();
    textarea.insert_str(&new_text);

    textarea.move_cursor(tui_textarea::CursorMove::Top);
    textarea.move_cursor(tui_textarea::CursorMove::Head);
    for _ in 0..new_row {
        textarea.move_cursor(tui_textarea::CursorMove::Down);
    }
    for _ in 0..new_col {
        textarea.move_cursor(tui_textarea::CursorMove::Forward);
    }
}

/// Read text from the system clipboard.
fn read_clipboard() -> Option<String> {
    Platform::read_from_clipboard().ok()
}

fn matches_key(key: KeyEvent, binding: &str) -> bool {
    if let Some(expected) = parse_key(binding) {
        // Compare code and modifiers, ignore kind/state
        key.code == expected.code && key.modifiers == expected.modifiers
    } else {
        false
    }
}

fn rect_contains(r: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

fn keyboard_enhancement_flags() -> crossterm::event::KeyboardEnhancementFlags {
    // Keep printable input on the terminal's normal text path. In particular,
    // REPORT_ALL_KEYS_AS_ESCAPE_CODES replaces produced text with a logical key
    // identity. Crossterm 0.28 does not expose the protocol's associated-text
    // field, so keyboard layouts, IMEs, or remappers can otherwise turn a typed
    // character into a different shortcut (for example, `q` into `u`).
    //
    // REPORT_EVENT_TYPES is also unnecessary: the UI handles press events only,
    // and enabling it would turn key auto-repeat into ignored Repeat events.
    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
}

/// Enables button, drag, and scroll events without passive pointer-motion events.
///
/// Crossterm's `EnableMouseCapture` also enables DEC mode 1003 (all motion), which can
/// cause terminals to repeatedly focus or redraw while merely moving the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableMouseCaptureWithoutHover;

impl Command for EnableMouseCaptureWithoutHover {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Command::execute_winapi(&crossterm::event::EnableMouseCapture)
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        Command::is_ansi_code_supported(&crossterm::event::EnableMouseCapture)
    }
}

#[cfg(test)]
mod tests;

fn setup_terminal() -> Result<(Term, bool)> {
    terminal::enable_raw_mode()?;
    // Prefer /dev/tty when stdout is piped (Helix `:insert-output`, etc.).
    let mut out =
        crate::os::tty::open_tui_output().context("Failed to open terminal output for TUI")?;
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCaptureWithoutHover,
        crossterm::event::EnableFocusChange,
        crossterm::event::EnableBracketedPaste,
        cursor::Hide
    )?;
    // Detect graphics before Crossterm creates its internal response reader.
    crate::pager::image_preview::initialize();
    // Helix leaves progressive kitty keyboard enhancement enabled across
    // `:insert-output` and keeps a `/dev/tty` EventStream open. Probing races
    // that reader (blank hang); instead pop leftover stacks and push our flags
    // so CSI-u keys parse as KeyEvents.
    let keyboard_enhanced = if crate::os::tty::nested_tty_launch() {
        for _ in 0..4 {
            let _ = execute!(out, crossterm::event::PopKeyboardEnhancementFlags);
        }
        true
    } else {
        crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
    };
    if keyboard_enhanced {
        execute!(
            out,
            crossterm::event::PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
        )?;
    }
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok((terminal, keyboard_enhanced))
}

/// Put the terminal back the way we found it.
///
/// Nothing drains leftover input here: crossterm guards its reader with a
/// process-wide mutex that the input thread holds for the duration of its
/// blocking read, so any drain from this thread would silently no-op.
fn restore_terminal(terminal: &mut Term, keyboard_enhanced: bool) -> Result<()> {
    crate::pager::image_preview::flush_cleanup(terminal.backend_mut());
    // Helix `:insert-output` keeps its own alt-screen / raw mode / mouse / focus
    // / bracketed-paste / kitty stack across the child. If we tear those down
    // here, Helix resumes drawing into a world that no longer exists and the
    // user is left staring at the primary-screen `hx` launch line with a dead
    // TUI. Nested exit must only undo *our* transient state and hand the tty
    // foreground back.
    let nested = crate::os::tty::nested_tty_launch();
    if nested {
        // Hide our cursor leftovers, pop the kitty flags we pushed, then put
        // Helix's progressive flags back (DISAMBIGUATE | REPORT_ALTERNATE_KEYS).
        execute!(
            terminal.backend_mut(),
            cursor::SetCursorStyle::DefaultUserShape,
            cursor::Show
        )?;
        if keyboard_enhanced {
            execute!(
                terminal.backend_mut(),
                crossterm::event::PopKeyboardEnhancementFlags
            )?;
            execute!(
                terminal.backend_mut(),
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | crossterm::event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )?;
        }
        terminal.backend_mut().flush()?;
        crate::os::tty::restore_foreground_tty();
        return Ok(());
    }

    // Standalone / non-nested: full teardown.
    crate::os::tty::restore_foreground_tty();
    if keyboard_enhanced {
        execute!(
            terminal.backend_mut(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableFocusChange,
            crossterm::event::PopKeyboardEnhancementFlags,
            crossterm::event::DisableBracketedPaste,
            cursor::SetCursorStyle::DefaultUserShape,
            cursor::Show,
            LeaveAlternateScreen
        )?;
    } else {
        execute!(
            terminal.backend_mut(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableFocusChange,
            crossterm::event::DisableBracketedPaste,
            cursor::SetCursorStyle::DefaultUserShape,
            cursor::Show,
            LeaveAlternateScreen
        )?;
    }
    terminal.backend_mut().flush()?;

    terminal::disable_raw_mode()?;

    Ok(())
}
