# P4.1b — NPHYS002 sockets: TCP_NODELAY and one-write frames

Plan: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D7 (one replication connection), found
while measuring P4.1 (`docs/plans/specs/P4_1_recover_from_primary_fast_forward.md`) on a
1 GB dataset. Status: spec. Implementation agents never run cargo or git.

## 1. The defect

Every frame on the replication channel (`src/replication/physical_wire.rs`,
`write_frame`, ~line 1702) is written as **two** `write_all` calls — the 16-byte header,
then the payload — and **no replication socket sets `TCP_NODELAY`** (the only
`set_nodelay(true)` in the tree is the PostgreSQL listener's,
`src/protocol/postgres/timeouts.rs:266`). With Nagle's algorithm on, the second small write
of a request frame (a 32-byte `ChunkRequest`, a `LeafChunkRequest`, a `Poll`, a heartbeat, a
planning re-ask) waits for the ACK of the first, and the peer, having nothing to send,
delays that ACK (Linux: up to 40 ms). The channel is request/response, one frame in flight,
so every round trip pays the stall:

| path | unit per round trip | observed (loopback, release b5e66988) |
|---|---|---|
| checkpoint download (`read_chunk`, 1 MiB chunks) | 1 MiB | 82 MiB in 5.6 s = **15 MB/s** |
| fast-forward leaf transfer (`LeafChunkRequest`, 1 MiB) | 1 MiB | same ceiling |
| streaming `Poll` | ≤ 8 MiB | ≥ 25 polls/s, so ~200 MB/s ceiling; not the streaming limit |

A loopback copy of SST files should run at hundreds of MB/s; the maintainer's target for
rebuilding and following large databases is a few GB per minute (≥ 50 MB/s sustained).

## 2. The change

1. **`TCP_NODELAY` on both ends.**
   - Client: in `ReplicationClient::connect` (physical_wire.rs ~3078) call
     `stream.set_nodelay(true)?` right after `TcpStream::connect` succeeds, before the TLS
     handshake or `authenticate`.
   - Source: in `accept_replication` (physical_wire.rs ~1923) call `stream.set_nodelay(true)?`
     on the accepted `TcpStream` first thing, before the TLS handshake.
   - A failure of `set_nodelay` is an `io::Error` → `WireError::Io` like any other socket
     error (do not ignore it silently).
   - Any other place that builds a `ReplicationStream` from a raw `TcpStream` (tests that
     bind `127.0.0.1:0` in physical_receiver.rs / recover_primary.rs included) goes through
     these two functions or sets it the same way.
2. **One write per frame.** In `write_frame`, when `payload.len() <= SMALL_FRAME_BYTES`
   (`64 * 1024`), build one buffer `header ‖ payload` and issue a single `write_all`; larger
   payloads keep `write_all(header)` + `write_all(payload)` (no copy of a multi-MiB payload).
   `flush()` stays. Document on the constant why (one segment / one TLS record per control
   frame; independent of the socket option).
3. Nothing on the wire changes: same bytes, same framing, NPHYS002 version untouched.

## 3. Tests (red first where it can be)

- `physical_wire.rs` unit test: a frame with a small payload reaches the peer in one
  `poll_write` call (use a test `AsyncWrite` that records the number of writes), a large one
  in two; the decoded frame is identical either way.
- Unit test on a real loopback `TcpListener`/`TcpStream` pair that `connect` and
  `accept_replication` leave `nodelay() == true` on both sockets (plaintext and, if a TLS
  fixture exists in the file, TLS).
- No change to integration tests is needed; the physical tier must stay green.

## 4. Docs

- `CHANGELOG.md`, Unreleased → Fixed: "replication sockets set TCP_NODELAY and send control
  frames in one write; checkpoint downloads and fast-forward leaf transfers no longer stall
  ~40 ms per 1 MiB chunk (loopback copy 15 MB/s → measured after the fix)". The measured
  number is filled in by the gate run, not guessed.
- `docs/guides/physical-standby.md`, the "limits"/sizing paragraph that mentions transfer
  rates, if any, is updated with the measured figure in the same commit.

## 5. Out of scope (filed separately)

- Pipelining chunk requests (more than one in flight) and larger chunk frames.
- The streaming applier's own throughput (P1 derived-state publish), measured separately.
- Sprinter 6146b39a5beb (stale completed full copy adopted without a Resume probe).
