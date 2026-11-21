//! MySQL account statements, mapped onto the server's roles.
//!
//! `CREATE USER 'u'@'%' IDENTIFIED BY 'p'`, `ALTER USER ... IDENTIFIED BY`,
//! `DROP USER` and `SET PASSWORD` act on the same roles `CREATE ROLE ... LOGIN
//! PASSWORD` does, so an account made on either wire logs in on both. Roles
//! are not bound to a host: the host part must be `%` or `localhost` (or be
//! left out), and anything else is refused rather than dropped.

use super::vars::{is_single_literal, split_top_level, strip_keyword, unquote_word, quote_literal, VarError};

/// One account statement, as the role statements that carry it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountStatement {
    /// `CREATE USER`: per account, its name and the password to give it.
    Create {
        if_not_exists: bool,
        accounts: Vec<(String, Option<String>)>,
    },
    /// `ALTER USER ... IDENTIFIED BY` / `SET PASSWORD`. `None` is the session's own account.
    SetPassword {
        if_exists: bool,
        accounts: Vec<(Option<String>, String)>,
    },
    /// `DROP USER`.
    Drop { if_exists: bool, accounts: Vec<String> },
}

fn refused(operation: &str, account: &str, reason: &str) -> VarError {
    VarError {
        code: 1396,
        state: "HY000",
        message: format!("Operation {operation} failed for {account}: {reason}"),
    }
}

/// Read `'name'@'host'` (any quoting, host optional). `Ok(None)` is the
/// session's own account (`CURRENT_USER`, `USER()`).
pub(crate) fn parse_account(operation: &str, text: &str) -> Result<Option<String>, VarError> {
    let text = text.trim();
    let upper = text.to_ascii_uppercase();
    if matches!(upper.as_str(), "CURRENT_USER" | "CURRENT_USER()" | "USER()") {
        return Ok(None);
    }
    // The `@` that separates name and host is the one outside quotes.
    let parts = split_top_level(text, '@');
    let (name, host) = match parts.as_slice() {
        [name] => (*name, None),
        [name, host] => (*name, Some(*host)),
        _ => return Err(VarError::syntax(text)),
    };
    let word_ok = |word: &str| {
        let word = word.trim();
        !word.is_empty()
            && (is_single_literal(word)
                || (word.starts_with('`') && word.ends_with('`') && word.len() >= 2)
                || word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.' | '-' | '%')))
    };
    if !word_ok(name) || host.is_some_and(|h| !word_ok(h)) {
        return Err(VarError::syntax(text));
    }
    let name = unquote_word(name);
    if name.is_empty() {
        return Err(refused(operation, text, "an account needs a name"));
    }
    if let Some(host) = host.map(unquote_word) {
        if host != "%" && !host.eq_ignore_ascii_case("localhost") {
            return Err(refused(
                operation,
                &format!("'{name}'@'{host}'"),
                "HeliosDB Nano accounts are not bound to a host; use '%' or 'localhost'",
            ));
        }
    }
    Ok(Some(name))
}

/// The password of `IDENTIFIED [WITH plugin] BY 'text'`, and `Ok(None)` for no
/// clause at all.
fn parse_identified(operation: &str, account: &str, clause: &str) -> Result<Option<String>, VarError> {
    let clause = clause.trim();
    if clause.is_empty() {
        return Ok(None);
    }
    let rest = strip_keyword(clause, "IDENTIFIED").ok_or_else(|| {
        refused(
            operation,
            account,
            "only IDENTIFIED BY 'password' is supported (no REQUIRE, WITH, PASSWORD EXPIRE or ACCOUNT options)",
        )
    })?;
    let rest = match strip_keyword(rest, "WITH") {
        Some(after_with) => {
            let plugin_end = after_with.find(char::is_whitespace).unwrap_or(after_with.len());
            let plugin = unquote_word(after_with.get(..plugin_end).unwrap_or(""));
            if !matches!(
                plugin.to_ascii_lowercase().as_str(),
                "caching_sha2_password" | "mysql_native_password" | "sha256_password"
            ) {
                return Err(refused(operation, account, &format!("unknown authentication plugin '{plugin}'")));
            }
            after_with.get(plugin_end..).unwrap_or("").trim_start()
        }
        None => rest,
    };
    let password = strip_keyword(rest, "BY").ok_or_else(|| {
        refused(
            operation,
            account,
            "a password is given with IDENTIFIED BY 'password' (a stored hash, IDENTIFIED ... AS, is not accepted)",
        )
    })?;
    if !is_single_literal(password) {
        return Err(refused(operation, account, "the password must be one quoted string"));
    }
    Ok(Some(unquote_word(password)))
}

/// Where the account name ends and its options begin.
fn split_account_clause(piece: &str) -> (&str, &str) {
    let view = super::translator::code_view(piece);
    match view.find("IDENTIFIED").or_else(|| {
        ["REQUIRE", "WITH", "PASSWORD", "ACCOUNT", "DEFAULT", "COMMENT", "ATTRIBUTE"]
            .iter()
            .filter_map(|keyword| view.find(keyword))
            .min()
    }) {
        Some(at) => (piece.get(..at).unwrap_or(piece), piece.get(at..).unwrap_or("")),
        None => (piece, ""),
    }
}

/// Read an account statement. `None`: the statement is not one.
pub(crate) fn parse_account_statement(sql: &str) -> Option<Result<AccountStatement, VarError>> {
    let sql = sql.trim().trim_end_matches(';').trim();
    let (operation, rest) = if let Some(rest) = strip_keyword(sql, "CREATE").and_then(|r| strip_keyword(r, "USER")) {
        ("CREATE USER", rest)
    } else if let Some(rest) = strip_keyword(sql, "ALTER").and_then(|r| strip_keyword(r, "USER")) {
        ("ALTER USER", rest)
    } else if let Some(rest) = strip_keyword(sql, "DROP").and_then(|r| strip_keyword(r, "USER")) {
        ("DROP USER", rest)
    } else {
        return None;
    };
    Some(parse_body(operation, rest))
}

fn parse_body(operation: &str, rest: &str) -> Result<AccountStatement, VarError> {
    let (flag, rest) = match strip_keyword(rest, "IF") {
        Some(after_if) => {
            let after = if operation == "CREATE USER" {
                strip_keyword(after_if, "NOT").and_then(|r| strip_keyword(r, "EXISTS"))
            } else {
                strip_keyword(after_if, "EXISTS")
            };
            (true, after.ok_or_else(|| VarError::syntax(rest))?)
        }
        None => (false, rest),
    };
    if rest.trim().is_empty() {
        return Err(VarError::syntax(operation));
    }
    let pieces = split_top_level(rest, ',');
    match operation {
        "CREATE USER" => {
            let mut accounts = Vec::new();
            for piece in pieces {
                let (account, clause) = split_account_clause(piece);
                let name = parse_account(operation, account)?
                    .ok_or_else(|| refused(operation, account.trim(), "name the account to create"))?;
                let password = parse_identified(operation, account.trim(), clause)?;
                accounts.push((name, password));
            }
            Ok(AccountStatement::Create {
                if_not_exists: flag,
                accounts,
            })
        }
        "ALTER USER" => {
            let mut accounts = Vec::new();
            for piece in pieces {
                let (account, clause) = split_account_clause(piece);
                let name = parse_account(operation, account)?;
                let password = parse_identified(operation, account.trim(), clause)?.ok_or_else(|| {
                    refused(
                        operation,
                        account.trim(),
                        "only ALTER USER ... IDENTIFIED BY 'password' is supported",
                    )
                })?;
                accounts.push((name, password));
            }
            Ok(AccountStatement::SetPassword {
                if_exists: flag,
                accounts,
            })
        }
        _ => {
            let mut accounts = Vec::new();
            for piece in pieces {
                let name = parse_account(operation, piece)?
                    .ok_or_else(|| refused(operation, piece.trim(), "name the account to drop"))?;
                accounts.push(name);
            }
            Ok(AccountStatement::Drop {
                if_exists: flag,
                accounts,
            })
        }
    }
}

/// `SET PASSWORD [FOR account] = 'text'` as an account statement.
pub(crate) fn set_password_statement(account: Option<&str>, expr: &str) -> Result<AccountStatement, VarError> {
    let name = match account {
        Some(text) => parse_account("SET PASSWORD", text)?,
        None => None,
    };
    if !is_single_literal(expr) {
        return Err(refused(
            "SET PASSWORD",
            account.unwrap_or("CURRENT_USER()"),
            "the password must be one quoted string (PASSWORD() and stored hashes are not accepted)",
        ));
    }
    Ok(AccountStatement::SetPassword {
        if_exists: false,
        accounts: vec![(name, unquote_word(expr))],
    })
}

/// `"name"`: a role name the engine keeps exactly as written.
pub(crate) fn quote_role(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The role statement that creates `name`.
pub(crate) fn create_role_sql(name: &str, password: Option<&str>) -> String {
    match password {
        Some(password) => format!("CREATE ROLE {} LOGIN PASSWORD {}", quote_role(name), quote_literal(password)),
        None => format!("CREATE ROLE {} LOGIN", quote_role(name)),
    }
}

/// The role statement that sets `name`'s password.
pub(crate) fn alter_role_password_sql(name: &str, password: &str) -> String {
    format!("ALTER ROLE {} PASSWORD {}", quote_role(name), quote_literal(password))
}

/// The role statement that drops `name`.
pub(crate) fn drop_role_sql(name: &str) -> String {
    format!("DROP ROLE {}", quote_role(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Result<AccountStatement, VarError> {
        parse_account_statement(sql).expect("an account statement")
    }

    #[test]
    fn create_user_is_read_in_its_spellings() {
        assert_eq!(
            parse("CREATE USER 'app'@'%' IDENTIFIED BY 'it''s'").unwrap(),
            AccountStatement::Create {
                if_not_exists: false,
                accounts: vec![("app".into(), Some("it's".into()))]
            }
        );
        assert_eq!(
            parse("create user if not exists `a b`@localhost identified with caching_sha2_password by 'p,q', c").unwrap(),
            AccountStatement::Create {
                if_not_exists: true,
                accounts: vec![("a b".into(), Some("p,q".into())), ("c".into(), None)]
            }
        );
    }

    #[test]
    fn a_host_other_than_any_or_localhost_is_refused() {
        let err = parse("CREATE USER 'app'@'10.0.0.%' IDENTIFIED BY 'p'").unwrap_err();
        assert_eq!(err.code, 1396);
        assert!(err.message.contains("'%' or 'localhost'"), "{}", err.message);
        assert_eq!(parse("DROP USER app@example.com").unwrap_err().code, 1396);
    }

    #[test]
    fn options_that_would_be_dropped_are_refused() {
        for sql in [
            "CREATE USER 'a'@'%' IDENTIFIED BY 'p' REQUIRE SSL",
            "CREATE USER 'a'@'%' IDENTIFIED WITH mysql_native_password AS '*HASH'",
            "CREATE USER 'a'@'%' PASSWORD EXPIRE",
            "ALTER USER 'a'@'%' ACCOUNT LOCK",
            "CREATE USER 'a'@'%' IDENTIFIED WITH ldap BY 'p'",
        ] {
            assert_eq!(parse(sql).unwrap_err().code, 1396, "{sql}");
        }
    }

    #[test]
    fn alter_drop_and_set_password_are_read() {
        assert_eq!(
            parse("ALTER USER IF EXISTS 'app'@'localhost' IDENTIFIED BY 'n'").unwrap(),
            AccountStatement::SetPassword {
                if_exists: true,
                accounts: vec![(Some("app".into()), "n".into())]
            }
        );
        assert_eq!(
            parse("ALTER USER USER() IDENTIFIED BY 'n'").unwrap(),
            AccountStatement::SetPassword {
                if_exists: false,
                accounts: vec![(None, "n".into())]
            }
        );
        assert_eq!(
            parse("DROP USER IF EXISTS 'a'@'%', b").unwrap(),
            AccountStatement::Drop {
                if_exists: true,
                accounts: vec!["a".into(), "b".into()]
            }
        );
        assert_eq!(
            set_password_statement(Some("'a'@'%'"), "'x'").unwrap(),
            AccountStatement::SetPassword {
                if_exists: false,
                accounts: vec![(Some("a".into()), "x".into())]
            }
        );
        assert_eq!(set_password_statement(None, "PASSWORD('x')").unwrap_err().code, 1396);
        assert!(parse_account_statement("CREATE TABLE user (a int)").is_none());
        assert!(parse_account_statement("DROP USERS").is_none());
    }

    #[test]
    fn role_statements_quote_names_and_passwords() {
        assert_eq!(create_role_sql("a\"b", Some("p'q")), "CREATE ROLE \"a\"\"b\" LOGIN PASSWORD 'p''q'");
        assert_eq!(alter_role_password_sql("App", "x"), "ALTER ROLE \"App\" PASSWORD 'x'");
        assert_eq!(drop_role_sql("app"), "DROP ROLE \"app\"");
    }
}
