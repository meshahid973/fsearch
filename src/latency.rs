//! Small fixed-size latency history for the terminal sparkline.
//!
//! Only completed searches enter the history. Samples are kept in memory
//! and the oldest are discarded; the chart never creates background work.

use std::collections::VecDeque;
use std::time::Duration;

const CAPACITY: usize = 64;
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Default)]
pub struct LatencyHistory {
    samples: VecDeque<Duration>,
}

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub count: usize,
    pub average_ms: f64,
    pub p95_ms: f64,
    pub peak_ms: f64,
}

impl LatencyHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, value: Duration) {
        if self.samples.len() == CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn stats(&self) -> Option<Stats> {
        if self.samples.is_empty() {
            return None;
        }
        let mut millis: Vec<_> = self.samples.iter().map(|v| v.as_secs_f64() * 1000.0).collect();
        let average_ms = millis.iter().sum::<f64>() / millis.len() as f64;
        millis.sort_by(f64::total_cmp);
        let p95_index = ((millis.len() * 95).div_ceil(100)).saturating_sub(1);
        Some(Stats {
            count: millis.len(),
            average_ms,
            p95_ms: millis[p95_index],
            peak_ms: *millis.last().unwrap_or(&0.0),
        })
    }

    /// Samples run oldest to newest, left to right. Scale to the recent
    /// maximum so spikes are visible; a flat history stays visually flat.
    pub fn sparkline(&self, width: usize) -> String {
        if width == 0 || self.samples.is_empty() {
            return String::new();
        }
        let available = width.min(CAPACITY);
        let offset = self.samples.len().saturating_sub(available);
        let samples: Vec<_> = self.samples.iter().skip(offset)
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        let peak = samples.iter().copied().fold(0.0_f64, f64::max);
        let mut text = String::with_capacity(samples.len() * 3);
        for value in samples {
            let level = if peak <= 0.0 {
                0
            } else {
                ((value / peak) * (BARS.len() - 1) as f64).round() as usize
            };
            text.push(BARS[level.min(BARS.len() - 1)]);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_bounded() {
        let mut history = LatencyHistory::new();
        for i in 0..100 {
            history.push(Duration::from_millis(i));
        }
        assert_eq!(history.stats().unwrap().count, 64);
        assert_eq!(history.sparkline(12).chars().count(), 12);
    }

    #[test]
    fn graph_tracks_spikes_over_time() {
        let mut history = LatencyHistory::new();
        assert!(history.stats().is_none());
        assert_eq!(history.sparkline(20), "");
        history.push(Duration::from_millis(1));
        history.push(Duration::from_millis(2));
        history.push(Duration::from_millis(10));
        let graph = history.sparkline(20);
        assert_eq!(graph.chars().count(), 3);
        assert!(graph.ends_with('█'));
        let stats = history.stats().unwrap();
        assert_eq!(stats.peak_ms, 10.0);
        assert_eq!(stats.p95_ms, 10.0);
    }

    #[test]
    fn clearing_resets_samples() {
        let mut history = LatencyHistory::new();
        history.push(Duration::from_micros(100));
        history.clear();
        assert!(history.stats().is_none());
        assert!(history.sparkline(0).is_empty());
    }
}
