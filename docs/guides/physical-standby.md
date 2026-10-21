# Physical standby

A HeliosDB-Nano standby is a **physical replica** of its primary: a read-only copy
of the primary's RocksDB keyspace, kept current by applying the primary's own
write groups, byte for byte. It serves read-only SQL on the PostgreSQL wire while
it applies, reports exactly how current it is, and stays up when its primary
cannot serve it.

This guide covers starting a pair, readiness, the PostgreSQL-wire contract that
HeliosDB Proxy relies on, the replication connection, synchronous replication,
rebuilding a standby, and the limits of this release. The design is
`docs/plans/PHYSICAL_STANDBY_REPLICATION.md`; the one-connection protocol is
spec P2.1 (`docs/plans/specs/P2_1_single_connection_control_plane.md`).

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
| Redo transport | The replication connection (`NPHYS002`) to the primary's `replication_port`. The standby pulls write groups |
| Redo apply (MRP) | The physical applier: each write group is applied verbatim and fsynced, then the standby's in-memory indexes are updated |
| Archived redo | The RocksDB WAL archive a primary keeps for its standbys, bounded by `[replication.physical] wal_retention_secs` and `wal_retention_size_mb`. No catalog. Sealed archive segments arrive in P3 |
| `LOG_ARCHIVE_DEST_n`, `archive_command` | Not yet (P3) |
| RMAN catalog, `V$ARCHIVED_LOG` | Not yet (P3) |
| FAL (gap fetch) | None. A standby whose next sequence the primary no longer retains is refused (`WalPruned`) and is rebuilt |
| `RECOVERY WINDOW OF n DAYS` | `wal_retention_secs` (60..86400, default 3600). A multi-day recovery window arrives with the archive in P3 |
| `DB_RECOVERY_FILE_DEST_SIZE` | `wal_retention_size_mb` (64..65536, default 1024) |
| `DUPLICATE … FOR STANDBY FROM ACTIVE DATABASE` | The **bootstrap copy**: a standby started on an empty directory downloads a verified checkpoint of the primary, then follows it |
| `RECOVER STANDBY DATABASE FROM SERVICE` | Not yet (P4, incremental rebuild). Today a rebuild is a full bootstrap copy into a new directory |
| Managed recovery from archived logs | Not yet (P3) |
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
address when that is a loopback address, and on `127.0.0.1` otherwise: it stays
on loopback whatever `--listen` opens, because the replication connection is
plaintext until TLS replication (spec P2.2), and a log line says so. The
primary checkpoints into `[replication.physical] export_dir` to serve a
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
If the primary encrypts at rest, give the standby the same key: it holds the
primary's ciphertext unchanged. `--primary-host` must resolve to loopback in
this release. `--replication-port` is unused on a standby. A standby started
before its primary is up and not ready, and bootstraps once the primary
appears.

**Ports.** Every node on one host needs its own PostgreSQL `--port`, HTTP
`--http-port` (`0` disables HTTP), and, on the primary, `--replication-port`.
A standby accepts no MySQL listener (`--mysql` and `--mysql-socket` are refused
before storage opens), and its HTTP listener mounts only `/`, `/health`,
`/ready` and `/version`.

## Readiness

A standby's **posture** says what it is doing. One snapshot of it feeds `/health`,
`/ready`, the `heliosdb_standby_status` view and the PostgreSQL-wire read guard,
so those surfaces can never disagree.

| Posture | Entered when | Data reads | `/ready` |
|---|---|---|---|
| `connecting` | no session has applied anything since the store was attached | refused, `55000` | 503 |
| `recovering` | a bootstrap copy is downloading | refused, `55000` | 503 |
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
   "primary":"127.0.0.1:5433","sync_mode":"async","last_ack_sequence":500,"epoch":1}}
  ```

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
| `Hello` | R to S | node id; role `standby` or `resync`; history (none while bootstrapping); epoch; durable cursor (none for a fresh snapshot); capabilities; PostgreSQL port; sync mode |
| `HelloOk` | S to R | node id, history, epoch, barrier (the primary's written sequence), capabilities, replication endpoint, PostgreSQL port |
| `Refuse` | S to R | a code and a reason; the session ends |
| `Open` | R to S | `snapshot`, or `resume {history, after}` |
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
changed key ranges arrives in plan phase P4, and recovery from archived WAL in
P3. An interrupted bootstrap copy is never installed (the copy is verified and
renamed into place only when complete), and the next start downloads it again
from the beginning; a resumable download arrives with P2.2.

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
  `--max-standby-lag`, `--timeout`.
- Every run ends with one JSON line: ports, pids, `ops_per_s`, the `/ready`
  status of each side, and `standbys`, the number of standbys the primary's
  `/health` lists.

The test suites are listed in the [HA testing guide](ha_testing.md).

## Limits of this release

- **Loopback only.** The replication port listens on loopback and a standby's
  `--primary-host` must resolve to loopback, until remote replication over TLS
  (spec P2.2). Reach a primary on another host through an authenticated tunnel
  that ends on loopback at both sides, for example
  `ssh -N -L 127.0.0.1:5433:127.0.0.1:5433 primary.example` on the standby host.
  `--allow-insecure-replication-remote` / `[replication] allow_insecure_remote`
  are accepted but change nothing yet (a WARN says so).
- **No promotion or failover** until plan phase P5. `SWITCHOVER` is refused, and
  a standby's data directory cannot be opened as a primary or standalone
  database. Writes are never routed through a standby: connect to the primary to
  write.
- **No cascading.** A standby cannot serve other standbys.
- **One primary per standby.**
- **Full-copy rebuilds** that restart from zero when interrupted (incremental
  rebuild P4, archive recovery P3, resumable download P2.2).
- **The multi-container lab under `tests/docker` is out of service** until remote
  replication (P2.2); see the [HA testing guide](ha_testing.md).
