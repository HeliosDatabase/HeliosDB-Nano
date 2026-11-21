//! The catalog of row-level security: which tables have it switched on, their
//! policies and their owners.
//!
//! With a storage engine attached (every database handle) the records in the
//! store are the truth and this type holds a loaded image of them, rebuilt
//! whenever the engine's schema generation moves. Every change of a record
//! moves it, a `CREATE` / `DROP` / `RENAME` of a table moves it, and a physical
//! standby moves it when it applies catalog bytes, so the image never outlives
//! what it was read from. Without an engine (a bare `TenantManager`) the image
//! is all there is.
//!
//! A database with no row-level security answers [`PolicyStore::active`] with
//! two atomic loads.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use super::record::{
    self, PolicyRecord, TableId, TableSecurity, OWNER_PREFIX, POLICY_PREFIX, ROWSEC_PREFIX, TENANT_PREFIX,
};
use crate::sql::LogicalExpr;
use crate::storage::StorageEngine;
use crate::{Error, Result};

/// Key family of the table incarnation ids (`storage::catalog`).
const INCARNATION_PREFIX: &str = "meta:table_incarnation:";

/// Everything known about one table.
#[derive(Debug, Default)]
pub struct TableEntry {
    /// The table's current name (its storage key).
    pub name: String,
    /// ENABLE / FORCE.
    pub security: TableSecurity,
    /// The role that created the table; `None` is the bootstrap superuser.
    pub owner: Option<String>,
    /// Its policies, by name.
    pub policies: Vec<Arc<PolicyRecord>>,
    /// Policy expressions already turned into plan expressions, by their text.
    /// The entry is rebuilt on every schema change, so nothing here outlives
    /// the table definition it was planned against.
    compiled: parking_lot::Mutex<HashMap<String, LogicalExpr>>,
}

impl TableEntry {
    /// The compiled form of `text`, compiling it with `compile` the first time.
    pub(crate) fn compiled(&self, text: &str, compile: impl FnOnce() -> Result<LogicalExpr>) -> Result<LogicalExpr> {
        if let Some(found) = self.compiled.lock().get(text) {
            return Ok(found.clone());
        }
        let expr = compile()?;
        self.compiled.lock().insert(text.to_string(), expr.clone());
        Ok(expr)
    }

    /// The policy named `name`.
    pub fn policy(&self, name: &str) -> Option<&Arc<PolicyRecord>> {
        self.policies.iter().find(|p| p.name == name)
    }

    fn with_parts(name: &str, security: TableSecurity, owner: Option<String>, policies: Vec<Arc<PolicyRecord>>) -> Self {
        Self {
            name: name.to_string(),
            security,
            owner,
            policies,
            compiled: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Default)]
struct Image {
    tables: HashMap<String, Arc<TableEntry>>,
    /// Names of the materialized views (a read of one may need its base
    /// tables' policies).
    materialized_views: HashSet<String>,
    /// Role name -> exempt (SUPERUSER or BYPASSRLS), as far as looked up.
    roles: parking_lot::Mutex<HashMap<String, bool>>,
    /// Set when the records could not be read: every lookup then fails, so a
    /// statement is refused instead of running unfiltered.
    unreadable: Option<String>,
}

/// See the module documentation.
pub struct PolicyStore {
    storage: OnceLock<Arc<StorageEngine>>,
    image: parking_lot::RwLock<Arc<Image>>,
    /// The schema generation the image was loaded at; `u64::MAX` before the
    /// first load.
    loaded_generation: AtomicU64,
    /// Does any table have row security switched on, or any policy?
    active: AtomicBool,
}

impl Default for PolicyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PolicyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyStore")
            .field("attached", &self.storage.get().is_some())
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// The roles that exist without a record and are superusers.
pub fn is_bootstrap_superuser(role: &str) -> bool {
    role == "postgres" || role == "helios"
}

impl PolicyStore {
    /// An empty store with no storage engine.
    pub fn new() -> Self {
        Self {
            storage: OnceLock::new(),
            image: parking_lot::RwLock::new(Arc::new(Image::default())),
            loaded_generation: AtomicU64::new(u64::MAX),
            active: AtomicBool::new(false),
        }
    }

    /// Bind the store to a database. Policies made before (through a bare
    /// manager) are written to it; from here on the stored records are the
    /// truth. A second call is ignored.
    pub fn attach(&self, storage: Arc<StorageEngine>) -> Result<()> {
        let earlier: Vec<Arc<PolicyRecord>> = self
            .image
            .read()
            .tables
            .values()
            .flat_map(|entry| entry.policies.iter().cloned())
            .collect();
        if self.storage.set(storage).is_err() {
            return Ok(());
        }
        for policy in earlier {
            self.put_policy(&policy.table, (*policy).clone())?;
        }
        self.reload_now();
        match &self.image.read().unreadable {
            Some(why) => Err(Error::storage(why.clone())),
            None => Ok(()),
        }
    }

    /// The attached storage engine.
    pub fn storage(&self) -> Option<&Arc<StorageEngine>> {
        self.storage.get()
    }

    fn sync(&self) {
        if let Some(storage) = self.storage.get() {
            if self.loaded_generation.load(Ordering::Acquire) != storage.schema_generation() {
                self.reload_now();
            }
        }
    }

    fn reload_now(&self) {
        let Some(storage) = self.storage.get() else { return };
        let mut slot = self.image.write();
        // Read the generation first: a change that lands while the records are
        // being read leaves the image older than the generation, and the next
        // call loads again.
        let generation = storage.schema_generation();
        if self.loaded_generation.load(Ordering::Acquire) == generation {
            // Another caller loaded it while this one waited for the lock.
            return;
        }
        let image = match Self::load(storage) {
            Ok(image) => image,
            Err(e) => Image {
                unreadable: Some(format!("row-level security records cannot be read: {e}")),
                ..Image::default()
            },
        };
        let active = image.unreadable.is_some()
            || image
                .tables
                .values()
                .any(|t| t.security.enabled || !t.policies.is_empty());
        *slot = Arc::new(image);
        self.active.store(active, Ordering::Release);
        self.loaded_generation.store(generation, Ordering::Release);
    }

    fn load(storage: &StorageEngine) -> Result<Image> {
        let switches = storage.meta_blobs_with_prefix(ROWSEC_PREFIX)?;
        let policies = storage.meta_blobs_with_prefix(POLICY_PREFIX)?;
        if switches.is_empty() && policies.is_empty() {
            return Ok(Image::default());
        }
        let mut by_incarnation: HashMap<u64, String> = HashMap::new();
        for (table, bytes) in storage.meta_blobs_with_prefix(INCARNATION_PREFIX)? {
            let incarnation: u64 = bincode::deserialize(&bytes)
                .map_err(|e| Error::storage(format!("table incarnation of '{table}': {e}")))?;
            by_incarnation.insert(incarnation, table);
        }

        struct Parts {
            security: TableSecurity,
            owner: Option<String>,
            policies: Vec<Arc<PolicyRecord>>,
        }
        fn part<'a>(parts: &'a mut HashMap<String, Parts>, table: &str) -> &'a mut Parts {
            parts.entry(table.to_string()).or_insert_with(|| Parts {
                security: TableSecurity::default(),
                owner: None,
                policies: Vec::new(),
            })
        }
        let mut parts: HashMap<String, Parts> = HashMap::new();
        for (suffix, bytes) in switches {
            // A record whose table is gone is left for the next change to remove.
            let Some(table) = record::parse_incarnation(&suffix).and_then(|i| by_incarnation.get(&i)) else {
                continue;
            };
            part(&mut parts, table).security = record::decode_table_security(&bytes)?;
        }
        for (suffix, bytes) in policies {
            let table = match record::split_policy_suffix(&suffix) {
                Some((TableId::Incarnation(i), _)) => match by_incarnation.get(&i) {
                    Some(table) => table.clone(),
                    None => continue,
                },
                Some((TableId::Name(name), _)) => name,
                None => return Err(Error::storage(format!("unreadable policy key '{POLICY_PREFIX}{suffix}'"))),
            };
            let mut policy = record::decode_policy(&bytes)?;
            policy.table.clone_from(&table);
            part(&mut parts, &table).policies.push(Arc::new(policy));
        }
        for (suffix, bytes) in storage.meta_blobs_with_prefix(OWNER_PREFIX)? {
            let Some(table) = record::parse_incarnation(&suffix).and_then(|i| by_incarnation.get(&i)) else {
                continue;
            };
            if let Some(found) = parts.get_mut(table) {
                found.owner = Some(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        let mut tables = HashMap::with_capacity(parts.len());
        for (name, mut found) in parts {
            found.policies.sort_by(|a, b| a.name.cmp(&b.name));
            tables.insert(
                name.clone(),
                Arc::new(TableEntry::with_parts(
                    &name,
                    found.security,
                    found.owner,
                    found.policies,
                )),
            );
        }
        let materialized_views = storage.mv_catalog().list_views()?.into_iter().collect();
        Ok(Image {
            tables,
            materialized_views,
            roles: parking_lot::Mutex::new(HashMap::new()),
            unreadable: None,
        })
    }

    fn image(&self) -> Arc<Image> {
        self.sync();
        Arc::clone(&self.image.read())
    }

    /// Does this database have any row-level security at all?
    pub fn active(&self) -> bool {
        self.sync();
        self.active.load(Ordering::Acquire)
    }

    /// The entry of `table`, if it has a switch or a policy.
    pub fn table(&self, table: &str) -> Result<Option<Arc<TableEntry>>> {
        let image = self.image();
        if let Some(why) = &image.unreadable {
            return Err(Error::storage(why.clone()));
        }
        Ok(image.tables.get(table).cloned())
    }

    /// Every table with a switch or a policy, by name.
    pub fn tables(&self) -> Result<Vec<Arc<TableEntry>>> {
        let image = self.image();
        if let Some(why) = &image.unreadable {
            return Err(Error::storage(why.clone()));
        }
        let mut tables: Vec<_> = image.tables.values().cloned().collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tables)
    }

    /// Is `name` a materialized view (as of the loaded image)?
    pub fn is_materialized_view(&self, name: &str) -> bool {
        self.image().materialized_views.contains(name)
    }

    /// The materialized view a scan of `name` reads: `name` itself, or the
    /// view whose stored rows the table `name` holds (a materialized view's
    /// rows live in a table of their own, which can be read by its name too).
    pub fn materialized_view_behind(&self, name: &str) -> Option<String> {
        let image = self.image();
        if image.materialized_views.contains(name) {
            return Some(name.to_string());
        }
        let stored = name.strip_prefix("__mv_")?;
        // A refresh works on `…__temp_<time>` and `…__old_<time>` copies.
        let view = stored.split("__temp_").next()?.split("__old_").next()?;
        image.materialized_views.contains(view).then(|| view.to_string())
    }

    /// Is `role` exempt from every policy (a superuser, or BYPASSRLS)?
    pub fn role_is_exempt(&self, role: &str) -> bool {
        if is_bootstrap_superuser(role) {
            return true;
        }
        let Some(storage) = self.storage.get() else {
            return false;
        };
        let image = self.image();
        if let Some(known) = image.roles.lock().get(role) {
            return *known;
        }
        // A role record that cannot be read exempts nobody.
        let exempt = matches!(
            storage.catalog().get_role(role),
            Ok(Some(record)) if record.rolsuper || record.rolbypassrls
        );
        image.roles.lock().insert(role.to_string(), exempt);
        exempt
    }

    /// Edit the in-memory image of a store with no storage engine.
    fn edit_unattached(&self, table: &str, edit: impl FnOnce(&mut TableSecurity, &mut Vec<Arc<PolicyRecord>>)) {
        let mut slot = self.image.write();
        let mut tables: HashMap<String, Arc<TableEntry>> = slot.tables.clone();
        let (mut security, mut policies) = match tables.get(table) {
            Some(entry) => (entry.security, entry.policies.clone()),
            None => (TableSecurity::default(), Vec::new()),
        };
        edit(&mut security, &mut policies);
        policies.sort_by(|a, b| a.name.cmp(&b.name));
        if security == TableSecurity::default() && policies.is_empty() {
            tables.remove(table);
        } else {
            tables.insert(
                table.to_string(),
                Arc::new(TableEntry::with_parts(table, security, None, policies)),
            );
        }
        let active = tables.values().any(|t| t.security.enabled || !t.policies.is_empty());
        *slot = Arc::new(Image {
            tables,
            ..Image::default()
        });
        self.active.store(active, Ordering::Release);
    }

    /// Store a policy (replacing one of the same name on the same table).
    pub fn put_policy(&self, table: &str, mut policy: PolicyRecord) -> Result<()> {
        match self.storage.get() {
            Some(storage) => {
                write_policy(storage, table, policy)?;
                self.reload_now();
            }
            None => {
                policy.table = table.to_string();
                if policy.oid == 0 {
                    policy.oid = stable_oid(table, &policy.name);
                }
                self.edit_unattached(table, |_, policies| {
                    policies.retain(|p| p.name != policy.name);
                    policies.push(Arc::new(policy));
                });
            }
        }
        Ok(())
    }

    /// Remove a policy. `false` when there was none.
    pub fn delete_policy(&self, table: &str, name: &str) -> Result<bool> {
        let existed = self.table(table)?.is_some_and(|entry| entry.policy(name).is_some());
        if !existed {
            return Ok(false);
        }
        match self.storage.get() {
            Some(storage) => {
                remove_policy(storage, table, name)?;
                self.reload_now();
            }
            None => self.edit_unattached(table, |_, policies| policies.retain(|p| p.name != name)),
        }
        Ok(true)
    }

    /// Set a table's switches.
    pub fn set_table_security(&self, table: &str, security: TableSecurity) -> Result<()> {
        match self.storage.get() {
            Some(storage) => {
                write_table_security(storage, table, security)?;
                self.reload_now();
            }
            None => self.edit_unattached(table, |current, _| *current = security),
        }
        Ok(())
    }

    // ---- the tenant registry -------------------------------------------

    /// Store a tenant.
    pub fn put_tenant(&self, tenant: &record::TenantRecord) -> Result<()> {
        if let Some(storage) = self.storage.get() {
            storage.put(&record::tenant_key(&tenant.id), &record::encode_tenant(tenant)?)?;
            // Catalog-visible; also what makes a standby's manager read it.
            storage.bump_schema_generation();
        }
        Ok(())
    }

    /// Remove a tenant.
    pub fn delete_tenant(&self, id: &str) -> Result<()> {
        if let Some(storage) = self.storage.get() {
            storage.delete(&record::tenant_key(id))?;
            storage.bump_schema_generation();
        }
        Ok(())
    }

    /// Every stored tenant.
    pub fn load_tenants(&self) -> Result<Vec<record::TenantRecord>> {
        let Some(storage) = self.storage.get() else {
            return Ok(Vec::new());
        };
        storage
            .meta_blobs_with_prefix(TENANT_PREFIX)?
            .iter()
            .map(|(_, bytes)| record::decode_tenant(bytes))
            .collect()
    }
}

/// The stored row-level security of every table, read from `storage` now.
/// What a statement that changes it checks against.
pub(crate) fn read_tables(storage: &StorageEngine) -> Result<HashMap<String, Arc<TableEntry>>> {
    Ok(PolicyStore::load(storage)?.tables)
}

/// The role that owns `table`: its recorded creator, `None` for the bootstrap
/// superuser. (The merge point with the ownership catalog of the privilege
/// gate.)
pub(crate) fn table_owner(storage: &StorageEngine, table: &str) -> Result<Option<String>> {
    let Some(incarnation) = storage.catalog().main_table_incarnation(table)? else {
        return Ok(None);
    };
    Ok(storage
        .get(&record::owner_key(incarnation))?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// The role a table created now belongs to: the session's login role, unless
/// that is a bootstrap superuser (an unrecorded owner is the bootstrap
/// superuser) or there is none.
pub(crate) fn creating_role() -> Option<String> {
    let role = crate::session_login_user_tls()?;
    (!is_bootstrap_superuser(&role)).then(|| role.to_string())
}

/// Stage the deletion of everything this module stores for `table`, in the
/// batch that drops the table. Best effort, like the drop's other cleanups: a
/// record left behind names a table id that no longer exists, applies to
/// nothing, and is removed by [`remove_orphans`].
pub(crate) fn stage_table_drop(storage: &StorageEngine, table: &str, batch: &mut rocksdb::WriteBatch) {
    let Ok(Some(incarnation)) = storage.catalog().main_table_incarnation(table) else {
        return;
    };
    batch.delete(record::owner_key(incarnation));
    batch.delete(record::rowsec_key(incarnation));
    let prefix = format!("{POLICY_PREFIX}{incarnation:016x}:");
    if let Ok(policies) = storage.meta_blobs_with_prefix(&prefix) {
        for (suffix, _) in policies {
            batch.delete(format!("{prefix}{suffix}").into_bytes());
        }
    }
}

fn table_id(storage: &StorageEngine, table: &str) -> Result<TableId> {
    Ok(match storage.catalog().main_table_incarnation(table)? {
        Some(incarnation) => TableId::Incarnation(incarnation),
        None => TableId::Name(table.to_string()),
    })
}

/// Write a policy record (replacing one of the same name on the same table).
pub(crate) fn write_policy(storage: &StorageEngine, table: &str, mut policy: PolicyRecord) -> Result<()> {
    policy.table = table.to_string();
    if policy.oid == 0 {
        policy.oid = stable_oid(table, &policy.name);
    }
    // One record per (table, name): a policy stored under the table's name
    // before the table existed is replaced too.
    let id = table_id(storage, table)?;
    if matches!(id, TableId::Incarnation(_)) {
        storage.delete(&record::policy_key(&TableId::Name(table.to_string()), &policy.name))?;
    }
    storage.put(&record::policy_key(&id, &policy.name), &record::encode_policy(&policy)?)?;
    storage.bump_schema_generation();
    Ok(())
}

/// Delete a policy record.
pub(crate) fn remove_policy(storage: &StorageEngine, table: &str, name: &str) -> Result<()> {
    if let TableId::Incarnation(i) = table_id(storage, table)? {
        storage.delete(&record::policy_key(&TableId::Incarnation(i), name))?;
    }
    storage.delete(&record::policy_key(&TableId::Name(table.to_string()), name))?;
    storage.bump_schema_generation();
    Ok(())
}

/// Write a table's switches.
pub(crate) fn write_table_security(storage: &StorageEngine, table: &str, security: TableSecurity) -> Result<()> {
    let incarnation = storage
        .catalog()
        .main_table_incarnation(table)?
        .ok_or_else(|| Error::query_execution(format!("relation \"{table}\" does not exist")))?;
    let key = record::rowsec_key(incarnation);
    if security == TableSecurity::default() {
        storage.delete(&key)?;
    } else {
        storage.put(&key, &record::encode_table_security(&security)?)?;
    }
    remove_orphans(storage)?;
    storage.bump_schema_generation();
    Ok(())
}

/// Delete the records of tables that no longer exist. Run with a change of a
/// table's switches, so the records of dropped tables do not pile up.
fn remove_orphans(storage: &StorageEngine) -> Result<()> {
    let mut live: HashSet<u64> = HashSet::new();
    for (table, bytes) in storage.meta_blobs_with_prefix(INCARNATION_PREFIX)? {
        let incarnation: u64 = bincode::deserialize(&bytes)
            .map_err(|e| Error::storage(format!("table incarnation of '{table}': {e}")))?;
        live.insert(incarnation);
    }
    for prefix in [ROWSEC_PREFIX, OWNER_PREFIX] {
        for (suffix, _) in storage.meta_blobs_with_prefix(prefix)? {
            if record::parse_incarnation(&suffix).is_some_and(|i| !live.contains(&i)) {
                storage.delete(&format!("{prefix}{suffix}").into_bytes())?;
            }
        }
    }
    for (suffix, _) in storage.meta_blobs_with_prefix(POLICY_PREFIX)? {
        if let Some((TableId::Incarnation(i), _)) = record::split_policy_suffix(&suffix) {
            if !live.contains(&i) {
                storage.delete(&format!("{POLICY_PREFIX}{suffix}").into_bytes())?;
            }
        }
    }
    Ok(())
}

/// A number for `pg_policy.oid` that stays the same for the life of the
/// policy: it is stored in the record.
fn stable_oid(table: &str, policy: &str) -> u32 {
    // FNV-1a, folded into the positive range of an int4 above the reserved
    // catalog oids.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in table.bytes().chain([0u8]).chain(policy.bytes()) {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    0x0100_0000 + (hash % 0x7000_0000)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::record::PolicyCommand;
    use super::*;

    fn policy(name: &str) -> PolicyRecord {
        PolicyRecord {
            name: name.to_string(),
            table: String::new(),
            permissive: true,
            command: PolicyCommand::All,
            roles: Vec::new(),
            using: Some("true".to_string()),
            with_check: None,
            oid: 0,
        }
    }

    #[test]
    fn a_store_without_an_engine_keeps_policies_in_memory() {
        let store = PolicyStore::new();
        assert!(!store.active());
        store.put_policy("docs", policy("b")).unwrap();
        store.put_policy("docs", policy("a")).unwrap();
        assert!(store.active());
        let entry = store.table("docs").unwrap().unwrap();
        let names: Vec<&str> = entry.policies.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_ne!(entry.policies[0].oid, 0);
        assert!(store.delete_policy("docs", "a").unwrap());
        assert!(!store.delete_policy("docs", "a").unwrap());
        assert!(store.delete_policy("docs", "b").unwrap());
        assert!(store.table("docs").unwrap().is_none());
        assert!(!store.active());
    }

    #[test]
    fn the_switches_are_kept_without_an_engine() {
        let store = PolicyStore::new();
        store
            .set_table_security(
                "t",
                TableSecurity {
                    enabled: true,
                    forced: true,
                },
            )
            .unwrap();
        assert!(store.active());
        assert!(store.table("t").unwrap().unwrap().security.forced);
        store.set_table_security("t", TableSecurity::default()).unwrap();
        assert!(!store.active());
    }

    #[test]
    fn only_the_bootstrap_roles_are_exempt_without_an_engine() {
        let store = PolicyStore::new();
        assert!(store.role_is_exempt("postgres"));
        assert!(store.role_is_exempt("helios"));
        assert!(!store.role_is_exempt("alice"));
    }
}
