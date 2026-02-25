/// A fixed-capacity ring buffer for recent daemon log lines.
/// Tracks a monotonically increasing total so callers can poll for new lines.
use std::collections::VecDeque;
use std::sync::Mutex;

struct Inner {
    lines: VecDeque<String>,
    /// Total number of lines ever pushed (never decreases).
    total: usize,
}

pub struct LogBuffer {
    capacity: usize,
    inner: Mutex<Inner>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner {
                lines: VecDeque::with_capacity(capacity),
                total: 0,
            }),
        }
    }

    pub fn push(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.lines.len() == self.capacity {
            inner.lines.pop_front();
        }
        inner.lines.push_back(line);
        inner.total += 1;
    }

    /// Return the last `n` lines and the current total count.
    /// Pass the returned total as `offset` to `since()` on the next poll.
    pub fn tail(&self, n: usize) -> (Vec<String>, usize) {
        let inner = self.inner.lock().unwrap();
        let start = inner.lines.len().saturating_sub(n);
        let lines = inner.lines.iter().skip(start).cloned().collect();
        (lines, inner.total)
    }

    /// Return all lines pushed after `offset` and the new total.
    /// If `offset` refers to lines that have already been evicted from the
    /// ring buffer, the oldest available lines are returned instead.
    pub fn since(&self, offset: usize) -> (Vec<String>, usize) {
        let inner = self.inner.lock().unwrap();
        let total = inner.total;
        if offset >= total {
            return (vec![], total);
        }
        // How many lines back from `total` does `offset` sit?
        let lines_back = total - offset;
        // We can only serve as far back as the ring buffer holds.
        let available = inner.lines.len();
        let skip = available.saturating_sub(lines_back);
        let lines = inner.lines.iter().skip(skip).cloned().collect();
        (lines, total)
    }
}
