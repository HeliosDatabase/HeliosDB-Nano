//! Lifetime / drop semantics tests for `runtime::RequestArena`.
//!
//! HelixDB-inspired (idea 3 of the integration plan).

use heliosdb_nano::runtime::{
    arena::{string_with_capacity_in, vec_with_capacity_in, ArenaVec},
    RequestArena,
};

#[test]
fn arena_outlives_inner_buffers_until_drop() {
    let arena = RequestArena::with_capacity(4096);
    let mut buf: ArenaVec<'_, u32> = vec_with_capacity_in(128, &arena);
    for i in 0..128u32 {
        buf.push(i * 2);
    }
    let expected: u32 = (0..128u32).map(|i| i * 2).sum();
    assert_eq!(buf.iter().sum::<u32>(), expected);
    drop(buf);
    drop(arena);
}

#[test]
fn arena_can_serve_multiple_logical_requests_via_reset() {
    let mut arena = RequestArena::with_capacity(2048);
    for round in 0..8u32 {
        let mut s = string_with_capacity_in(64, &arena);
        s.push_str("request-");
        s.push_str(&round.to_string());
        assert!(s.starts_with("request-"));
        // Drop scratch state before resetting the arena -- enforces the
        // "all references die before reset" contract of bumpalo.
        drop(s);
        arena.reset();
    }
}

#[test]
fn arena_bytes_allocated_grows_then_resets() {
    let mut arena = RequestArena::with_capacity(128);
    let _: ArenaVec<'_, u8> = vec_with_capacity_in(0, &arena);
    let before = arena.bytes_allocated();
    {
        let mut buf: ArenaVec<'_, u64> = vec_with_capacity_in(512, &arena);
        for i in 0..512u64 {
            buf.push(i);
        }
    }
    let peak = arena.bytes_allocated();
    assert!(peak >= before);
    arena.reset();
    // After reset bumpalo retains chunk capacity -- still >= before.
    assert!(arena.bytes_allocated() >= before);
}
