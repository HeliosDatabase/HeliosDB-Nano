# Task 184 — HTTP `POST /mcp` + SSE progress pairing

## Goal

Today `notifications/progress` streams over WebSocket and stdio,
but `POST /mcp` is single-shot — no way to carry interleaved
progress events back to the client. Wire the spec-shape pairing:
SSE `GET /mcp/sse` opens a server-push channel keyed by a session
ID; subsequent `POST /mcp` requests with `_meta.progressToken` route
their progress events through the matching SSE stream while the
final RPC response goes back over the POST.

## Acceptance

* Client opens `GET /mcp/sse?session=abc` → receives an `endpoint`
  event with the POST URL plus a stream of subsequent
  `notifications/progress` SSE events.
* Client sends `POST /mcp` with header `Mcp-Session-Id: abc` and
  body `{... _meta.progressToken: "tok-1"}`.
* While the handler runs, progress events arrive on the SSE
  stream. The POST response carries the final
  `tools/call` result.
* Tests: round-trip with `helios_graphrag_search` confirms ≥1 SSE
  progress event interleaved before the POST response returns.

## Design

* New module `src/mcp/session.rs`: process-static `DashMap<String,
  mpsc::UnboundedSender<axum::response::sse::Event>>` keyed by
  session ID. SSE handler creates a (sender, receiver) pair on
  connect, registers the sender, returns the receiver's stream.
* `handle_sse` (existing) accepts an optional `?session=<id>`
  query param; if present, registers the channel; if absent,
  generates a UUID and announces it via the `endpoint` event so
  clients can echo it back as the session id.
* `handle_post` extracts `Mcp-Session-Id` from request headers
  alongside `_meta.progressToken`. When both are present, tools/call
  takes the streaming path and sinks events to the matching SSE
  sender.
* When the session disconnects, sessions table drops the entry;
  POSTs that target a missing session fall back to the regular
  no-streaming path.
* TTL: a session entry expires after 5 minutes of inactivity;
  expiry sweep runs piggy-backed on registration.

## Files to touch

* `src/mcp/session.rs` — new.
* `src/mcp/axum_routes.rs` — extend `handle_sse`, `handle_post`.
* `src/mcp/mod.rs` — re-exports.
* New test: `tests/mcp_progress_http.rs`.

## Tests

1. POST with progress token + valid session → SSE stream sees
   ≥1 progress notification.
2. POST without session header → no SSE event; client gets the
   final response only.
3. POST with stale session ID → falls back to no-streaming path,
   final response still arrives intact.

## Out of scope

- Multi-tenant session namespacing — single process, single
  session pool. Auth gating is via the existing JWT middleware.
- Bidirectional resumption (clients resuming an interrupted SSE
  stream). MCP 2024-11-05 spec doesn't require it.
