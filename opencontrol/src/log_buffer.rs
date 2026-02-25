use std::collections::VecDeque;
use std::sync::Mutex;

struct Inner {
    lines: VecDeque<String>,
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

    pub fn tail(&self, n: usize) -> (Vec<String>, usize) {
        let inner = self.inner.lock().unwrap();
        let start = inner.lines.len().saturating_sub(n);
        let lines = inner.lines.iter().skip(start).cloned().collect();
        (lines, inner.total)
    }

    pub fn since(&self, offset: usize) -> (Vec<String>, usize) {
        let inner = self.inner.lock().unwrap();
        let total = inner.total;
        if offset >= total {
            return (vec![], total);
        }
        let lines_back = total - offset;
        let available = inner.lines.len();
        let skip = available.saturating_sub(lines_back);
        let lines = inner.lines.iter().skip(skip).cloned().collect();
        (lines, total)
    }
}
