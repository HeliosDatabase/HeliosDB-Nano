//! Set operations: UNION, INTERSECT, EXCEPT
//!
//! This module implements SQL set operations that combine results from two queries.

use std::collections::HashSet;
use std::sync::Arc;

use crate::{Result, Schema, Tuple};
use super::PhysicalOperator;

/// Hash a tuple's values for set operations
fn hash_tuple(tuple: &Tuple) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    for value in &tuple.values {
        // Use debug representation for hashing - works for all value types
        format!("{:?}", value).hash(&mut hasher);
    }
    hasher.finish()
}

/// Check if two tuples are equal (for set operations)
fn tuples_equal(a: &Tuple, b: &Tuple) -> bool {
    if a.values.len() != b.values.len() {
        return false;
    }
    for (va, vb) in a.values.iter().zip(b.values.iter()) {
        if format!("{:?}", va) != format!("{:?}", vb) {
            return false;
        }
    }
    true
}

/// UNION operator
///
/// Combines results from two queries. If `all` is false (UNION),
/// removes duplicates. If `all` is true (UNION ALL), keeps all rows.
pub struct UnionOperator {
    /// All results materialized
    results: Vec<Tuple>,
    /// Current position in results
    position: usize,
    /// Output schema
    schema: Arc<Schema>,
}

impl UnionOperator {
    pub fn new(
        mut left: Box<dyn PhysicalOperator>,
        mut right: Box<dyn PhysicalOperator>,
        all: bool,
    ) -> Result<Self> {
        let schema = left.schema();
        let mut results = Vec::new();

        // Collect all tuples from left
        while let Some(tuple) = left.next()? {
            results.push(tuple);
        }

        // Collect all tuples from right
        while let Some(tuple) = right.next()? {
            results.push(tuple);
        }

        // If not UNION ALL, remove duplicates
        if !all {
            let mut seen: HashSet<u64> = HashSet::new();
            let mut unique_results = Vec::new();

            for tuple in results {
                let hash = hash_tuple(&tuple);
                // Check for actual equality in case of hash collision
                let is_duplicate = seen.contains(&hash) &&
                    unique_results.iter().any(|t| tuples_equal(t, &tuple));

                if !is_duplicate {
                    seen.insert(hash);
                    unique_results.push(tuple);
                }
            }
            results = unique_results;
        }

        Ok(Self {
            results,
            position: 0,
            schema,
        })
    }
}

impl PhysicalOperator for UnionOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.position < self.results.len() {
            let tuple = self.results.get(self.position).cloned()
                .ok_or_else(|| crate::Error::query_execution("Union index out of bounds"))?;
            self.position += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// INTERSECT operator
///
/// Returns rows that appear in both queries. If `all` is false (INTERSECT),
/// removes duplicates. If `all` is true (INTERSECT ALL), keeps duplicates
/// up to the minimum count in both sides.
pub struct IntersectOperator {
    /// All results materialized
    results: Vec<Tuple>,
    /// Current position in results
    position: usize,
    /// Output schema
    schema: Arc<Schema>,
}

impl IntersectOperator {
    pub fn new(
        mut left: Box<dyn PhysicalOperator>,
        mut right: Box<dyn PhysicalOperator>,
        all: bool,
    ) -> Result<Self> {
        let schema = left.schema();

        // Collect all tuples from both sides
        let mut left_tuples = Vec::new();
        while let Some(tuple) = left.next()? {
            left_tuples.push(tuple);
        }

        let mut right_tuples = Vec::new();
        while let Some(tuple) = right.next()? {
            right_tuples.push(tuple);
        }

        let mut results = Vec::new();

        if all {
            // INTERSECT ALL: for each tuple, include min(left_count, right_count) copies
            // Build a map of tuple -> count for right side
            let mut right_counts: std::collections::HashMap<u64, Vec<Tuple>> = std::collections::HashMap::new();
            for tuple in right_tuples {
                let hash = hash_tuple(&tuple);
                right_counts.entry(hash).or_default().push(tuple);
            }

            for left_tuple in left_tuples {
                let hash = hash_tuple(&left_tuple);
                if let Some(right_list) = right_counts.get_mut(&hash) {
                    // Find a matching tuple and remove it
                    if let Some(pos) = right_list.iter().position(|t| tuples_equal(t, &left_tuple)) {
                        right_list.remove(pos);
                        results.push(left_tuple);
                    }
                }
            }
        } else {
            // INTERSECT: find tuples that exist in both, no duplicates
            let mut seen: HashSet<u64> = HashSet::new();

            // Build hash set of right side
            let right_hashes: HashSet<u64> = right_tuples.iter()
                .map(|t| hash_tuple(t))
                .collect();

            for left_tuple in left_tuples {
                let hash = hash_tuple(&left_tuple);
                if right_hashes.contains(&hash) && !seen.contains(&hash) {
                    // Verify there's an actual matching tuple (handle collisions)
                    if right_tuples.iter().any(|t| tuples_equal(t, &left_tuple)) {
                        seen.insert(hash);
                        results.push(left_tuple);
                    }
                }
            }
        }

        Ok(Self {
            results,
            position: 0,
            schema,
        })
    }
}

impl PhysicalOperator for IntersectOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.position < self.results.len() {
            let tuple = self.results.get(self.position).cloned()
                .ok_or_else(|| crate::Error::query_execution("Intersect index out of bounds"))?;
            self.position += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// EXCEPT operator
///
/// Returns rows from left that don't appear in right. If `all` is false (EXCEPT),
/// removes duplicates. If `all` is true (EXCEPT ALL), for each row in left,
/// removes one matching row from right.
pub struct ExceptOperator {
    /// All results materialized
    results: Vec<Tuple>,
    /// Current position in results
    position: usize,
    /// Output schema
    schema: Arc<Schema>,
}

impl ExceptOperator {
    pub fn new(
        mut left: Box<dyn PhysicalOperator>,
        mut right: Box<dyn PhysicalOperator>,
        all: bool,
    ) -> Result<Self> {
        let schema = left.schema();

        // Collect all tuples from both sides
        let mut left_tuples = Vec::new();
        while let Some(tuple) = left.next()? {
            left_tuples.push(tuple);
        }

        let mut right_tuples = Vec::new();
        while let Some(tuple) = right.next()? {
            right_tuples.push(tuple);
        }

        let mut results = Vec::new();

        if all {
            // EXCEPT ALL: for each left tuple, subtract one matching right tuple
            let mut right_remaining: std::collections::HashMap<u64, Vec<Tuple>> = std::collections::HashMap::new();
            for tuple in right_tuples {
                let hash = hash_tuple(&tuple);
                right_remaining.entry(hash).or_default().push(tuple);
            }

            for left_tuple in left_tuples {
                let hash = hash_tuple(&left_tuple);
                let mut found = false;
                if let Some(right_list) = right_remaining.get_mut(&hash) {
                    if let Some(pos) = right_list.iter().position(|t| tuples_equal(t, &left_tuple)) {
                        right_list.remove(pos);
                        found = true;
                    }
                }
                if !found {
                    results.push(left_tuple);
                }
            }
        } else {
            // EXCEPT: find unique tuples in left that don't exist in right
            let mut seen: HashSet<u64> = HashSet::new();

            // Build hash set of right side
            let right_hashes: HashSet<u64> = right_tuples.iter()
                .map(|t| hash_tuple(t))
                .collect();

            for left_tuple in left_tuples {
                let hash = hash_tuple(&left_tuple);
                if seen.insert(hash) {
                    // Check if tuple exists in right
                    let in_right = right_hashes.contains(&hash) &&
                        right_tuples.iter().any(|t| tuples_equal(t, &left_tuple));

                    if !in_right {
                        results.push(left_tuple);
                    }
                }
            }
        }

        Ok(Self {
            results,
            position: 0,
            schema,
        })
    }
}

impl PhysicalOperator for ExceptOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.position < self.results.len() {
            let tuple = self.results.get(self.position).cloned()
                .ok_or_else(|| crate::Error::query_execution("Except index out of bounds"))?;
            self.position += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}
