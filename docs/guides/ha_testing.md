# HeliosDB Nano HA testing guide

High availability in this release is one primary and its
[physical standbys](physical-standby.md): read-only replicas that apply the
primary's RocksDB write groups over one replication connection (`NPHYS004`) on
the primary's replication port. This guide covers the automated suites, manual
testing with the loopback pair fixture, and what to look at when something is
wrong.

## The multi-container lab is out of service

The Docker lab under `tests/docker` (the compose cluster, its images and its
tutorial scripts) is **out of service**. It starts each standby in its own
container, which needs remote replication: that exists now (TLS on the
replication port, spec P2.2), but the lab has not been brought up to date for
it, and it also exercised features that no longer exist: the observer node,
transparent write routing from standbys, and the native replication channel.
Do not use it to test this release; test with the suites and the pair fixture
below (`--tls` runs the pair over TLS), on one host. `tests/physical_standby_tls.rs`
covers remote replication on the loopback aliases `127.0.0.2` / `127.0.0.3`,
and `tests/physical_snapshot_resume.rs` (built with `--features internal-tests`)
the resumable bootstrap copy.

## Automated suites

`ha-tier1` is a default feature, so no feature flag is needed:

```bash
cargo test --test physical_standby_serving --test physical_standby_sync \
  --test physical_standby_refusals --test ha_integration --test replication_restart_gap
```

Each target starts real daemons on private loopback ports, with one replication
token per cluster (`tests/support/physical_pair.rs`), and drives them over the
real PostgreSQL wire and HTTP:

| Target | Covers |
|---|---|
| `physical_standby_serving` | bootstrap into a new directory; `/ready` 200 and the standby serving exactly the primary's rows; the PostgreSQL-wire contract (`heliosdb_require_ready()`, `pg_is_in_recovery()`, both LSN functions, `pg_control_checkpoint()`, `heliosdb_standby_status`, `in_hot_standby`); writes and `nextval` refused with `25006`; SIGKILL of the primary (the standby stays up, ready within `max_standby_lag`, then `55000`) and of the standby (restart resumes from its cursor, no second copy); a 4.41.x logical standby directory refused; `on_unrecoverable = "exit"` |
| `physical_standby_sync` | `sync`: an INSERT blocks while the standby is stopped (SIGSTOP) and returns once it runs (SIGCONT); `semi-sync` with `min_acks = 2` and one standby, under `warn` (commits with a WARN) and `fail` (the client gets "synchronous replication not confirmed", the row is durable on the primary); 15 s of heartbeat-only traffic never moves the primary's recorded sequences back; the primary's `/health` lists the standby with advancing positions |
| `physical_standby_refusals` | `HistoryMismatch`, `WalPruned` (small retention), `AuthFailed` (wrong token), and an `NPHYS001` or `NPHYS002` client answered `Refuse { Version }`; in every case the standby stays up, `/ready` answers 503 with `needs_recovery`, and `heliosdb_require_ready()` refuses with `55000` naming the reason |
| `ha_integration` | start order (a standby started before its primary waits, then bootstraps); SIGKILL and restart of the primary, stop and restart of the standby, then both; sync and semi-sync commits readable on the standby at once; lag on the primary's `/health` |
| `replication_restart_gap` | no silent gap: rows committed while the standby was down arrive after its restart; a standby that joins after the data and DDL exist |

Related targets:

- `txn_replication_sync`: the commit path's wait on the physical sequence for
  explicit transactions, against the standby registry in-process (no daemons);
- `physical_standby_open_mode`, `physical_standby_local_namespace`,
  `physical_standby_derived_state`, `physical_standby_vector`,
  `physical_standby_embedded`: the standby's storage mode, node-local column
  family, derived-state maintenance, vector indexes and embedded use, in-process;
- `physical_large_write_group`: one ~80 MiB write group (a 20 000-row
  `INSERT … SELECT`) followed by a standby, a cascaded standby, the archive and
  `recover --from-archive` (spec P4.1c); with `--features internal-tests`, a
  standby killed with the group's frame in flight resumes from its cursor;
- `physical_promote`: `heliosdb-nano promote` (spec P5.1) — a graceful
  promotion fencing the old primary (25006 on its writes, `/ready` 503,
  `state = fenced`, FATAL 57P01 for its old sessions, `NANO-FENCED.json`), the
  promoted node at epoch 2 of the same history, a standby of the old primary
  fast-forwarded to the new one, the fenced primary restarted FENCED as a
  primary (reads, 25006, `transaction_read_only = on`), refused as a standby
  and rebuilt with `recover --from-primary --replace`, a forced promotion and
  the superseded primary fencing itself when an epoch-2 receiver reaches it, a
  running standby refused, a second promotion of the same epoch refused
  (`pg_promote` on the new primary 55000), no logical-WAL replay at the first
  open (the old primary's checkpoint never advanced, a trigger workload), both
  epochs' archives; with `--features internal-tests`, a promotion killed after
  each step and finished by a re-run;
- `physical_promote_online` (spec P5.2, real daemons): `SELECT pg_promote(true,
  60)` on a running standby while the primary takes writes answers `t`, idle
  sessions on the standby and on the fenced primary are pushed FATAL 57P01
  without a statement, a fresh connection sees `in_hot_standby = off`,
  `pg_is_in_recovery() = f`, `timeline_id = 2`, writes succeed, the old
  primary is fenced, the standby holds exactly what it holds (every write it
  acknowledged during the promotion) and `repl:epoch` says fenced at its
  recorded final sequence, a standby of it is refused `pg_promote`
  (`needs_recovery`) and fast-forwarded to the new one; Proxy's exact forms
  (`SELECT pg_promote(true, 5)` → `t`, then a fresh `pg_is_in_recovery()::text`
  → `false`), also under writes; `wait = false`; a primary dead before the
  call with `promote_without_fence = "refuse"` (55000 naming the offline
  command, "NOT fenced", the standby keeps following) and `"allow"` (`t`,
  `fenced = false`); the fence's push to idle sessions within a second; the
  extended protocol (Describe `boolean` OID 16, the row, then FATAL 57P01 on
  the connection after Sync); a cascading standby promoted online (its port
  serves epoch 2, its cascaded standby recovers from it); with `--features
  internal-tests`: `f` on a short budget and a second call refused meanwhile
  (the promotion held on a file), a statement held inside the engine (a
  budget it outlasts aborts with the primary unfenced and taking writes; one
  it does not outlast waits, then fences), a primary killed after `Fenced`
  (the abort names both sequences and `promote --force`; restarted, the
  re-run is answered the cached `Fenced` and completes), a promotion aborted
  after its replication stopped (receiver and cascading port back), and a
  failure (or a kill) after the record ending the process with exit 2,
  `start` refusing the directory and `promote` finishing it;
- `logical_standby_removed`: a source scan that fails if a removed replication
  flag, module or symbol reappears in the code, tests, fixture, guides or skills;
- `offline_reseed_tests`: the [offline snapshot helper](offline-reseed.md).

## Manual testing with the pair fixture

`scripts/regress/nano-physical-pair.sh` starts a primary and a standby from a
built binary, on loopback, with trust authentication:

| Node | PostgreSQL | Replication | HTTP |
|---|---|---|---|
| primary | 127.0.0.1:27721 | 127.0.0.1:27741 | 27751 |
| standby | 127.0.0.1:27722 | (none) | 27752 |

```bash
P=scripts/regress/nano-physical-pair.sh
B=target/release/heliosdb-nano
D=/tmp/nano-pair

$P --binary $B --dir $D --start          # both sides, waits for the standby's /ready
$P --binary $B --dir $D --load 500       # 500 single-row INSERTs/s for 10 s; prints ops/s
$P --binary $B --dir $D --status         # the JSON summary only
```

Every invocation ends with one JSON line: ports, pids, `ops_per_s`, the `/ready`
status of each side, and `standbys` (how many standbys the primary's `/health`
lists). The script rewrites the configuration it starts nodes with on every run,
so pass the same options (`--sync-mode`, `--min-acks`, `--sync-timeout-ms`,
`--sync-timeout-action`, `--max-standby-lag`) to every invocation of one pair.

**Data reaches the standby.**

```bash
psql -h 127.0.0.1 -p 27721 -U postgres -c "CREATE TABLE t (id INT PRIMARY KEY)"
psql -h 127.0.0.1 -p 27721 -U postgres -c "INSERT INTO t VALUES (1), (2)"
psql -h 127.0.0.1 -p 27722 -U postgres -c "SELECT * FROM t"
psql -h 127.0.0.1 -p 27722 -U postgres -c "INSERT INTO t VALUES (3)"   # ERROR, SQLSTATE 25006
```

**Status and the Proxy probes.**

```bash
curl -s http://127.0.0.1:27752/ready; echo
curl -s http://127.0.0.1:27751/health; echo     # the primary lists its standbys
psql -h 127.0.0.1 -p 27722 -U postgres -c "SELECT heliosdb_require_ready()"
psql -h 127.0.0.1 -p 27722 -U postgres -c "SELECT pg_last_wal_replay_lsn()::text"
psql -h 127.0.0.1 -p 27721 -U postgres -c "SELECT pg_current_wal_lsn()::text"
psql -h 127.0.0.1 -p 27722 -U postgres -c "SELECT * FROM heliosdb_standby_status"
psql -h 127.0.0.1 -p 27721 -U postgres -c "SELECT * FROM pg_replication_standbys"
```

**Losing the primary.** Kill it, watch the standby stay up and ready for
`max_standby_lag`, then report 503 and refuse data reads with `55000`; restart
the primary and watch the standby catch up:

```bash
O="--max-standby-lag 5s"
$P --binary $B --dir $D $O --stop --start        # the standby reads max_standby_lag at start
$P --binary $B --dir $D $O --kill primary
curl -s http://127.0.0.1:27752/ready; echo      # reconnecting: 200 for at most 5 s, then 503
$P --binary $B --dir $D $O --restart primary
```

**Restarting the standby.** It resumes at its durable cursor; its log says no
copy is made:

```bash
$P --binary $B --dir $D --kill standby --restart standby
grep -i "resuming physical standby" $D/standby.log
```

**Synchronous commits.** With `sync`, a commit waits until the standby has
applied it. Pause the standby and an INSERT on the primary blocks until it is
resumed (or until `sync_timeout_ms` passes):

```bash
O="--sync-mode sync --sync-timeout-ms 60000"
$P --binary $B --dir $D $O --stop --start
psql -h 127.0.0.1 -p 27721 -U postgres -c "CREATE TABLE s (id INT)"
$P --binary $B --dir $D $O --pause standby
psql -h 127.0.0.1 -p 27721 -U postgres -c "INSERT INTO s VALUES (1)" &   # blocks
$P --binary $B --dir $D $O --resume standby                              # the INSERT returns
```

With `--sync-timeout-action fail` and a short `--sync-timeout-ms`, the paused
standby makes the client receive an error containing "synchronous replication
not confirmed", while the row is committed on the primary.

**Refusals.** Every refusal leaves the standby up in `needs_recovery`, with the
reason in its log, in `/ready` and in `heliosdb_require_ready()`'s error. For
example, start a second standby of the same primary with another token (inside
`$D`, which the fixture keeps owner-only; `/tmp` itself is refused as a parent):

```bash
(umask 077 && head -c 32 /dev/urandom > $D/other.token)
$B start --data-dir $D/other-standby --listen 127.0.0.1 --port 27723 --auth trust \
  --replication-role standby --primary-host 127.0.0.1:27741 \
  --replication-token-file $D/other.token --http-port 27753 &
curl -s http://127.0.0.1:27753/ready; echo      # 503; needs_recovery after three AuthFailed
```

To rebuild a refused standby, stop it, move its data directory aside and start
it again: it bootstraps a new copy.

`--stop` ends both sides with SIGTERM.

### The promote drill (spec P5.1)

With the pair running and some rows written (`$D` is the fixture's directory,
`$B` the binary; the fixture's token is `$D/replication.token`):

```bash
psql -h 127.0.0.1 -p 27721 -U postgres -c "INSERT INTO t VALUES (1001, 'before')"
kill -TERM $(cat $D/standby.pid)                     # stop the standby
psql -h 127.0.0.1 -p 27721 -U postgres -c "INSERT INTO t VALUES (1002, 'tail')"
$B promote --data-dir $D/standby --primary-host 127.0.0.1:27741 \
  --replication-token-file $D/replication.token     # fences the primary, tails it, converts
curl -s http://127.0.0.1:27751/ready; echo          # 503, "state":"fenced"
psql -h 127.0.0.1 -p 27721 -U postgres -c "INSERT INTO t VALUES (1003, 'x')"   # 25006 write refused
$B status --data-dir $D/primary                      # role: fenced primary
$B start --data-dir $D/standby --listen 127.0.0.1 --port 27722 --auth trust \
  --replication-role primary --replication-port 27742 \
  --replication-token-file $D/replication.token --http-port 27752 &
psql -h 127.0.0.1 -p 27722 -U postgres -c "SELECT timeline_id::bigint FROM pg_control_checkpoint()"  # 2
psql -h 127.0.0.1 -p 27722 -U postgres -c "SELECT count(*) FROM t"          # includes 1002
```

Then rebuild the old primary as a standby of the new one: stop it (restarted
as a primary it comes up fenced — reads only, `/ready` 503; as a standby its
`start` is refused, exit 2), and run `$B recover --from-primary --primary-host
127.0.0.1:27742 --replace --data-dir $D/primary --replication-token-file
$D/replication.token`. The forced variant: SIGKILL the primary first;
`promote` without `--force` exits 2 after `--fence-timeout`, with `--force` it
promotes and says `NOT fenced (--force)` (`--no-fence` skips contacting the
primary entirely, for a rehearsal on a cloned directory). Kill a `promote`
half-way (an `internal-tests` build honours
`HELIOSDB_NANO_TEST_KILL_PROMOTE_AT=after-epoch`) and every open of the
directory is refused until `promote` is run again.
`tests/physical_promote.rs` automates all of it.

## Log lines

Primary:

```
Replication port listening on 127.0.0.1:27741 (NPHYS004, dataset history <uuid>, epoch <n>)
replication: this primary (history <uuid>, epoch 1) was FENCED by epoch 2 (promotion <uuid>) at its sequence <n>: …
```

Standby, first start and restart:

```
Installed physical standby copy at <dir> (snapshot <id>, history <uuid>, source sequence <n>)
Resuming physical standby <dir> at its durable cursor (history <uuid>); no copy is made
Physical standby streaming from 127.0.0.1:27741 at <lsn> (primary at <lsn>)
```

A standby that stays up after a refusal logs that it remains in posture
`needs_recovery` and that its listeners refuse data reads with SQLSTATE 55000.

## Troubleshooting

- **The standby is never ready.** Read `/ready`'s body: `state` and the standby's
  log say why. `recovering` is a bootstrap copy still downloading; `connecting`
  or `reconnecting` means it cannot reach the primary; `needs_recovery` names the
  refusal.
- **"--primary-host must name the primary's --replication-port".** The standby
  points at the primary's PostgreSQL or HTTP port. Use the primary's
  `--replication-port`.
- **"--primary-host ... resolves to ...: a non-loopback replication port is reached over TLS".**
  The standby names a remote address with `--primary-tls=false` and no
  `--allow-insecure-replication-remote`. Use TLS (the default for a remote
  primary), or an authenticated tunnel ending on loopback.
- **The standby keeps reconnecting with `"reason": "tls: …"`.** Its CA does not
  match the primary's certificate (`UnknownIssuer`), the certificate does not
  name the `--primary-host` host (`NotValidForName`; set
  `--primary-server-name`), or a mutual-TLS primary wants a client certificate.
  The standby re-reads its CA and client certificate at every attempt: fix the
  files and it recovers by itself.
- **The primary lists no standbys.** It serves no replication port without a
  replication token, in memory, or with `--physical-replication=false`; its
  startup log says so.
- **"directory must be owned and not group/world writable".** The standby's
  data-directory parent (or the primary's export directory) has loose
  permissions. Make it owner-only.
- **Synchronous commits wait out `sync_timeout_ms` although a standby is
  connected.** Only a serving standby that follows the primary counts toward
  `min_acks`; a standby still downloading its bootstrap copy (`bootstrapping` in
  the primary's `/health`) does not, and neither does a `resync` receiver.
