//! The MySQL wording of a privilege refusal.
//!
//! The gate words its refusals as PostgreSQL does. A MySQL client expects
//! MySQL's own codes and texts, so the MySQL listener translates:
//!
//! * a relation: 1142, `SELECT command denied to user 'u'@'%' for table 't'`;
//! * a schema or database: 1044, `Access denied for user 'u'@'%' to database 'd'`;
//! * a routine or sequence: 1370, `execute command denied to user 'u'@'%' for routine 'f'`;
//! * anything else (role administration, superuser-only statements): 1227.
//!
//! The refusal does not say which privilege was missing, so the command named
//! in a 1142 text is the statement's own verb.

use super::gate::{MUST_BE_OWNER, PERMISSION_DENIED};

const COMMANDS: [&str; 12] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "INDEX",
    "REFERENCES",
    "TRIGGER",
    "GRANT",
];

/// The first word of a statement, upper-cased, for [`translate`].
pub fn statement_verb(sql: &str) -> String {
    sql.trim_start()
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn first_word(text: &str) -> &str {
    text.split(|c: char| c == ':' || c.is_whitespace())
        .find(|word| !word.is_empty())
        .unwrap_or("")
}

/// Translate one of the gate's refusals into `(code, sqlstate, message)`.
/// `None` for any other error.
pub fn translate(message: &str, user: &str, verb: &str) -> Option<(u16, &'static str, String)> {
    let (rest, owner) = if let Some(index) = message.find(MUST_BE_OWNER) {
        (message.get(index + MUST_BE_OWNER.len()..)?.trim_start(), true)
    } else {
        let index = message.find(PERMISSION_DENIED)?;
        let rest = message.get(index + PERMISSION_DENIED.len()..)?.trim_start();
        match rest.strip_prefix("for ") {
            Some(rest) => (rest, false),
            None => {
                return Some((
                    1227,
                    "42000",
                    "Access denied; you need (at least one of) the SUPER privilege(s) for this operation".to_string(),
                ))
            }
        }
    };
    let command = match verb {
        "WITH" | "TABLE" | "VALUES" | "EXPLAIN" | "" => "SELECT",
        "REPLACE" => "INSERT",
        known if COMMANDS.contains(&known) => known,
        _ if owner => "ALTER",
        _ => "SELECT",
    };
    let (kind, object) = if let Some(object) = rest.strip_prefix("materialized view ") {
        ("table", first_word(object))
    } else {
        let kind = first_word(rest);
        (kind, first_word(rest.get(kind.len()..).unwrap_or("")))
    };
    Some(match kind {
        "table" | "view" | "relation" => (
            1142,
            "42000",
            format!("{command} command denied to user '{user}'@'%' for table '{object}'"),
        ),
        "schema" | "database" => (
            1044,
            "42000",
            format!("Access denied for user '{user}'@'%' to database '{object}'"),
        ),
        "function" | "procedure" | "sequence" => (
            1370,
            "42000",
            format!("execute command denied to user '{user}'@'%' for routine '{object}'"),
        ),
        _ => (
            1227,
            "42000",
            "Access denied; you need (at least one of) the SUPER privilege(s) for this operation".to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_refusal_is_1142_with_the_statement_verb() {
        let got = translate(
            "Query execution error: permission denied for table orders",
            "app",
            "INSERT",
        );
        assert_eq!(
            got,
            Some((
                1142,
                "42000",
                "INSERT command denied to user 'app'@'%' for table 'orders'".to_string()
            ))
        );
    }

    #[test]
    fn a_view_and_a_materialized_view_are_tables_to_mysql() {
        for message in [
            "permission denied for view v",
            "permission denied for materialized view v",
        ] {
            let (code, _, text) = translate(message, "app", "SELECT").unwrap_or_default();
            assert_eq!(code, 1142, "{message}");
            assert_eq!(text, "SELECT command denied to user 'app'@'%' for table 'v'");
        }
    }

    #[test]
    fn a_schema_refusal_is_1044() {
        let got = translate("permission denied for schema sales", "app", "SELECT");
        assert_eq!(
            got,
            Some((
                1044,
                "42000",
                "Access denied for user 'app'@'%' to database 'sales'".to_string()
            ))
        );
    }

    #[test]
    fn an_ownership_refusal_names_the_ddl_verb() {
        let (code, _, text) = translate("must be owner of table t", "app", "DROP").unwrap_or_default();
        assert_eq!(code, 1142);
        assert_eq!(text, "DROP command denied to user 'app'@'%' for table 't'");
    }

    #[test]
    fn a_refusal_without_an_object_is_1227() {
        let (code, state, _) = translate("permission denied to create role", "app", "CREATE").unwrap_or_default();
        assert_eq!((code, state), (1227, "42000"));
    }

    #[test]
    fn an_anonymous_refusal_keeps_the_table_name() {
        let (code, _, text) = translate(
            "permission denied for table t: no authenticated role on this connection",
            "",
            "SELECT",
        )
        .unwrap_or_default();
        assert_eq!(code, 1142);
        assert!(text.ends_with("for table 't'"), "{text}");
    }

    #[test]
    fn other_errors_are_left_alone() {
        assert_eq!(translate("Table 't' does not exist", "app", "SELECT"), None);
    }
}
