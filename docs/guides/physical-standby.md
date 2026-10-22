# Physical standby

A HeliosDB-Nano standby is a **physical replica** of its primary: a read-only copy
of the primary's RocksDB keyspace, kept current by applying the primary's own
write groups, byte for byte. It serves read-only SQL on the PostgreSQL wire while
it applies, reports exactly how current it is, and stays up when its primary
cannot serve it.

This guide covers starting a pair, remote standbys over TLS, readiness, the
PostgreSQL-wire contract that HeliosDB Proxy relies on, the replication
connection, synchronous replication, the archived WAL and its recovery window,
rebuilding a standby (and resuming an interrupted copy), and the limits of this
release. The design is `docs/plans/PHYSICAL_STANDBY_REPLICATION.md`; the
one-connection protocol is spec P2.1
(`docs/plans/specs/P2_1_single_connection_control_plane.md`), TLS and the
resumable copy are spec P2.2 (`docs/plans/specs/P2_2_tls_remote_resumable.md`),
the archive is spec P3.1
(`docs/plans/specs/P3_1_archive_segments_recovery_window.md`).

## What a physical standby is

- **1:1 with the primary.** Every write on the primary is a RocksDB write group:
  one WAL record that holds the whole group, never split. The standby receives
  those records and applies each one verbatim, so its replicated keyspace at
  physical sequence `T` is identical to the primary's at `T`. Nothing is
  re-encoded: rows, catalog, counters, sequences, branches, vector index keys
  and encrypted values arrive exactly as the primary wrote them. That is why a
  physical standby has none of the coverage gaps of the former logical standby,
  which replayed a separate log of row changes.
- **Read-only.** The standby's storage engine opens in a mode whose write gate
  refuses every write the engine would otherwise make on its own (counter
  reseeds, snapshot registration, garbage collection, close-time flushes). The
  replication applier is the only writer. User writes, DDL and `nextval()` are
  refused with SQLSTATE `25006`. A write that reached the store some other way
  would poison it, and the standby would stop in `needs_recovery` rather than
  serve a copy that differs from the primary.
- **Current derived state.** After each applied write group the standby updates
  its in-memory structures (primary-key and unique indexes, vector indexes, the
  time-travel snapshot map, schema and plan caches), so `COUNT(*)`, index
  lookups, kNN searches and `AS OF` queries answer as the primary would at the
  same sequence.
- **Its own node state stays separate.** The standby keeps its cursor and
  posture in a node-local column family that is never part of the replicated
  keyspace.

## Vocabulary: Oracle terms in HeliosDB

Adapted from `docs/plans/STANDBY_RECOVERY_PLAN.md` §1 to what this release
ships. "P3", "P4" and "P5" are phases of the physical standby plan.

| Oracle | HeliosDB-Nano |
|---|---|
| Physical standby database | `--replication-role standby`: a physical replica of one primary |
| Active Data Guard (open read-only while applying redo) | Always: the standby serves read-only SQL while it applies. Its posture (below) says whether data reads are allowed |
| Standby `MOUNTED`, not open | No equivalent: a standby that may not serve data reads stays open and refuses them with SQLSTATE `55000` |
| SCN | The **physical sequence**: the RocksDB sequence number of the primary's writes. Shown as PostgreSQL LSN text, `HI/LO` hex (`hi = seq >> 32`, `lo = seq & 0xFFFFFFFF`) |
| Online redo | The RocksDB WAL (`*.log`) |
| Redo transport | The replication connection (`NPHYS002`) to the primary's `replication_port`, over TLS for a remote standby. The standby pulls write groups |
| Redo apply (MRP) | The physical applier: each write group is applied verbatim and fsynced, then the standby's in-memory indexes are updated |
| Archived redo log | An **archived WAL segment**: `<archive>/<history>/<epoch>/<first>-<last>.hseg`, a sealed, checksummed file of whole write groups (see [Archived WAL and the recovery window](#archived-wal-and-the-recovery-window)). The primary's own RocksDB WAL retention (`[replication.physical] wal_retention_secs`, `wal_retention_size_mb`) only covers the archiver's lag and a reconnecting standby |
| `LOG_ARCHIVE_DEST_n` | `[replication.archive] dir` (`--archive-dir`, default `<data dir>.archive`). Every primary serving its replication port and every standby archives; `--archive=false` turns it off |
| RMAN catalog, `V$ARCHIVED_LOG` | The `heliosdb_archived_wal` view (and `heliosdb-nano archive list`): one row per segment file. No catalog database: the files describe themselves |
| `V$ARCHIVE_GAP` | The `heliosdb_archive_gaps` view, `heliosdb-nano archive gaps` |
| `RMAN> CROSSCHECK`, `VALIDATE ARCHIVELOG` | `heliosdb-nano archive verify` (header and footer), `archive verify --deep` (every byte and every frame) |
| `DELETE OBSOLETE` | The pruner of a running node; `heliosdb-nano archive prune` offline |
| FAL (gap fetch) | None. A standby whose next sequence the primary no longer retains is refused (`WalPruned`) and is rebuilt; recovering it from archived segments arrives in P3.2 |
| `RECOVERY WINDOW OF n DAYS` | `[replication.archive] recovery_window` (default `"7d"`); `heliosdb_recovery_window` shows the configured and the effective window |
| `DB_RECOVERY_FILE_DEST_SIZE` | `[replication.archive] max_bytes` (default `"unlimited"`) and `disk_reserve` (default `"10%"` of the filesystem). Unlike Oracle, a full archive never stops the primary: it shortens the window |
| `DUPLICATE … FOR STANDBY FROM ACTIVE DATABASE` | The **bootstrap copy**: a standby started on an empty directory downloads a verified checkpoint of the primary, then follows it. An interrupted copy resumes (like RMAN's restartable duplicate) |
| `RECOVER STANDBY DATABASE FROM SERVICE` | Not yet (P4, incremental rebuild). Today a rebuild is a full bootstrap copy into a new directory |
| Managed recovery from archived logs | Not yet (P3.2: `standby recover --from-archive`, archive-only standbys) |
| Datafile block, block SCN, block change tracking | None: one key-value space with no per-key sequence. P4 compares key ranges by digest |
| Incarnation, `RESETLOGS` | The dataset **history id** (`NANO-PRIMARY-HISTORY.json`) plus the **epoch** (`timeline_id`, 1 until promotion arrives in P5) |
| Protection modes | `[replication] sync_mode`: `async` is Maximum Performance; `semi-sync` with `sync_timeout_action = "warn"` behaves like Maximum Availability. Maximum Protection has no equivalent (`fail` reports an error but never stops the primary) |
| `NET_TIMEOUT` | `[replication] sync_timeout_ms` |
| Switchover, failover | Refused until P5 |
| `V$DATAGUARD_STATS`, `V$MANAGED_STANDBY` | `/health`, `/ready`, the `heliosdb_standby_status` view; on a primary, the `standbys` list of `/health` and the `pg_replication_standbys` view |

## Start a pair

Both nodes need the same replication token: one owner-only file of exactly 32
random bytes. The token bytes are never read from TOML or the environment, only
the file's path.

```bash
head -c 32 /dev/urandom > /etc/heliosdb/replication.token
chmod 600 /etc/heliosdb/replication.token
# The data directories' parent: owned by the user that runs Nano, owner-only.
install -d -m 700 /srv/nano
```

The primary serves its standbys on **one** port, its `--replication-port`
(default 5433). The standby's `--primary-host` names that port, never the
primary's PostgreSQL or HTTP port.

```bash
# Primary: PostgreSQL 5432, replication 5433, HTTP 8080
heliosdb-nano start --data-dir /srv/nano/primary --listen 127.0.0.1 --port 5432 \
  --replication-role primary --replication-port 5433 \
  --replication-token-file /etc/heliosdb/replication.token --http-port 8080

# Standby: PostgreSQL 5442, HTTP 8081, following the primary's replication port
heliosdb-nano start --data-dir /srv/nano/standby --listen 127.0.0.1 --port 5442 \
  --replication-role standby --primary-host 127.0.0.1:5433 \
  --replication-token-file /etc/heliosdb/replication.token --http-port 8081
```

The equivalent configuration (CLI flags override file values, which override
defaults; `config.example.toml` documents every key):

```toml
# primary
[replication]
role = "primary"
replication_port = 5433
token_file = "/etc/heliosdb/replication.token"
sync_mode = "async"          # async | semi-sync | sync

# standby
[replication]
role = "standby"
primary_host = "127.0.0.1:5433"
token_file = "/etc/heliosdb/replication.token"
max_standby_lag = "30s"
on_unrecoverable = "stay-up" # stay-up | exit
```

**The primary.** A persistent primary with a replication token serves its
replication port on its own; `--physical-replication=false` turns it off, and
with it every standby. An in-memory primary, or one without a token, serves no
replication port and says so once at startup. The port binds on `--listen`'s
address when that is a loopback address, or when it serves TLS
(`--replication-tls`, see "Remote standbys: TLS"); otherwise on `127.0.0.1`,
whatever `--listen` opens, and a log line says so: a plaintext token never
crosses the network unless `--allow-insecure-replication-remote` says it may.
The primary checkpoints into `[replication.physical] export_dir` to serve a
bootstrap copy; when none is set it is `<data dir>.physical-export` beside the
data directory (same filesystem, so a checkpoint is hard links), created mode
`0700`. `[replication.physical] max_sessions` (default 2) bounds concurrent
replication sessions, bootstrap copies included.

**The standby.** What it does depends on its data directory:

| Data directory | What the standby does |
|---|---|
| absent, or an existing empty directory | bootstraps: downloads a verified copy of the primary, renames it into place, then follows |
| an installed physical standby | resumes at its durable cursor; no second copy is made |
| a 4.41.x logical standby | refused (see "Converting a 4.41.x logical standby") |
| a primary's data directory | refused: a standby is never built over a primary's data |
| any other non-empty directory | refused, and left untouched |

A refused directory is never repaired, reset or deleted. The parent of the
standby's data directory must be owned by the user running Nano and not group-
or world-writable, because the copy is renamed into place there (so `/tmp`
itself will not do; use an owner-only directory inside it). The primary refuses
to serve from a group- or world-writable data directory for the same reason.
If the primary encrypts at rest, start the standby with the same dataset key:
it holds the primary's ciphertext unchanged (see "Encryption keys" under the
limits). A `--primary-host` on another host is reached over TLS (see "Remote
standbys: TLS"). `--replication-port` is unused on a standby. A standby started
before its primary is up and not ready, and bootstraps once the primary
appears.

**Ports.** Every node on one host needs its own PostgreSQL `--port`, HTTP
`--http-port` (`0` disables HTTP), and, on the primary, `--replication-port`.
A standby accepts no MySQL listener (`--mysql` and `--mysql-socket` are refused
before storage opens), and its HTTP listener mounts only `/`, `/health`,
`/ready` and `/version`.

## Remote standbys: TLS

A standby on another host reaches its primary's replication port over TLS. TLS
is the transport only: the replication token is still required, and checked,
in every mode, mutual TLS included. The protocol is unchanged (`NPHYS002`
inside the TLS stream; each side sets the `CAP_TLS` capability bit for its end
of the session).

```bash
# Primary on 10.0.0.5: the replication port binds 10.0.0.5:5433, over TLS.
heliosdb-nano start --data-dir /srv/nano/primary --listen 10.0.0.5 --port 5432 \
  --replication-role primary --replication-port 5433 \
  --replication-token-file /etc/heliosdb/replication.token \
  --replication-tls --replication-tls-cert /etc/heliosdb/repl.crt --replication-tls-key /etc/heliosdb/repl.key

# Standby on another host: TLS is the default for a non-loopback primary.
heliosdb-nano start --data-dir /srv/nano/standby --listen 10.0.0.6 --port 5432 \
  --replication-role standby --primary-host 10.0.0.5:5433 --primary-ca /etc/heliosdb/repl-ca.crt \
  --replication-token-file /etc/heliosdb/replication.token
```

Every key is a flat `[replication]` key with a CLI flag (the flag wins):

| `[replication]` key | Flag | Side | Meaning |
|---|---|---|---|
| `tls_enabled` | `--replication-tls` | primary | serve the replication port over TLS; it then binds on `--listen`'s address even when that is not loopback |
| `tls_cert_path`, `tls_key_path` | `--replication-tls-cert`, `--replication-tls-key` | primary | PEM certificate chain and key; when unset, the PostgreSQL pair (`--tls-cert` / `--tls-key`, or `[server] tls_cert_path` / `tls_key_path` with `[server] tls_enabled`); with neither the primary refuses to start, naming the keys |
| `tls_ca_path` | `--replication-tls-ca` | primary | mutual TLS: every standby must present a client certificate issued by this CA (and its token) |
| `tls_post_quantum` | `--replication-tls-post-quantum[=BOOL]` | primary | offer the X25519MLKEM768 hybrid key exchange; default: follows `--tls-post-quantum` |
| `primary_tls` | `--primary-tls[=BOOL]` | standby | connect over TLS; default **on when `--primary-host` resolves to a non-loopback address**, off on loopback |
| `primary_tls_ca_path` | `--primary-ca` | standby | the CA the primary's certificate must chain to; default: the system trust store (`SSL_CERT_FILE` or the distribution's CA bundle) |
| `primary_tls_server_name` | `--primary-server-name` | standby | the name the primary's certificate must carry; default: the host part of `--primary-host`. An IP literal is verified as an IP subject alternative name |
| `primary_tls_client_cert_path`, `primary_tls_client_key_path` | `--primary-client-cert`, `--primary-client-key` | standby | the client certificate for a mutual-TLS primary |
| `primary_tls_insecure_skip_verify` | `--primary-tls-insecure-skip-verify` | standby | accept any certificate (no chain or name check); only together with `allow_insecure_remote`, warned at start |
| `allow_insecure_remote` | `--allow-insecure-replication-remote` | both | plaintext over a non-loopback network, token included: isolated development only; an `INSECURE DEVELOPMENT OVERRIDE` warning is printed |
| `snapshot_export_lease_secs` | `--snapshot-export-lease-secs` | primary | how long a bootstrap copy's export outlives its session (see "Resuming an interrupted copy"); default 3600 |
| `snapshot_export_max_leases` | `--snapshot-export-max-leases` | primary | at most this many leased exports (1..=64, default 4; the oldest is dropped first) |

The rules:

- **The listener.** A plaintext replication port binds on loopback (on
  `--listen`'s address when that is loopback, else `127.0.0.1`). It binds on a
  non-loopback `--listen` address only with `--replication-tls`, or with
  `--allow-insecure-replication-remote`. Peers from any address are then
  accepted; each still authenticates with the token.
- **The standby.** A `--primary-host` that resolves to a non-loopback address
  is reached over TLS (the default there), or in plaintext only with
  `--allow-insecure-replication-remote`; `--primary-tls=false` without the
  opt-in is refused at start. Loopback stays plaintext unless `--primary-tls`
  asks for TLS.
- **Certificate failures are transport failures.** A wrong CA, an untrusted
  certificate, a name mismatch, or a mutual-TLS primary that refuses the
  standby's certificate leave the standby `connecting` / `reconnecting` (never
  `needs_recovery`: the data is fine, the network is not) with the reason on
  `/health` (`"reason": "tls: invalid peer certificate: UnknownIssuer"`), and
  it retries with the usual backoff. The primary logs every failed handshake
  with the peer and counts it; no `Refuse` frame can be sent before a
  handshake.
- **Rotation.** A standby reads its CA and client certificate again at every
  connection attempt: replace the files and the next reconnect uses them. A
  primary loads its certificate at start; restart it to rotate.
- **Reporting.** `/health`, `/ready` and `heliosdb_standby_status` carry
  `"tls": true|false` on both roles: on a standby, whether its current session
  is on TLS (both ends said so); on a primary, whether its replication port
  serves TLS.

The PostgreSQL wire is unaffected: PG-wire TLS (`--tls-cert`) stays optional and
independent, and carries no replication token.

## Readiness

A standby's **posture** says what it is doing. One snapshot of it feeds `/health`,
`/ready`, the `heliosdb_standby_status` view and the PostgreSQL-wire read guard,
so those surfaces can never disagree.

| Posture | Entered when | Data reads | `/ready` |
|---|---|---|---|
| `connecting` | no session has applied anything since the store was attached | refused, `55000` | 503 |
| `recovering` | a bootstrap copy is downloading (`progress`: bytes held / total) | refused, `55000` | 503 |
| `catching_up` | the first session has not yet reached its connect barrier (the primary's written sequence when the session began) | refused, `55000` | 503 |
| `streaming` | the session's cursor reached that barrier; kept while the session lives | served | 200 |
| `reconnecting` | a streaming session was lost; data is current as of the cursor | served for at most `max_standby_lag` after the loss, then refused | 200, then 503 |
| `needs_recovery` | a refusal the standby cannot resolve by itself (below); sticky until restart | refused, `55000` naming the reason | 503 |

- **`GET /ready`** answers 200 if and only if the node is ready (a primary is
  always ready), else 503. The body is the replication block below. `/ready` and
  `/health` stay public when the HTTP API requires credentials.
- **`GET /health`** is liveness: always 200 while the process runs, with the
  replication block:

  ```json
  {"status":"ok","replication":{"role":"standby","state":"streaming","ready":true,
   "cursor":"0/1F4","primary_cursor":"0/1F4","lag_bytes":0,"lag_secs":0.4,
   "primary":"127.0.0.1:5433","sync_mode":"async","last_ack_sequence":500,"epoch":1,
   "tls":false,"reason":null,"progress":null}}
  ```

  The first eleven fields are frozen for HeliosDB Proxy (names and types).
  Spec P2.2 added three after them: `tls` (the replication session is on TLS),
  `reason` (why a standby is not streaming: the reason of a `needs_recovery`,
  or the last connection failure while `connecting` / `reconnecting`, such as
  `tls: invalid peer certificate: UnknownIssuer`; null otherwise) and
  `progress` (a `recovering` standby's download, `"<held>/<total> bytes"`). The
  `heliosdb_standby_status` view has the same three columns after the frozen
  ones.

  `cursor` is the sequence the standby has applied through and `primary_cursor`
  the primary's last reported barrier, both as LSN text. `lag_bytes` is their
  difference: write operations behind, not bytes. `lag_secs` is the time since
  the standby last confirmed it held everything its primary had. It is not the
  time since the last apply, which would grow forever on a current standby of
  an idle primary. `last_ack_sequence` is the durable sequence the standby last
  acknowledged to its primary. `epoch` is 1 until promotion arrives (P5). A
  primary reports the same block with `role = "primary"` and `ready = true`,
  plus its standbys (see "A primary's /health").
- **`[replication] max_standby_lag`** (default `"30s"`) is how long a standby
  that lost its primary still reports itself ready and serves data reads.
- **`[replication] on_unrecoverable`**: `"stay-up"` (default) keeps a refused
  standby up in `needs_recovery`, so a supervisor does not crash-loop it and
  status stays readable; `"exit"` exits instead, for supervisors that alert on
  exit.
- **`[replication] startup_timeout_secs`** (1..=3600, default 30;
  `--replication-startup-timeout-secs` overrides it) is how long a standby's
  listeners wait for it to stream before they open anyway, not ready. A daemon
  parent allows this budget plus its usual 5-second startup allowance.

## The PostgreSQL-wire contract (HeliosDB Proxy)

HeliosDB Proxy probes nodes over the PostgreSQL wire only; it never fetches
`/ready`. Each query below returns exactly one row with one column.

| Query | Primary | Standby |
|---|---|---|
| `SELECT heliosdb_require_ready()` | `t` | `t` when ready; otherwise ERROR, SQLSTATE `55000`, `standby is not ready: <state> (cursor X, primary Y)` plus the reason of a `needs_recovery` |
| `SELECT pg_is_in_recovery()` (also `::text`) | `f` | `t`, in every posture |
| `SELECT pg_current_wal_lsn()::text` | its written physical sequence, `HI/LO` hex | the same as the replay LSN |
| `SELECT pg_last_wal_replay_lsn()::text` | NULL | its applied physical sequence, `HI/LO` hex |
| `SELECT timeline_id::bigint FROM pg_control_checkpoint()` | `1` | `1` (the epoch, until P5) |

- ParameterStatus `in_hot_standby` is `on` on standby sessions and `off` on a
  primary. `server_version` is unchanged.
- Because LSNs are physical sequences, Proxy's `lag_bytes` (primary LSN minus
  standby LSN) counts write operations behind. At a load of N writes per second,
  `max_lag_bytes = N * 30` allows about 30 seconds of lag. Nano's own
  `max_standby_lag` (seconds) is the readiness gate; the two are independent.
- Statement refusals are severity ERROR: `55000` for a data read on a standby
  that is not ready, `25006` for a write or `nextval()` on any standby. The
  readiness and status functions, `SET`, `SHOW`, `BEGIN`/`COMMIT`, `SELECT 1`
  and the system views answer in every posture.
- Logins are never refused for readiness, and the replication changes in this
  release do not touch this surface: as in P1, Proxy's health and topology
  logins (user `postgres`, application names `heliosdb-proxy-health` and
  `helios-topology`) are accepted in every posture, SSLRequest is always
  answered, and readiness is never a startup ErrorResponse. The node's
  authentication method applies as usual; PostgreSQL-wire TLS stays optional and
  carries no replication token.

## The replication connection

One port, one connection per standby, one protocol: `NPHYS002`. That connection
carries the bootstrap copy, the physical WAL, and the control plane (identity,
dataset history, epoch, heartbeats and acknowledgements). Until this release a
standby ran a second, native channel for its handshake and acknowledgements. Two
channels could disagree: a history mismatch only the native channel could see, a
refusal on the data channel that looked like a dropped socket and was retried
forever, and acknowledgements that named a logical position while the data moved
by physical sequence. On one connection every frame of a session is ordered and
names physical sequences only, and a primary that will not serve a standby says
why.

A frame is a 16-byte header, `MAGIC "NPHYS002" (8) | version u16 = 2 | kind u16 |
length u32`, big-endian, then a payload whose size every kind bounds before it is
allocated. The session is request/response, driven by the standby (R, the
receiver) against the primary (S, the source):

| Frame | Direction | Payload |
|---|---|---|
| `Auth` | R to S | the 32-byte token, checked in constant time before anything else is read |
| `AuthOk` | S to R | none |
| `Hello` | R to S | node id; role `standby` or `resync`; history (none while bootstrapping); epoch; durable cursor (none for a fresh snapshot); capabilities (`CAP_TLS` when the standby's end is TLS); PostgreSQL port; sync mode |
| `HelloOk` | S to R | node id, history, epoch, barrier (the primary's written sequence), capabilities (`CAP_TLS` when the primary's end is TLS), replication endpoint, PostgreSQL port |
| `Refuse` | S to R | a code and a reason; the session ends |
| `Open` | R to S | `snapshot`, `snapshot of <id>` (a resumed download: the leased export it belongs to, else a fresh one), or `resume {history, after}` |
| `Manifest` / `ResumeInfo` | S to R | the session the primary opened |
| `ChunkRequest` / `Chunk` | R to S / S to R | one bounded, digested piece of a checkpoint file |
| `PollRequest` | R to S | session, the sequence to continue after, a byte limit |
| `PollStart`, `Batch`*, `PollEnd` | S to R | a contiguous interval of write groups |
| `Heartbeat` | R to S | applied sequence, durable sequence, received-at time. This is **the acknowledgement**: one frame, two triggers (every 5 s when idle, and after a poll advanced the durable cursor, coalesced to at most one per 50 ms) |
| `HeartbeatAck` | S to R | the primary's barrier and clock |
| `Fence` | either | the epoch; reserved for promotion (P5) and refused by both sides now |
| `Close` / `Closed` | R to S / S to R | orderly end |

Kind 13 of `NPHYS001` (`AuthRejected`) is retired: a wrong token is
`Refuse { AuthFailed }`. A peer that speaks `NPHYS001` is answered
`Refuse { Version }` naming both versions, and a 4.41.x native peer is told to
upgrade. A standby pointed at a PostgreSQL or HTTP port gets an error saying
that `--primary-host` must name the primary's `--replication-port`.

**Refusals.** What the standby does with each code:

| Code | Sent when | The standby |
|---|---|---|
| `HistoryMismatch` | the primary is another dataset than the one the standby follows | `needs_recovery` |
| `Gap { expected, have_from, have_to }` | the primary cannot provide the next sequence contiguously | `needs_recovery` |
| `WalPruned { oldest }` | the WAL the standby needs is gone; `oldest` is the first sequence still retained | `needs_recovery` |
| `EpochStale { have, want }` | the epochs differ (possible only once promotion exists, P5) | `needs_recovery` |
| `Version` | another protocol version, or a feature this release does not serve (fencing) | `needs_recovery` |
| `AuthFailed` | the token does not match | retried, 3 attempts in all (a token file being replaced on both nodes), then `needs_recovery` |
| `TooManySessions` | the primary already serves `max_sessions` sessions | `reconnecting`, with backoff |
| `SourceNotServing` | the primary is shutting down or failing on its side | `reconnecting`, with backoff |
| (transport error) | connection refused, reset, timed out | `reconnecting`, with backoff |
| (TLS failure) | wrong CA, untrusted or misnamed certificate, refused client certificate | `reconnecting` (or `connecting` before the first session), reason `tls: …`, with backoff |

The reconnect backoff starts at 1 s and doubles up to 5 s. A standby in
`needs_recovery` stays up (unless `on_unrecoverable = "exit"`), refuses data
reads with `55000` naming the reason, and does not reconnect until it is
restarted. Reconnecting cannot help: the same primary would refuse it the same
way.

## Synchronous replication

`[replication] sync_mode` on the **primary** decides what a commit waits for. A
commit reads the primary's physical sequence right after its data write and
waits until `[replication] min_acks` standbys (default 1, range 1..=64)
acknowledge it:

| `sync_mode` | What a commit waits for | Comparable to |
|---|---|---|
| `async` (default) | nothing: it returns at once | Oracle Maximum Performance; PostgreSQL `synchronous_commit = local` |
| `semi-sync` | `min_acks` standbys of **this dataset** (same history) hold it durably (fsynced) | MySQL semi-synchronous replication; PostgreSQL `synchronous_commit = on` |
| `sync` | `min_acks` standbys have applied it (visible to their readers) | PostgreSQL `synchronous_commit = remote_apply` |

- **Who counts.** Only a serving standby that follows this primary (a resumed
  session) counts. A standby that is still downloading its bootstrap copy, and
  an offline `heliosdb-nano resync` receiver, never count. A primary that serves
  no replication port never waits, whatever `sync_mode` says, and warns at
  startup. With fewer serving standbys than `min_acks`, a commit times out:
  under `warn` only the first one (the primary then commits asynchronously
  until enough standbys acknowledge again), under `fail` every one.
- **Readable on return.** A standby applies and fsyncs a poll before it
  acknowledges it, so once a `semi-sync` or `sync` commit returns, the row is
  already readable on the acknowledging standbys.
- **Latency.** An idle standby polls every 100 ms, so a synchronous commit takes
  up to about the poll interval, plus the standby's apply and fsync, plus a round
  trip.
- **Timeout.** `[replication] sync_timeout_ms` (default 10000, range
  1..=86400000) bounds the wait. `[replication] sync_timeout_action` decides
  what happens next:
  - **`"warn"` (default).** The commit returns after the timeout, with a WARN:
    `synchronous replication not confirmed: N of M required standbys
    acknowledged physical sequence S within T ms ...`. The primary then commits
    asynchronously, without waiting, until `min_acks` standbys acknowledge the
    sequence that timed out, and then waits again (as MySQL semi-sync falls back
    to asynchronous). A primary whose standby is gone pays the timeout once, not
    on every commit.
  - **`"fail"`.** The PostgreSQL-wire client gets an ERROR, SQLSTATE `25000`,
    whose message contains `synchronous replication not confirmed` (and says the
    commit is durable on this primary but may not be on any standby), for a
    commit that **is durable on the primary**, like a cancelled synchronous
    commit in PostgreSQL. Do not blindly retry it: a retry writes it twice.
    Check whether the row exists first. `fail` never degrades to asynchronous: every commit
    waits the full timeout while too few standbys acknowledge. The error is
    reported on the PostgreSQL wire (simple and extended protocol); embedded
    API, MySQL-wire and REST writes only log it, and their commit stands.
- During shutdown, waiting commits are released.

## A primary's /health

A primary's `/health` lists every standby its replication port serves, beside the
replication block:

```json
{"status":"ok","replication":{"role":"primary","state":"streaming","ready":true, ...},
 "standbys":[{"node_id":"6f1c…","address":"127.0.0.1:40122","pg_port":5442,
   "history":"9a2e…","state":"streaming","sync_mode":"async","counts_for_sync":true,
   "applied":"0/1F4","durable":"0/1F4","applied_sequence":500,"durable_sequence":500,
   "lag_bytes":0,"lag_secs":0.0,"last_seen":1761050000}]}
```

- `state` is `streaming` (following) or `bootstrapping` (downloading its first
  copy, `history` null, `counts_for_sync` false).
- `applied` / `durable` are LSN text; `applied_sequence` / `durable_sequence` the
  same positions as numbers. Both only move forward: a late or stale heartbeat
  can never move them back.
- `lag_bytes` is the primary's sequence minus `applied_sequence` (write
  operations behind); `lag_secs` is how long the standby has not held everything
  the primary had written (0 while it does); `last_seen` is the Unix time of its
  last `Hello` or heartbeat.
- `sync_mode` is the mode the standby was configured with, informational only:
  the primary's own `sync_mode` decides what a commit waits for.
- A standby is listed while it has a session and leaves the list when its
  connection ends. The SQL view `pg_replication_standbys` reads the same
  registry.
- Both roles' `/health` also carry an `archive` object (next section), beside
  the replication block, never inside it.

## Archived WAL and the recovery window

Every write group a node applies is also **archived**: sealed into rotating,
self-describing segment files beside its data directory and kept for a
**recovery window** (default 7 days), bounded by disk. This is the database's
archived redo. A primary serving its replication port archives its own writes;
every standby archives the write groups it applies, so a standby's segments
hold, frame for frame, the bytes its primary archived for the same sequences.
Nothing reads the archive back yet: recovering a standby from segments
(`standby recover --from-archive`, archive-only standbys, gap fill from the
primary's archive) arrives in P3.2. Until then the archive is the record an
operator can list, verify and copy.

| Oracle | HeliosDB-Nano |
|---|---|
| Archived redo log | A segment, `<dir>/<history>/<epoch>/<first:020>-<last:020>.hseg` |
| `LOG_ARCHIVE_DEST_n` | `[replication.archive] dir` / `--archive-dir` (default `<data dir>.archive`) |
| RMAN catalog / `V$ARCHIVED_LOG` | `SELECT * FROM heliosdb_archived_wal`, `heliosdb-nano archive list` |
| `V$ARCHIVE_GAP` | `SELECT * FROM heliosdb_archive_gaps`, `heliosdb-nano archive gaps` |
| `CONFIGURE RETENTION POLICY TO RECOVERY WINDOW OF 7 DAYS` | `recovery_window = "7d"`; `SELECT * FROM heliosdb_recovery_window` |
| `RMAN> CROSSCHECK` / `VALIDATE ARCHIVELOG ALL` | `heliosdb-nano archive verify` / `archive verify --deep` |
| `DELETE OBSOLETE` | The running node's pruner; `heliosdb-nano archive prune` offline |
| `DB_RECOVERY_FILE_DEST_SIZE` | `max_bytes`, `disk_reserve` (a full archive shortens the window, it never stops the primary) |

### Segments

A segment is sealed when it would pass `segment_max_bytes` (default 64 MiB; a
single write group never splits, so a larger one gets a segment of its own) or
when it is older than `segment_max_secs` (default 300). Its header names the
dataset history, the epoch, the first sequence, the node and (for an encrypted
dataset) which dataset key sealed it; its footer the last sequence, the number
of write groups, when the first and the last were **observed** by the archiver
(RocksDB records no commit time, so nothing here is called one), and a SHA-256
over the whole file. Each frame is one write group exactly as the replication
connection carries it, digest included, so every frame is verified on its own.
On an encrypted dataset every frame is sealed with the dataset key: keys inside
a write group are plaintext data, so the whole frame is sealed. Files are mode
0600 in 0700 directories.

A segment being written is `<first>-open.hseg.partial`; it is sealed by writing
its footer, fsyncing and renaming it. The frames are fsynced at every seal and
every `fsync_bytes` (default 8 MiB) in between: the archive's durability is
secondary to the store's own WAL, which keeps everything the archiver has not
read yet for `[replication.physical] wal_retention_secs`. After a crash the
next start repairs the open segment — every intact frame is kept, the torn tail
is cut, the rest is sealed (`archive: repaired the open segment …`) — and the
archive continues right after it.

An empty archive on a primary that already holds data starts at the primary's
current position: take a snapshot (a standby's bootstrap copy is one) for the
history before it. When the archive lacks an interval the store no longer holds
(the node was down longer than `wal_retention_secs`, say), the archiver logs
`archive gap <from>..<to>: the store no longer holds it`, lists it in
`heliosdb_archive_gaps`, and continues from what the store still has.

### The recovery window, and why it can be shortened

Every `prune_interval_secs` (default 60) and after each seal, the pruner:

1. removes segments whose newest write group was observed before
   `now - recovery_window` — expected, silent;
2. removes the oldest segments while the archive holds more than `max_bytes`;
3. removes the oldest segments while the filesystem's free space is below
   `disk_reserve` (a percentage of the filesystem's size, or a size),
   measured again after each removal.

Steps 2 and 3 remove segments the window still covers: the recovery window is
**shortened**. The primary's writes are never held back for the archive —
there is no check in the write path and no back-pressure. Instead the node
warns once per episode, `recovery window shortened to <effective>: <reason>`,
counts `pruned_inside_window_total`, and `heliosdb_recovery_window` shows
`shortened = true`, state `shortened`. When everything that was pruned would
have expired anyway the episode ends (`recovery window restored to …`, INFO);
a later shortening warns again. A shortened window means the archive reaches
back less far than configured: raise `max_bytes`, free disk space or move the
archive to a larger filesystem.

The newest sealed segment and corrupt files are never pruned (removing a corrupt
file is the operator's call). A segment a connected standby still needs is
skipped by steps 2 and 3 while it is inside the window; beyond the window it is
pruned, and that standby, if it falls that far behind, needs recovery.

`heliosdb_recovery_window` (one row): `enabled`, `archive_dir`,
`configured_window_secs`, `effective_window_secs` (now minus the oldest retained
observation), `oldest_retained_at`, `newest_sealed_at`, `segments`,
`bytes_retained`, `max_bytes`, `disk_reserve_bytes`, `free_bytes`,
`pruned_inside_window_total`, `shortened`, and `state`: `ok`, `shortened`, `gap`
(the archive lacks an interval) or `disabled`.

`/health` carries the summary as a sibling of the replication block:

```json
{"status":"ok","replication":{...},
 "archive":{"enabled":true,"segments":42,"bytes_retained":187654321,
            "effective_window_secs":604211,"shortened":false,"gaps":0}}
```

### Settings

`[replication.archive]` in the config file; each key has a `start` flag, which
wins over the file:

| Key | Flag | Default | |
|---|---|---|---|
| `enabled` | `--archive[=BOOL]` | `true` | applies to a primary serving its replication port and to a standby |
| `dir` | `--archive-dir` | `<data dir>.archive` | refused inside the data directory and on the physical export directory |
| `segment_max_bytes` | `--archive-segment-max-bytes` | `"64MiB"` | 4KiB..=4GiB |
| `segment_max_secs` | `--archive-segment-max-secs` | `300` | 1..=86400 |
| `poll_ms` | `--archive-poll-ms` | `1000` | a primary's archiver reads its WAL this often (10..=60000) |
| `fsync_bytes` | `--archive-fsync-bytes` | `"8MiB"` | 4KiB..=4GiB |
| `recovery_window` | `--archive-recovery-window` | `"7d"` | seconds, or a number with one of the suffixes s, m, h, d, w; 1s..=3650d |
| `max_bytes` | `--archive-max-bytes` | `"unlimited"` | or a size |
| `disk_reserve` | `--archive-disk-reserve` | `"10%"` | a percentage of the filesystem (0..99) or a size |
| `prune_interval_secs` | `--archive-prune-interval-secs` | `60` | 1..=86400 |

Each node archives into its own directory: the archiver holds a lock
(`archiver.lock`) on it, and a second node pointed at the same directory runs
without an archive and says so.

### The `archive` command

Offline: it reads the files only and takes no database lock.

```bash
heliosdb-nano archive list   --data-dir /var/lib/heliosdb/data      # or --archive-dir DIR, --config FILE; --json
heliosdb-nano archive verify --archive-dir /backup/archive           # header and footer of every segment
heliosdb-nano archive verify --archive-dir /backup/archive --deep --key-file /etc/heliosdb/dataset.key
heliosdb-nano archive gaps   --archive-dir /backup/archive --from 1048576
heliosdb-nano archive prune  --data-dir /var/lib/heliosdb/data --dry-run
```

- `--archive-dir` defaults like the node's own: `[replication.archive] dir` of
  `--config`, else `<data dir>.archive` beside `--data-dir` (or the config's
  `[storage] path`).
- `verify` checks every segment's header and footer (magic, both hashes, the
  size, the name, the history and epoch of its directory); `--deep` also
  re-hashes each file and decodes every frame, which on an encrypted dataset
  needs the dataset key (`--key-file`, or the config's `[encryption]`). It exits
  1 and names the first failing file; `--keep-going` lists all of them.
- `gaps` lists the intervals the archive lacks and why: a hole between
  segments, a corrupt segment, or what the archiver recorded.
- `prune` applies the configured policy (`--recovery-window`, `--max-bytes`,
  `--disk-reserve` override it) and is refused while a node runs on the
  directory — a running node prunes its own archive. `--dry-run` changes
  nothing.
- A directory of segments copied from another host is just a directory to
  `list` and `verify`: a segment filed under the wrong history or epoch is
  named (`other history`, `other epoch`), never trusted.

### Design note: no catalog database

The plan sketched catalog rows in the node-local column family. They are not
needed: a segment's header and footer describe it completely and names sort in
sequence order, so the catalog is rebuilt by scanning the directory — at start,
by the `archive` command, on another host — and kept in memory while the node
runs. A primary has no node-local column family to keep one in, and a catalog
that could disagree with its files would only be one more thing to repair.

## Refusals and rebuild

A standby in `needs_recovery` names its reason in its log, in `/ready`, in the
view and in every `55000`. It never serves data from another history and never
continues over a hole. To rebuild it:

1. Stop the standby.
2. Keep its data directory: move it aside, or leave it and choose a new path. Do
   not delete it until the new copy is verified, and never remove marker files to
   get past a refusal.
3. Start the standby with a new, empty data directory (an absent path, or an
   existing empty directory). It bootstraps a fresh, verified copy from the
   primary and then follows it.

A rebuild is a full copy today. An incremental rebuild that ships only the
changed key ranges arrives in plan phase P4, and recovery from archived WAL
segments in P3.2 (the segments are already written, see above). An interrupted
bootstrap copy is never installed (the copy is verified and renamed into place
only when complete); it is resumed. The old standby's archive directory
(`<data dir>.archive`) belongs to the old directory: move it aside with it, or
give the new standby its own `--archive-dir`.

### Resuming an interrupted copy

A bootstrap copy (and `heliosdb-nano resync`) stages into one private sibling
of the data directory, `.nano-resync-<hash of the directory's name>`, and keeps
its progress there in `NANO-PHYSICAL-PARTIAL.json`: the snapshot, the
manifest's SHA-256, the dataset history, and per file its size, digest and the
bytes that are on disk and fsynced. The record is rewritten (temporary file,
rename, fsync) after every completed file and after at most 8 MiB inside a
file.

- **Within one run**, a transport error during the download reconnects with
  the usual backoff and continues where it was, instead of failing the copy.
- **After a restart** (a crash, a SIGKILL, a host reboot), the standby asks the
  primary for the snapshot its record names. The primary keeps a copy's
  checkpoint export **leased** for `[replication] snapshot_export_lease_secs`
  (default 3600) after the session that opened it ends, keyed by snapshot id,
  so it answers with the SAME manifest, and the standby continues: every file
  the record marks complete is hashed again (a mismatch is downloaded again),
  the file in progress is cut back to its recorded length, and the download
  goes on from there. The standby logs `resuming the download of snapshot …:
  N of M bytes already held and verified`.
- **Anything else starts over**, and the log says why
  (`the snapshot download is starting over …`): no record, another snapshot
  (the lease expired, or the primary restarted), another manifest, a record
  that cannot be read, a file whose size disagrees with it, or an unexpected
  file in the staging directory. The staging directory is deleted only when it
  carries this receiver's staging marker; anything else at that path is
  refused and left alone.
- Once every file is complete the record is removed, and the copy is verified
  and installed exactly as a fresh one (file census, per-file SHA-256, the
  checkpoint's sequence): an installed standby carries no extra file.

**What a lease costs.** A checkpoint is hard links to the primary's SST files:
a leased export pins their disk space (files compaction has since replaced are
not freed) until the lease ends. At most `snapshot_export_max_leases` (default
4) are kept, the oldest dropped first; an export a session still uses stays
until that session ends. A leased export is offered again only while the
primary's WAL still holds everything after its checkpoint, so a resumed copy
can continue streaming; otherwise the standby gets a fresh snapshot.

Before rebuilding, consider why it was refused:

- `WalPruned`: the standby was away longer than the primary keeps WAL for it.
  Raise `[replication.physical] wal_retention_secs` (at most 86400) and
  `wal_retention_size_mb` on the primary to survive longer outages.
- `HistoryMismatch`: the primary's endpoint now serves another dataset, for
  example a primary that was recreated. Decide which history to keep before
  rebuilding; see [recovering after a primary is recreated](offline-reseed.md)
  and the [history guard](replication-history-guard.md).
- `AuthFailed`: the token files differ. Fix them and restart the standby; no
  rebuild is needed.
- `Version`: upgrade the older node. Both ends must run this release.

## Converting a 4.41.x logical standby

There is no compatibility with the 4.41.x logical standby. A 4.41.x standby's
data directory is refused with:

> this directory is a logical standby from 4.41.x; physical standbys are rebuilt
> once: start with a new, empty data directory (docs/guides/physical-standby.md)

Upgrade the primary, start it with a replication token so it serves its
replication port, then rebuild each standby once as above: start it on a new,
empty data directory. Keep the old directory until the new standby is verified,
then remove it. If you would rather move the data than copy the primary, export
it over the PostgreSQL wire (for example `COPY … TO STDOUT` and
`COPY … FROM STDIN` with psql) into a new node.

## Test pair

`scripts/regress/nano-physical-pair.sh` runs a primary and its physical standby on
loopback from a built binary, the pair HeliosDB Proxy's lab and the live probes
use:

```bash
scripts/regress/nano-physical-pair.sh --binary target/release/heliosdb-nano --dir /tmp/pair --start
scripts/regress/nano-physical-pair.sh --binary target/release/heliosdb-nano --dir /tmp/pair --load 1000
scripts/regress/nano-physical-pair.sh --binary target/release/heliosdb-nano --dir /tmp/pair --kill primary --restart primary
scripts/regress/nano-physical-pair.sh --binary target/release/heliosdb-nano --dir /tmp/pair --stop
```

- Ports: primary PostgreSQL 127.0.0.1:27721, its replication port 27741, standby
  PostgreSQL 27722, HTTP 27751 (primary) and 27752 (standby). Both use trust
  authentication.
- Actions: `--start`, `--load N` (N single-row inserts per second, printing the
  measured ops/s), `--kill`, `--pause` and `--resume` `primary|standby`
  (SIGKILL, SIGSTOP, SIGCONT; a paused standby holds up synchronous commits),
  `--restart primary|standby|both`, `--stop`, `--status`.
- Options: `--sync-mode`, `--min-acks`, `--sync-timeout-ms`,
  `--sync-timeout-action` (the primary's `[replication]` keys),
  `--max-standby-lag`, `--timeout`, and `--tls`: a throwaway CA and server
  certificate are generated with `openssl` in the state directory and the pair
  replicates over TLS (remembered for that directory). The PostgreSQL ports are
  unchanged; TLS is on the replication port only.
- Every run ends with one JSON line: ports, pids, `ops_per_s`, the `/ready`
  status of each side, `standbys`, the number of standbys the primary's
  `/health` lists, `tls`, whether the standby's session is on TLS, and
  `archive`, the primary's archived segments and bytes (`{"segments":N,
  "bytes":N}`, from its `/health`).

The test suites are listed in the [HA testing guide](ha_testing.md).

## Limits of this release

- **Remote standbys need TLS.** Plaintext replication stays on loopback unless
  `--allow-insecure-replication-remote` (isolated development only). An
  authenticated tunnel that ends on loopback at both sides (for example
  `ssh -N -L 127.0.0.1:5433:127.0.0.1:5433 primary.example`) still works, with
  plaintext on loopback at each end.
- **Encryption keys.** A standby must be started with the same dataset key as
  its primary: it holds the primary's ciphertext verbatim. There is no key
  rotation command in this release (the internal `KeyManager::rotate` has no
  caller and re-encrypts nothing); rotating the key while standbys are attached
  is not supported until promotion and fencing arrive (P5), which add the
  control-plane check that every standby holds the new key first.
- **No promotion or failover** until plan phase P5. `SWITCHOVER` is refused, and
  a standby's data directory cannot be opened as a primary or standalone
  database. Writes are never routed through a standby: connect to the primary to
  write.
- **No cascading.** A standby cannot serve other standbys.
- **One primary per standby.**
- **Full-copy rebuilds** (incremental rebuild P4, recovery from archived
  segments P3.2); an interrupted copy resumes from its progress record while the
  primary still leases its snapshot.
- **The archive is written, not yet read.** Segments are sealed, verified,
  listed and pruned; replaying them into a standby arrives in P3.2.
- **The multi-container lab under `tests/docker` is still out of service**: it
  has not yet been brought up to date for remote physical standbys over TLS;
  see the [HA testing guide](ha_testing.md).
