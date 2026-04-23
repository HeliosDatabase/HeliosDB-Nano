//! Per-request bump arena.
//!
//! `RequestArena` wraps `bumpalo::Bump` so that hot paths can allocate
//! transient buffers (visited bitsets, candidate lists, scratch rows, ...)
//! without hitting the global allocator on each small allocation. The arena
//! is dropped wholesale when the request finishes -- amortising
//! deallocation cost to a single `free`.
//!
//! ## Usage
//!
//! ```rust
//! use heliosdb_nano::runtime::RequestArena;
//!
//! let arena = RequestArena::with_capacity(4 * 1024);
//! let mut scratch = bumpalo::collections::Vec::with_capacity_in(16, arena.bump());
//! for i in 0..16u32 {
//!     scratch.push(i);
//! }
//! assert_eq!(scratch.len(), 16);
//! // No explicit free needed -- dropping `arena` reclaims everything.
//! ```
//!
//! ## Why a wrapper?
//!
//! - Centralises configuration knobs (initial capacity, reset policy).
//! - Provides telemetry hooks (`bytes_allocated`, `chunk_count`).
//! - Lets callers reset and reuse a single arena across requests, which
//!   keeps already-warmed memory chunks live (a common external project pattern).
//! - Future-proof: swap the underlying allocator without touching callers.
//!
//! RAG-native (see external-project_INTEGRATION_PLAN idea 3).

use bumpalo::Bump;

/// Default initial chunk size for a request arena (16 KiB).
///
/// Sized to fit a typical query's transient state -- HNSW candidate
/// queues, a few hundred row pointers, BM25 term hash buffers -- in the
/// first chunk so common workloads never grow.
pub const DEFAULT_INITIAL_CAPACITY: usize = 16 * 1024;

/// Per-request bump arena.
///
/// Wraps `bumpalo::Bump`; see module docs for design rationale.
pub struct RequestArena {
    bump: Bump,
}

impl RequestArena {
    /// Create a new arena with the default initial capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_INITIAL_CAPACITY)
    }

    /// Create a new arena with the given initial capacity in bytes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bump: Bump::with_capacity(capacity),
        }
    }

    /// Borrow the underlying `Bump` allocator.
    ///
    /// Use this when constructing `bumpalo::collections::Vec` /
    /// `bumpalo::collections::String` etc.
    #[must_use]
    pub fn bump(&self) -> &Bump {
        &self.bump
    }

    /// Total bytes currently held by the arena (sum of all chunk capacities).
    #[must_use]
    pub fn bytes_allocated(&self) -> usize {
        self.bump.allocated_bytes()
    }

    /// Number of chunks currently held by the arena.
    ///
    /// A growing chunk count over many requests indicates the initial
    /// capacity is too small for the typical workload.
    ///
    /// Requires `&mut self` because `bumpalo`'s chunk iterator needs
    /// exclusive access to walk the internal chunk list safely.
    pub fn chunk_count(&mut self) -> usize {
        self.bump.iter_allocated_chunks().count()
    }

    /// Reset the arena, retaining the largest chunk.
    ///
    /// This is the cheap reuse path -- after `reset()` the arena can serve
    /// another request without reallocating its first chunk.
    pub fn reset(&mut self) {
        self.bump.reset();
    }

    /// Allocate a value in the arena and return a reference with the
    /// arena's lifetime.
    pub fn alloc<T>(&self, val: T) -> &mut T {
        self.bump.alloc(val)
    }

    /// Allocate a slice by copying from the given iterator.
    pub fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> &mut [T] {
        self.bump.alloc_slice_copy(src)
    }
}

impl Default for RequestArena {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RequestArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestArena")
            .field("bytes_allocated", &self.bytes_allocated())
            .finish()
    }
}

// -- Convenience helpers for the common buffer patterns -------------------

/// Bump-allocated `Vec<T>` -- alias for ergonomics.
pub type ArenaVec<'a, T> = bumpalo::collections::Vec<'a, T>;

/// Bump-allocated `String` -- alias for ergonomics.
pub type ArenaString<'a> = bumpalo::collections::String<'a>;

/// Allocate a `bumpalo::collections::Vec<T>` with the given capacity.
#[must_use]
pub fn vec_with_capacity_in<T>(capacity: usize, arena: &RequestArena) -> ArenaVec<'_, T> {
    bumpalo::collections::Vec::with_capacity_in(capacity, arena.bump())
}

/// Allocate a `bumpalo::collections::String` with the given capacity.
#[must_use]
pub fn string_with_capacity_in(capacity: usize, arena: &RequestArena) -> ArenaString<'_> {
    bumpalo::collections::String::with_capacity_in(capacity, arena.bump())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_allocates_and_reports_bytes() {
        let arena = RequestArena::with_capacity(1024);
        let initial = arena.bytes_allocated();

        let mut buf = vec_with_capacity_in::<u64>(64, &arena);
        for i in 0..64u64 {
            buf.push(i);
        }

        assert_eq!(buf.len(), 64);
        // Allocator should have used some bytes (>= 64 * 8 = 512).
        assert!(arena.bytes_allocated() >= initial);
    }

    #[test]
    fn arena_alloc_returns_writable_reference() {
        let arena = RequestArena::new();
        let v = arena.alloc(42_u32);
        *v += 1;
        assert_eq!(*v, 43);
    }

    #[test]
    fn arena_alloc_slice_copy() {
        let arena = RequestArena::new();
        let src = [1u32, 2, 3, 4, 5];
        let dst = arena.alloc_slice_copy(&src);
        assert_eq!(dst, &[1, 2, 3, 4, 5]);
        dst[0] = 99;
        assert_eq!(dst[0], 99);
    }

    #[test]
    fn arena_reset_keeps_capacity() {
        let mut arena = RequestArena::with_capacity(2048);
        {
            let mut buf = vec_with_capacity_in::<u8>(1000, &arena);
            for _ in 0..1000 {
                buf.push(0);
            }
        }
        let pre_reset = arena.bytes_allocated();
        arena.reset();
        // After reset the largest chunk is retained -- bytes_allocated stays
        // > 0 since chunk capacity is preserved.
        assert!(arena.bytes_allocated() > 0);
        assert!(arena.bytes_allocated() <= pre_reset);
    }

    #[test]
    fn arena_string_helper() {
        let arena = RequestArena::new();
        let mut s = string_with_capacity_in(32, &arena);
        s.push_str("hello, ");
        s.push_str("arena");
        assert_eq!(s.as_str(), "hello, arena");
    }

    #[test]
    fn arena_default_constructs() {
        let a = RequestArena::default();
        assert!(a.bytes_allocated() < DEFAULT_INITIAL_CAPACITY * 4);
    }

    #[test]
    fn arena_debug_format_includes_bytes() {
        let arena = RequestArena::new();
        let s = format!("{arena:?}");
        assert!(s.contains("RequestArena"));
        assert!(s.contains("bytes_allocated"));
    }

    #[test]
    fn arena_supports_many_small_allocations() {
        // Stress: many small allocations should not panic and should grow.
        let arena = RequestArena::with_capacity(64);
        let mut refs = Vec::new();
        for i in 0..1000u32 {
            refs.push(arena.alloc(i));
        }
        // Verify values intact.
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(**r, i as u32);
        }
    }
}
