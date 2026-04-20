//! Top-K operator: combined `Sort → Limit` for large inputs.
//!
//! Instead of fully sorting N rows (O(N log N)) and discarding all but the
//! first `k = limit + offset`, `TopKOperator` streams the input through a
//! bounded max-heap of size `k`, yielding O(N log k) complexity and O(k)
//! memory. The outer `LimitOperator` still applies the `offset` skip.
//!
//! This lands as part of the pagination work (see
//! FEATURE_REQUEST_pagination.md) so that `ORDER BY ... LIMIT N OFFSET M`
//! stays fast on large tables when the application can't use keyset
//! pagination.

use crate::{Result, Tuple, Schema, Value};
use super::{PhysicalOperator, TimeoutContext, compare_values};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

/// Heap entry: the sort key vector + the tuple it came from. Ordered so
/// that `BinaryHeap` (a max-heap) keeps the *largest* sort key at the
/// top. When we hit capacity and the next tuple's sort key is smaller
/// than the heap's max, we pop the max and push the new entry — leaving
/// the `k` smallest at the end. Descending order (`asc = false`) is
/// handled by reversing per-field comparison when building the key.
struct HeapEntry {
    key: Vec<Value>,
    asc: Arc<Vec<bool>>,
    tuple: Tuple,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        for (i, (a, b)) in self.key.iter().zip(other.key.iter()).enumerate() {
            let mut c = compare_values(a, b);
            // Apply asc/desc per column
            if !self.asc.get(i).copied().unwrap_or(true) {
                c = c.reverse();
            }
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    }
}

/// Streaming Top-K operator. Materialises only `k` tuples instead of the
/// full input.
pub struct TopKOperator {
    sorted: Vec<Tuple>,
    cursor: usize,
    schema: Arc<Schema>,
}

impl TopKOperator {
    /// `k` is the number of rows to keep (typically `limit + offset`).
    /// The caller is responsible for wrapping in a `LimitOperator` to
    /// apply the actual `offset`/`limit` window.
    pub fn new(
        mut input: Box<dyn PhysicalOperator>,
        exprs: Vec<crate::sql::LogicalExpr>,
        asc: Vec<bool>,
        k: usize,
        timeout_ctx: Option<TimeoutContext>,
    ) -> Result<Self> {
        let schema = input.schema();
        let evaluator = crate::sql::Evaluator::new(schema.clone());
        let asc = Arc::new(asc);

        // Degenerate case: k == 0 ⇒ no rows to return.
        if k == 0 {
            return Ok(Self { sorted: Vec::new(), cursor: 0, schema });
        }

        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(k + 1);
        while let Some(tuple) = input.next()? {
            if let Some(ref ctx) = timeout_ctx {
                ctx.check_timeout()?;
            }
            // Build the sort key for this tuple
            let mut key = Vec::with_capacity(exprs.len());
            for expr in &exprs {
                match evaluator.evaluate(expr, &tuple) {
                    Ok(v) => key.push(v),
                    Err(_) => key.push(Value::Null),
                }
            }
            let entry = HeapEntry { key, asc: Arc::clone(&asc), tuple };
            if heap.len() < k {
                heap.push(entry);
            } else {
                // Peek at heap max (largest-so-far)
                let replace = heap.peek().map(|top| entry < *top).unwrap_or(false);
                if replace {
                    heap.pop();
                    heap.push(entry);
                }
            }
        }

        // Drain heap in ascending order of the comparator — this is the
        // final top-k result in correct order.
        let mut entries: Vec<HeapEntry> = heap.into_sorted_vec();
        // `into_sorted_vec` returns ascending order for a max-heap, which
        // (because our Ord already applies asc/desc) gives the user-visible
        // sort direction directly.
        let sorted: Vec<Tuple> = entries.drain(..).map(|e| e.tuple).collect();

        Ok(Self { sorted, cursor: 0, schema })
    }

    /// No-op — timeout is handled during construction, kept for API symmetry.
    pub fn with_timeout(self, _: Option<TimeoutContext>) -> Self { self }
}

impl PhysicalOperator for TopKOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        let out = self.sorted.get(self.cursor).cloned();
        if out.is_some() {
            self.cursor += 1;
        }
        Ok(out)
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}
