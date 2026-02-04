//! Vector clock implementation for causality tracking

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Vector clock for tracking causality
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorClock {
    /// Map of node_id → version
    pub clocks: HashMap<Uuid, u64>,
}

impl VectorClock {
    /// Create a new vector clock
    pub fn new() -> Self {
        Self {
            clocks: HashMap::new(),
        }
    }

    /// Increment this node's clock
    pub fn increment(&mut self, node_id: Uuid) {
        *self.clocks.entry(node_id).or_insert(0) += 1;
    }

    /// Get the current value for a node
    pub fn get(&self, node_id: &Uuid) -> u64 {
        *self.clocks.get(node_id).unwrap_or(&0)
    }

    /// Check if this clock happens-before other
    pub fn happens_before(&self, other: &VectorClock) -> bool {
        // Returns true if all our clocks <= other's clocks
        // and at least one is strictly less
        let all_less_or_equal = self.clocks.iter().all(|(id, v)| {
            other.clocks.get(id).map_or(*v == 0, |ov| v <= ov)
        });

        let at_least_one_less = self.clocks.iter().any(|(id, v)| {
            other.clocks.get(id).map_or(*v > 0, |ov| v < ov)
        }) || other.clocks.iter().any(|(id, ov)| {
            self.clocks.get(id).map_or(*ov > 0, |v| v < ov)
        });

        all_less_or_equal && at_least_one_less
    }

    /// Detect conflict (concurrent updates)
    pub fn conflicts_with(&self, other: &VectorClock) -> bool {
        !self.happens_before(other) && !other.happens_before(self)
    }

    /// Merge two vector clocks (take max for each node)
    pub fn merge(&mut self, other: &VectorClock) {
        for (id, v) in &other.clocks {
            let entry = self.clocks.entry(*id).or_insert(0);
            *entry = (*entry).max(*v);
        }
    }

    /// Check if this clock is strictly less than other
    pub fn less_than(&self, other: &VectorClock) -> bool {
        self.happens_before(other) && !other.happens_before(self)
    }

    /// Check if this clock is concurrent with other
    pub fn concurrent(&self, other: &VectorClock) -> bool {
        !self.happens_before(other) && !other.happens_before(self) && self != other
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_clock_increment() {
        let mut clock = VectorClock::new();
        let node_id = Uuid::new_v4();

        assert_eq!(clock.get(&node_id), 0);

        clock.increment(node_id);
        assert_eq!(clock.get(&node_id), 1);

        clock.increment(node_id);
        assert_eq!(clock.get(&node_id), 2);
    }

    #[test]
    fn test_happens_before() {
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut clock1 = VectorClock::new();
        clock1.increment(node1);
        clock1.increment(node1);

        let mut clock2 = VectorClock::new();
        clock2.increment(node1);
        clock2.increment(node1);
        clock2.increment(node1);
        clock2.increment(node2);

        assert!(clock1.happens_before(&clock2));
        assert!(!clock2.happens_before(&clock1));
    }

    #[test]
    fn test_concurrent() {
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut clock1 = VectorClock::new();
        clock1.increment(node1);
        clock1.increment(node1);

        let mut clock2 = VectorClock::new();
        clock2.increment(node2);
        clock2.increment(node2);

        assert!(clock1.concurrent(&clock2));
        assert!(clock2.concurrent(&clock1));
        assert!(clock1.conflicts_with(&clock2));
    }

    #[test]
    fn test_merge() {
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut clock1 = VectorClock::new();
        clock1.increment(node1);
        clock1.increment(node1);

        let mut clock2 = VectorClock::new();
        clock2.increment(node1);
        clock2.increment(node2);
        clock2.increment(node2);

        let mut merged = clock1.clone();
        merged.merge(&clock2);

        assert_eq!(merged.get(&node1), 2); // max(2, 1)
        assert_eq!(merged.get(&node2), 2); // max(0, 2)
    }
}
