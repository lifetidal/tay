//! Bounded in-flight input and protocol-response flow control.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Unified user-input budget across queued commands and pending PTY writes.
pub const MAX_INFLIGHT_INPUT_BYTES: usize = 512 * 1024;
/// Extra bounded allowance reserved for terminal protocol replies.
pub const MAX_RESPONSE_SLACK_BYTES: usize = 256 * 1024;

pub(super) const RESPONSE_HIGH_WATER: usize =
    MAX_INFLIGHT_INPUT_BYTES + MAX_RESPONSE_SLACK_BYTES / 2;
pub(super) const RESPONSE_LOW_WATER: usize = MAX_INFLIGHT_INPUT_BYTES / 2;
const RESPONSE_STALL_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn response_budget_ok(total_inflight: usize) -> bool {
    total_inflight <= MAX_INFLIGHT_INPUT_BYTES + MAX_RESPONSE_SLACK_BYTES
}

pub(super) fn reserve_input(inflight: &AtomicUsize, len: usize) -> bool {
    inflight
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            cur.checked_add(len)
                .filter(|&n| n <= MAX_INFLIGHT_INPUT_BYTES)
        })
        .is_ok()
}

pub(super) fn reserve_response(inflight: &AtomicUsize, len: usize) -> bool {
    inflight
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            cur.checked_add(len).filter(|&n| response_budget_ok(n))
        })
        .is_ok()
}

pub(super) fn release_input(inflight: &AtomicUsize, len: usize) {
    let _ = inflight.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        Some(cur.saturating_sub(len))
    });
}

#[derive(Default)]
pub(super) struct Backpressure {
    pub(super) paused: bool,
    last: usize,
    progress_at: Option<Instant>,
}

impl Backpressure {
    pub(super) fn update(&mut self, bytes: usize, now: Instant) -> bool {
        if !self.paused && bytes >= RESPONSE_HIGH_WATER {
            self.paused = true;
            self.progress_at = Some(now);
        }
        if self.paused {
            if bytes <= RESPONSE_LOW_WATER {
                self.paused = false;
                self.progress_at = None;
            } else if bytes < self.last {
                self.progress_at = Some(now);
            }
        }
        self.last = bytes;
        self.progress_at
            .is_some_and(|at| now.duration_since(at) >= RESPONSE_STALL_TIMEOUT)
    }
}
