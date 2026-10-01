//! Client-side budgets for Slack's per-workspace method limits, and event dedupe.
//!
//! Slack limits each method per workspace, not per message: `chat.appendStream`
//! (Tier 4, ~100/min) and `chat.startStream`/`chat.stopStream` (Tier 2,
//! ~20/min) are shared by every answer streaming at once. Updates that don't
//! fit are skipped (the text goes out with the next one), and a turn that can't
//! get a stream slot is delivered as one message at the end.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

/// A token bucket.
#[derive(Debug)]
struct Bucket {
    capacity: f64,
    per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn per_minute(n: u32) -> Self {
        Self {
            capacity: n as f64,
            per_sec: n as f64 / 60.0,
            tokens: n as f64,
            last: Instant::now(),
        }
    }

    fn try_take(&mut self, n: f64) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

/// Budgets for one workspace.
pub struct Limits {
    enabled: bool,
    stream: Mutex<Bucket>,
    append: Mutex<Bucket>,
}

impl Limits {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            // Tier 2 is "20+ per minute"; keep a little headroom.
            stream: Mutex::new(Bucket::per_minute(18)),
            append: Mutex::new(Bucket::per_minute(90)),
        }
    }

    /// Reserve a `chat.startStream` and its `chat.stopStream`.
    pub fn start_stream(&self) -> bool {
        !self.enabled || self.stream.lock().unwrap().try_take(2.0)
    }

    /// One `chat.appendStream`.
    pub fn append(&self) -> bool {
        !self.enabled || self.append.lock().unwrap().try_take(1.0)
    }
}

/// Remembers recent event ids so redeliveries (HTTP retries, Socket Mode
/// reconnects) are handled once.
pub struct Seen {
    cap: usize,
    inner: Mutex<(VecDeque<String>, HashSet<String>)>,
}

impl Seen {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new((VecDeque::new(), HashSet::new())),
        }
    }

    /// True the first time `id` is seen.
    pub fn first(&self, id: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let (order, set) = &mut *g;
        if !set.insert(id.to_string()) {
            return false;
        }
        order.push_back(id.to_string());
        if order.len() > self.cap
            && let Some(old) = order.pop_front()
        {
            set.remove(&old);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_and_dedupe() {
        let l = Limits::new(true);
        let starts = (0..20).filter(|_| l.start_stream()).count();
        assert_eq!(starts, 9);
        let appends = (0..200).filter(|_| l.append()).count();
        assert_eq!(appends, 90);
        assert!(Limits::new(false).start_stream());

        let s = Seen::new(2);
        assert!(s.first("a"));
        assert!(!s.first("a"));
        assert!(s.first("b") && s.first("c"));
        assert!(s.first("a"), "evicted after cap");
    }
}
