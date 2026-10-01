# PostgreSQL generation integration — unfinished executable path

Owner: replication_config. Deadline work must preserve all frozen trees and shared host build lock. Only external files changed. No builds, services, or live generation tests ran.

## Implemented external prerequisite

`generation-portability.patch` SHA256 d297fb4662cfc5be202aca23b72bcb8b36ab4f028a5bc42e3192907380e4f045, generated against frozen nano-resync src/lib.rs. Depends on root `generation-session-transfer.patch` SHA9235234e791ce6fa1c9eb2b7d9b3c9efde8a91ae870b54da03fd6e11d04fc859. Full lib file, method/test fragments, and generator alongside. Rustfmt parsing and git apply --check pass; no compile. Two authored unit tests preserve exact Session Arc/PID/application_name/search_path/target fast-path hint and pin transaction/notices/tenant/temp states.

API: `EmbeddedDatabase::try_transfer_idle_physical_session(&self, target:&Self, session_id:SessionId)->Result<bool>`. `false` retains original owner; caller must retain old lease and avoid swapping database/catalog. Caller must exclusively own the idle wire session. It checks both source/target transaction slots, ART undo and pending notices; Session active txn, tenant and sticky temp hint. It sets target any_session_schema_active before publishing a non-default search path, then invokes root's manager transfer. Manager helper alone misses that hint and the engine-owned state checks.

Root's session-transfer patch independently reviewed in `../generation-session-transfer-independent-review-config.md`.

## Exact stable generation APIs from transport

Module `replication::physical_generations` in external `physical-serving-draft`:

- `ServingGenerations::new(parent:PathBuf, config:Config, limits:TransferLimits, max_resident:usize)->Result<Arc<Self>,String>`.
- `lease(&self)->Option<ServingLease>`; `last_error()->Option<String>`; `close_and_join()/reap_retired()->Result<(),String>`.
- `ServingLease:Clone`, `database()->&EmbeddedDatabase`, crate-private `database_arc()->Arc<EmbeddedDatabase>`, `history()->Uuid`, `source_sequence()->u64`.
- `ReceiverOptions` additionally `require_committed:bool` and `serving:Option<Arc<ServingGenerations>>`. Existing raw main must use false/None until complete source certification+PG admission is integrated.

Generation object owns strict hydrated DB + raw DB sentinel + private directory + resident permit. Lease lifetime must outlive every PG DB/catalog reference. Worker cannot delete retired image behind escaped DB references. Do not replace this with a raw Arc only.

## Protocol code not yet authored (must not claim integration)

1. **Server attach/accept** (`src/protocol/postgres/server.rs`): add optional slot and builder/attachment API. Preserve listener AuthManager, TLS and ConnectionPolicy. Acquire one lease at accept before choosing handler database. Refuse/unavailable if none; do not accept on old placeholder data. Avoid retaining the initial generation forever through PgServer.database: use an explicit static-versus-generations database source (or Option static Arc cleared at attach). Carry the initial lease through pre-auth negotiation and attach before startup. Existing `handle_connection` performs one bounded negotiate then handler.handle; preserve its authentication deadline. Do not pass generation solely through a temporary local that drops before handler.

2. **Handler owner** (`handler.rs`): last field `Option<GenerationBinding {slot:Arc<ServingGenerations>, lease:ServingLease, cycle_open:bool}>` under ha-tier1. Put it LAST so database, PgCatalog and session teardown drop before directory lease. Update all TCP/Unix/generic/test struct literals (rg shows constructor literals and tests around lines6170,6265,6899). Handler Drop currently destroys session through self.database; this must remain correct after successful transfer.

3. **Cycle pinning**: do not use per-query Arc refresh. Before the first message of a new protocol cycle, attempt refresh only if no generation extended cycle, no awaiting_sync_after_error, transaction_status==Idle, !implicit_transaction, !database.session_in_transaction(session_id), and no executable/suspended portal. Acquire latest lease, compare actual DB identity (not plan-cache epoch); call wrapper, then replace DB and PgCatalog, invalidate prepared engine plans, finally replace lease. Simple Query retains one lease through its entire semicolon batch and resulting ReadyForQuery. Parse/Bind/Describe/Execute/Close/Flush retain it through Sync; Sync after error ends the cycle too. Explicit active AND failed transactions keep old lease across Sync and Query until COMMIT/ROLLBACK returns idle. Root requirement permits Parse/Bind preparation of writes in logical guarded mode; physical immutable mode must effect-check before Parse catalog probes too.

   Existing loop's `at_ready_for_query` is local in run_message_loop (~920) and tests use dispatch_message directly (~1098). Prefer explicit handler cycle state shared by dispatch and error-Sync helper (~1110) so production and tests obey same contract. Do not refresh between messages of one extended cycle, inside handle_single_query, or while suspended portal still owns rows.

4. **Portals** (`prepared.rs`): conservative rule is to pin when any Ready or Suspended portal belonging to a live statement exists; Complete portals do not pin. Closed statement's orphan portals need explicit cleanup/ignore rule so they do not pin forever. Preserve suspended rows on their existing generation until completion/Close. Do not silently resume rows from a new generation or clear a suspended portal on Sync.

5. **Prepared plans** (`prepared.rs`, `handler_extended.rs`): preserve names/query/declared wire descriptors and parameter OIDs, but clear every cached_plan, cached_plan_epoch and is_catalog decision on engine switch even when numeric epochs match. A manager-owned stale-name set avoids modifying every public PreparedStatement struct literal. At Bind, Describe and Execute lazily reparse/replan on current generation and compare old versus new wire RowDescription (name/type OID/size/typmod/table OID/attribute count; format handled by portal) and resolved parameter signature. If incompatible, return SQLSTATE0A000 (`cached plan must not change result type`) and retain statement name until explicit Close/reparse. Do not silently overwrite the promised descriptor. Never reuse old Arc<LogicalPlan>. shared_query_schema uses public plan cache only when session schema inactive; preserve private schema derivation/search-path behavior. PgCatalog must be rebuilt with new DB.

6. **Physical-only effect admission before any action**: existing guarded_standby_statement_allowed from f345 patch is shape-only and explicitly allows SELECT effects. Reuse its query/CTE/SELECT INTO/locks/utility filtering only as first stage. Add AST Visitor function effects with explicit known-pure builtin allowlist; deny nextval/setval/advisory mutations/custom/UDF/unknown function effects. Check function_registry for shadowed names even for an allowlisted spelling. Check before Parse's `catalog.handle_query` probe and before simple Query/Execute, including COPY TO expressions if ever supported. SET only known session settings; no global branch/HA utility. EXPLAIN ANALYZE must pass same effective query admission. SQL PREPARE/EXECUTE utilities remain refused so they cannot bypass effect analysis.

   Hidden views: `Planner::resolve_table_ref` and `normalize_nontable_name` are pub(crate). Mirror planner's exact view resolution (~4137): resolved view first, flat fallback only if no real table at resolved name, then recursively parse/check `ViewMetadata.query_sql` using creator_schema with bounded cycle/depth detection. Reject stored function calls under any schema spelling. Unknown table-function variants must not bypass Expr::Function visitor. Root explicitly requires physical listener tenant binding be rejected until RLS/policy-effect validation exists; no full tenant support claim. AST alone does not establish storage-wide immutability, so root's engine mutation guard remains a prerequisite.

7. **Error framing**: generation descriptor/effect refusal needs explicit mapped marker->0A000 or typed error, not generic XX000. Extended errors set awaiting_sync and emit ErrorResponse+flush without premature Ready; subsequent input discarded until Sync. Simple errors terminate batch and send one Ready. Preserve active->failed transaction rules. Startup unsupported tenant refusal must be fatal before Ready.

8. **Shutdown/publication**: root/transport own publication. Listener must stop accepting and join all handlers before close_and_join; detached PgServer tasks currently need explicit lifecycle ownership. A generation CommittedCut is not SQL readiness. No writable HTTP/MCP/MySQL/UDS must be attached without equivalent policy.

## Required meaningful tests before acceptance

- Owned duplex/wire two actual generations: same SELECT sees old then new only at idle cycle boundary; SELECT1;SELECT2 batch stays same generation despite publication between statements.
- Parse on old, publish, Bind/Execute/Sync remains old; next cycle revalidates named and unnamed statement against new. Force equal plan-cache epochs in both engines to prove pointer/generation identity invalidation.
- BEGIN/SELECT, publish, Sync and further SELECT still old; failed transaction remains old through error-Sync; ROLLBACK then next idle query moves. Backend PID, authenticated current_user, application_name, search_path, timezone/GUCs unchanged.
- ALTER column descriptor change causes0A000 at Bind/Describe/Execute with no mutation, recovery after Sync, statement name persists; same descriptor replans and returns new data.
- Suspended portal max_rows retains old generation and remaining rows through Sync/publication, then releases at completion/Close. Complete/orphan portals do not retain unbounded generations.
- Deny simple/extended COPY FROM, DMLCTE, SELECT INTO, direct nextval/setval/UDF, pure-looking builtin shadow, nested view containing mutation, table-function mutation, EXPLAIN ANALYZE wrapper. Verify raw sequence and in-memory sequence/ART/function state unchanged; RO RocksDB refusal after mutation is insufficient.
- Tenant startup is explicitly refused without Ready; normal primary listener unchanged.
- Actual server TCP accept chooses active lease and leaves auth/TLS settings unchanged; no placeholder readiness before first hydrated generation.
- Shutdown/cancel releases sessions, portal owners and DB Arcs before retiring directories; publication while old failed txn pinned respects resident cap/backpressure.
- Feature builds, full protocol regressions and startup/connect/OLTP performance comparison after independent reviews. None of these executable PG generation tests have run or been authored yet.

## Latest completed adjacent work

- gh36-delete-namespace-draft/gh36-delete-namespace.patch dad4aac... fixes real guard-default failure without weakening strict replay; transport+resync independent reviews passed, root integrates isolated admission candidate.
- Standby protocol admission f345e04... addresses runtime-proven logical standby COPY/extended local writes; separate SELECT-effect limitation still documented.
- DDL CREATE TABLE scoped phase draft4b644616... independently reviewed by transport; remaining DDL audit belongs to root.
- Raw CLI NON-SERVING harness a3d92ab3... + RO inspector7e4e1e91... independent resync review passed; needs exact compile/run under root host lock.
