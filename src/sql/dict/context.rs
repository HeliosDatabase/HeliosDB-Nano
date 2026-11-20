//! What a system-view producer may read.
//!
//! A producer used to receive the storage engine and nothing else, so a view
//! over anything the engine does not hold (routines, triggers, tenants and
//! their policies, the session asking) could only be empty. [`DictContext`]
//! carries the storage engine and optional handles to the rest.
//!
//! The handles of an open database are recorded once, at open, under the
//! storage engine's instance id ([`install_sources`]); the scan operator, which
//! holds only the storage engine, finds them again with
//! [`DictContext::for_engine`]. Only weak references are recorded, so the
//! record never keeps a database open. Two databases open in one process have
//! two records and never read each other's.

use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use crate::session::scoped::{BackendSnapshot, SessionScopedState};
use crate::sql::{FunctionRegistry, TriggerRegistry};
use crate::storage::StorageEngine;
use crate::tenant::TenantManager;

/// The name reported for a session that has published no login identity.
const SERVICE_USER: &str = "heliosdb";
/// The name of the branch a session is on when it has switched to none.
const MAIN_BRANCH: &str = "main";

struct InstalledSources {
    engine: u64,
    functions: Weak<FunctionRegistry>,
    triggers: Weak<TriggerRegistry>,
    tenants: Weak<TenantManager>,
}

static INSTALLED: OnceLock<Mutex<Vec<InstalledSources>>> = OnceLock::new();

fn installed() -> &'static Mutex<Vec<InstalledSources>> {
    INSTALLED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Record the dictionary sources of the database that owns `storage`. Called
/// once per open. Records of databases that have closed are dropped here.
pub(crate) fn install_sources(
    storage: &StorageEngine,
    functions: &Arc<FunctionRegistry>,
    triggers: &Arc<TriggerRegistry>,
    tenants: &Arc<TenantManager>,
) {
    let engine = storage.instance_id();
    let mut guard = installed().lock();
    guard.retain(|entry| entry.engine != engine && entry.functions.strong_count() > 0);
    guard.push(InstalledSources {
        engine,
        functions: Arc::downgrade(functions),
        triggers: Arc::downgrade(triggers),
        tenants: Arc::downgrade(tenants),
    });
}

/// Everything a system-view producer may read.
pub struct DictContext<'a> {
    storage: &'a StorageEngine,
    functions: Option<Arc<FunctionRegistry>>,
    triggers: Option<Arc<TriggerRegistry>>,
    tenants: Option<Arc<TenantManager>>,
    session: Option<Arc<SessionScopedState>>,
}

impl<'a> DictContext<'a> {
    /// A context with the storage engine only: every optional source is
    /// absent. For callers that hold a bare storage engine no database owns.
    pub fn new(storage: &'a StorageEngine) -> Self {
        Self {
            storage,
            functions: None,
            triggers: None,
            tenants: None,
            session: None,
        }
    }

    /// The context of the database that owns `storage`: its routine and
    /// trigger registries and its tenant manager when that database is open,
    /// and the session running the current statement.
    pub fn for_engine(storage: &'a StorageEngine) -> Self {
        let engine = storage.instance_id();
        let (functions, triggers, tenants) = {
            let guard = installed().lock();
            guard
                .iter()
                .rev()
                .find(|entry| entry.engine == engine)
                .map_or((None, None, None), |entry| {
                    (entry.functions.upgrade(), entry.triggers.upgrade(), entry.tenants.upgrade())
                })
        };
        Self {
            storage,
            functions,
            triggers,
            tenants,
            session: crate::session_scoped_state_tls(),
        }
    }

    /// Replace the routine registry (a database building its own context).
    #[must_use]
    pub fn with_functions(mut self, functions: Arc<FunctionRegistry>) -> Self {
        self.functions = Some(functions);
        self
    }

    /// Replace the trigger registry.
    #[must_use]
    pub fn with_triggers(mut self, triggers: Arc<TriggerRegistry>) -> Self {
        self.triggers = Some(triggers);
        self
    }

    /// Replace the tenant manager.
    #[must_use]
    pub fn with_tenants(mut self, tenants: Arc<TenantManager>) -> Self {
        self.tenants = Some(tenants);
        self
    }

    /// The storage engine: tables, indexes, views, sequences, branches.
    pub fn storage(&self) -> &'a StorageEngine {
        self.storage
    }

    /// User functions and procedures.
    pub fn functions(&self) -> Option<&FunctionRegistry> {
        self.functions.as_deref()
    }

    /// Trigger definitions.
    pub fn triggers(&self) -> Option<&TriggerRegistry> {
        self.triggers.as_deref()
    }

    /// Tenants and their row-level-security policies.
    pub fn tenants(&self) -> Option<&TenantManager> {
        self.tenants.as_deref()
    }

    /// The session running the current statement, when there is one.
    pub fn session(&self) -> Option<&SessionScopedState> {
        self.session.as_deref()
    }

    /// The login name of the current session; the service user when the
    /// session has published none.
    pub fn current_user(&self) -> String {
        self.session
            .as_ref()
            .map(|session| session.username())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| SERVICE_USER.to_string())
    }

    /// The branch the current session is on, `main` when it is on none of
    /// this database's branches.
    pub fn current_branch(&self) -> String {
        let engine = self.storage.instance_id();
        self.session
            .as_ref()
            .and_then(|session| session.branch_binding())
            .filter(|binding| binding.engine_instance == engine)
            .map_or_else(|| MAIN_BRANCH.to_string(), |binding| binding.name.clone())
    }

    /// The live sessions of this database, ordered by backend pid.
    pub fn live_sessions(&self) -> Vec<BackendSnapshot> {
        crate::session::scoped::snapshot_live_backends(self.storage.instance_id())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;

    #[test]
    fn a_bare_context_has_storage_and_nothing_else() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let ctx = DictContext::new(&db.storage);
        assert!(ctx.functions().is_none() && ctx.triggers().is_none() && ctx.tenants().is_none());
        assert_eq!(ctx.storage().instance_id(), db.storage.instance_id());
        assert_eq!(ctx.current_branch(), "main");
    }

    #[test]
    fn an_open_database_is_found_by_its_storage_engine() {
        let a = EmbeddedDatabase::new_in_memory().unwrap();
        let b = EmbeddedDatabase::new_in_memory().unwrap();
        a.execute("CREATE FUNCTION ctx_only_a(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
            .unwrap();
        let ctx_a = DictContext::for_engine(&a.storage);
        let ctx_b = DictContext::for_engine(&b.storage);
        assert!(ctx_a.functions().expect("registry of a").function_exists("ctx_only_a"));
        assert!(!ctx_b.functions().expect("registry of b").function_exists("ctx_only_a"));
        assert!(ctx_a.triggers().is_some() && ctx_a.tenants().is_some());
        assert!(!ctx_a.current_user().is_empty());
    }

    #[test]
    fn a_closed_database_leaves_no_sources_behind() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let storage = Arc::clone(&db.storage);
        assert!(DictContext::for_engine(&storage).functions().is_some());
        drop(db);
        assert!(
            DictContext::for_engine(&storage).functions().is_none(),
            "the record holds weak references only"
        );
    }
}
