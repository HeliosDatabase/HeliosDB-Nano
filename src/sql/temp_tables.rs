//! Session-scoped `CREATE TEMPORARY TABLE` — sprinter `1703dba8e82d`.
//!
//! # The defect this module exists to close
//!
//! `TEMPORARY` was parsed and thrown away. `Planner`'s `Statement::CreateTable`
//! arm never read `create_table.temporary`, so `CREATE TEMPORARY TABLE t (…)`
//! took the byte-identical path a plain `CREATE TABLE t (…)` takes: one
//! durable, globally-visible row at the storage key `t`. Every other connection
//! could read it, write it and drop it; it survived the disconnect; and two
//! sessions running the same code collided on the name.
//!
//! That is a statement returning SUCCESS for a promise it does not keep, which
//! is strictly worse than refusing it.
//!
//! # The design, and why it is this one
//!
//! A temp table is stored under a **per-backend schema key** —
//! `pg_temp_<backend_pid>.t` — which is structurally the SAME thing the engine
//! already produces for `SET search_path TO s; CREATE TABLE t` (the key `s.t`,
//! see `Planner::schema_qualified_key`). Nothing in the storage layer, the
//! executor, the index maintenance or the constraint machinery had to learn a
//! new concept: a temp table is an ordinary table in a schema whose name says
//! whose it is.
//!
//! Three properties fall out of that choice, and each one is a test in
//! `tests/temporary_tables_i3.rs`:
//!
//! * **Isolation.** A bare reference from another session probes ITS temp
//!   schema (or none) and then `public`; neither key exists, so the name does
//!   not resolve and the wire reports `42P01 undefined_table` — PostgreSQL's
//!   answer.
//! * **No collisions.** Two sessions may each hold `tmp_foo`, because the keys
//!   are `pg_temp_7.tmp_foo` and `pg_temp_9.tmp_foo`. PostgreSQL gives each
//!   backend its own `pg_temp_NNN` schema for exactly this reason, so the
//!   spelling is also the one a user already recognises.
//! * **Shadowing.** `pg_temp` is implicitly FIRST in PostgreSQL's search path,
//!   so a session's own `tmp_foo` shadows a permanent `public.tmp_foo` for that
//!   session only, and `public.tmp_foo` still reaches the permanent one. That
//!   is `Planner::resolve_table_ref`'s temp probe, which runs ahead of the
//!   declared `search_path` walk.
//!
//! # Why the namespace is the BACKEND PID and not the `SessionId`
//!
//! Both are process-monotonic and never reused, so either would isolate. The
//! backend pid wins on three counts:
//!
//! * it is the identifier the server ALREADY publishes to the client
//!   (`pg_backend_pid()`), so `SELECT * FROM pg_temp_<pg_backend_pid()>.t`
//!   works and the schema name is predictable the way PostgreSQL's is;
//! * it lives on [`SessionScopedState`](crate::session::scoped::SessionScopedState)
//!   — the established home for per-connection state — and is immutable for the
//!   life of the connection, so there is no new field, no new lock and no new
//!   lifetime to reason about;
//! * the EMBEDDED library handle has one too (`EmbeddedDatabase::embedded_scoped`),
//!   so `db.execute("CREATE TEMPORARY TABLE …")` gets a private namespace
//!   without inventing a session for a caller that has none.
//!
//! # The two carriers, and why there are two
//!
//! [`current_temp_namespace`] answers "whose statement is running on this
//! thread". It reads, in order:
//!
//! 1. the **protocol-layer override** ([`TempNamespaceGuard`]) — installed by
//!    the wire handlers around the catalog interceptors, which run AHEAD of any
//!    engine entry point and therefore ahead of the per-statement scoped-state
//!    guard. This is the standing trap in this codebase: a protocol-layer
//!    caller that resolves session state from the engine's thread-local finds
//!    nothing, because the engine has not been entered yet. The handlers hold a
//!    `SessionId`, so they install the answer explicitly;
//! 2. the engine's per-statement scoped state
//!    (`crate::session_scoped_state_tls`), which every `_for_session` entry
//!    point and every embedded funnel installs before a statement runs.
//!
//! Absent both, the answer is `None` — and `None` means **see no temp tables at
//! all**, never "see everyone's". Every visibility decision in this module
//! fails closed, so a caller with no session identity (a dump, a checkpoint, a
//! REST request, a background sweep) is treated as entitled to nothing rather
//! than to everything.
//!
//! # Lifetime
//!
//! Two independent reclaim paths, because either one alone leaks:
//!
//! * **Session end** — [`drop_backend_temp_tables`], called from
//!   `EmbeddedDatabase::destroy_session`. That is the ONE funnel every
//!   disconnect reaches (both wire handlers call it from their `Drop`), so a
//!   clean Terminate, a dropped socket and an error-path teardown all reclaim.
//!   It is the same argument the advisory-lock release and the tenant
//!   connection-slot release in that function already rely on.
//! * **Database open** — [`sweep_orphaned_temp_tables`], called from
//!   `StorageEngine::open`. A `SIGKILL` runs no `Drop`, so a crash CAN leave
//!   `pg_temp_7.t` in the catalog; and the backend-pid counter restarts at 1 in
//!   the next process, so the seventh connection of the new process would
//!   otherwise INHERIT a dead session's table. Sweeping at open is what makes
//!   the restart case fail closed instead of resurrecting a temp table as a
//!   permanent, globally-visible relation. PostgreSQL does the same thing
//!   lazily (autovacuum removes orphaned temp schemas); doing it eagerly at
//!   open is strictly safer and costs one prefix scan of a key range that is
//!   empty on every clean start.
//!
//! # `UNLOGGED` is deliberately NOT changed here
//!
//! The item pairs `UNLOGGED` with `TEMPORARY`. They are not the same problem:
//!
//! * `CREATE UNLOGGED TABLE t (…)` does not parse at all — `sqlparser` 0.53's
//!   `parse_create` has no `UNLOGGED` arm, so the statement is REJECTED today.
//!   It never silently succeeds, which is the entire complaint about
//!   `TEMPORARY`.
//! * `SELECT … INTO UNLOGGED t` does parse and is accepted-and-ignored — but an
//!   unlogged table in PostgreSQL is a PERMANENT, globally-visible table that
//!   merely is not crash-safe and is not replicated. Treating it as durable
//!   gives the caller a table that is *more* durable than asked for. Nothing
//!   leaks and nothing is lost, which is the opposite of the `TEMPORARY` case,
//!   where the table was more VISIBLE and more PERSISTENT than asked for.
//!
//! So `UNLOGGED` stays as it is, and the reasoning is recorded here rather than
//! left to be rediscovered.

use std::cell::RefCell;

/// The schema-name prefix every session-private table lives under.
///
/// PostgreSQL reserves the whole `pg_temp` / `pg_temp_NNN` family for exactly
/// this purpose, so borrowing the spelling costs nothing and buys recognition.
pub const TEMP_SCHEMA_PREFIX: &str = "pg_temp_";

/// The temp schema a backend owns. Pure function of the backend pid, so it
/// needs no storage and can never drift from the connection it names.
pub fn temp_schema_for(backend_pid: i32) -> String {
    format!("{TEMP_SCHEMA_PREFIX}{backend_pid}")
}

/// Is `schema` one of the reserved per-session temp namespaces?
///
/// Deliberately prefix-only rather than `prefix + all-digits`: the check gates
/// what a user may NAME, and a user who writes `pg_temp_wat` is reaching into
/// the reserved family whether or not the suffix parses as a pid.
pub fn is_temp_schema(schema: &str) -> bool {
    schema.starts_with(TEMP_SCHEMA_PREFIX)
}

/// The temp schema a storage KEY belongs to, if any.
///
/// Storage keys are `<schema>.<table>` (bare for `public`), so the schema is
/// everything before the FIRST `.`. A quoted identifier may itself contain a
/// dot, but it cannot produce a false positive here: a key whose first segment
/// starts with `pg_temp_` was written by this module, because
/// `Planner::resolve_create_target` is what stops a user from spelling one.
pub fn temp_schema_of_key(key: &str) -> Option<&str> {
    let schema = key.split_once('.')?.0;
    is_temp_schema(schema).then_some(schema)
}

/// Is this storage key a session-private temp table?
pub fn is_temp_key(key: &str) -> bool {
    temp_schema_of_key(key).is_some()
}

thread_local! {
    /// The temp namespace a PROTOCOL-LAYER caller has declared for the work it
    /// is about to do (see the module docs, "the two carriers").
    ///
    /// Per-STATEMENT and installed by [`TempNamespaceGuard`], exactly like the
    /// `search_path` override in `crate::SESSION_SCHEMA_OVERRIDE`: a session's
    /// statements hop Tokio workers, so the guard installs and clears around
    /// each one and no namespace can leak into the next connection a worker
    /// thread serves. The guard is `!Send` for the same reason the engine's
    /// scoped-state guard is — it must not be carried across an `.await`.
    static TEMP_NAMESPACE_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// RAII installer for the protocol-layer temp namespace.
///
/// Restores the PREVIOUS value on `Drop`, including on an unwinding panic, so a
/// nested install (a catalog interceptor reached from inside another handler)
/// cannot clear the outer one early.
pub struct TempNamespaceGuard {
    previous: Option<String>,
    /// Makes "never held across an `.await`" structural rather than a comment.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl TempNamespaceGuard {
    /// Publish `backend_pid`'s temp namespace for the duration of one unit of
    /// protocol-layer work.
    pub fn install(backend_pid: i32) -> Self {
        Self::install_namespace(temp_schema_for(backend_pid))
    }

    /// Publish an already-composed namespace. Used by the reclaim paths, which
    /// know the namespace from the storage KEY they are about to drop rather
    /// than from a live connection — there is no backend left to ask.
    fn install_namespace(namespace: String) -> Self {
        let previous = TEMP_NAMESPACE_OVERRIDE.with(|c| c.borrow_mut().replace(namespace));
        Self {
            previous,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for TempNamespaceGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        TEMP_NAMESPACE_OVERRIDE.with(|c| *c.borrow_mut() = previous);
    }
}

/// The temp schema of whoever's statement is running on this thread, or `None`.
///
/// `None` is "this caller owns no temp namespace", which every visibility
/// decision below reads as *see nothing*, never as *see everything*.
pub fn current_temp_namespace() -> Option<String> {
    if let Some(explicit) = TEMP_NAMESPACE_OVERRIDE.with(|c| c.borrow().clone()) {
        return Some(explicit);
    }
    crate::session_scoped_state_tls().map(|s| temp_schema_for(s.backend_pid()))
}

/// Has the connection whose statement is running on this thread created a temp
/// table?
///
/// Narrower than [`current_temp_namespace`] on purpose. The namespace exists
/// for every session — it is just a function of the backend pid — but only a
/// session that actually OWNS a temp table may:
///
/// * skip the shared, SQL-text-keyed plan and result caches (a plan whose bare
///   `t` resolved to `pg_temp_7.t` must never be handed to a session whose `t`
///   is `public.t`), and
/// * pay the implicit `pg_temp`-first catalog probe on every bare name.
///
/// Those two must agree, always: a session that probes into its temp namespace
/// but keeps using the shared plan cache would publish its private plan to
/// every other connection, which is this item's leak wearing a different hat.
/// One predicate serves both so they cannot drift.
pub fn caller_holds_temp_tables() -> bool {
    crate::session_scoped_state_tls().is_some_and(|s| s.holds_temp_tables())
}

/// Record that the calling connection has just created a temp table.
///
/// Called from `storage::Catalog::create_table` — the single registration site
/// for any table — so the session flag and the per-engine hint are armed by the
/// same event and cannot disagree about whether temp tables are in play.
pub fn note_temp_table_created(storage: &crate::storage::StorageEngine) {
    note_temp_table_planned(Some(storage));
}

/// Arm the same two flags at PLAN time, from `Planner::resolve_create_target`.
///
/// Earlier than [`note_temp_table_created`] on purpose, and the earliness is
/// load-bearing rather than an optimisation: the plan a temp `CREATE` produces
/// names THIS connection's private namespace, and the shared, SQL-text-keyed
/// plan cache would otherwise hand that plan to the next connection sending the
/// same statement text — so the gate that keeps it out of the cache has to be
/// armed before the plan is returned, not when the catalog row is finally
/// written.
///
/// `storage` is `None` only for the catalog-less `Planner::new()` (test and
/// pure-AST paths), which has no engine to leak into.
pub fn note_temp_table_planned(storage: Option<&crate::storage::StorageEngine>) {
    if let Some(storage) = storage {
        storage.note_temp_table_present();
    }
    if let Some(state) = crate::session_scoped_state_tls() {
        state.note_temp_table();
    }
}

/// May the caller identified by `mine` see the table at storage key `key`?
///
/// Permanent tables are visible to everyone (that is the control case in
/// `tests/temporary_tables_i3.rs`); a temp table is visible only to the backend
/// whose namespace it is in.
pub fn key_is_visible(key: &str, mine: Option<&str>) -> bool {
    match temp_schema_of_key(key) {
        None => true,
        Some(owner) => mine == Some(owner),
    }
}

/// PostgreSQL's `42P16 invalid_table_definition` wording for a `CREATE
/// TEMPORARY` that names a non-temporary schema, and the reserved-name refusal
/// for a plain `CREATE` that reaches into the `pg_temp_*` family.
///
/// Owned here, by the single emitter, so the wire layer's message-shape
/// SQLSTATE classification cannot drift from the text — the marker-const
/// discipline `session::scoped`'s `LASTVAL_UNDEFINED_MESSAGE` already follows.
pub const TEMP_IN_NON_TEMP_SCHEMA: &str = "cannot create temporary relation in non-temporary schema";

/// Refuse an explicit `pg_temp_*` qualifier on a NON-temporary create.
///
/// Without this a session could spell `CREATE TABLE pg_temp_9.t (…)` and plant
/// a relation inside another live backend's private namespace — the very leak
/// this module closes, re-opened from the other direction. PostgreSQL rejects
/// the same statement (`pg_temp` is not a schema a user may create into by
/// name), so refusing costs no compatibility.
pub fn reserved_temp_schema_message(key: &str) -> String {
    let schema = temp_schema_of_key(key).unwrap_or(TEMP_SCHEMA_PREFIX);
    format!("schema \"{schema}\" is reserved for session-private temporary tables")
}

/// Drop every temp table belonging to `backend_pid`. Returns how many could
/// NOT be reclaimed.
///
/// Called from `EmbeddedDatabase::destroy_session` — the ONE funnel every
/// disconnect reaches (both wire handlers call it from their `Drop`), so a
/// clean Terminate, a dropped socket and an error-path teardown all reclaim.
/// Best-effort per table: one undroppable relation must not strand the rest,
/// and a disconnect has nobody to report an error to.
pub fn drop_backend_temp_tables(storage: &crate::storage::StorageEngine, backend_pid: i32) -> usize {
    // Every disconnect on every deployment reaches this; the overwhelming
    // majority of them never created a temp table, and they must not pay a
    // catalog scan for it.
    if !storage.temp_tables_present() {
        return 0;
    }
    let namespace = temp_schema_for(backend_pid);
    drop_temp_tables_matching(storage, |key| temp_schema_of_key(key) == Some(namespace.as_str()))
}

/// Drop EVERY temp table in the catalog, whoever owned it. Returns how many
/// could NOT be reclaimed — a non-zero answer is the caller's signal to arm
/// `StorageEngine::note_temp_table_present`, because a survivor must stay
/// hidden rather than read as a permanent table.
///
/// Called once from `StorageEngine::open`. See the module docs for why a
/// session-teardown hook alone is not enough: a `SIGKILL` runs no `Drop`, and
/// the backend-pid counter restarts at 1 in the next process, so a survivor
/// would be silently adopted by an unrelated connection.
pub fn sweep_orphaned_temp_tables(storage: &crate::storage::StorageEngine) -> usize {
    drop_temp_tables_matching(storage, is_temp_key)
}

/// The shared body of the two reclaim paths. Returns the number of MATCHING
/// tables still present when it finishes.
///
/// Enumerates through the UNFILTERED listing on purpose: the filtered
/// [`Catalog::list_tables`](crate::storage::Catalog::list_tables) hides exactly
/// the rows this is trying to delete — the sweep runs with no session identity
/// at all, and the teardown runs after the session's own thread-locals are
/// gone. Reading through the filter here would have made both reclaim paths
/// silently no-ops, which is the failure mode this whole item is about.
fn drop_temp_tables_matching<F>(storage: &crate::storage::StorageEngine, mut wanted: F) -> usize
where
    F: FnMut(&str) -> bool,
{
    let catalog = storage.catalog();
    let Ok(keys) = catalog.list_tables_including_temp() else {
        // An unreadable catalog is not proof that nothing is there. Report one
        // unreclaimed table so the caller arms the visibility filter.
        return 1;
    };
    let (mut dropped, mut failed) = (0usize, 0usize);
    for key in keys.into_iter().filter(|k| wanted(k.as_str())) {
        // Act AS the namespace being reclaimed, for the whole drop.
        //
        // `Catalog::drop_table` is a deep teardown (index definitions, ART
        // trees, triggers, statistics, data rows), and anything inside it that
        // resolves the relation goes through `Catalog::get_table_schema` —
        // which refuses a temp key that is not the caller's. Without this the
        // reclaim would be refused by the very guard it exists to enforce, and
        // a dead session's table would survive its own cleanup.
        let Some(namespace) = temp_schema_of_key(&key) else {
            continue;
        };
        let _acting_as = TempNamespaceGuard::install_namespace(namespace.to_string());
        match catalog.drop_table(&key) {
            Ok(()) => dropped += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!("temp-table reclaim: could not drop '{key}': {e}");
            }
        }
    }
    if dropped > 0 {
        tracing::debug!("temp-table reclaim: dropped {dropped} session-private table(s)");
    }
    failed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_key_names_the_backend_that_owns_it() {
        assert_eq!(temp_schema_for(7), "pg_temp_7");
        assert_eq!(temp_schema_of_key("pg_temp_7.t"), Some("pg_temp_7"));
        assert_eq!(temp_schema_of_key("public_temp.t"), None);
        // A bare (public) key is never temp, however it is spelled.
        assert_eq!(temp_schema_of_key("pg_temp_7"), None);
        assert_eq!(temp_schema_of_key("t"), None);
        // An ordinary schema-qualified key is not temp.
        assert_eq!(temp_schema_of_key("s.t"), None);
    }

    /// The load-bearing invariant: absent a namespace, a caller sees NO temp
    /// tables — never every temp table.
    #[test]
    fn visibility_fails_closed_without_a_namespace() {
        assert!(key_is_visible("t", None), "a permanent table is visible to everyone");
        assert!(key_is_visible("s.t", None), "a schema-qualified table is not temp");
        assert!(
            !key_is_visible("pg_temp_7.t", None),
            "a caller with no namespace saw a temp table"
        );
        assert!(!key_is_visible("pg_temp_7.t", Some("pg_temp_9")), "a temp table leaked");
        assert!(key_is_visible("pg_temp_7.t", Some("pg_temp_7")));
    }

    /// The protocol-layer override wins over the engine's scoped state, and
    /// unwinds cleanly.
    #[test]
    fn the_protocol_override_is_installed_and_restored() {
        assert!(current_temp_namespace().is_none());
        {
            let _outer = TempNamespaceGuard::install(3);
            assert_eq!(current_temp_namespace().as_deref(), Some("pg_temp_3"));
            {
                let _inner = TempNamespaceGuard::install(4);
                assert_eq!(current_temp_namespace().as_deref(), Some("pg_temp_4"));
            }
            assert_eq!(
                current_temp_namespace().as_deref(),
                Some("pg_temp_3"),
                "a nested guard cleared the outer namespace"
            );
        }
        assert!(current_temp_namespace().is_none(), "a namespace leaked past its guard");
    }
}
