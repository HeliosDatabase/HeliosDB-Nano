//! The privilege catalog: grants, column grants, owners, role membership.
//!
//! Everything is stored as an ACL record in the existing `meta:acl:` namespace
//! (`Catalog::grant_privileges` / `revoke_privileges`), so it is written,
//! encrypted, replicated and scanned exactly as grants already were. The
//! record's object type says what a record is:
//!
//! | object type | object name            | grantee | privileges            |
//! |-------------|------------------------|---------|-----------------------|
//! | `table`     | relation key           | role    | table privileges      |
//! | `sequence` `schema` `function` `database` | object name | role | that kind's privileges |
//! | `column`    | relation, 0x1F, column | role    | column privileges     |
//! | `owner`     | kind, 0x1F, object     | owner   | `OWNER`               |
//! | `member`    | granted role           | member  | `MEMBER` (grantable = ADMIN OPTION) |
//! | `nodefault` | kind, 0x1F, object     | `public`| the default privilege that was revoked from PUBLIC |
//!
//! Readers work on a [`Snapshot`] of the whole catalog, rebuilt when the
//! storage engine's schema generation moves. Every writer here goes through
//! the catalog functions that bump it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::storage::{AclRecord, Catalog, RoleRecord, StorageEngine};
use crate::{Error, Result};

use super::access::{Principal, BOOTSTRAP_SUPERUSER};

/// Separator inside a composite object name. Not a character an identifier
/// can contain through SQL.
pub const SEP: char = '\u{1f}';

/// Object type of an ordinary relation grant (tables, views, materialized views).
pub const TABLE: &str = "table";
/// Object type of a sequence grant.
pub const SEQUENCE: &str = "sequence";
/// Object type of a schema grant.
pub const SCHEMA: &str = "schema";
/// Object type of a function or procedure grant.
pub const FUNCTION: &str = "function";
/// Object type of a database grant.
pub const DATABASE: &str = "database";
/// Object type of a column grant.
pub const COLUMN: &str = "column";
/// Object type of an ownership record.
pub const OWNER: &str = "owner";
/// Object type of a role membership record.
pub const MEMBER: &str = "member";
/// Object type of a "PUBLIC's default privilege was revoked" record.
pub const NODEFAULT: &str = "nodefault";

/// The pseudo-role every role belongs to.
pub const PUBLIC: &str = "public";

/// Privileges defined for each grantable object kind, in `ALL` expansion order.
pub fn privileges_of(kind: &str) -> &'static [&'static str] {
    match kind {
        TABLE => &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"],
        SEQUENCE => &["USAGE", "SELECT", "UPDATE"],
        SCHEMA => &["USAGE", "CREATE"],
        FUNCTION => &["EXECUTE"],
        DATABASE => &["CONNECT", "CREATE", "TEMPORARY"],
        _ => &[],
    }
}

/// The privileges that can be granted on single columns.
pub const COLUMN_PRIVILEGES: [&str; 4] = ["SELECT", "INSERT", "UPDATE", "REFERENCES"];

/// The privilege PUBLIC holds by default on an object of `kind`, if any.
/// `object` matters for schemas: only `public` is usable by default.
pub fn default_public_privileges(kind: &str, object: &str) -> &'static [&'static str] {
    match kind {
        FUNCTION => &["EXECUTE"],
        DATABASE => &["CONNECT", "TEMPORARY"],
        SCHEMA if object == PUBLIC => &["USAGE"],
        _ => &[],
    }
}

/// Composite object name of a column grant.
pub fn column_object(table: &str, column: &str) -> String {
    format!("{table}{SEP}{column}")
}

/// Composite object name of an owner / nodefault record.
pub fn kind_object(kind: &str, object: &str) -> String {
    format!("{kind}{SEP}{object}")
}

/// Lower-cased names of the user-defined functions and procedures. Read from
/// storage each time: defining a routine does not move the schema generation,
/// so this is not part of the [`Snapshot`].
pub fn routine_names(storage: &StorageEngine) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for prefix in ["meta:function:", "meta:procedure:"] {
        for (name, _) in storage.meta_blobs_with_prefix(prefix)? {
            names.insert(name.to_ascii_lowercase());
        }
    }
    Ok(names)
}

/// One role's membership in another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    /// The role that was granted.
    pub role: String,
    /// `WITH ADMIN OPTION`.
    pub admin: bool,
    /// Who granted it.
    pub grantor: String,
}

/// The whole privilege catalog at one schema generation.
#[derive(Debug, Default)]
pub struct Snapshot {
    roles: HashMap<String, RoleRecord>,
    /// `(kind, object)` to the grant records on it.
    grants: HashMap<(String, String), Vec<AclRecord>>,
    /// relation to `(column, record)`.
    column_grants: HashMap<String, Vec<(String, AclRecord)>>,
    /// `(kind, object)` to owner.
    owners: HashMap<(String, String), String>,
    /// member to the roles granted to it.
    memberships: HashMap<String, Vec<Membership>>,
    /// `(kind, object, privilege)` whose PUBLIC default was revoked.
    revoked_defaults: HashSet<(String, String, String)>,
}

type CacheSlot = (u64, Arc<Snapshot>);

fn cache() -> &'static parking_lot::RwLock<HashMap<u64, CacheSlot>> {
    static CACHE: std::sync::OnceLock<parking_lot::RwLock<HashMap<u64, CacheSlot>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::RwLock::new(HashMap::new()))
}

/// The privilege catalog of `storage`, shared until the schema generation moves.
pub fn snapshot(storage: &StorageEngine) -> Result<Arc<Snapshot>> {
    let engine = storage.instance_id();
    let generation = storage.schema_generation();
    if let Some((cached_generation, snapshot)) = cache().read().get(&engine) {
        if *cached_generation == generation {
            return Ok(Arc::clone(snapshot));
        }
    }
    let snapshot = Arc::new(Snapshot::load(storage)?);
    let mut slots = cache().write();
    // Engines come and go (tests open thousands); keep the map small.
    if slots.len() >= 256 {
        slots.clear();
    }
    slots.insert(engine, (generation, Arc::clone(&snapshot)));
    Ok(snapshot)
}

impl Snapshot {
    fn load(storage: &StorageEngine) -> Result<Self> {
        let catalog = storage.catalog();
        let mut snapshot = Snapshot::default();
        for role in catalog.list_roles()? {
            snapshot.roles.insert(role.name.clone(), role);
        }
        for record in catalog.list_acls()? {
            let record_type = record.object_type.clone();
            match record_type.as_str() {
                COLUMN => {
                    if let Some((table, column)) = record.object_name.split_once(SEP) {
                        snapshot
                            .column_grants
                            .entry(table.to_string())
                            .or_default()
                            .push((column.to_string(), record.clone()));
                    }
                }
                OWNER => {
                    if let Some((kind, object)) = record.object_name.split_once(SEP) {
                        snapshot
                            .owners
                            .insert((kind.to_string(), object.to_string()), record.grantee.clone());
                    }
                }
                MEMBER => {
                    let admin = record.privileges.iter().any(|(_, grantable)| *grantable);
                    snapshot
                        .memberships
                        .entry(record.grantee.clone())
                        .or_default()
                        .push(Membership {
                            role: record.object_name.clone(),
                            admin,
                            grantor: record.grantor.clone(),
                        });
                }
                NODEFAULT => {
                    if let Some((kind, object)) = record.object_name.split_once(SEP) {
                        for (privilege, _) in &record.privileges {
                            snapshot
                                .revoked_defaults
                                .insert((kind.to_string(), object.to_string(), privilege.clone()));
                        }
                    }
                }
                _ => snapshot
                    .grants
                    .entry((record.object_type.clone(), record.object_name.clone()))
                    .or_default()
                    .push(record),
            }
        }
        Ok(snapshot)
    }

    /// The role record, for a role in the catalog.
    pub fn role(&self, name: &str) -> Option<&RoleRecord> {
        self.roles.get(name)
    }

    /// True for a built-in role or a role in the catalog (not `public`).
    pub fn role_exists(&self, name: &str) -> bool {
        (crate::storage::is_reserved_role_name(name) && name != PUBLIC) || self.roles.contains_key(name)
    }

    /// True if the role is a superuser: a built-in role, or `SUPERUSER`.
    pub fn role_is_superuser(&self, name: &str) -> bool {
        if name == PUBLIC {
            return false;
        }
        if crate::storage::is_reserved_role_name(name) {
            return true;
        }
        self.roles.get(name).is_some_and(|role| role.rolsuper)
    }

    /// True if the principal bypasses every check.
    pub fn is_superuser(&self, principal: &Principal) -> bool {
        match principal {
            Principal::Unrestricted => true,
            Principal::Anonymous => false,
            Principal::Role { operator: true, .. } => true,
            Principal::Role { name, .. } => self.role_is_superuser(name),
        }
    }

    /// The recorded owner of an object; the bootstrap superuser when none is.
    pub fn owner_of(&self, kind: &str, object: &str) -> &str {
        self.owners
            .get(&(kind.to_string(), object.to_string()))
            .map_or(BOOTSTRAP_SUPERUSER, String::as_str)
    }

    /// The roles granted directly to `member`.
    pub fn direct_memberships(&self, member: &str) -> &[Membership] {
        self.memberships.get(member).map_or(&[], Vec::as_slice)
    }

    /// Every role `name` is a member of, directly or through other roles,
    /// whether or not privileges are inherited. Includes `name` itself.
    pub fn member_closure(&self, name: &str) -> Vec<String> {
        self.closure(name, false)
    }

    /// The identities whose privileges `name` holds: itself plus every role it
    /// inherits from. A role declared `NOINHERIT` does not pass on the
    /// privileges of the roles granted to it.
    pub fn identities(&self, name: &str) -> Vec<String> {
        self.closure(name, true)
    }

    fn closure(&self, name: &str, honour_inherit: bool) -> Vec<String> {
        let mut seen: Vec<String> = vec![name.to_string()];
        let mut index = 0;
        while let Some(current) = seen.get(index).cloned() {
            index += 1;
            let inherits = !honour_inherit || self.roles.get(&current).is_none_or(|role| role.rolinherit);
            if !inherits {
                continue;
            }
            for membership in self.direct_memberships(&current) {
                if !seen.iter().any(|known| known == &membership.role) {
                    seen.push(membership.role.clone());
                }
            }
        }
        seen
    }

    /// True if `member` may administer membership in `role`: it holds the role
    /// (directly or through others) `WITH ADMIN OPTION`.
    pub fn has_admin_option(&self, member: &str, role: &str) -> bool {
        self.member_closure(member).iter().any(|identity| {
            self.direct_memberships(identity)
                .iter()
                .any(|membership| membership.role == role && membership.admin)
        })
    }

    /// Whether any of `identities` (or PUBLIC) holds `privilege` on the whole
    /// object. `Some(true)` when one of them holds it `WITH GRANT OPTION`.
    pub fn object_grant(&self, kind: &str, object: &str, privilege: &str, identities: &[String]) -> Option<bool> {
        let mut found: Option<bool> = None;
        if let Some(records) = self.grants.get(&(kind.to_string(), object.to_string())) {
            for record in records {
                if record.grantee != PUBLIC && !identities.iter().any(|identity| identity == &record.grantee) {
                    continue;
                }
                for (held, grantable) in &record.privileges {
                    if held == privilege {
                        found = Some(found.unwrap_or(false) || *grantable);
                    }
                }
            }
        }
        if found.is_none()
            && default_public_privileges(kind, object).contains(&privilege)
            && !self
                .revoked_defaults
                .contains(&(kind.to_string(), object.to_string(), privilege.to_string()))
        {
            found = Some(false);
        }
        found
    }

    /// The columns of `table` on which any of `identities` (or PUBLIC) holds
    /// `privilege`. With `grantable_only`, only those held `WITH GRANT OPTION`.
    pub fn granted_columns(
        &self,
        table: &str,
        privilege: &str,
        identities: &[String],
        grantable_only: bool,
    ) -> BTreeSet<String> {
        let mut columns = BTreeSet::new();
        if let Some(records) = self.column_grants.get(table) {
            for (column, record) in records {
                if record.grantee != PUBLIC && !identities.iter().any(|identity| identity == &record.grantee) {
                    continue;
                }
                if record
                    .privileges
                    .iter()
                    .any(|(held, grantable)| held == privilege && (*grantable || !grantable_only))
                {
                    columns.insert(column.clone());
                }
            }
        }
        columns
    }

    /// Every grant record on `(kind, object)`.
    pub fn grant_records(&self, kind: &str, object: &str) -> &[AclRecord] {
        self.grants
            .get(&(kind.to_string(), object.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// Every column grant record on `table`, as `(column, record)`.
    pub fn column_records(&self, table: &str) -> &[(String, AclRecord)] {
        self.column_grants.get(table).map_or(&[], Vec::as_slice)
    }

    /// Objects of `kind` that `role` owns.
    pub fn owned_by(&self, role: &str) -> Vec<(String, String)> {
        let mut owned: Vec<(String, String)> = self
            .owners
            .iter()
            .filter(|(_, owner)| owner.as_str() == role)
            .map(|(key, _)| key.clone())
            .collect();
        owned.sort();
        owned
    }
}

// ---------------------------------------------------------------------------
// Writers. Each goes through `Catalog::grant_privileges` /
// `Catalog::revoke_privileges`, which bump the schema generation.
// ---------------------------------------------------------------------------

/// Record `owner` as the owner of an object, replacing any previous owner.
pub fn set_owner(catalog: &Catalog<'_>, kind: &str, object: &str, owner: &str) -> Result<()> {
    clear_owner(catalog, kind, object)?;
    catalog.grant_privileges(
        OWNER,
        &kind_object(kind, object),
        owner,
        owner,
        &["OWNER".to_string()],
        true,
    )
}

/// Remove the ownership record of an object (it returns to the bootstrap superuser).
pub fn clear_owner(catalog: &Catalog<'_>, kind: &str, object: &str) -> Result<()> {
    let composite = kind_object(kind, object);
    for record in catalog.list_acls()? {
        if record.object_type == OWNER && record.object_name == composite {
            catalog.revoke_privileges(OWNER, &composite, &record.grantee, &["OWNER".to_string()])?;
        }
    }
    Ok(())
}

fn delete_record(catalog: &Catalog<'_>, record: &AclRecord) -> Result<()> {
    let privileges: Vec<String> = record.privileges.iter().map(|(name, _)| name.clone()).collect();
    catalog.revoke_privileges(&record.object_type, &record.object_name, &record.grantee, &privileges)
}

/// True if the record describes `(kind, object)`: a grant on it, a column grant
/// on it (relations), its owner, or its revoked default.
fn record_is_about(record: &AclRecord, kind: &str, object: &str) -> bool {
    match record.object_type.as_str() {
        COLUMN => {
            kind == TABLE
                && record
                    .object_name
                    .split_once(SEP)
                    .is_some_and(|(table, _)| table == object)
        }
        OWNER | NODEFAULT => record
            .object_name
            .split_once(SEP)
            .is_some_and(|(record_kind, record_object)| record_kind == kind && record_object == object),
        MEMBER => false,
        other => other == kind && record.object_name == object,
    }
}

/// Forget everything recorded about an object: its grants, column grants,
/// owner and revoked defaults. Called when the object is created (so a new
/// object never starts with the grants of a dropped one of the same name) and
/// when it is dropped.
pub fn forget_object(catalog: &Catalog<'_>, kind: &str, object: &str) -> Result<()> {
    for record in catalog.list_acls()? {
        if record_is_about(&record, kind, object) {
            delete_record(catalog, &record)?;
        }
    }
    Ok(())
}

/// Forget everything recorded about every relation, sequence and routine in a
/// dropped schema, and about the schema itself.
pub fn forget_schema(catalog: &Catalog<'_>, schema: &str) -> Result<()> {
    let prefix = format!("{schema}.");
    for record in catalog.list_acls()? {
        let object = match record.object_type.as_str() {
            COLUMN => record.object_name.split_once(SEP).map(|(table, _)| table.to_string()),
            OWNER | NODEFAULT => record
                .object_name
                .split_once(SEP)
                .filter(|(kind, _)| *kind != SCHEMA && *kind != DATABASE)
                .map(|(_, object)| object.to_string()),
            MEMBER | SCHEMA | DATABASE => None,
            _ => Some(record.object_name.clone()),
        };
        if object.is_some_and(|object| object.starts_with(&prefix)) {
            delete_record(catalog, &record)?;
        }
    }
    forget_object(catalog, SCHEMA, schema)
}

fn rewrite_record(catalog: &Catalog<'_>, old: &AclRecord, new: &AclRecord) -> Result<()> {
    delete_record(catalog, old)?;
    for grantable in [false, true] {
        let privileges: Vec<String> = new
            .privileges
            .iter()
            .filter(|(_, flag)| *flag == grantable)
            .map(|(name, _)| name.clone())
            .collect();
        if !privileges.is_empty() {
            catalog.grant_privileges(
                &new.object_type,
                &new.object_name,
                &new.grantee,
                &new.grantor,
                &privileges,
                grantable,
            )?;
        }
    }
    Ok(())
}

/// Move everything recorded about a renamed object to its new name.
pub fn rename_object(catalog: &Catalog<'_>, kind: &str, old: &str, new: &str) -> Result<()> {
    if old == new {
        return Ok(());
    }
    for record in catalog.list_acls()? {
        if !record_is_about(&record, kind, old) {
            continue;
        }
        let mut moved = record.clone();
        moved.object_name = match record.object_type.as_str() {
            COLUMN => match record.object_name.split_once(SEP) {
                Some((_, column)) => column_object(new, column),
                None => continue,
            },
            OWNER | NODEFAULT => kind_object(kind, new),
            _ => new.to_string(),
        };
        rewrite_record(catalog, &record, &moved)?;
    }
    Ok(())
}

/// Forget the column grants on a dropped column.
pub fn forget_column(catalog: &Catalog<'_>, table: &str, column: &str) -> Result<()> {
    let composite = column_object(table, column);
    for record in catalog.list_acls()? {
        if record.object_type == COLUMN && record.object_name == composite {
            delete_record(catalog, &record)?;
        }
    }
    Ok(())
}

/// Move the column grants of a renamed column.
pub fn rename_column(catalog: &Catalog<'_>, table: &str, old: &str, new: &str) -> Result<()> {
    let composite = column_object(table, old);
    for record in catalog.list_acls()? {
        if record.object_type == COLUMN && record.object_name == composite {
            let mut moved = record.clone();
            moved.object_name = column_object(table, new);
            rewrite_record(catalog, &record, &moved)?;
        }
    }
    Ok(())
}

/// Rename a role everywhere a record names it: as grantee, grantor, owner,
/// member or granted role.
pub fn rename_role_references(catalog: &Catalog<'_>, old: &str, new: &str) -> Result<()> {
    for record in catalog.list_acls()? {
        let mut moved = record.clone();
        if moved.grantee == old {
            moved.grantee = new.to_string();
        }
        if moved.grantor == old {
            moved.grantor = new.to_string();
        }
        if moved.object_type == MEMBER && moved.object_name == old {
            moved.object_name = new.to_string();
        }
        if moved != record {
            rewrite_record(catalog, &record, &moved)?;
        }
    }
    Ok(())
}

/// Remove every membership record that names `role` on either side.
pub fn forget_memberships(catalog: &Catalog<'_>, role: &str) -> Result<()> {
    for record in catalog.list_acls()? {
        if record.object_type == MEMBER && (record.object_name == role || record.grantee == role) {
            delete_record(catalog, &record)?;
        }
    }
    Ok(())
}

/// What still depends on `role`, as PostgreSQL words it for `DROP ROLE`:
/// objects it owns and privileges it holds.
pub fn role_dependencies(catalog: &Catalog<'_>, role: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for record in catalog.list_acls()? {
        if record.grantee != role || record.object_type == MEMBER {
            continue;
        }
        match record.object_type.as_str() {
            OWNER => {
                if let Some((kind, object)) = record.object_name.split_once(SEP) {
                    out.push(format!("owner of {kind} {object}"));
                }
            }
            NODEFAULT => {}
            COLUMN => {
                if let Some((table, column)) = record.object_name.split_once(SEP) {
                    out.push(format!("privileges for column {column} of table {table}"));
                }
            }
            kind => out.push(format!("privileges for {kind} {}", record.object_name)),
        }
    }
    Ok(out)
}

/// The error for a role that is not in the catalog.
pub fn no_such_role(name: &str) -> Error {
    Error::query_execution(format!("role \"{name}\" does not exist"))
}
