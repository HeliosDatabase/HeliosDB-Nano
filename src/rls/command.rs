//! The statements: `CREATE POLICY`, `ALTER POLICY`, `DROP POLICY` and
//! `ALTER TABLE … ENABLE | DISABLE | FORCE | NO FORCE ROW LEVEL SECURITY`.
//!
//! The planner turns the parsed statement into an [`RlsCommand`]; the executor
//! runs it against the storage engine. Only the table's owner or a superuser
//! may run one.

use serde::{Deserialize, Serialize};
use sqlparser::ast::{
    AlterPolicyOperation, AlterTableOperation, CreatePolicyCommand, CreatePolicyType, Expr, Owner, Statement,
};

use super::record::{PolicyCommand, PolicyRecord, TableSecurity};
use super::store;
use crate::sql::{LogicalPlan, Planner};
use crate::storage::StorageEngine;
use crate::{Error, Result};

/// The rule name `FORCE ROW LEVEL SECURITY` is carried under through the SQL
/// parser, which has no grammar for it (see [`rewrite_force_syntax`]).
const FORCE_RULE: &str = "__hdb_force_row_level_security";

/// A `TO` entry that names the role running the statement.
const CURRENT_ROLE: &str = "\u{1}current_role";

/// One table switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Switch {
    Enable,
    Disable,
    Force,
    NoForce,
}

/// A row-level security statement, planned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RlsCommand {
    /// `CREATE POLICY`.
    CreatePolicy { table: String, policy: PolicyRecord },
    /// `ALTER POLICY … RENAME TO`.
    RenamePolicy {
        table: String,
        name: String,
        new_name: String,
    },
    /// `ALTER POLICY … [TO …] [USING (…)] [WITH CHECK (…)]`.
    AlterPolicy {
        table: String,
        name: String,
        roles: Option<Vec<String>>,
        using: Option<String>,
        with_check: Option<String>,
    },
    /// `DROP POLICY [IF EXISTS]`.
    DropPolicy {
        table: String,
        name: String,
        if_exists: bool,
    },
    /// `ALTER TABLE … ROW LEVEL SECURITY` switches, in the order written.
    TableSwitches { table: String, switches: Vec<Switch> },
}

impl RlsCommand {
    /// The command tag of the statement.
    pub fn tag(&self) -> &'static str {
        match self {
            RlsCommand::CreatePolicy { .. } => "CREATE POLICY",
            RlsCommand::RenamePolicy { .. } | RlsCommand::AlterPolicy { .. } => "ALTER POLICY",
            RlsCommand::DropPolicy { .. } => "DROP POLICY",
            RlsCommand::TableSwitches { .. } => "ALTER TABLE",
        }
    }
}

/// `ALTER TABLE t FORCE ROW LEVEL SECURITY` and `NO FORCE …` in a spelling the
/// SQL parser reads. Tried only for a statement that did not parse; `None`
/// when the text holds neither.
pub fn rewrite_force_syntax(sql: &str) -> Option<String> {
    use std::sync::OnceLock;
    static NO_FORCE: OnceLock<Option<regex::Regex>> = OnceLock::new();
    static FORCE: OnceLock<Option<regex::Regex>> = OnceLock::new();
    let head = sql.trim_start();
    if !head.get(..11).is_some_and(|h| h.eq_ignore_ascii_case("ALTER TABLE")) {
        return None;
    }
    let no_force = NO_FORCE
        .get_or_init(|| regex::Regex::new(r"(?i)\bNO\s+FORCE\s+ROW\s+LEVEL\s+SECURITY\b").ok())
        .as_ref()?;
    let force = FORCE
        .get_or_init(|| regex::Regex::new(r"(?i)\bFORCE\s+ROW\s+LEVEL\s+SECURITY\b").ok())
        .as_ref()?;
    if !force.is_match(sql) {
        return None;
    }
    let step = no_force.replace_all(sql, format!("DISABLE RULE {FORCE_RULE}").as_str());
    Some(
        force
            .replace_all(&step, format!("ENABLE RULE {FORCE_RULE}").as_str())
            .into_owned(),
    )
}

/// The switch an `ALTER TABLE` action is, if it is one.
pub fn switch_of(operation: &AlterTableOperation) -> Option<Switch> {
    match operation {
        AlterTableOperation::EnableRowLevelSecurity => Some(Switch::Enable),
        AlterTableOperation::DisableRowLevelSecurity => Some(Switch::Disable),
        AlterTableOperation::EnableRule { name } if name.value == FORCE_RULE => Some(Switch::Force),
        AlterTableOperation::DisableRule { name } if name.value == FORCE_RULE => Some(Switch::NoForce),
        _ => None,
    }
}

/// The plan of an `ALTER TABLE` whose actions are all switches; `None` when
/// none is. A statement mixing a switch with another action is refused: the
/// two are not applied as one change.
pub fn plan_table_switches(table: &str, operations: &[AlterTableOperation]) -> Result<Option<LogicalPlan>> {
    let switches: Vec<Switch> = operations.iter().filter_map(switch_of).collect();
    if switches.is_empty() {
        return Ok(None);
    }
    if switches.len() != operations.len() {
        return Err(Error::query_execution(format!(
            "ALTER TABLE with a ROW LEVEL SECURITY action and another action in one statement {}; run them as separate statements",
            super::UNSUPPORTED_MARKER
        )));
    }
    Ok(Some(LogicalPlan::RowSecurity(RlsCommand::TableSwitches {
        table: table.to_string(),
        switches,
    })))
}

/// `CREATE VIEW … WITH (security_invoker = …)`.
///
/// A view is read with the reader's rights: the policies of the tables under
/// it apply to whoever reads the view. That is PostgreSQL's
/// `security_invoker = true`, which is accepted and is the default here.
/// `security_invoker = false` (the view owner's rights) is refused rather than
/// accepted and ignored.
pub fn check_view_options(options: &sqlparser::ast::CreateTableOptions) -> Result<()> {
    use sqlparser::ast::{CreateTableOptions, SqlOption};
    let (CreateTableOptions::With(options) | CreateTableOptions::Options(options)) = options else {
        return Ok(());
    };
    for option in options {
        let SqlOption::KeyValue { key, value } = option else {
            continue;
        };
        if !key.value.eq_ignore_ascii_case("security_invoker") {
            continue;
        }
        let value = value.to_string();
        let value = value.trim_matches('\'').to_ascii_lowercase();
        match value.as_str() {
            "true" | "on" | "1" | "yes" => {}
            "false" | "off" | "0" | "no" => {
                return Err(Error::query_execution(format!(
                    "a view with security_invoker = false {}: a view is read with the reader's rights, \
                     so the policies of the tables under it apply to the reader",
                    super::UNSUPPORTED_MARKER
                )));
            }
            other => {
                return Err(Error::query_execution(format!(
                    "invalid value for parameter \"security_invoker\": \"{other}\""
                )));
            }
        }
    }
    Ok(())
}

fn roles_of(to: Option<&Vec<Owner>>) -> Vec<String> {
    let mut roles = Vec::new();
    for owner in to.into_iter().flatten() {
        let role = match owner {
            Owner::Ident(ident) => Planner::normalize_ident(ident),
            Owner::CurrentRole | Owner::CurrentUser | Owner::SessionUser => CURRENT_ROLE.to_string(),
        };
        // PUBLIC is everyone: the empty list.
        if role == "public" {
            return Vec::new();
        }
        if !roles.contains(&role) {
            roles.push(role);
        }
    }
    roles
}

fn text_of(expr: Option<&Expr>) -> Option<String> {
    expr.map(ToString::to_string)
}

/// The plan of a policy statement. `Ok(None)` for any other statement.
pub fn plan_statement(planner: &Planner<'_>, statement: &Statement) -> Result<Option<LogicalPlan>> {
    let command = match statement {
        Statement::CreatePolicy {
            name,
            table_name,
            policy_type,
            command,
            to,
            using,
            with_check,
        } => {
            let table = planner.resolve_table_ref(table_name);
            RlsCommand::CreatePolicy {
                policy: PolicyRecord {
                    name: Planner::normalize_ident(name),
                    table: table.clone(),
                    permissive: !matches!(policy_type, Some(CreatePolicyType::Restrictive)),
                    command: match command {
                        None | Some(CreatePolicyCommand::All) => PolicyCommand::All,
                        Some(CreatePolicyCommand::Select) => PolicyCommand::Select,
                        Some(CreatePolicyCommand::Insert) => PolicyCommand::Insert,
                        Some(CreatePolicyCommand::Update) => PolicyCommand::Update,
                        Some(CreatePolicyCommand::Delete) => PolicyCommand::Delete,
                    },
                    roles: roles_of(to.as_ref()),
                    using: text_of(using.as_ref()),
                    with_check: text_of(with_check.as_ref()),
                    oid: 0,
                },
                table,
            }
        }
        Statement::AlterPolicy {
            name,
            table_name,
            operation,
        } => {
            let table = planner.resolve_table_ref(table_name);
            let name = Planner::normalize_ident(name);
            match operation {
                AlterPolicyOperation::Rename { new_name } => RlsCommand::RenamePolicy {
                    table,
                    name,
                    new_name: Planner::normalize_ident(new_name),
                },
                AlterPolicyOperation::Apply { to, using, with_check } => RlsCommand::AlterPolicy {
                    table,
                    name,
                    roles: to.as_ref().map(|to| roles_of(Some(to))),
                    using: text_of(using.as_ref()),
                    with_check: text_of(with_check.as_ref()),
                },
            }
        }
        Statement::DropPolicy {
            if_exists,
            name,
            table_name,
            ..
        } => RlsCommand::DropPolicy {
            table: planner.resolve_table_ref(table_name),
            name: Planner::normalize_ident(name),
            if_exists: *if_exists,
        },
        _ => return Ok(None),
    };
    Ok(Some(LogicalPlan::RowSecurity(command)))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn no_such_policy(name: &str, table: &str) -> Error {
    Error::query_execution(format!("policy \"{name}\" for table \"{table}\" does not exist"))
}

/// The table must be an ordinary table of the main branch.
fn require_table(storage: &StorageEngine, table: &str) -> Result<()> {
    if storage.is_branch_active() {
        return Err(Error::query_execution(format!(
            "changing policies on a branch {}; run the statement on main",
            super::UNSUPPORTED_MARKER
        )));
    }
    if storage.mv_catalog().view_exists(table).unwrap_or(false) {
        return Err(Error::query_execution(format!(
            "\"{table}\" is a materialized view: a policy on a materialized view {}; \
             the policies of its base tables apply to it",
            super::UNSUPPORTED_MARKER
        )));
    }
    if !storage.catalog().table_exists(table)? {
        return Err(Error::query_execution(format!("relation \"{table}\" does not exist")));
    }
    Ok(())
}

/// Only the table's owner or a superuser changes its row-level security.
/// (The merge point with the privilege gate's ownership check.)
fn require_owner(storage: &StorageEngine, table: &str) -> Result<()> {
    let Some(role) = crate::session_login_user_tls() else {
        return Ok(());
    };
    if store::is_bootstrap_superuser(&role) {
        return Ok(());
    }
    if matches!(storage.catalog().get_role(&role), Ok(Some(record)) if record.rolsuper) {
        return Ok(());
    }
    if store::table_owner(storage, table)?.as_deref() == Some(&*role) {
        return Ok(());
    }
    // "row-level security" in the text is what makes this SQLSTATE 42501.
    Err(Error::ConstraintViolation(format!(
        "must be owner of table \"{table}\" to change its row-level security"
    )))
}

fn resolve_roles(storage: &StorageEngine, roles: Vec<String>) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(roles.len());
    for role in roles {
        let role = if role == CURRENT_ROLE {
            match crate::session_login_user_tls() {
                Some(login) => login.to_string(),
                None => "postgres".to_string(),
            }
        } else {
            role
        };
        if !store::is_bootstrap_superuser(&role) && !storage.catalog().role_exists(&role)? {
            return Err(Error::query_execution(format!("role \"{role}\" does not exist")));
        }
        if !resolved.contains(&role) {
            resolved.push(role);
        }
    }
    Ok(resolved)
}

fn check_clauses(policy: &PolicyRecord) -> Result<()> {
    match policy.command {
        PolicyCommand::Select | PolicyCommand::Delete if policy.with_check.is_some() => {
            Err(Error::sql_parse("WITH CHECK cannot be applied to SELECT or DELETE"))
        }
        PolicyCommand::Insert if policy.using.is_some() => {
            Err(Error::sql_parse("only WITH CHECK expression allowed for INSERT"))
        }
        _ => Ok(()),
    }
}

fn check_expressions(storage: &StorageEngine, table: &str, policy: &PolicyRecord) -> Result<()> {
    for text in [&policy.using, &policy.with_check].into_iter().flatten() {
        // Through the planner, so a column the table does not have is refused
        // here and not when the table is next read.
        super::rewrite::plan_policy_expr(storage, table, text)?;
    }
    Ok(())
}

/// Run a planned statement.
pub fn execute(storage: &StorageEngine, command: &RlsCommand) -> Result<()> {
    match command {
        RlsCommand::CreatePolicy { table, policy } => {
            require_table(storage, table)?;
            require_owner(storage, table)?;
            let tables = store::read_tables(storage)?;
            if tables.get(table).is_some_and(|t| t.policy(&policy.name).is_some()) {
                return Err(Error::query_execution(format!(
                    "policy \"{}\" for table \"{table}\" already exists",
                    policy.name
                )));
            }
            let mut policy = policy.clone();
            policy.roles = resolve_roles(storage, policy.roles)?;
            check_clauses(&policy)?;
            check_expressions(storage, table, &policy)?;
            store::write_policy(storage, table, policy)
        }
        RlsCommand::RenamePolicy { table, name, new_name } => {
            require_table(storage, table)?;
            require_owner(storage, table)?;
            let tables = store::read_tables(storage)?;
            let entry = tables.get(table).ok_or_else(|| no_such_policy(name, table))?;
            let existing = entry.policy(name).ok_or_else(|| no_such_policy(name, table))?;
            if entry.policy(new_name).is_some() {
                return Err(Error::query_execution(format!(
                    "policy \"{new_name}\" for table \"{table}\" already exists"
                )));
            }
            let mut renamed = (**existing).clone();
            renamed.name.clone_from(new_name);
            // Written before the old record goes: a failure between the two
            // leaves the policy under both names, never under neither.
            store::write_policy(storage, table, renamed)?;
            store::remove_policy(storage, table, name)
        }
        RlsCommand::AlterPolicy {
            table,
            name,
            roles,
            using,
            with_check,
        } => {
            require_table(storage, table)?;
            require_owner(storage, table)?;
            let tables = store::read_tables(storage)?;
            let existing = tables
                .get(table)
                .and_then(|t| t.policy(name))
                .ok_or_else(|| no_such_policy(name, table))?;
            let mut altered = (**existing).clone();
            if let Some(roles) = roles {
                altered.roles = resolve_roles(storage, roles.clone())?;
            }
            if let Some(using) = using {
                altered.using = Some(using.clone());
            }
            if let Some(with_check) = with_check {
                altered.with_check = Some(with_check.clone());
            }
            check_clauses(&altered)?;
            check_expressions(storage, table, &altered)?;
            store::write_policy(storage, table, altered)
        }
        RlsCommand::DropPolicy { table, name, if_exists } => {
            if *if_exists && !storage.catalog().table_exists(table)? {
                return Ok(());
            }
            require_table(storage, table)?;
            require_owner(storage, table)?;
            let tables = store::read_tables(storage)?;
            if !tables.get(table).is_some_and(|t| t.policy(name).is_some()) {
                return if *if_exists {
                    Ok(())
                } else {
                    Err(no_such_policy(name, table))
                };
            }
            store::remove_policy(storage, table, name)
        }
        RlsCommand::TableSwitches { table, switches } => {
            require_table(storage, table)?;
            require_owner(storage, table)?;
            let tables = store::read_tables(storage)?;
            let mut security: TableSecurity = tables.get(table).map(|t| t.security).unwrap_or_default();
            for switch in switches {
                match switch {
                    Switch::Enable => security.enabled = true,
                    Switch::Disable => security.enabled = false,
                    Switch::Force => security.forced = true,
                    Switch::NoForce => security.forced = false,
                }
            }
            store::write_table_security(storage, table, security)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn force_and_no_force_are_rewritten_only_in_alter_table() {
        let force = rewrite_force_syntax("ALTER TABLE t FORCE ROW LEVEL SECURITY").unwrap();
        assert_eq!(force, format!("ALTER TABLE t ENABLE RULE {FORCE_RULE}"));
        let no_force = rewrite_force_syntax("alter table s.t no force  row level security;").unwrap();
        assert_eq!(no_force, format!("alter table s.t DISABLE RULE {FORCE_RULE};"));
        assert!(rewrite_force_syntax("SELECT 'FORCE ROW LEVEL SECURITY'").is_none());
        assert!(rewrite_force_syntax("ALTER TABLE t ADD COLUMN c INT").is_none());
    }

    #[test]
    fn the_rewritten_spelling_parses_back_to_the_switch() {
        let parser = crate::sql::Parser::new();
        for (sql, expected) in [
            ("ALTER TABLE t FORCE ROW LEVEL SECURITY", Switch::Force),
            ("ALTER TABLE t NO FORCE ROW LEVEL SECURITY", Switch::NoForce),
            ("ALTER TABLE t ENABLE ROW LEVEL SECURITY", Switch::Enable),
            ("ALTER TABLE t DISABLE ROW LEVEL SECURITY", Switch::Disable),
        ] {
            let Statement::AlterTable { operations, .. } = parser.parse_one(sql).unwrap() else {
                panic!("{sql}: not an ALTER TABLE");
            };
            assert_eq!(operations.iter().filter_map(switch_of).collect::<Vec<_>>(), [expected], "{sql}");
        }
    }

    #[test]
    fn to_public_is_everyone_and_names_are_folded() {
        let parser = crate::sql::Parser::new();
        let Statement::CreatePolicy { to, .. } = parser
            .parse_one("CREATE POLICY p ON t TO Alice, \"Bob\", alice USING (true)")
            .unwrap()
        else {
            panic!("not a CREATE POLICY");
        };
        assert_eq!(roles_of(to.as_ref()), ["alice", "Bob"]);
        let Statement::CreatePolicy { to, .. } = parser
            .parse_one("CREATE POLICY p ON t TO alice, PUBLIC USING (true)")
            .unwrap()
        else {
            panic!("not a CREATE POLICY");
        };
        assert_eq!(roles_of(to.as_ref()), Vec::<String>::new());
    }
}
