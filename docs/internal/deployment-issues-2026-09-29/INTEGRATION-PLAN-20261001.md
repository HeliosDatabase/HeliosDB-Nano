# Nano deployment campaign — integration review and plan

**Status:** review complete, plan approved by the owner 2026-10-02 (R6: one replication secret), **not yet
implemented**. Author: Opus 5.5, nano session (campaign
owner since 2026-10-01; evaluation/acceptance with Astra). Implementation: DeepSeek V4.1 Flash under this
review. Nothing here accepts, closes, merges or releases anything.
**Scope:** archive issues 1, 5, 6, 7, 8 only (`HANDOFF-TO-NANO-20261001.md`). Proxy stays with `hproxy`.
**Supersedes:** the handoff's step 2 ("choose candidate integration order explicitly") — the order is no
longer a choice; §1 shows the candidates have a fixed ancestry.

## How this was built

Everything below was produced from the **hashed candidate patches**, not from the worktrees (which drift —
`nano-native-auth-20260930` now sits on `c8b5f22` with 84 changes, inconsistent with its recorded lineage).
All three patches match `handoff/candidate-index.json`, record base `916138311c69`, and apply cleanly to it.

Trees were built with temporary index files (`GIT_INDEX_FILE=… git read-tree 9161383 && git apply --cached`)
and unreferenced tree/commit objects; no worktree, index, branch or ref was modified. Merges were trial-run
with `git merge-tree --write-tree` (git 2.47.3), which performs a real 3-way merge in memory.

Each claim is marked **[verified]** (checked mechanically against objects or source) or **[static]** (from
reading code; not executed). Nothing here has been compiled or run. The first `cargo check` of the
integrated tree is the confirmation for every **[static]** compile claim.

## 1. The candidates are a tree, not three alternatives

The handoff calls the patches "alternative full patches … do not stack". That is correct as an instruction
— they cannot be applied in sequence — but they are not independent. **[verified]** Every file each candidate
changes was compared, blob for blob, against the base, the original (`2f161df`) and the history-guard
worktree (`nano-history-guard-20260929`):

| Candidate | Files | Identical to guard | Identical to original only | Identical to both | Own change |
| --- | --- | --- | --- | --- | --- |
| admission | 34 | **20** | 0 | 9 | **5** |
| physical-resync | 52 | **19** | 0 | 12 | **21** |
| native-auth | 27 | **0** | 5 | 13 | **9** |

```
9161383  base (v4.41.0)
 └── 2f161df  original — endpoints, config, trust opt-in, replay/cache, offline reseed   [issues 5, 6, 8; part of 1, 7]
      ├── native-auth   = original + 9 files       native credential auth              [issue 1]
      └── GUARD         = original + history identity / durable receiver binding        [issue 7 containment]
           ├── admission       = guard + 5 files   standby SQL-shape write admission, GH36 row-delete fix
           └── physical-resync = guard + 21 files  raw checkpoint transfer + strict hydration (NON-SERVING)
```

**The history guard is a fourth change set with no patch of its own.** It exists only embedded in two
candidates. A tree built from the guard worktree (19 untracked source files + 20 tracked modifications,
`docs/internal/` and build output excluded) reproduces every inherited file exactly: **admission 29/29,
physical-resync 31/31** [verified].

**Is the original the guard's true parent?** In 7 of the 11 files where they differ, the guard keeps 100% of
the original's added lines and adds more [verified] — it extends the original. The exceptions are small:
`CHANGELOG.md` (guard keeps 3 of the original's 9 added lines — the original's entry was written after the
guard was copied) and ~4% of lines in `src/main.rs`, `docs/guides/deployment-endpoints.md` and
`tests/replication_listener_startup.rs`. Treating `2f161df` as the guard's parent is sound; those few lines
surface as ordinary merge differences to review, which is the correct outcome.

## 2. Trial merges

With the ancestry encoded as real parents, git does the work [verified]:

| Merge | Merge base | Result |
| --- | --- | --- |
| admission + physical-resync | guard | **clean — zero conflicts** |
| admission + native-auth | original | 3 files |
| physical-resync + native-auth | original | 3 files |
| **(admission + physical-resync) + native-auth** | original | **3 files, 5 conflict blocks** |

The handoff's warning that 22 core files are touched by all three candidates was the overlap of the
**guard copy carried inside two of them**. Against the right merge base it cancels completely.

## 3. Required changes — blocking for acceptance

A clean textual merge is not a correct program. These are mandatory in the integration itself:

### R1. Wire protocol must become version 3 — silent semantic conflict [verified]

| | `PROTOCOL_VERSION` | `HandshakeRequest` change |
| --- | --- | --- |
| base / original | 1 | — |
| guard | **2** | `expected_history: Option<Uuid>` added **first** |
| native-auth | **2** | `auth: Option<ReplicationAuth>` added **last** |
| merged | **2** | **both** — a third layout |

Both lineages made the identical textual edit `1 → 2`, so git merges that line with **no conflict**; the only
marker is on the doc comment above it. bincode encodes struct fields **by position with no tags**, so an
auth-only v2 peer receiving a merged handshake decodes `expected_history`'s bytes as `node_id`: a **silent
mis-decode**, not a refusal — and the explicit version check native-auth added precisely to prevent this
("any mismatch is refused explicitly rather than guessed") would *pass*, because the numbers match.

**Required:** `PROTOCOL_VERSION = 3`; doc comment states v3 = guard identity fields + auth field; test that
v1, guard-v2 and auth-v2 peers are each refused before decode. No candidate binary was ever released, so
there is no deployed v2 to stay compatible with.

### R2. Seven silent compile breaks [static]

Each lineage wrote struct literals against its own version of a struct; the merge combined the struct
definitions without conflict. Missing field ⇒ E0063.

| Location | Struct | Missing | Code |
| --- | --- | --- | --- |
| `src/replication/streaming.rs:309` | `HandshakeResponse` | `history_id` | **production** (version-mismatch rejection) |
| `src/replication/streaming.rs:361` | `HandshakeResponse` | `history_id` | **production** (auth rejection) |
| `src/replication/transport.rs:1393` | `HandshakeRequest` | `auth` | test (`mod tests`, guard) |
| `tests/replication_history_transport.rs:28` | `StreamingClientConfig` | `auth` | test |
| `tests/replication_history_transport.rs:39` | `HandshakeRequest` | `auth` | test |
| `tests/native_replication_auth.rs:66` | `HandshakeRequest` | `expected_history` | test |
| `tests/native_replication_auth.rs:334` | `HandshakeResponse` | `history_id` | test |

Line numbers are in the merged tree before conflict resolution. `failover_watcher.rs:424` also shows as
incomplete but only because it still holds conflict markers; §4 resolves it. **The two production rejections
must carry `history_id: Uuid::nil()`, not the real id** — see R5.

### R3. Native auth makes every failover watcher report a healthy primary as failed [static]

`FailoverWatcher::do_health_check` sends `auth: None` ("Health probes are unauthenticated liveness/failover
discovery"), but `connection_loop` has **no probe exemption**: with `required_auth` set it answers
`accepted: false`. The chain:

1. probe rejected → `do_health_check` returns `Err(Failover)` → classified `NodeHealth::Failed`;
2. every probe fires `PrimaryUnhealthy` and a warning;
3. at `failover_threshold` consecutive failures `initiate_failover` runs. `send_promote_request` is a
   **pre-existing stub** (base `9161383` lines 680/806) that only checks TCP connectivity, yet logs
   **"Standby … promoted successfully"** and emits a promotion event.

No real split brain today — only because promotion is a stub. But an auth-enabled cluster gets continuous
false alarms and then a **false promotion event**; any consumer that reroutes on it routes writes to a
standby, the hazard this campaign forbids. native-auth's 9 auth tests never ran a watcher against an
auth-enabled primary.

**Required:** the probe presents the configured credential, exactly like a standby. Not a server-side probe
exemption — that would add an unauthenticated surface. Remove the misleading comment. Add a test: watcher
against an auth-enabled primary reports `Healthy`, and against a wrong-token primary does not trigger
failover silently.

### R4. The FIPS build compares the token in non-constant time [verified]

```rust
#[cfg(feature = "ring-crypto")]      pub fn constant_time_eq(a, b) -> bool { ring::…::verify_slices_are_equal(a, b).is_ok() }
#[cfg(not(feature = "ring-crypto"))] pub fn constant_time_eq(a, b) -> bool { a == b }
```

The documented FIPS build is `--no-default-features --features fips,encryption,vector-search`, and
`fips = ["dep:aws-lc-rs"]` does not enable `ring-crypto`. So the deployment that chose the hardened build gets
`a == b`, which returns at the first differing byte — a network timing side-channel on the replication
token. This regression was introduced by review: native-auth originally had a hand-written constant-time
compare, and the Astra response replaced it with `ring` behind a feature gate.

**Required:** constant-time in **every** feature combination, independent of crypto feature selection.
**Resolved by R6:** the native channel adopts `TransferToken::accepts` (`subtle`, fixed 32 bytes) and
`constant_time_eq` is deleted, so no build has an `a == b` path. The requirement still stands as a check:
`grep` the integrated tree for any token comparison not routed through `TransferToken::accepts`, and test
the native channel under `--no-default-features --features fips,encryption,vector-search`.

### R5. Rejections disclose server state to unauthenticated peers [static]

Both pre-acceptance rejection responses (protocol mismatch, which runs before auth; auth failure) send
`server_node_id`, `primary_lsn` (current write position) and `fencing_token` to a peer that has proven
nothing. **Required:** rejection responses carry nil/zero values for all of them, plus `history_id:
Uuid::nil()` (R2). Only `accepted: false` and the error text are needed. The order inside `connection_loop`
is already correct after the merge [verified]: auth (≈347) → history check (≈389) → registration (≈418), so
an unauthenticated peer cannot probe the dataset identity through the history path.

### R6. One replication secret, on the physical model — DECIDED 2026-10-02

| | physical-resync | native-auth (as handed over) |
| --- | --- | --- |
| Config | `[replication.physical] token_file` | `[replication] auth_token`, or env `HELIOSDB_REPLICATION_AUTH_TOKEN` |
| Secret | "Exactly 32 raw random bytes in an owner-only regular file; **never inline TOML**" | any non-empty string (`"password"` passes) |
| Exposure | file, permission-checked | config files (often committed), `/proc/<pid>/environ`, `docker inspect` |

**Decision (owner, 2026-10-02): one secret for both channels, on the physical model.** physical-resync
already has the right primitives, so this is reuse, not new design:

- `physical_wire::TransferToken` — `Zeroizing<[u8; 32]>`, refuses an all-zero token, and `accepts()` is a
  length check plus `subtle::ConstantTimeEq`: constant-time in **every** feature combination, no `ring`.
- `read_physical_token` (`src/main.rs`) — `O_NOFOLLOW`, owner uid, mode `0600` or stricter, single hard
  link, exactly 32 bytes, zeroized read buffer, size-change check; refuses non-Unix.

Required shape:

1. **Config:** one `[replication] token_file` (path) serves both channels; CLI
   `--replication-token-file`. It replaces `[replication.physical] token_file` /
   `--physical-replication-token-file` and native-auth's `auth_token`. Nothing was released, so no aliases
   or deprecation path.
2. **Removed:** `ReplicationConfig.auth_token`, the `HELIOSDB_REPLICATION_AUTH_TOKEN` override, its
   empty-string validation, `ReplicationAuth::SharedSecret { token: String }`, and `constant_time_eq` with
   both of its arms (the `ring` one and the `a == b` one). No secret is ever read from TOML or the environment.
3. **Loader:** move `read_physical_token` from `src/main.rs` into the library (e.g. `replication::token`),
   rename `read_replication_token`, keep every check and error message intent unchanged; both channels and
   the tests call the one function.
4. **Native handshake:** `HandshakeRequest.auth` carries the 32 raw token bytes (part of the v3 layout, R1);
   the server verifies with `TransferToken::accepts`. Missing vs wrong remain distinguished in the error
   text and both are rejected before registration, exactly as today.
5. **Health probe (R3):** presents the same token.
6. **Debug/logging:** `TransferToken` and anything holding it must not print the bytes — confirm its `Debug`
   is redacting or absent, and keep native-auth's redaction test equivalent.
7. **Docs:** one procedure for both nodes — `head -c 32 /dev/urandom > replication.token && chmod 600
   replication.token`, the identical file on primary and standby. Update `config.example.toml`,
   `docs/guides/deployment-endpoints.md` and the live harness (`native_auth_live.py` moves from the env var
   to a token file).
8. **Tests:** valid / wrong / missing token on the native channel; the physical channel's existing token
   tests unchanged in substance; a token-file permission test (mode `0644` and a symlink are each refused);
   the same token file authenticates both channels.

One consequence to keep in mind: the token is still a **bearer** secret sent over a plaintext channel, so the
plaintext-remote opt-in (§5.1) still matters, and a leak compromises both channels at once — intended, since
they serve one replication relationship. A challenge-response (HMAC over a server nonce) would stop the token
from ever crossing the wire; it fits naturally on a fixed 32-byte key and is a recommended follow-up alongside
TLS, **not** part of this integration.

## 4. Conflict resolutions (5 blocks, all mechanical)

| File | Block | Resolution |
| --- | --- | --- |
| `src/replication/transport.rs` | doc comment above `PROTOCOL_VERSION` | replace both with the v3 comment (R1) |
| `src/replication/failover_watcher.rs` | probe `HandshakeRequest` literal | keep `expected_history: None` **and** `auth: <configured credential>` (R3) |
| `src/main.rs` #1 | config wiring | keep `physical.apply(&mut replication.physical)`; **drop** native-auth's `HELIOSDB_REPLICATION_AUTH_TOKEN` override (R6) |
| `src/main.rs` #2 | `use replication::{…}` | union: `StreamingClientStatus` **and** `transport::ReplicationAuth` |
| `src/main.rs` #3 | streaming server construction | native-auth's loopback/plaintext guard, **then** the guard's fallible 4-argument `StreamingServer::new(server_config, node_id, history_id, wal_store).map_err(…)?`; confirm `server_config.auth` is still set |

Also merged without a marker but touched by both lineages — review in the diff, not just at markers:
`CHANGELOG.md`, `src/config.rs` (merged correctly: both `physical` and `auth_token` present and both
validated [verified]), `src/replication/streaming.rs` (R2, R5), `tests/ha_tests/cluster_tests.rs`,
`tests/ha_tests/streaming_tests.rs`, `tests/replication_listener_startup.rs`.

## 5. The handoff's open native-auth questions — verdicts

1. **Plaintext remote opt-in** (`HELIOSDB_REPLICATION_AUTH_ALLOW_REMOTE=1`). **Accept for this campaign** as a
   fail-closed stopgap: non-loopback is refused by default and the bypass is an explicit acknowledgment.
   Two conditions: log a startup WARNING whenever it is active, and name it consistently with issue 8's
   `--allow-insecure-trust` so insecure opt-ins share one recognisable shape. **Follow-up (file, not
   blocking):** put the native replication channel on TLS — Nano already ships TLS and post-quantum hybrid
   TLS for the PostgreSQL and MySQL wires (v4.36.0), so the infrastructure exists. A bearer token in clear
   on an untunneled network is replayable by anyone who sees it.
2. **No-crypto comparison fallback.** **Reject as shipped** — it is R4, resolved by R6's move to
   `TransferToken::accepts`.
3. **Wire-version compatibility with history-guard protocol 2.** **Answered by R1** — they are incompatible
   under the same number; the integrated build must be v3.
4. **Fixture task ownership.** The Astra response records `stop()` aborting and awaiting on timeout and
   panicking on unjoined tasks. Verify at the gate (§6) rather than by reading; no design objection.

## 6. Gates for the integrated candidate

None of the candidates' own gate results apply to the integrated build — the handoff is explicit about that,
and R1–R5 change shipped code. The integrated binary needs the full set, on its own exact hash:

- **Correctness:** `cargo check --all-targets` (confirms R2); lib; full default suite **and** `internal-tests`,
  `--no-fail-fast`, with the §3b empty-suite check; doc tests; the FIPS feature combination at least to
  `cargo test` on the replication targets (R4).
- **Static:** fmt; clippy set-diff against `9161383` — the 28 recorded findings (native 1, physical 27) are
  fixed or individually waived in writing, not carried silently; `cargo deny`.
- **Live scenarios on the exact binary:** native-auth valid / wrong / missing / empty; guard
  primary-replacement containment; admission simple/COPY/extended matrix; raw-resync CLI fixture
  (NON-SERVING); **new:** failover watcher against an auth-enabled primary stays `Healthy` (R3); **new:**
  protocol mismatch matrix — v1, guard-v2 and auth-v2 peers each refused before decode (R1).
- **Performance:** against `9161383` on the same host, interleaved, `load1 < 6`. pg35 is judged by
  **category flips**, not by an absolute 35-0: `9161383` already shows ALTER TABLE and 4-table JOIN as
  PostgreSQL wins. Before attributing any delta to the patch, check whether the changed code is reachable on
  that workload's hot path.

## 7. Unchanged boundaries

- Issue 7: raw resync stays **NON-SERVING**, `resync_certified=false`. Online SQL serving, source durable-cut
  certification and generation/session wiring remain unfinished and are **not** part of this integration.
- Promotion stays **unmet** until separately designed, fenced and runtime-verified. R3 does not change that;
  it only stops a false promotion signal.
- **Never route writes to a standby.**

## 8. Pre-existing defects found — file separately, not blocking

- `send_promote_request` is a stub that logs "Standby … promoted successfully" and emits a promotion event
  after only a TCP connect (base `9161383`, `failover_watcher.rs` 680/806). A log that claims an action that
  did not happen.
- A failover health probe performs a full handshake, so an accepted probe is **registered as a connected
  standby** before it disconnects (base `9161383`, `streaming.rs` "Register standby"). Probes should not
  appear in standby counts.
- TLS for the native replication channel (§5.1).

## 9. Implementation steps (DeepSeek, under this review)

All heavy steps under `flock /home/gpc/HDB/sprint/coordination/build.lock` + `systemd-run --user --scope
--collect -p MemoryMax=24G -p MemorySwapMax=0`, `CARGO_BUILD_JOBS=2`, own target directory; clean the local
crate (`cargo clean --locked --package heliosdb-nano`) before the first build so no stale binary is reused.

0. **Lineage scaffold** (pure history, no code change): new branch from `2f161df`. Commit the guard as its own
   reviewable commit — tracked diff of `nano-history-guard-20260929` against `9161383` plus its 19 untracked
   source files, excluding `docs/internal/` and build output; verify it reproduces admission's 29 and
   physical-resync's 31 inherited files byte-for-byte (§1). Then a commit whose tree is the applied
   `admission.patch` (parent: guard), another for `physical-resync.patch` (parent: guard), and one for
   `native-auth.patch` (parent: `2f161df`).
1. Merge admission + physical-resync. Expect **no conflicts**; stop and report if there are any.
2. Merge native-auth. Expect exactly the **5 blocks in §4**; resolve as specified. Any other conflict: stop and
   report.
3. Apply R1, R2, R5, R6 (which also resolves R4) and R3, in that order — R6 changes the handshake's
   `auth` field and the probe's credential, so R3 is written against R6's token type.
4. `cargo check --all-targets`; report every error with file:line before fixing.
5. Hand back for review **before** running the full gate. Do not edit sources while any gate runs.
6. After review: the §6 gates, recorded per issue in Sprinter by appending (never replacing notes).

Expected delivery is a reviewable branch, not a merge into `main`.
