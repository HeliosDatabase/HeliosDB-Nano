//! Real-Time EXPLAIN During Execution
//!
//! Provides live query progress tracking with actual vs estimated statistics.
//! Features:
//! - Live execution tracking
//! - Actual vs estimated row counts
//! - Execution time per node
//! - Memory usage per node
//! - I/O statistics per node
//! - Real-time bottleneck detection

use crate::Result;
use super::explain::PlanNode;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::collections::HashMap;

/// Real-time execution statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStats {
    /// Node identifier
    pub node_id: String,

    /// Execution state
    pub state: ExecutionState,

    /// Estimated row count
    pub estimated_rows: usize,

    /// Actual row count (updated during execution)
    pub actual_rows: usize,

    /// Estimated cost
    pub estimated_cost: f64,

    /// Actual execution time
    pub actual_time_ms: f64,

    /// Memory used in bytes
    pub memory_bytes: usize,

    /// I/O operations (reads/writes)
    pub io_reads: usize,
    pub io_writes: usize,

    /// Cache statistics
    pub cache_hits: usize,
    pub cache_misses: usize,

    /// Lock wait time
    pub lock_wait_ms: f64,

    /// Network latency (for distributed queries)
    pub network_latency_ms: f64,

    /// Child node stats
    pub children: Vec<ExecutionStats>,

    /// Progress percentage (0-100)
    pub progress_percent: f64,

    /// Bottleneck score (0-100, higher = bigger bottleneck)
    pub bottleneck_score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ExecutionStats {
    pub fn new(node_id: String, plan_node: &PlanNode) -> Self {
        Self {
            node_id,
            state: ExecutionState::Pending,
            estimated_rows: plan_node.rows,
            actual_rows: 0,
            estimated_cost: plan_node.cost,
            actual_time_ms: 0.0,
            memory_bytes: 0,
            io_reads: 0,
            io_writes: 0,
            cache_hits: 0,
            cache_misses: 0,
            lock_wait_ms: 0.0,
            network_latency_ms: 0.0,
            children: vec![],
            progress_percent: 0.0,
            bottleneck_score: 0.0,
        }
    }

    /// Calculate accuracy of row estimate
    pub fn row_estimate_accuracy(&self) -> f64 {
        if self.estimated_rows == 0 {
            return 100.0;
        }

        let error = (self.actual_rows as f64 - self.estimated_rows as f64).abs();
        let accuracy = 100.0 - (error / self.estimated_rows as f64 * 100.0);
        accuracy.max(0.0).min(100.0)
    }

    /// Calculate cache hit rate
    pub fn cache_hit_rate(&self) -> f64 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            return 0.0;
        }
        (self.cache_hits as f64 / total as f64) * 100.0
    }

    /// Detect if this node is a bottleneck
    pub fn is_bottleneck(&self) -> bool {
        self.bottleneck_score > 70.0
    }
}

/// Real-time execution tracker
pub struct RealtimeExplainer {
    stats: Arc<Mutex<ExecutionStats>>,
    start_time: Instant,
}

impl RealtimeExplainer {
    /// Create new real-time explainer
    pub fn new(node_id: String, plan_node: &PlanNode) -> Self {
        Self {
            stats: Arc::new(Mutex::new(ExecutionStats::new(node_id, plan_node))),
            start_time: Instant::now(),
        }
    }

    /// Mark execution as started
    pub fn start(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.state = ExecutionState::Running;
        }
    }

    /// Update progress
    pub fn update_progress(&self, rows_processed: usize, total_rows: usize) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.actual_rows = rows_processed;
            stats.progress_percent = if total_rows > 0 {
                (rows_processed as f64 / total_rows as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
        }
    }

    /// Update I/O statistics
    pub fn update_io(&self, reads: usize, writes: usize) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.io_reads += reads;
            stats.io_writes += writes;
        }
    }

    /// Update cache statistics
    pub fn update_cache(&self, hits: usize, misses: usize) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.cache_hits += hits;
            stats.cache_misses += misses;
        }
    }

    /// Update memory usage
    pub fn update_memory(&self, bytes: usize) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.memory_bytes = stats.memory_bytes.max(bytes);
        }
    }

    /// Record lock wait time
    pub fn add_lock_wait(&self, duration: Duration) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.lock_wait_ms += duration.as_secs_f64() * 1000.0;
        }
    }

    /// Record network latency
    pub fn add_network_latency(&self, duration: Duration) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.network_latency_ms += duration.as_secs_f64() * 1000.0;
        }
    }

    /// Mark execution as completed
    pub fn complete(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.state = ExecutionState::Completed;
            stats.actual_time_ms = self.start_time.elapsed().as_secs_f64() * 1000.0;
            stats.progress_percent = 100.0;

            // Calculate bottleneck score
            stats.bottleneck_score = self.calculate_bottleneck_score(&stats);
        }
    }

    /// Mark execution as failed
    pub fn fail(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.state = ExecutionState::Failed;
            stats.actual_time_ms = self.start_time.elapsed().as_secs_f64() * 1000.0;
        }
    }

    /// Get current statistics
    ///
    /// Returns the current execution statistics. If the mutex is poisoned
    /// (e.g., a thread panicked while holding the lock), returns default stats
    /// with a Failed state to indicate the error condition.
    pub fn get_stats(&self) -> ExecutionStats {
        match self.stats.lock() {
            Ok(stats) => stats.clone(),
            Err(poisoned) => {
                // Mutex was poisoned - recover the data but mark as failed
                let mut stats = poisoned.into_inner().clone();
                stats.state = ExecutionState::Failed;
                stats
            }
        }
    }

    /// Calculate bottleneck score (0-100)
    fn calculate_bottleneck_score(&self, stats: &ExecutionStats) -> f64 {
        let mut score = 0.0;

        // Factor 1: Time overhead (40% weight)
        let time_ratio = stats.actual_time_ms / stats.estimated_cost.max(1.0);
        if time_ratio > 2.0 {
            score += 40.0 * ((time_ratio - 1.0) / time_ratio);
        }

        // Factor 2: Cache miss rate (30% weight)
        let cache_miss_rate = 100.0 - stats.cache_hit_rate();
        if cache_miss_rate > 50.0 {
            score += 30.0 * (cache_miss_rate / 100.0);
        }

        // Factor 3: Lock wait time (20% weight)
        if stats.lock_wait_ms > 10.0 {
            let lock_wait_ratio = stats.lock_wait_ms / stats.actual_time_ms.max(1.0);
            score += 20.0 * lock_wait_ratio;
        }

        // Factor 4: I/O intensity (10% weight)
        let total_io = stats.io_reads + stats.io_writes;
        if total_io > 1000 {
            score += 10.0 * ((total_io as f64).log10() / 6.0).min(1.0);
        }

        score.min(100.0)
    }

    /// Generate live EXPLAIN output
    pub fn format_live_explain(&self) -> String {
        let stats = self.get_stats();

        let mut output = String::new();
        output.push_str("═══════════════════════════════════════════════════════════════\n");
        output.push_str("                 REAL-TIME EXECUTION ANALYSIS                  \n");
        output.push_str("═══════════════════════════════════════════════════════════════\n\n");

        output.push_str(&format!("Node: {}\n", stats.node_id));
        output.push_str(&format!("State: {:?}\n", stats.state));
        output.push_str(&format!("Progress: {:.1}%\n\n", stats.progress_percent));

        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  ROW COUNT ANALYSIS\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");
        output.push_str(&format!("Estimated Rows: {}\n", stats.estimated_rows));
        output.push_str(&format!("Actual Rows:    {}\n", stats.actual_rows));
        output.push_str(&format!("Accuracy:       {:.1}%\n\n", stats.row_estimate_accuracy()));

        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  TIMING ANALYSIS\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");
        output.push_str(&format!("Estimated Cost: {:.2}\n", stats.estimated_cost));
        output.push_str(&format!("Actual Time:    {:.2}ms\n", stats.actual_time_ms));
        output.push_str(&format!("Lock Wait:      {:.2}ms\n", stats.lock_wait_ms));
        output.push_str(&format!("Network:        {:.2}ms\n\n", stats.network_latency_ms));

        output.push_str("───────────────────────────────────────────────────────────────\n");
        output.push_str("  RESOURCE USAGE\n");
        output.push_str("───────────────────────────────────────────────────────────────\n\n");
        output.push_str(&format!("Memory:         {} bytes\n", stats.memory_bytes));
        output.push_str(&format!("I/O Reads:      {}\n", stats.io_reads));
        output.push_str(&format!("I/O Writes:     {}\n", stats.io_writes));
        output.push_str(&format!("Cache Hit Rate: {:.1}%\n\n", stats.cache_hit_rate()));

        if stats.is_bottleneck() {
            output.push_str("───────────────────────────────────────────────────────────────\n");
            output.push_str("  BOTTLENECK DETECTED\n");
            output.push_str("───────────────────────────────────────────────────────────────\n\n");
            output.push_str(&format!("Bottleneck Score: {:.1}/100\n", stats.bottleneck_score));

            if stats.cache_hit_rate() < 50.0 {
                output.push_str("  - Low cache hit rate detected\n");
                output.push_str("    Suggestion: Increase buffer pool size\n");
            }

            if stats.lock_wait_ms / stats.actual_time_ms.max(1.0) > 0.1 {
                output.push_str("  - High lock wait time detected\n");
                output.push_str("    Suggestion: Review locking strategy or enable MVCC\n");
            }

            if stats.io_reads + stats.io_writes > 10000 {
                output.push_str("  - High I/O activity detected\n");
                output.push_str("    Suggestion: Add indexes or optimize query\n");
            }

            output.push_str("\n");
        }

        output.push_str("═══════════════════════════════════════════════════════════════\n");

        output
    }
}

/// Comparison between estimated and actual execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionComparison {
    pub node_id: String,
    pub estimated_cost: f64,
    pub actual_time_ms: f64,
    pub cost_accuracy_percent: f64,
    pub estimated_rows: usize,
    pub actual_rows: usize,
    pub row_accuracy_percent: f64,
    pub variance_explanation: String,
}

impl ExecutionComparison {
    pub fn from_stats(stats: &ExecutionStats) -> Self {
        let cost_accuracy = if stats.estimated_cost > 0.0 {
            100.0 - ((stats.actual_time_ms - stats.estimated_cost).abs() / stats.estimated_cost * 100.0)
        } else {
            100.0
        }.max(0.0).min(100.0);

        let variance_explanation = if stats.actual_rows > stats.estimated_rows * 2 {
            "Actual rows significantly higher than estimated. Statistics may be stale.".to_string()
        } else if stats.actual_rows < stats.estimated_rows / 2 {
            "Actual rows significantly lower than estimated. Better selectivity than expected.".to_string()
        } else {
            "Row estimates are accurate.".to_string()
        };

        Self {
            node_id: stats.node_id.clone(),
            estimated_cost: stats.estimated_cost,
            actual_time_ms: stats.actual_time_ms,
            cost_accuracy_percent: cost_accuracy,
            estimated_rows: stats.estimated_rows,
            actual_rows: stats.actual_rows,
            row_accuracy_percent: stats.row_estimate_accuracy(),
            variance_explanation,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sql::explain::PlanNode;
    use std::collections::HashMap;
    use std::thread;

    fn create_test_node() -> PlanNode {
        PlanNode {
            node_type: "Scan".to_string(),
            operation: "Seq Scan on users".to_string(),
            cost: 100.0,
            rows: 1000,
            details: HashMap::new(),
            children: vec![],
        }
    }

    #[test]
    fn test_realtime_explainer_creation() {
        let node = create_test_node();
        let explainer = RealtimeExplainer::new("test_node".to_string(), &node);
        let stats = explainer.get_stats();

        assert_eq!(stats.node_id, "test_node");
        assert_eq!(stats.state, ExecutionState::Pending);
        assert_eq!(stats.estimated_rows, 1000);
    }

    #[test]
    fn test_execution_lifecycle() {
        let node = create_test_node();
        let explainer = RealtimeExplainer::new("test_node".to_string(), &node);

        explainer.start();
        let stats = explainer.get_stats();
        assert_eq!(stats.state, ExecutionState::Running);

        explainer.update_progress(500, 1000);
        let stats = explainer.get_stats();
        assert_eq!(stats.actual_rows, 500);
        assert_eq!(stats.progress_percent, 50.0);

        explainer.complete();
        let stats = explainer.get_stats();
        assert_eq!(stats.state, ExecutionState::Completed);
        assert_eq!(stats.progress_percent, 100.0);
    }

    #[test]
    fn test_row_estimate_accuracy() {
        let mut stats = ExecutionStats::new("test".to_string(), &create_test_node());
        stats.estimated_rows = 1000;
        stats.actual_rows = 950;

        let accuracy = stats.row_estimate_accuracy();
        assert!(accuracy > 90.0);
    }

    #[test]
    fn test_cache_hit_rate() {
        let mut stats = ExecutionStats::new("test".to_string(), &create_test_node());
        stats.cache_hits = 800;
        stats.cache_misses = 200;

        let hit_rate = stats.cache_hit_rate();
        assert_eq!(hit_rate, 80.0);
    }

    #[test]
    fn test_io_tracking() {
        let node = create_test_node();
        let explainer = RealtimeExplainer::new("test_node".to_string(), &node);

        explainer.update_io(100, 50);
        explainer.update_io(50, 25);

        let stats = explainer.get_stats();
        assert_eq!(stats.io_reads, 150);
        assert_eq!(stats.io_writes, 75);
    }

    #[test]
    fn test_memory_tracking() {
        let node = create_test_node();
        let explainer = RealtimeExplainer::new("test_node".to_string(), &node);

        explainer.update_memory(1024);
        explainer.update_memory(2048);

        let stats = explainer.get_stats();
        assert_eq!(stats.memory_bytes, 2048); // Should keep max
    }

    #[test]
    fn test_bottleneck_detection() {
        let mut stats = ExecutionStats::new("test".to_string(), &create_test_node());
        stats.actual_time_ms = 500.0;
        stats.estimated_cost = 100.0;
        stats.cache_hits = 100;
        stats.cache_misses = 900;
        stats.lock_wait_ms = 100.0;
        stats.io_reads = 50000;

        // Manually set high bottleneck score
        stats.bottleneck_score = 80.0;

        assert!(stats.is_bottleneck());
    }

    #[test]
    fn test_live_explain_format() {
        let node = create_test_node();
        let explainer = RealtimeExplainer::new("test_scan".to_string(), &node);

        explainer.start();
        explainer.update_progress(500, 1000);
        explainer.update_io(1000, 500);
        explainer.update_cache(700, 300);

        let output = explainer.format_live_explain();

        assert!(output.contains("REAL-TIME EXECUTION ANALYSIS"));
        assert!(output.contains("test_scan"));
        assert!(output.contains("Progress:"));
    }
}
