# Independent compile/type source review of final candidate

Reviewed 2026-09-29 current working-tree changes in `src/main.rs`, `src/config.rs`,
`src/protocol/postgres/server.rs`, `src/replication/streaming.rs`,
`src/replication/transport.rs`, and `tests/replication_listener_startup.rs`.
Reviewer authored only the offline reseed script/tests/docs and excluded those
from this independent review. No build was run; the root coordinates the shared
host lock. This review is not compiler or runtime evidence.

**Result: no compile/type blocker identified in the reviewed current candidate.**

- The prior prebound-listener Trust bypass is addressed: `serve_with_listener`
  obtains the actual listener address, clones configuration, then checks that
  address with the effective `AuthManager::method()` before entering its loop.
  `PgServerConfig` is Clone and `AuthMethod` is Copy. The wildcard-listener
  regression exercises the default rejection before accepting any client.
- The effective role and sync mode are normalized before exact role checks and
  before construction of `HAConfig`. CLI replication fields are `Option`s;
  explicit present values overwrite TOML fields. `Config::from_file` parses
  without calling the new replication validator before these overrides, so an
  otherwise valid config's overridden semantic values do not preempt CLI input.
  The resolved replication struct is installed into `db_config` before the latter
  is moved into `EmbeddedDatabase::with_config`.
- `Option<SocketAddr>` is Copy, so separate `map_or` and `map` uses when creating
  `HAConfig` are legal. `Option<u16>` and all port conversions are consistent.
  Literal IPv6 parsing and HTTP disabling return the declared types.
- Adding `Config.replication` is covered by its Default implementation. The only
  other in-tree exact `Config` struct literal uses `..Config::default()`.
  In-tree PostgreSQL server callers use builders/defaults rather than exhaustive
  `PgServerConfig` literals. External exhaustive literals remain a public Rust
  API compatibility consideration for release notes.
- `HAConfig` is constructed with all fields. Daemon argument construction moves
  individual optional String fields; its later access is only to the remaining
  Copy boolean, with no whole-struct borrow. `auth.clone()` retains `auth` for the
  later warning. Both true-default TLS booleans use `ArgAction::Set`, matching
  daemon re-exec's explicit `true`/`false` values.
- Parent readiness owns its temporary directory until child acknowledgement;
  the child receives its path through the environment, creates the file only
  after reaching the startup checkpoint, and records its own PID. Parent reads
  compare owned String values legally. `Result::is_ok_and` is available at the
  declared Rust 1.85 minimum. Timeout/PID-write failures reap the owned child.
  This proves the PG/native startup checkpoint, not optional HTTP/MySQL/UDS
  availability: those pre-existing optional bind paths may still log and continue.
- PG and native listeners are bound before being handed by value to their server
  futures; neither path probes and then rebinds. The spawned native server is
  owned by the async move closure. The integration test instead moves an owned
  Arc and listener into its task. Method signatures and feature gates match.
- Persistent `WalStoreConfig.wal_dir` is constructed from the effective storage
  path. In-memory mode owns a `tempfile::TempDir` in `HAHandles`; the handle
  remains in scope while the server serves. `tempfile` is a normal dependency,
  and `HAHandles`' Default works for both optional fields. Existing dead-code
  allowance covers the lifetime-only field. No borrow outlives a temporary path.
- Transport magic inspection uses `u32::from(b'E')`, matching the decoded u32.
  Diagnostic String allocation stays in the invalid-magic path. Regression
  tests use valid current handshake fields, supported async byte-slice readers,
  and bounded ephemeral listeners; `close`, `shutdown`, and `WalStore::close`
  signatures match their calls.

Remaining required evidence: compile default and applicable feature tiers, run
the new and existing suites, and execute CLI/daemon configuration and bind
regressions under the shared build lock. The WalStore directory fix does not
establish durable catch-up or resolve issue #7's snapshot/history protocol gap.

Root test-API follow-up before candidate gate: offline reseed tests called
`row_count()` on `EmbeddedDatabase::query`, which returns `Vec<Tuple>`. Corrected
the test to use `len()` and strengthened it to compare ordered row IDs and labels
before and after an independent write and when reopening the original. Only
that regression source changed; source-freeze manifest updated before execution.
The offline-helper agent independently re-reviewed the root correction against
`Vec<Tuple>`, `Tuple::get`, Value variants and PartialEq; no remaining source
blocker was found. Compiler/runtime evidence remains pending.
