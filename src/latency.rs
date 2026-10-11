//! Bounded latency samples and a cached terminal sparkline.

use std::time::Duration;

const CAPACITY: usize = 48;
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub struct LatencyHistory {
    samples: [Duration; CAPACITY],
    next: usize,
    len: usize,
    dirty: bool,
    graph_width: usize,
    cached_graph: String,
    cached_median: Option<f64>,
}

impl Default for LatencyHistory {
    fn default() -> Self {
        Self {
            samples: [Duration::ZERO; CAPACITY],
            next: 0,
            len: 0,
            dirty: false,
            graph_width: 0,
            cached_graph: String::with_capacity(CAPACITY * 3),
            cached_median: None,
        }
    }
}

impl LatencyHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Only completed, current-generation searches are recorded by the TUI.
    pub fn push(&mut self, elapsed: Duration) {
        self.samples[self.next] = elapsed;
        self.next = (self.next + 1) % CAPACITY;
        self.len = (self.len + 1).min(CAPACITY);
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.next = 0;
        self.len = 0;
        self.cached_graph.clear();
        self.cached_median = None;
        self.dirty = false;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn median_ms(&mut self) -> Option<f64> {
        self.update_cache(self.graph_width);
        self.cached_median
    }

    pub fn sparkline(&mut self, width: usize) -> &str {
        self.update_cache(width.min(CAPACITY));
        &self.cached_graph
    }

    fn update_cache(&mut self, width: usize) {
        if !self.dirty && self.graph_width == width {
            return;
        }

        self.graph_width = width;
        self.cached_graph.clear();
        if self.len == 0 {
            self.cached_median = None;
            self.dirty = false;
            return;
        }

        // Median on a fixed stack array: no per-keystroke heap allocation.
        let mut values = [0.0_f64; CAPACITY];
        for (i, value) in values.iter_mut().take(self.len).enumerate() {
            *value = self.sample(i);
        }
        values[..self.len].sort_unstable_by(f64::total_cmp);
        let middle = self.len / 2;
        self.cached_median = Some(if self.len % 2 == 0 {
            (values[middle - 1] + values[middle]) / 2.0
        } else {
            values[middle]
        });

        let shown = width.min(self.len);
        let start = self.len - shown;
        let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
        for i in start..self.len {
            let value = self.sample(i);
            min = min.min(value);
            max = max.max(value);
        }

        for i in start..self.len {
            let value = self.sample(i);
            let level = if max - min < f64::EPSILON {
                0
            } else {
                (((value - min) / (max - min)) * (BARS.len() - 1) as f64)
                    .round() as usize
            };
            self.cached_graph.push(BARS[level.min(BARS.len() - 1)]);
        }
        self.dirty = false;
    }

    /// Chronological order: index zero is the oldest retained measurement.
    fn sample(&self, index: usize) -> f64 {
        let oldest = (self.next + CAPACITY - self.len) % CAPACITY;
        self.samples[(oldest + index) % CAPACITY].as_secs_f64() * 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_uses_middle_samples() {
        let mut history = LatencyHistory::new();
        for ms in [4, 1, 10, 2] {
            history.push(Duration::from_millis(ms));
        }
        assert_eq!(history.median_ms(), Some(3.0));
        assert_eq!(history.sparkline(48).chars().count(), 4);
        history.push(Duration::from_millis(3));
        assert_eq!(history.median_ms(), Some(3.0));
    }

    #[test]
    fn retains_only_recent_samples() {
        let mut history = LatencyHistory::new();
        for ms in 0..100 {
            history.push(Duration::from_millis(ms));
        }
        assert_eq!(history.sparkline(48).chars().count(), 48);
        assert_eq!(history.median_ms(), Some(75.5));
        assert_eq!(history.sparkline(8).chars().count(), 8);
        assert!(history.sparkline(8).ends_with('█'));
    }

    #[test]
    fn caches_and_resets_graph() {
        let mut history = LatencyHistory::new();
        assert!(history.is_empty());
        assert_eq!(history.median_ms(), None);
        history.push(Duration::from_micros(400));
        history.push(Duration::from_micros(900));
        let first = history.sparkline(8).to_owned();
        assert_eq!(history.sparkline(8), first);
        assert_eq!(history.median_ms(), Some(0.65));
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.median_ms(), None);
        assert!(history.sparkline(8).is_empty());
    }

    #[test]
    fn graph_handles_zero_width_and_constant_latency() {
        let mut history = LatencyHistory::new();
        history.push(Duration::from_millis(1));
        history.push(Duration::from_millis(1));
        assert_eq!(history.sparkline(0), "");
        assert_eq!(history.sparkline(2), "▁▁");
    }
}
