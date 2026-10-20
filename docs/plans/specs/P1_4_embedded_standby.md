# Spec P1.4 — `EmbeddedDatabase::with_physical_standby` and the derived-state listener

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.5. Sprinter `db11a1ef3b76`. After P1.1.

Value: the SQL layer (`EmbeddedDatabase`) holds state the storage engine does not: the
function/procedure registry, trigger registry, plan/parse/result caches, the MV scheduler. A
physical standby needs an `EmbeddedDatabase` built over `StorageEngine::open_physical_standby`
that never writes and refreshes that state when the applier reports catalog changes.

## 1. `EmbeddedDatabase::with_physical_standby(config: Config, storage: Arc<StorageEngine>) -> Result<Self>`
Variant of `with_config_open_policy` (lib.rs ~10527-10575): takes the already-open standby
engine; runs the strict rebuild branch (`rebuild_all_indexes_for_physical_snapshot`,
catalog.rs ~561, already gated on `is_physical()`); loads the function/procedure registry from
`meta:function:`/`meta:procedure:` (lib.rs ~6782) and triggers; does NOT create the
`MVScheduler` (~10543) nor any auto-refresh; `durable_commit` off; `Drop` performs no writes
(the P0.2 gate already refuses; make the close path skip them explicitly so no refusals are
counted on a clean shutdown — assert `refusals() == 0` across open→query→drop).

## 2. Listener
`impl DerivedStateListener for EmbeddedDatabase` (or a small handle it hands out):
`MetaFunction/MetaProcedure` → reload that one entry into the function registry (or the whole
registry if per-entry is awkward — say which); `MetaTrigger` → trigger registry reload;
`MetaMv` → MV metadata cache; `ViewMeta` → view cache; any catalog class → `invalidate_plan_cache()`
(lib.rs ~13892) and reconcile the result cache (`reconcile_result_cache_with_storage`, ~3978).

## 3. Read-only guarantees at the SQL layer
On a standby `EmbeddedDatabase` every write entry point of the embedded API must fail closed
with one typed error ("writes are not permitted on a physical standby") BEFORE reaching the
engine (the gate is the backstop): `execute` of DML/DDL, `insert_vectors`, `restore_from_dump`,
`refresh_materialized_view`, branch create/switch/merge/drop, `code_index`/`lsp_rename_apply`,
`graph_rag_ingest_*`, `enable_lockfree_ingestion`, `dump`-with-side-effects if any. Read APIs
(`query`, `execute` of SELECT, dumps that only read) work. A `pub fn is_physical_standby(&self)`.

## 4. Tests — `tests/physical_standby_embedded.rs` (feature `ha-tier1`)
Open a standby `EmbeddedDatabase` over a stage produced as in P1.1's harness; `query` works;
each write entry point above returns the typed error and `write_gate().refusals()` stays 0
(refused before the engine); apply a batch with `CREATE FUNCTION`/`CREATE VIEW`/`CREATE TRIGGER`
from the primary → the standby can call the function / read the view afterwards without a
restart; drop → no refusals, no files written (dir hash census as in P0.3's tests).

## Constraints
No cargo/git; read before edit; match style; keep the primary's behaviour unchanged. Report
files, the listener's class→action table, the list of write entry points guarded, deviations,
commands.
