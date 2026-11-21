//! What a dump carries of row-level security: the statements that recreate
//! every table's switches and policies, in the order they must run.

use super::record::PolicyRecord;
use super::store;
use crate::storage::StorageEngine;
use crate::Result;

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_table(table: &str) -> String {
    let (schema, name) = crate::sql::Planner::split_schema_key(table);
    if schema == "public" {
        quote_ident(&name)
    } else {
        format!("{}.{}", quote_ident(&schema), quote_ident(&name))
    }
}

/// `CREATE POLICY …` for one policy.
pub fn policy_ddl(table: &str, policy: &PolicyRecord) -> String {
    let mut sql = format!(
        "CREATE POLICY {} ON {} AS {} FOR {}",
        quote_ident(&policy.name),
        quote_table(table),
        if policy.permissive { "PERMISSIVE" } else { "RESTRICTIVE" },
        policy.command.keyword()
    );
    if policy.roles.is_empty() {
        sql.push_str(" TO PUBLIC");
    } else {
        let roles: Vec<String> = policy.roles.iter().map(|r| quote_ident(r)).collect();
        sql.push_str(&format!(" TO {}", roles.join(", ")));
    }
    if let Some(using) = &policy.using {
        sql.push_str(&format!(" USING ({using})"));
    }
    if let Some(with_check) = &policy.with_check {
        sql.push_str(&format!(" WITH CHECK ({with_check})"));
    }
    sql.push(';');
    sql
}

/// Every statement, for the tables `include` accepts: policies first, then
/// the switches, table by table.
pub fn statements(storage: &StorageEngine, include: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
    let mut tables: Vec<_> = store::read_tables(storage)?.into_values().collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::new();
    for table in tables {
        if !include(&table.name) || !storage.catalog().table_exists(&table.name)? {
            continue;
        }
        for policy in &table.policies {
            out.push(policy_ddl(&table.name, policy));
        }
        if table.security.enabled {
            out.push(format!("ALTER TABLE {} ENABLE ROW LEVEL SECURITY;", quote_table(&table.name)));
        }
        if table.security.forced {
            out.push(format!("ALTER TABLE {} FORCE ROW LEVEL SECURITY;", quote_table(&table.name)));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::record::PolicyCommand;
    use super::*;

    #[test]
    fn a_policy_is_written_as_the_statement_that_makes_it() {
        let policy = PolicyRecord {
            name: "own rows".to_string(),
            table: "docs".to_string(),
            permissive: false,
            command: PolicyCommand::Update,
            roles: vec!["alice".to_string(), "Bob".to_string()],
            using: Some("owner = current_user".to_string()),
            with_check: Some("owner = current_user AND NOT locked".to_string()),
            oid: 1,
        };
        assert_eq!(
            policy_ddl("app.docs", &policy),
            "CREATE POLICY \"own rows\" ON \"app\".\"docs\" AS RESTRICTIVE FOR UPDATE TO \"alice\", \"Bob\" \
             USING (owner = current_user) WITH CHECK (owner = current_user AND NOT locked);"
        );
        let mut open = policy;
        open.roles.clear();
        open.permissive = true;
        open.command = PolicyCommand::All;
        open.with_check = None;
        assert_eq!(
            policy_ddl("docs", &open),
            "CREATE POLICY \"own rows\" ON \"docs\" AS PERMISSIVE FOR ALL TO PUBLIC USING (owner = current_user);"
        );
    }
}
