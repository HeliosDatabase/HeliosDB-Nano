//! Lightweight query tracing for embedded benchmarking.
//!
//! This intentionally records coarse query lifecycle timing from Nano's public
//! query/execute entry points. It is small enough to keep always compiled and
//! disabled by default.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct QueryTrace {
    sql: String,
    total: Duration,
    rows: u64,
}

impl QueryTrace {
    pub fn new(sql: &str, total: Duration, rows: u64) -> Self {
        Self {
            sql: sql.trim().to_string(),
            total,
            rows,
        }
    }
}

pub struct QueryProfiler {
    enabled: AtomicBool,
    traces: Mutex<VecDeque<QueryTrace>>,
    max_traces: usize,
}

impl QueryProfiler {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            traces: Mutex::new(VecDeque::with_capacity(1024)),
            max_traces: 1024,
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.traces.lock().clear();
    }

    pub fn record(&self, trace: QueryTrace) {
        if !self.enabled() {
            return;
        }
        let mut traces = self.traces.lock();
        if traces.len() >= self.max_traces {
            traces.pop_front();
        }
        traces.push_back(trace);
    }

    pub fn report(&self) -> String {
        let traces = self.traces.lock();
        if traces.is_empty() {
            return "No query traces recorded. Enable with: SET helios.trace_queries = on".to_string();
        }

        let mut total_us: Vec<u64> = traces.iter().map(|t| t.total.as_micros() as u64).collect();
        total_us.sort_unstable();

        let mut top: Vec<_> = traces.iter().collect();
        top.sort_unstable_by_key(|t| std::cmp::Reverse(t.total));

        let mut out = String::new();
        out.push_str("=== Nano Query Trace Report ===\n");
        out.push_str(&format!("Traced queries: {}\n", traces.len()));
        out.push_str(&format!(
            "Total latency avg / p50 / p95 / p99: {} / {} / {} / {}\n",
            format_us(avg(&total_us)),
            format_us(percentile(&total_us, 50.0)),
            format_us(percentile(&total_us, 95.0)),
            format_us(percentile(&total_us, 99.0)),
        ));
        out.push_str("\nTop slowest queries:\n");
        for (idx, trace) in top.into_iter().take(5).enumerate() {
            out.push_str(&format!(
                "  {}. {} ({} rows) {}\n",
                idx + 1,
                format_duration(trace.total),
                trace.rows,
                truncate_sql(&trace.sql, 96),
            ));
        }
        out
    }
}

impl Default for QueryProfiler {
    fn default() -> Self {
        Self::new()
    }
}

fn avg(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.iter().sum::<u64>() / values.len() as u64
}

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * pct / 100.0).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn format_duration(duration: Duration) -> String {
    format_us(duration.as_micros() as u64)
}

fn format_us(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.2}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}us")
    }
}

fn truncate_sql(sql: &str, max_len: usize) -> String {
    if sql.len() <= max_len {
        return sql.to_string();
    }
    let end = sql.char_indices().nth(max_len).map_or(sql.len(), |(idx, _)| idx);
    format!("{}...", &sql[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_tracks_slowest_queries() {
        let profiler = QueryProfiler::new();
        profiler.set_enabled(true);
        profiler.record(QueryTrace::new("SELECT 1", Duration::from_micros(10), 1));
        profiler.record(QueryTrace::new("SELECT * FROM t", Duration::from_micros(2000), 4));

        let report = profiler.report();
        assert!(report.contains("Nano Query Trace Report"));
        assert!(report.contains("Traced queries: 2"));
        assert!(report.contains("SELECT * FROM t"));
    }
}
