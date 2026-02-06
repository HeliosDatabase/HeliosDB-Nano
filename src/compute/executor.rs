//! Query executor trait and implementations
//!
//! Standard Volcano model (iterator-based execution).

use crate::{Result, Tuple, Value, Error};
use super::aggregation::{AggregateState, CountState, create_aggregate};
use std::collections::HashMap;
use std::cmp::Ordering;

/// Executor trait (Volcano model)
///
/// Standard iterator-based execution model from database textbooks.
pub trait Executor: Send {
    /// Open the executor (initialize state)
    fn open(&mut self) -> Result<()>;

    /// Get next tuple
    fn next(&mut self) -> Result<Option<Tuple>>;

    /// Close the executor (cleanup)
    fn close(&mut self) -> Result<()>;
}

// =============================================================================
// ScanExecutor: Table scan
// =============================================================================

/// Table scan executor
///
/// Iterates over all tuples in a table.
pub struct ScanExecutor {
    /// Tuples to scan (materialized from storage)
    tuples: Vec<Tuple>,
    /// Current position in the scan
    position: usize,
    /// Whether the executor is open
    is_open: bool,
}

impl ScanExecutor {
    /// Create a new scan executor with pre-loaded tuples
    pub fn new(tuples: Vec<Tuple>) -> Self {
        Self {
            tuples,
            position: 0,
            is_open: false,
        }
    }
}

// SAFETY: self.position is bounds-checked (< self.tuples.len()) before indexing.
#[allow(clippy::indexing_slicing)]
impl Executor for ScanExecutor {
    fn open(&mut self) -> Result<()> {
        self.position = 0;
        self.is_open = true;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        if !self.is_open {
            return Err(Error::Generic("Executor not open".to_string()));
        }

        if self.position < self.tuples.len() {
            let tuple = self.tuples[self.position].clone();
            self.position += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn close(&mut self) -> Result<()> {
        self.is_open = false;
        Ok(())
    }
}

// =============================================================================
// FilterExecutor: WHERE clause
// =============================================================================

/// Predicate function type
pub type PredicateFn = Box<dyn Fn(&Tuple) -> bool + Send>;

/// Filter executor for WHERE clause
///
/// Filters tuples based on a predicate.
pub struct FilterExecutor {
    /// Child executor providing input tuples
    child: Box<dyn Executor>,
    /// Predicate function to evaluate
    predicate: PredicateFn,
}

impl FilterExecutor {
    /// Create a new filter executor
    pub fn new(child: Box<dyn Executor>, predicate: PredicateFn) -> Self {
        Self { child, predicate }
    }
}

impl Executor for FilterExecutor {
    fn open(&mut self) -> Result<()> {
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        // Keep pulling from child until we find a matching tuple or exhaust input
        loop {
            match self.child.next()? {
                Some(tuple) => {
                    if (self.predicate)(&tuple) {
                        return Ok(Some(tuple));
                    }
                    // Otherwise continue to next tuple
                }
                None => return Ok(None),
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

// =============================================================================
// ProjectExecutor: SELECT columns
// =============================================================================

/// Projection function type
pub type ProjectFn = Box<dyn Fn(&Tuple) -> Tuple + Send>;

/// Project executor for SELECT columns
///
/// Projects specific columns from input tuples.
pub struct ProjectExecutor {
    /// Child executor providing input tuples
    child: Box<dyn Executor>,
    /// Projection function
    project: ProjectFn,
}

impl ProjectExecutor {
    /// Create a new project executor
    pub fn new(child: Box<dyn Executor>, project: ProjectFn) -> Self {
        Self { child, project }
    }

    /// Create a project executor that selects specific column indices
    pub fn by_indices(child: Box<dyn Executor>, indices: Vec<usize>) -> Self {
        let project: ProjectFn = Box::new(move |tuple| {
            let values: Vec<Value> = indices
                .iter()
                .map(|&i| tuple.values.get(i).cloned().unwrap_or(Value::Null))
                .collect();
            Tuple::new(values)
        });
        Self { child, project }
    }
}

impl Executor for ProjectExecutor {
    fn open(&mut self) -> Result<()> {
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        match self.child.next()? {
            Some(tuple) => Ok(Some((self.project)(&tuple))),
            None => Ok(None),
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

// =============================================================================
// NestedLoopJoinExecutor: JOIN (nested loop)
// =============================================================================

/// Join condition function type
pub type JoinConditionFn = Box<dyn Fn(&Tuple, &Tuple) -> bool + Send>;

/// Nested loop join executor
///
/// Simple O(n*m) join implementation.
pub struct NestedLoopJoinExecutor {
    /// Left (outer) child executor
    left: Box<dyn Executor>,
    /// Right (inner) child executor
    right: Box<dyn Executor>,
    /// Join condition
    condition: JoinConditionFn,
    /// Current left tuple
    current_left: Option<Tuple>,
    /// Materialized right tuples (reloaded for each left tuple)
    right_tuples: Vec<Tuple>,
    /// Current position in right tuples
    right_position: usize,
    /// Whether right side has been materialized
    right_materialized: bool,
}

impl NestedLoopJoinExecutor {
    /// Create a new nested loop join executor
    pub fn new(
        left: Box<dyn Executor>,
        right: Box<dyn Executor>,
        condition: JoinConditionFn,
    ) -> Self {
        Self {
            left,
            right,
            condition,
            current_left: None,
            right_tuples: Vec::new(),
            right_position: 0,
            right_materialized: false,
        }
    }
}

// SAFETY: right_position is bounds-checked (< right_tuples.len()) in while loop condition.
#[allow(clippy::indexing_slicing)]
impl Executor for NestedLoopJoinExecutor {
    fn open(&mut self) -> Result<()> {
        self.left.open()?;
        self.right.open()?;
        self.current_left = None;
        self.right_tuples.clear();
        self.right_position = 0;
        self.right_materialized = false;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        // Materialize right side on first call
        if !self.right_materialized {
            while let Some(tuple) = self.right.next()? {
                self.right_tuples.push(tuple);
            }
            self.right_materialized = true;
        }

        loop {
            // Get current left tuple or advance to next
            if self.current_left.is_none() {
                self.current_left = self.left.next()?;
                self.right_position = 0;
            }

            // Process current left tuple if available
            let Some(left_tuple) = self.current_left.as_ref() else {
                return Ok(None); // Left side exhausted
            };

            // Scan through right tuples
            while self.right_position < self.right_tuples.len() {
                let right_tuple = &self.right_tuples[self.right_position];
                self.right_position += 1;

                if (self.condition)(left_tuple, right_tuple) {
                    // Combine tuples (concatenate values)
                    let mut combined_values = left_tuple.values.clone();
                    combined_values.extend(right_tuple.values.clone());
                    return Ok(Some(Tuple::new(combined_values)));
                }
            }

            // Right side exhausted for current left tuple, move to next left
            self.current_left = None;
        }
    }

    fn close(&mut self) -> Result<()> {
        self.left.close()?;
        self.right.close()?;
        self.right_tuples.clear();
        Ok(())
    }
}

// =============================================================================
// HashJoinExecutor: JOIN (hash-based)
// =============================================================================

/// Key extractor function type
pub type KeyExtractorFn = Box<dyn Fn(&Tuple) -> Value + Send>;

/// Hash join executor
///
/// Hash-based equi-join implementation.
pub struct HashJoinExecutor {
    /// Left (build) child executor
    left: Box<dyn Executor>,
    /// Right (probe) child executor
    right: Box<dyn Executor>,
    /// Left key extractor
    left_key: KeyExtractorFn,
    /// Right key extractor
    right_key: KeyExtractorFn,
    /// Hash table: key -> list of matching tuples
    hash_table: HashMap<String, Vec<Tuple>>,
    /// Current right tuple
    current_right: Option<Tuple>,
    /// Matching left tuples for current right
    current_matches: Vec<Tuple>,
    /// Position in current matches
    match_position: usize,
    /// Whether hash table has been built
    built: bool,
}

impl HashJoinExecutor {
    /// Create a new hash join executor
    pub fn new(
        left: Box<dyn Executor>,
        right: Box<dyn Executor>,
        left_key: KeyExtractorFn,
        right_key: KeyExtractorFn,
    ) -> Self {
        Self {
            left,
            right,
            left_key,
            right_key,
            hash_table: HashMap::new(),
            current_right: None,
            current_matches: Vec::new(),
            match_position: 0,
            built: false,
        }
    }

    /// Convert a value to a hashable string key
    fn value_to_key(value: &Value) -> String {
        format!("{:?}", value)
    }
}

// SAFETY: match_position is bounds-checked (< current_matches.len()) before indexing.
#[allow(clippy::indexing_slicing)]
impl Executor for HashJoinExecutor {
    fn open(&mut self) -> Result<()> {
        self.left.open()?;
        self.right.open()?;
        self.hash_table.clear();
        self.current_right = None;
        self.current_matches.clear();
        self.match_position = 0;
        self.built = false;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        // Build phase: create hash table from left side
        if !self.built {
            while let Some(tuple) = self.left.next()? {
                let key = Self::value_to_key(&(self.left_key)(&tuple));
                self.hash_table
                    .entry(key)
                    .or_insert_with(Vec::new)
                    .push(tuple);
            }
            self.built = true;
        }

        // Probe phase
        loop {
            // Return remaining matches for current right tuple
            if self.match_position < self.current_matches.len() {
                let left_tuple = &self.current_matches[self.match_position];
                self.match_position += 1;

                // current_right is guaranteed to be Some when we have matches
                let Some(right_tuple) = self.current_right.as_ref() else {
                    // This shouldn't happen, but handle gracefully by fetching next
                    self.current_matches.clear();
                    continue;
                };
                let mut combined_values = left_tuple.values.clone();
                combined_values.extend(right_tuple.values.clone());
                return Ok(Some(Tuple::new(combined_values)));
            }

            // Get next right tuple and find matches
            match self.right.next()? {
                Some(right_tuple) => {
                    let key = Self::value_to_key(&(self.right_key)(&right_tuple));

                    self.current_matches = self
                        .hash_table
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    self.match_position = 0;
                    self.current_right = Some(right_tuple);
                }
                None => return Ok(None),
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.left.close()?;
        self.right.close()?;
        self.hash_table.clear();
        Ok(())
    }
}

// =============================================================================
// AggregateExecutor: GROUP BY aggregations
// =============================================================================

/// Group key extractor function type
pub type GroupKeyFn = Box<dyn Fn(&Tuple) -> Vec<Value> + Send>;

/// Aggregate specification
pub struct AggregateSpec {
    /// Aggregate function name (COUNT, SUM, etc.)
    pub function_name: String,
    /// Column index to aggregate (None for COUNT(*))
    pub column_index: Option<usize>,
}

/// Aggregate executor for GROUP BY
///
/// Groups tuples and computes aggregates.
pub struct AggregateExecutor {
    /// Child executor providing input tuples
    child: Box<dyn Executor>,
    /// Group key extractor (empty for no GROUP BY)
    group_key: GroupKeyFn,
    /// Aggregate specifications
    aggregates: Vec<AggregateSpec>,
    /// Computed results (materialized)
    results: Vec<Tuple>,
    /// Current position in results
    position: usize,
    /// Whether aggregation has been computed
    computed: bool,
}

// SAFETY: Accumulator array (entry.1) is sized to match self.aggregates via zip enumeration.
// self.position is bounds-checked (< self.results.len()) before indexing.
#[allow(clippy::indexing_slicing)]
impl AggregateExecutor {
    /// Create a new aggregate executor
    pub fn new(
        child: Box<dyn Executor>,
        group_key: GroupKeyFn,
        aggregates: Vec<AggregateSpec>,
    ) -> Self {
        Self {
            child,
            group_key,
            aggregates,
            results: Vec::new(),
            position: 0,
            computed: false,
        }
    }

    fn compute_aggregates(&mut self) -> Result<()> {
        // Materialize all input tuples grouped by key
        let mut groups: HashMap<String, (Vec<Value>, Vec<Box<dyn AggregateState>>)> = HashMap::new();

        while let Some(tuple) = self.child.next()? {
            let key_values = (self.group_key)(&tuple);
            let key_string = format!("{:?}", key_values);

            let entry = groups.entry(key_string).or_insert_with(|| {
                let states: Vec<Box<dyn AggregateState>> = self
                    .aggregates
                    .iter()
                    .map(|spec| {
                        create_aggregate(&spec.function_name)
                            .map(|f| f.init_state())
                            .unwrap_or_else(|| {
                                // Default to COUNT if unknown aggregate function
                                Box::new(CountState::default())
                            })
                    })
                    .collect();
                (key_values.clone(), states)
            });

            // Accumulate values into each aggregate
            for (i, spec) in self.aggregates.iter().enumerate() {
                let value = match spec.column_index {
                    Some(col_idx) => tuple.values.get(col_idx).cloned().unwrap_or(Value::Null),
                    None => Value::Int4(1), // COUNT(*) counts all rows
                };
                entry.1[i].accumulate(&value)?;
            }
        }

        // Finalize aggregates and build result tuples
        for (_, (key_values, states)) in groups {
            let mut result_values = key_values;
            for state in states {
                result_values.push(state.finalize()?);
            }
            self.results.push(Tuple::new(result_values));
        }

        self.computed = true;
        Ok(())
    }
}

// SAFETY: self.position is bounds-checked (< self.results.len()) before indexing.
#[allow(clippy::indexing_slicing)]
impl Executor for AggregateExecutor {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        self.results.clear();
        self.position = 0;
        self.computed = false;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        if !self.computed {
            self.compute_aggregates()?;
        }

        if self.position < self.results.len() {
            let tuple = self.results[self.position].clone();
            self.position += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()?;
        self.results.clear();
        Ok(())
    }
}

// =============================================================================
// SortExecutor: ORDER BY
// =============================================================================

/// Sort key specification
#[derive(Clone)]
pub struct SortKey {
    /// Column index to sort by
    pub column_index: usize,
    /// Sort direction (true = ascending, false = descending)
    pub ascending: bool,
    /// Nulls first or last
    pub nulls_first: bool,
}

/// Sort executor for ORDER BY
///
/// Sorts tuples by specified keys.
pub struct SortExecutor {
    /// Child executor providing input tuples
    child: Box<dyn Executor>,
    /// Sort keys
    sort_keys: Vec<SortKey>,
    /// Sorted results (materialized)
    results: Vec<Tuple>,
    /// Current position in results
    position: usize,
    /// Whether sorting has been done
    sorted: bool,
}

impl SortExecutor {
    /// Create a new sort executor
    pub fn new(child: Box<dyn Executor>, sort_keys: Vec<SortKey>) -> Self {
        Self {
            child,
            sort_keys,
            results: Vec::new(),
            position: 0,
            sorted: false,
        }
    }

    fn compare_values(a: &Value, b: &Value) -> Ordering {
        match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Int2(x), Value::Int2(y)) => x.cmp(y),
            (Value::Int4(x), Value::Int4(y)) => x.cmp(y),
            (Value::Int8(x), Value::Int8(y)) => x.cmp(y),
            (Value::Float4(x), Value::Float4(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
            (Value::Float8(x), Value::Float8(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
            (Value::String(x), Value::String(y)) => x.cmp(y),
            (Value::Boolean(x), Value::Boolean(y)) => x.cmp(y),
            _ => Ordering::Equal, // Can't compare different types
        }
    }

    fn compare_tuples(&self, a: &Tuple, b: &Tuple) -> Ordering {
        for key in &self.sort_keys {
            let a_val = a.values.get(key.column_index).unwrap_or(&Value::Null);
            let b_val = b.values.get(key.column_index).unwrap_or(&Value::Null);

            // Handle nulls
            let (first_null, second_null) = if key.nulls_first {
                (Ordering::Less, Ordering::Greater)
            } else {
                (Ordering::Greater, Ordering::Less)
            };

            let ordering = match (a_val, b_val) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => first_null,
                (_, Value::Null) => second_null,
                _ => Self::compare_values(a_val, b_val),
            };

            // Apply direction
            let ordering = if key.ascending {
                ordering
            } else {
                ordering.reverse()
            };

            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    fn sort_results(&mut self) -> Result<()> {
        // Materialize all input tuples
        while let Some(tuple) = self.child.next()? {
            self.results.push(tuple);
        }

        // Sort using comparison function
        let sort_keys = self.sort_keys.clone();
        self.results.sort_by(|a, b| {
            for key in &sort_keys {
                let a_val = a.values.get(key.column_index).unwrap_or(&Value::Null);
                let b_val = b.values.get(key.column_index).unwrap_or(&Value::Null);

                let (first_null, second_null) = if key.nulls_first {
                    (Ordering::Less, Ordering::Greater)
                } else {
                    (Ordering::Greater, Ordering::Less)
                };

                let ordering = match (a_val, b_val) {
                    (Value::Null, Value::Null) => Ordering::Equal,
                    (Value::Null, _) => first_null,
                    (_, Value::Null) => second_null,
                    _ => Self::compare_values(a_val, b_val),
                };

                let ordering = if key.ascending {
                    ordering
                } else {
                    ordering.reverse()
                };

                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });

        self.sorted = true;
        Ok(())
    }
}

// SAFETY: self.position is bounds-checked (< self.results.len()) before indexing.
#[allow(clippy::indexing_slicing)]
impl Executor for SortExecutor {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        self.results.clear();
        self.position = 0;
        self.sorted = false;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        if !self.sorted {
            self.sort_results()?;
        }

        if self.position < self.results.len() {
            let tuple = self.results[self.position].clone();
            self.position += 1;
            Ok(Some(tuple))
        } else {
            Ok(None)
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()?;
        self.results.clear();
        Ok(())
    }
}

// =============================================================================
// LimitExecutor: LIMIT/OFFSET
// =============================================================================

/// Limit executor for LIMIT/OFFSET
pub struct LimitExecutor {
    /// Child executor providing input tuples
    child: Box<dyn Executor>,
    /// Maximum number of tuples to return
    limit: Option<usize>,
    /// Number of tuples to skip
    offset: usize,
    /// Current count of returned tuples
    count: usize,
    /// Current count of skipped tuples
    skipped: usize,
}

impl LimitExecutor {
    /// Create a new limit executor
    pub fn new(child: Box<dyn Executor>, limit: Option<usize>, offset: usize) -> Self {
        Self {
            child,
            limit,
            offset,
            count: 0,
            skipped: 0,
        }
    }
}

impl Executor for LimitExecutor {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        self.count = 0;
        self.skipped = 0;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>> {
        // Check if we've hit the limit
        if let Some(limit) = self.limit {
            if self.count >= limit {
                return Ok(None);
            }
        }

        // Skip offset tuples
        while self.skipped < self.offset {
            match self.child.next()? {
                Some(_) => self.skipped += 1,
                None => return Ok(None),
            }
        }

        // Return next tuple
        match self.child.next()? {
            Some(tuple) => {
                self.count += 1;
                Ok(Some(tuple))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tuples() -> Vec<Tuple> {
        vec![
            Tuple::new(vec![Value::Int4(1), Value::String("Alice".to_string()), Value::Int4(30)]),
            Tuple::new(vec![Value::Int4(2), Value::String("Bob".to_string()), Value::Int4(25)]),
            Tuple::new(vec![Value::Int4(3), Value::String("Charlie".to_string()), Value::Int4(35)]),
        ]
    }

    #[test]
    fn test_scan_executor() {
        let mut scan = ScanExecutor::new(test_tuples());
        scan.open().unwrap();

        let mut count = 0;
        while scan.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 3);

        scan.close().unwrap();
    }

    #[test]
    fn test_filter_executor() {
        let scan = Box::new(ScanExecutor::new(test_tuples()));
        let predicate: PredicateFn = Box::new(|tuple| {
            matches!(&tuple.values[2], Value::Int4(age) if *age > 28)
        });
        let mut filter = FilterExecutor::new(scan, predicate);
        filter.open().unwrap();

        let mut count = 0;
        while filter.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 2); // Alice (30) and Charlie (35)

        filter.close().unwrap();
    }

    #[test]
    fn test_project_executor() {
        let scan = Box::new(ScanExecutor::new(test_tuples()));
        let mut project = ProjectExecutor::by_indices(scan, vec![1]); // Just names
        project.open().unwrap();

        let tuple = project.next().unwrap().unwrap();
        assert_eq!(tuple.values.len(), 1);
        assert_eq!(tuple.values[0], Value::String("Alice".to_string()));

        project.close().unwrap();
    }

    #[test]
    fn test_sort_executor() {
        let scan = Box::new(ScanExecutor::new(test_tuples()));
        let sort_keys = vec![SortKey {
            column_index: 2, // Sort by age
            ascending: true,
            nulls_first: true,
        }];
        let mut sort = SortExecutor::new(scan, sort_keys);
        sort.open().unwrap();

        // First should be Bob (25)
        let first = sort.next().unwrap().unwrap();
        assert_eq!(first.values[1], Value::String("Bob".to_string()));

        // Then Alice (30)
        let second = sort.next().unwrap().unwrap();
        assert_eq!(second.values[1], Value::String("Alice".to_string()));

        sort.close().unwrap();
    }

    #[test]
    fn test_limit_executor() {
        let scan = Box::new(ScanExecutor::new(test_tuples()));
        let mut limit = LimitExecutor::new(scan, Some(2), 0);
        limit.open().unwrap();

        let mut count = 0;
        while limit.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 2);

        limit.close().unwrap();
    }
}
