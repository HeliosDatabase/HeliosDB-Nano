//! Privileges: who may run which statement.
//!
//! * [`access`]: who a statement runs as (the principal).
//! * [`catalog`]: grants, column grants, owners and role membership.
//! * [`gate`]: the check, at the logical-plan level.
//! * [`ownership`]: owners and grants follow CREATE, DROP and RENAME.
//! * [`statement`]: `GRANT`, `REVOKE`, role membership, `SET ROLE`,
//!   `ALTER … OWNER TO`.
//! * [`functions`]: `has_table_privilege` and its siblings.
//! * [`mysql`]: the MySQL wording of a refusal.
//!
//! The gate is one function ([`gate::check_plan`]) called from every place a
//! plan is about to run, before any row policy is applied:
//!
//! * `Planner::statement_to_plan`, for every statement the planner plans,
//!   including the statements of a routine body;
//! * [`ownership::run_admitted`] / [`ownership::admit`], in the two statement
//!   executors, for plans that did not come from the planner or come from a
//!   plan cache;
//! * [`EmbeddedDatabase::privilege_precheck`], at every statement entry
//!   point, before the fast paths and the result caches that never plan.

pub mod access;
pub mod catalog;
pub mod functions;
pub mod gate;
pub mod mysql;
pub mod ownership;
pub mod statement;
pub(crate) mod views;

pub use access::{Principal, SessionAccess};
pub use gate::{MUST_BE_OWNER, PERMISSION_DENIED};

use crate::sql::LogicalPlan;
use crate::{EmbeddedDatabase, Result};

/// Statements that run without a plan and change more than the caller's own
/// data: a role that is not a superuser may not run them.
const SUPERUSER_ONLY_UTILITIES: [&[&str]; 12] = [
    &["VACUUM"],
    &["REINDEX"],
    &["CHECKPOINT"],
    &["CLUSTER"],
    &["LOAD"],
    &["ATTACH"],
    &["DETACH"],
    &["ALTER", "SYSTEM"],
    &["CREATE", "TABLESPACE"],
    &["DROP", "TABLESPACE"],
    &["ALTER", "TABLESPACE"],
    &["CREATE", "DOMAIN"],
];

fn leading_words(sql: &str, count: usize) -> Vec<String> {
    sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .take(count)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Resolves the relation names of an access-control statement the way the
/// planner resolves them for any other statement of the session. The planner
/// is built only when a statement actually names a relation.
struct SessionResolver<'d> {
    db: &'d EmbeddedDatabase,
}

impl statement::Resolver for SessionResolver<'_> {
    fn relation(&self, raw: &str) -> String {
        let catalog = self.db.storage.catalog();
        let planner = crate::sql::Planner::with_catalog(&catalog)
            .with_current_schema(self.db.current_schema())
            .with_search_path(self.db.current_search_path());
        let key = planner.resolve_partition_name(raw);
        if gate::relation_exists(&self.db.storage, &key) {
            return key;
        }
        // Views are stored under their bare name.
        let bare = key.rsplit_once('.').map_or(key.as_str(), |(_, bare)| bare);
        if bare != key && self.db.storage.view_catalog().view_exists(bare).unwrap_or(false) {
            return bare.to_string();
        }
        key
    }
}

impl EmbeddedDatabase {
    /// Plan `sql` if it is an access-control statement (`GRANT`, `REVOKE`,
    /// `SET ROLE`, `ALTER … OWNER TO`, …). `Ok(None)` for anything else.
    pub(crate) fn try_plan_access_statement(&self, sql: &str) -> Result<Option<LogicalPlan>> {
        let resolver = SessionResolver { db: self };
        Ok(statement::parse(sql, &resolver)?.map(|statement| LogicalPlan::AccessControl(Box::new(statement))))
    }

    /// Check a statement's text before anything runs it: the fast paths and
    /// the result caches answer without planning, so the statement is planned
    /// once here and that plan is checked. Free for an unrestricted caller and
    /// for a superuser.
    pub(crate) fn privilege_precheck(&self, sql: &str) -> Result<()> {
        if access::nothing_to_check() {
            return Ok(());
        }
        let Some(gate) = gate::Gate::current(&self.storage)? else {
            return Ok(());
        };
        if gate.is_superuser() {
            return Ok(());
        }
        let stripped = crate::sql::Parser::strip_sql_comments(sql);
        let words = leading_words(&stripped, 2);
        for utility in SUPERUSER_ONLY_UTILITIES {
            if words.len() >= utility.len() && utility.iter().zip(words.iter()).all(|(a, b)| *a == b.as_str()) {
                return Err(gate::denied_to(&format!(
                    "run {}: must be superuser",
                    utility.join(" ")
                )));
            }
        }
        // `EXECUTE name(...)`: the prepared statement can answer from a fast
        // path that never plans, so its text is checked each time it runs.
        if words.first().is_some_and(|word| word == "EXECUTE") {
            let rest = stripped.trim_start().get("EXECUTE".len()..).unwrap_or("").trim_start();
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '(' || c == ';')
                .unwrap_or(rest.len());
            let name = rest.get(..end).unwrap_or("");
            let body = self
                .prepared_statement_sql
                .read()
                .get(&self.prepared_statement_key(name))
                .cloned();
            if let Some(body) = body {
                return self.privilege_precheck(&body);
            }
        }
        if crate::sql::Parser::is_partition_attach_detach_statement(sql) {
            return Err(gate::denied_to("attach or detach a partition: must be superuser"));
        }
        match self.parameterized_plan_stamped(sql) {
            Ok(stamped) => gate.statement(&stamped.plan),
            Err(error) if gate::is_permission_error(&error) => Err(error),
            // Not a statement the planner plans on its own (several statements,
            // a session command, a syntax error): whatever runs it plans it,
            // and that plan is checked where it runs.
            Err(_) => Ok(()),
        }
    }

    /// Create the session of a network connection. Until a role is published
    /// on it (`set_session_login_user`), every statement that needs a
    /// privilege is refused.
    pub fn create_network_session(&self, user_name: &str) -> Result<crate::session::SessionId> {
        let session_id = self.create_wire_session(user_name)?;
        let session = self.session_manager.get_session(session_id)?;
        session.read().scoped.access().mark_network();
        Ok(session_id)
    }

    /// `CONNECT` on a database, for a role that has just authenticated. The
    /// refusal is the bare message for the wire. A login that names no role in
    /// the catalog is the server's own account and is not checked.
    pub(crate) fn check_connect_privilege(&self, role: &str, database: &str) -> std::result::Result<(), String> {
        if role.is_empty() || crate::storage::is_reserved_role_name(role) {
            return Ok(());
        }
        let refusal = |reason: &str| format!("{PERMISSION_DENIED} for database {database}: {reason}");
        match self.storage.catalog().role_exists(role) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => return Err(refusal(&format!("the role cannot be read ({error})"))),
        }
        let gate = gate::Gate::for_principal(
            &self.storage,
            Principal::Role {
                name: std::sync::Arc::from(role),
                operator: false,
            },
        )
        .map_err(|error| refusal(&format!("the privileges cannot be read ({error})")))?;
        if gate.has_object_privilege(catalog::DATABASE, database, "CONNECT") {
            Ok(())
        } else {
            Err(refusal(&format!(
                "role \"{role}\" does not have the CONNECT privilege"
            )))
        }
    }
}
