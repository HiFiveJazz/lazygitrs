use std::time::{Duration, Instant};

pub const LIST_SCROLL_DURATION: Duration = Duration::from_millis(100);

/// Adjust `scroll_offset` so that `selected` is visible within `visible_height`.
/// Only scrolls when the cursor would otherwise be outside the visible window.
pub fn ensure_visible(selected: usize, scroll_offset: &mut usize, visible_height: usize) {
    if visible_height == 0 {
        return;
    }

    if selected < *scroll_offset {
        *scroll_offset = selected;
    } else if selected >= *scroll_offset + visible_height {
        *scroll_offset = selected + 1 - visible_height;
    }
}

/// Return the viewport offset needed to make `selected` visible without
/// modifying the current offset.
pub fn visible_target(selected: usize, scroll_offset: usize, visible_height: usize) -> usize {
    let mut target = scroll_offset;
    ensure_visible(selected, &mut target, visible_height);
    target
}

/// Scroll viewport by `delta` rows, clamping to valid range.
/// Does NOT change selection.
pub fn scroll_viewport(
    scroll_offset: &mut usize,
    delta: isize,
    list_len: usize,
    visible_height: usize,
) {
    if list_len <= visible_height {
        *scroll_offset = 0;
        return;
    }

    let max_offset = list_len.saturating_sub(visible_height);

    if delta < 0 {
        *scroll_offset = scroll_offset.saturating_sub(delta.unsigned_abs());
    } else {
        *scroll_offset = (*scroll_offset + delta as usize).min(max_offset);
    }
}

#[derive(Debug, Clone)]
pub struct SmoothScroll {
    from: f64,
    target: f64,
    started_at: Instant,
    duration: Duration,
    active: bool,
}

impl SmoothScroll {
    pub fn new(offset: usize) -> Self {
        Self {
            from: offset as f64,
            target: offset as f64,
            started_at: Instant::now(),
            duration: LIST_SCROLL_DURATION,
            active: false,
        }
    }

    pub fn retarget(&mut self, current: usize, target: usize, duration: Duration) {
        // Don't restart the easing curve every rendered frame when we're
        // already travelling toward this target.
        if self.active && self.target as usize == target {
            return;
        }

        if current == target {
            self.from = target as f64;
            self.target = target as f64;
            self.active = false;
            return;
        }

        self.from = current as f64;
        self.target = target as f64;
        self.started_at = Instant::now();
        self.duration = duration;
        self.active = true;
    }

    pub fn tick(&mut self) -> usize {
        if !self.active {
            return self.target as usize;
        }

        if self.duration.is_zero() {
            self.active = false;
            return self.target as usize;
        }

        let elapsed = self.started_at.elapsed();
        let t = (elapsed.as_secs_f64() / self.duration.as_secs_f64()).min(1.0);

        if t >= 1.0 {
            self.active = false;
            return self.target as usize;
        }

        let value = self.from + (self.target - self.from) * t;

        value.round() as usize;

        if self.target >= self.from {
            value.floor() as usize
        } else {
            value.ceil() as usize
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn target(&self) -> usize {
        self.target as usize
    }
}
