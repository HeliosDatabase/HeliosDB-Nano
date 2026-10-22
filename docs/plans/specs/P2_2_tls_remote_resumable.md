# Spec P2.2 — remote replication over TLS; resumable snapshot download; fireproof odds and ends

Parent: `PHYSICAL_STANDBY_REPLICATION.md` §D5, §D7. Sprinter `86d44281eabd`. After P2.1 (`2814d89e`).

Value: the single replication port (NPHYS002, P2.1) is loopback-only: `physical_wire.rs`
`bind_loopback` / `PhysicalClient::connect_loopback`, the accept filter and the
`--primary-host` resolution check in `main.rs`, the receiver's source validation in
`physical_receiver.rs`, and `[replication] allow_insecure_remote`, which is accepted but
changes nothing. Proxy's lab and every real deployment need a standby on another host. A
snapshot download that dies midway restarts from zero today. This spec lifts the loopback
rule behind TLS, makes the download resumable, and closes the remaining §D5 holes.

## 1. TLS on the single replication port

Framing is already generic (`read_frame`/`write_frame`, `run_session<I>`, `PhysicalClient<I>`
over `AsyncRead + AsyncWrite`), so TLS is a stream wrapper, not a protocol change. Reuse
`src/protocol/tls_provider.rs` (`pq_capable_provider`, `load_cert_and_key`, `load_ca_certs`)
and the client-certificate verification path of `src/protocol/postgres/ssl.rs`
(`SslConfig::with_ca_cert`, `SslNegotiator`), `src/protocol/tls_stream.rs` for the framed
stream type. No new TLS dependency.

- **Server (primary)** — `[replication]`, flat keys in the existing style, each with a CLI flag:
  `tls_enabled` (`--replication-tls`), `tls_cert_path` (`--replication-tls-cert`),
  `tls_key_path` (`--replication-tls-key`), `tls_ca_path` (`--replication-tls-ca`; when set,
  client certificates are REQUIRED and verified against it = mTLS), `tls_post_quantum`
  (default follows `[server] tls_post_quantum`). When `tls_enabled` is set without a cert/key,
  fall back to the `[server] tls_cert_path`/`tls_key_path` pair if `[server] tls_enabled`;
  otherwise refuse to start naming the missing keys.
- **Client (standby)** — `[replication]`: `primary_tls` (`--primary-tls`, default: on when the
  primary address is not loopback), `primary_tls_ca_path` (`--primary-ca`; system roots when
  unset), `primary_tls_server_name` (`--primary-server-name`, default = the host part of
  `--primary-host`; IP literals are verified as IP SANs), `primary_tls_client_cert_path` /
  `primary_tls_client_key_path` (`--primary-client-cert` / `--primary-client-key`),
  `primary_tls_insecure_skip_verify` (`--primary-tls-insecure-skip-verify`, only honoured
  together with `allow_insecure_remote`, WARN at start).
- **Listener rule** replaces the loopback checks: the replication listener binds the
  replication address derived from `--listen` as today; a NON-loopback bind is refused unless
  `tls_enabled` or `allow_insecure_remote`. `allow_insecure_remote` alone permits a plaintext
  token over the network and prints the existing INSECURE banner once. The bearer token stays
  mandatory in every mode (TLS is transport, the token is authentication; with mTLS the token is
  still checked). Non-loopback peers are accepted; the per-peer `is_loopback` skip in the accept
  loop goes away. A TLS handshake failure is logged with the peer and counted; no `Refuse` frame
  can be sent before the handshake, so the client maps a TLS error to posture `reconnecting`
  with the TLS reason in `/health`.
- **Standby rule**: `--primary-host` may resolve to non-loopback addresses when `primary_tls`
  or `allow_insecure_remote` is set; otherwise keep today's error text. A wrong CA / untrusted
  server certificate / hostname mismatch → posture `reconnecting` with reason
  `tls: <rustls error>` (NOT `needs_recovery`: the data is fine, the network is not) and the
  usual backoff. `Hello` is unchanged; add capability bit `CAP_TLS = 1 << 2` set by both sides
  when the session is on TLS, reported in `/health` as `"tls": true|false` on both roles.
- **`/health` and `heliosdb_standby_status`** gain `tls` (bool) in the replication block; the
  frozen P1.3 fields keep their names and types (additive change only; tell Proxy).
- Docs: `docs/guides/physical-standby.md` (remote section replaces "loopback until P2.2"),
  `deployment-endpoints.md`, the server skill, `--help` texts that say "loopback only until TLS
  replication" (main.rs ~2052, ~2994, ~3422, ~5789), CHANGELOG.

## 2. Resumable snapshot download (`checkpoint_transfer.rs`, `physical_receiver.rs`)

- Receiver side: `append_chunk` already enforces sequential offsets per file. Persist progress
  in the staging dir as `NANO-PHYSICAL-PARTIAL.json` {format: 1, snapshot_id, manifest_sha256,
  history, per-file {name, size, sha256, bytes_done}}, rewritten (tmp + rename + fsync) after
  every completed file and at most every 8 MiB inside a file. On start with an existing staging
  dir: if the partial file parses and the primary's `Manifest` for its current export has the
  same `manifest_sha256`/snapshot_id, verify each file marked complete by SHA-256 (re-download
  on mismatch), truncate the in-progress file to its recorded `bytes_done`, and continue with
  `ChunkRequest { file, offset }` from there. Anything else (no partial, different manifest,
  unparsable, file sizes disagree) → delete the staging dir and start over, logging why. The
  "existing partial downloads are never adopted" rule (receiver module doc) becomes "adopted
  only through a verified partial record". Once all files are complete the stage is installed
  exactly as today (census + digest checks unchanged).
- Primary side: `OnlineCheckpointExport` is per session today and dropped with it. Add an
  **export lease**: the source keeps the checkpoint export alive for
  `[replication] snapshot_export_lease_secs` (default 3600) after the session that opened it
  ends, keyed by snapshot_id, so a reconnecting receiver can `Open` the SAME snapshot
  (`Open` gains an optional snapshot id: when present and still leased, the source answers with
  that export's `Manifest`; when absent or expired, a fresh export). Bound the number of leased
  exports (default 4, oldest dropped) and document that a leased checkpoint pins SST files
  (checkpoints are hard links).
- Receiver download loop: transport errors during the download reconnect with the existing
  backoff instead of failing the bootstrap; `Position` for the download stage reports
  `bytes_done/bytes_total` and `/health` shows `state: "recovering"` with that progress (the
  posture already has `recovering{mode, progress}`).

## 3. Fireproof items (§D5) not yet done

- `--dump-on-shutdown` on a standby: already refused (main.rs ~1685) — keep, add a test if none.
- `enable_lockfree_ingestion` refused with a clear error when the engine was opened by a
  process with `role = primary` (any HA role): it bypasses the logical WAL/MVCC/indexes. The
  `data/lockfree_wal` relative path (engine.rs ~4078) moves under the data dir
  (`<data_dir>/lockfree_wal`) for the remaining embedded use (Sprinter `96e5a8cfa254` item 3).
- `replication_wal/` creation: removed in P2.1 — verify with a source grep, add it to the
  `logical_standby_removed` scan.
- HNSW `hnsw_snapshots/` files: add the dump file's SHA-256 to the `vecsnap:` sidecar
  (`index_snapshot.rs`, `VectorGraphSidecar`) and refuse a mismatching or missing file at
  hydration by rebuilding from rows (log once). The full move into RocksDB stays P5.
- Encryption key rotation while a standby is attached: there is NO rotation command in this
  tree (verify); document in `physical-standby.md` that a standby must be started with the
  same dataset key and that key rotation is not supported with standbys attached until P5.

## 4. Tests (proof-first; real daemons via `tests/support/physical_pair.rs`)

- `tests/physical_standby_tls.rs`: certificates generated at run time with the `openssl` CLI
  exactly as `tests/mysql_ssl_tests.rs::generate_ca_and_signed_cert` does (factor that helper
  into `tests/support/` if it is not already shared). Pair on `127.0.0.2`/`127.0.0.3` (loopback
  aliases are routable on Linux without setup) for the real-daemon scenarios, plus a unit-level
  check of the "non-loopback needs TLS or the opt-in" rule on a non-loopback `SocketAddr`.
  Scenarios: TLS pair streams and `/health` says `tls: true`; mTLS pair; standby with the
  wrong CA → `reconnecting` with a `tls:` reason, data intact, recovers when pointed at the
  right CA; plaintext remote refused without `allow_insecure_remote` (both sides) and accepted
  with it (WARN present); token still required over TLS (`Refuse{AuthFailed}`).
- `tests/physical_snapshot_resume.rs`: a primary big enough for ≥ 3 SST files (flush between
  loads); kill the standby at ~30 % and ~70 % of the download (SIGKILL on the child when
  `/health` progress crosses the mark, or a chunk-count hook in the receiver behind
  `internal-tests`), restart, assert bytes downloaded after the restart < remaining + one chunk
  per file, the final census/digest equal the primary, and a manifest change (new export after
  the lease expired, use a 1 s lease in the test) → full restart with the "starting over" log.
- Unit: partial record round-trip; resume decision table (same manifest / different / corrupt /
  size mismatch); lease expiry; `CAP_TLS` in `Hello`/`HelloOk`.
- Fixture `scripts/regress/nano-physical-pair.sh --tls` generates a throwaway CA with `openssl`
  and starts the pair on TLS; Proxy's PG ports are unchanged (TLS is on the replication port
  only). `--status` JSON gains `"tls": true|false`.

## Constraints

No cargo/git; read before edit; match style; never weaken the token or census checks; keep
the P1.3 Proxy contract fields unchanged (additive only). Report: files; the config/flag table;
how the receiver decides resume vs restart; the lease semantics; tests red/green; commands.
