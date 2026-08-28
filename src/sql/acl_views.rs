//! Role / privilege catalog rows — the SINGLE source of truth shared by both
//! catalog surfaces.
//!
//! # This module reports privileges. It does not enforce them.
//!
//! HeliosDB Nano persists roles (`CREATE/ALTER/DROP ROLE`) and ACL records
//! (`GRANT`/`REVOKE`) so that introspection tells the truth about what was
//! asked for. **No privilege is checked at any DML choke point.** A row in
//! `information_schema.table_privileges` means "somebody ran GRANT", never
//! "access to this table is restricted". Enforcement is a named follow-up
//! (ROADMAP_V5); until it lands, treat every view built here as documentation
//! of intent, not as a security control.
//!
//! # Why one module
//!
//! There are TWO catalog surfaces that historically drifted:
//!
//! * the PostgreSQL wire interceptor (`protocol/postgres/catalog.rs`), and
//! * the live planner-backed registry (`sql/phase3/system_views.rs`), which
//!   answers the embedded API, the REPL, the Python binding, the MySQL wire
//!   and — since the catalog-unification pass — the PG wire too.
//!
//! Both call the builders here, so a role or grant cannot be visible on one
//! route and invisible on the other. Do not inline row construction into
//! either surface.
//!
//! # Built-in roles are virtual
//!
//! `postgres` (oid 10) and `helios` (oid 11) are synthesized here, never
//! persisted, and cannot be created, altered or dropped. They exist because
//! `pg_namespace.nspowner` / `pg_class.relowner` have always reported owner
//! 10, and because drivers expect at least one login role to be listed. Their
//! all-true attribute bits are RETAINED for backward compatibility with
//! pre-4.20 output — and they are exactly as meaningless as they were before,
//! because nothing enforces them. Persisted roles, by contrast, report their
//! REAL bits: a `CREATE ROLE analyst` shows `rolsuper = false`.

use crate::storage::{AclRecord, Catalog, RoleRecord, BUILTIN_HELIOS_ROLE_OID, BUILTIN_POSTGRES_ROLE_OID};
use crate::{Column, DataType, Result, Tuple, Value};

/// The privileges PostgreSQL defines for a table, in `GRANT ALL` expansion
/// order. One list, used by the executor's `GRANT ALL` expansion AND by the
/// tests — never re-spelled elsewhere.
pub const TABLE_PRIVILEGES: [&str; 7] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
];

/// The privileges PostgreSQL defines for a sequence.
pub const SEQUENCE_PRIVILEGES: [&str; 3] = ["USAGE", "SELECT", "UPDATE"];

/// Expand `GRANT ALL PRIVILEGES` for an object type. Returns `None` for an
/// object type this build does not model, so the caller can raise a loud
/// error rather than silently granting an empty set.
pub fn all_privileges_for(object_type: &str) -> Option<&'static [&'static str]> {
    match object_type {
        "table" => Some(&TABLE_PRIVILEGES[..]),
        "sequence" => Some(&SEQUENCE_PRIVILEGES[..]),
        _ => None,
    }
}

/// True if `privilege` is valid for `object_type`. Used to reject
/// `GRANT USAGE ON <table>` instead of storing a nonsense record.
pub fn is_valid_privilege(object_type: &str, privilege: &str) -> bool {
    all_privileges_for(object_type).is_some_and(|list| list.contains(&privilege))
}

/// The two virtual built-in roles, as `RoleRecord`s, so every view builder
/// below renders built-ins and persisted roles through the same code.
fn builtin_roles() -> Vec<RoleRecord> {
    let builtin = |oid: u32, name: &str| RoleRecord {
        oid,
        name: name.to_string(),
        // All-true, exactly as pre-4.20 output — and exactly as unenforced.
        rolsuper: true,
        rolinherit: true,
        rolcreaterole: true,
        rolcreatedb: true,
        rolcanlogin: true,
        rolreplication: true,
        rolbypassrls: true,
        rolconnlimit: -1,
        rolvaliduntil: None,
        password: None,
    };
    vec![
        builtin(BUILTIN_POSTGRES_ROLE_OID, "postgres"),
        builtin(BUILTIN_HELIOS_ROLE_OID, "helios"),
    ]
}

/// Every role the catalog knows: the two virtual built-ins first (stable oids
/// 10/11), then persisted roles sorted by name.
pub fn all_roles(catalog: &Catalog<'_>) -> Result<Vec<RoleRecord>> {
    let mut roles = builtin_roles();
    roles.extend(catalog.list_roles()?);
    Ok(roles)
}

fn oid_value(oid: u32) -> Value {
    // pg_roles.oid is Int4 on both surfaces; role oids start at 16384 and the
    // counter is u32, so this only narrows in an absurd (2^31 roles) case.
    Value::Int4(oid as i32)
}

fn bool_value(b: bool) -> Value {
    Value::Boolean(b)
}

fn opt_text(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::String(text.clone()),
        None => Value::Null,
    }
}

/// Column list for `pg_roles` (12 columns, PostgreSQL order).
pub fn pg_roles_columns() -> Vec<Column> {
    vec![
        Column::new("oid", DataType::Int4),
        Column::new("rolname", DataType::Text),
        Column::new("rolsuper", DataType::Boolean),
        Column::new("rolinherit", DataType::Boolean),
        Column::new("rolcreaterole", DataType::Boolean),
        Column::new("rolcreatedb", DataType::Boolean),
        Column::new("rolcanlogin", DataType::Boolean),
        Column::new("rolreplication", DataType::Boolean),
        Column::new("rolconnlimit", DataType::Int4),
        Column::new("rolpassword", DataType::Text),
        Column::new("rolvaliduntil", DataType::Text),
        Column::new("rolbypassrls", DataType::Boolean),
    ]
}

/// `pg_roles` rows. `rolpassword` is ALWAYS `********` for a role that has a
/// password and NULL otherwise — PostgreSQL's own behaviour, and the reason
/// the stored password never leaves `RoleRecord`.
pub fn pg_roles_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    Ok(all_roles(catalog)?
        .into_iter()
        .map(|role| {
            Tuple::new(vec![
                oid_value(role.oid),
                Value::String(role.name),
                bool_value(role.rolsuper),
                bool_value(role.rolinherit),
                bool_value(role.rolcreaterole),
                bool_value(role.rolcreatedb),
                bool_value(role.rolcanlogin),
                bool_value(role.rolreplication),
                Value::Int4(role.rolconnlimit as i32),
                masked_password(&role.password),
                opt_text(&role.rolvaliduntil),
                bool_value(role.rolbypassrls),
            ])
        })
        .collect())
}

/// Never the real password. `********` marks "a password is set" exactly the
/// way PostgreSQL's own `pg_roles` view does; NULL means none is set.
fn masked_password(password: &Option<String>) -> Value {
    match password {
        Some(_) => Value::String("********".to_string()),
        None => Value::Null,
    }
}

/// Column list for `pg_user` (9 columns).
pub fn pg_user_columns() -> Vec<Column> {
    vec![
        Column::new("usename", DataType::Text),
        Column::new("usesysid", DataType::Int4),
        Column::new("usecreatedb", DataType::Boolean),
        Column::new("usesuper", DataType::Boolean),
        Column::new("userepl", DataType::Boolean),
        Column::new("usebypassrls", DataType::Boolean),
        Column::new("passwd", DataType::Text),
        Column::new("valuntil", DataType::Text),
        Column::new("useconfig", DataType::Text),
    ]
}

/// `pg_user` rows. PostgreSQL defines `pg_user` as `pg_roles` filtered to
/// `rolcanlogin`, so a `NOLOGIN` role appears in `pg_roles` and NOT here.
pub fn pg_user_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    Ok(all_roles(catalog)?
        .into_iter()
        .filter(|role| role.rolcanlogin)
        .map(|role| {
            Tuple::new(vec![
                Value::String(role.name),
                oid_value(role.oid),
                bool_value(role.rolcreatedb),
                bool_value(role.rolsuper),
                bool_value(role.rolreplication),
                bool_value(role.rolbypassrls),
                masked_password(&role.password),
                opt_text(&role.rolvaliduntil),
                Value::Null,
            ])
        })
        .collect())
}

/// Column list for `pg_authid` — `pg_roles`'s superuser-only twin. Same shape
/// plus nothing extra: PostgreSQL's `pg_authid` has `rolpassword` too, and we
/// mask it identically. Registered so `SELECT * FROM pg_authid` stops being an
/// unknown relation on the embedded route.
pub fn pg_authid_columns() -> Vec<Column> {
    pg_roles_columns()
}

/// `pg_authid` rows — identical to `pg_roles`, including the masked password.
/// PostgreSQL restricts `pg_authid` to superusers because it exposes the real
/// verifier; HeliosDB never emits the verifier from any view, so there is
/// nothing extra to restrict (and, this slice enforcing nothing, nothing to
/// restrict it WITH).
pub fn pg_authid_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    pg_roles_rows(catalog)
}

/// Column list psql's `\du` / `\dg` expects (11 columns, its own order).
pub fn psql_du_columns() -> Vec<Column> {
    vec![
        Column::new("rolname", DataType::Text),
        Column::new("rolsuper", DataType::Boolean),
        Column::new("rolinherit", DataType::Boolean),
        Column::new("rolcreaterole", DataType::Boolean),
        Column::new("rolcreatedb", DataType::Boolean),
        Column::new("rolcanlogin", DataType::Boolean),
        Column::new("rolconnlimit", DataType::Int4),
        Column::new("rolvaliduntil", DataType::Text),
        Column::new("memberof", DataType::Text),
        Column::new("rolreplication", DataType::Boolean),
        Column::new("rolbypassrls", DataType::Boolean),
    ]
}

/// `\du` rows. `memberof` is always the empty array literal `{}`: role
/// membership (`CREATE ROLE … IN ROLE`, `GRANT <role> TO <role>`) is rejected
/// at plan time in this slice, so no role can be a member of another and `{}`
/// is the truthful answer rather than a placeholder.
pub fn psql_du_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    Ok(all_roles(catalog)?
        .into_iter()
        .map(|role| {
            Tuple::new(vec![
                Value::String(role.name),
                bool_value(role.rolsuper),
                bool_value(role.rolinherit),
                bool_value(role.rolcreaterole),
                bool_value(role.rolcreatedb),
                bool_value(role.rolcanlogin),
                Value::Int4(role.rolconnlimit as i32),
                opt_text(&role.rolvaliduntil),
                Value::String("{}".to_string()),
                bool_value(role.rolreplication),
                bool_value(role.rolbypassrls),
            ])
        })
        .collect())
}

/// Column list shared by `information_schema.table_privileges` and
/// `role_table_grants` (SQL-standard 7-column shape).
pub fn table_privileges_columns() -> Vec<Column> {
    vec![
        Column::new("grantor", DataType::Text),
        Column::new("grantee", DataType::Text),
        Column::new("table_catalog", DataType::Text),
        Column::new("table_schema", DataType::Text),
        Column::new("table_name", DataType::Text),
        Column::new("privilege_type", DataType::Text),
        Column::new("is_grantable", DataType::Text),
    ]
}

/// One row per (grantor, grantee, table, privilege) from the stored ACL
/// records. Sequence grants are deliberately NOT included: the SQL standard
/// puts those in `usage_privileges`, which this slice leaves shape-correct and
/// empty rather than mixing object kinds into a table view.
///
/// The rows describe stored intent. They do not describe enforced access.
pub fn table_privileges_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    let mut rows = Vec::new();
    for acl in catalog.list_acls()? {
        if acl.object_type != "table" {
            continue;
        }
        let (schema, table) = split_schema_key(&acl.object_name);
        for (privilege, grantable) in &acl.privileges {
            rows.push(Tuple::new(vec![
                Value::String(acl.grantor.clone()),
                Value::String(acl.grantee.clone()),
                Value::String("heliosdb".to_string()),
                Value::String(schema.clone()),
                Value::String(table.clone()),
                Value::String(privilege.clone()),
                Value::String(if *grantable { "YES" } else { "NO" }.to_string()),
            ]));
        }
    }
    Ok(rows)
}

/// `information_schema.role_table_grants`.
///
/// PostgreSQL defines this as `table_privileges` restricted to grants where
/// the CURRENT role is grantor or grantee. HeliosDB has no session identity
/// yet (`current_user` is a hardcoded literal — see ROADMAP_V5's session
/// identity follow-up), so there is nothing to filter by and this mirrors
/// `table_privileges` exactly. That is a documented over-report, not a
/// silent one.
pub fn role_table_grants_rows(catalog: &Catalog<'_>) -> Result<Vec<Tuple>> {
    table_privileges_rows(catalog)
}

/// Split a resolved storage key into `(schema, object)`. Mirrors
/// `Planner::split_schema_key`, which is `pub(crate)` on a different module;
/// kept as a two-line local so this module has no planner dependency.
fn split_schema_key(key: &str) -> (String, String) {
    match key.find('.') {
        Some(idx) => (key[..idx].to_string(), key[idx + 1..].to_string()),
        None => ("public".to_string(), key.to_string()),
    }
}

/// Render the MySQL `SHOW GRANTS` lines for one user from stored ACL records.
///
/// Pure so it is unit-testable without a live connection. It replaces a
/// hardcoded `GRANT ALL PRIVILEGES ON *.* TO … WITH GRANT OPTION` — an
/// affirmative false statement about privileges that any hardening check
/// would have believed.
///
/// * always emits the `GRANT USAGE ON *.*` baseline (MySQL's "the account
///   exists, with no global privileges" line);
/// * then one line per ACL record naming `user` or the `public` pseudo-role,
///   in catalog order;
/// * `WITH GRANT OPTION` appears only when at least one listed privilege was
///   actually stored as grantable.
pub fn mysql_show_grants_lines(user: &str, acls: &[AclRecord]) -> Vec<String> {
    let mut lines = vec![format!("GRANT USAGE ON *.* TO '{user}'@'%'")];
    for acl in acls {
        if acl.object_type != "table" || acl.privileges.is_empty() {
            continue;
        }
        let (schema, table) = split_schema_key(&acl.object_name);
        let privileges = acl
            .privileges
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let grantable = acl.privileges.iter().any(|(_, g)| *g);
        lines.push(format!(
            "GRANT {privileges} ON `{schema}`.`{table}` TO '{user}'@'%'{}",
            if grantable { " WITH GRANT OPTION" } else { "" }
        ));
    }
    lines
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn acl(object: &str, grantee: &str, privileges: &[(&str, bool)]) -> AclRecord {
        AclRecord {
            object_type: "table".to_string(),
            object_name: object.to_string(),
            grantee: grantee.to_string(),
            grantor: "helios".to_string(),
            privileges: privileges.iter().map(|(p, g)| ((*p).to_string(), *g)).collect(),
        }
    }

    #[test]
    fn show_grants_without_acls_is_usage_only() {
        let lines = mysql_show_grants_lines("app", &[]);
        assert_eq!(lines, vec!["GRANT USAGE ON *.* TO 'app'@'%'".to_string()]);
        assert!(
            !lines.iter().any(|l| l.contains("ALL PRIVILEGES")),
            "SHOW GRANTS must never fabricate ALL PRIVILEGES: {lines:?}"
        );
    }

    #[test]
    fn show_grants_renders_stored_privileges_without_grant_option() {
        let lines = mysql_show_grants_lines("app", &[acl("orders", "app", &[("SELECT", false), ("INSERT", false)])]);
        assert_eq!(lines.len(), 2, "usage baseline + one grant line: {lines:?}");
        assert_eq!(
            lines[1],
            "GRANT SELECT, INSERT ON `public`.`orders` TO 'app'@'%'".to_string()
        );
        assert!(!lines[1].contains("WITH GRANT OPTION"));
    }

    #[test]
    fn show_grants_marks_grantable_and_splits_schema() {
        let lines = mysql_show_grants_lines("app", &[acl("sales.orders", "app", &[("SELECT", true)])]);
        assert_eq!(
            lines[1],
            "GRANT SELECT ON `sales`.`orders` TO 'app'@'%' WITH GRANT OPTION".to_string()
        );
    }

    #[test]
    fn show_grants_skips_non_table_and_empty_records() {
        let mut sequence = acl("order_id_seq", "app", &[("USAGE", false)]);
        sequence.object_type = "sequence".to_string();
        let empty = acl("orders", "app", &[]);
        let lines = mysql_show_grants_lines("app", &[sequence, empty]);
        assert_eq!(lines.len(), 1, "only the USAGE baseline should survive: {lines:?}");
    }

    #[test]
    fn all_privileges_expansion_is_object_type_aware() {
        assert_eq!(all_privileges_for("table"), Some(&TABLE_PRIVILEGES[..]));
        assert_eq!(all_privileges_for("sequence"), Some(&SEQUENCE_PRIVILEGES[..]));
        assert_eq!(
            all_privileges_for("schema"),
            None,
            "an unmodelled object type must be None so the caller errors loudly"
        );
        assert!(is_valid_privilege("table", "TRUNCATE"));
        assert!(!is_valid_privilege("table", "USAGE"));
        assert!(is_valid_privilege("sequence", "USAGE"));
        assert!(!is_valid_privilege("sequence", "DELETE"));
    }

    #[test]
    fn split_schema_key_defaults_to_public() {
        assert_eq!(split_schema_key("orders"), ("public".to_string(), "orders".to_string()));
        assert_eq!(
            split_schema_key("sales.orders"),
            ("sales".to_string(), "orders".to_string())
        );
    }
}
