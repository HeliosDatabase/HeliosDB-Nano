//! The access-control statement parser, on its own: what it takes, what it
//! leaves to the SQL parser, and what it refuses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use super::*;

/// Resolves a relation name to itself, upper-cased, so a test can see that
/// relation names (and only those) went through the resolver.
struct Upper;

impl Resolver for Upper {
    fn relation(&self, raw: &str) -> String {
        raw.to_ascii_uppercase()
    }
}

fn parsed(sql: &str) -> AccessStatement {
    parse(sql, &Upper)
        .unwrap_or_else(|e| panic!("`{sql}` must parse: {e}"))
        .unwrap_or_else(|| panic!("`{sql}` must be an access-control statement"))
}

fn privilege(name: &str, columns: &[&str]) -> PrivilegeSpec {
    PrivilegeSpec {
        name: name.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
    }
}

fn objects(kind: &str, names: &[&str]) -> GrantTarget {
    GrantTarget::Objects {
        kind: kind.to_string(),
        names: names.iter().map(|n| (*n).to_string()).collect(),
    }
}

#[test]
fn a_table_grant_with_columns_options_and_several_grantees() {
    let statement = parsed(
        "GRANT SELECT (id, \"Name\"), insert, UPDATE(total) ON TABLE orders, sales.deals \
         TO app, PUBLIC, \"Mixed\" WITH GRANT OPTION GRANTED BY boss;",
    );
    assert_eq!(
        statement,
        AccessStatement::Grant(GrantSpec {
            privileges: vec![
                privilege("SELECT", &["id", "Name"]),
                privilege("INSERT", &[]),
                privilege("UPDATE", &["total"]),
            ],
            target: objects("table", &["ORDERS", "SALES.DEALS"]),
            grantees: vec!["app".to_string(), "public".to_string(), "Mixed".to_string()],
            with_grant_option: true,
            granted_by: Some("boss".to_string()),
        })
    );
}

#[test]
fn all_privileges_and_the_table_keyword_are_optional_spellings() {
    for sql in [
        "GRANT ALL ON t TO r",
        "GRANT ALL PRIVILEGES ON t TO r",
        "grant all privileges on table t to r",
    ] {
        assert_eq!(
            parsed(sql),
            AccessStatement::Grant(GrantSpec {
                privileges: vec![privilege("ALL", &[])],
                target: objects("table", &["T"]),
                grantees: vec!["r".to_string()],
                with_grant_option: false,
                granted_by: None,
            }),
            "{sql}"
        );
    }
}

#[test]
fn every_object_kind_is_named_by_its_keyword() {
    let target = |sql: &str| match parsed(sql) {
        AccessStatement::Grant(spec) => spec.target,
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(target("GRANT USAGE ON SEQUENCE s.Ticket_No TO r"), objects("sequence", &["ticket_no"]));
    assert_eq!(target("GRANT USAGE, CREATE ON SCHEMA a, b TO r"), objects("schema", &["a", "b"]));
    assert_eq!(target("GRANT CONNECT, TEMP ON DATABASE app TO r"), objects("database", &["app"]));
    assert_eq!(
        target("GRANT EXECUTE ON FUNCTION public.Dbl(int, text), one() TO r"),
        objects("function", &["dbl", "one"])
    );
    assert_eq!(target("GRANT EXECUTE ON PROCEDURE log_it TO r"), objects("function", &["log_it"]));
    assert_eq!(
        target("GRANT SELECT ON ALL TABLES IN SCHEMA public, sales TO r"),
        GrantTarget::AllInSchema {
            kind: "table".to_string(),
            schemas: vec!["public".to_string(), "sales".to_string()],
        }
    );
    assert_eq!(
        target("GRANT EXECUTE ON ALL ROUTINES IN SCHEMA public TO r"),
        GrantTarget::AllInSchema {
            kind: "function".to_string(),
            schemas: vec!["public".to_string()],
        }
    );
}

#[test]
fn temp_is_temporary() {
    match parsed("GRANT TEMP ON DATABASE app TO r") {
        AccessStatement::Grant(spec) => assert_eq!(spec.privileges, vec![privilege("TEMPORARY", &[])]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn revoke_takes_grant_option_for_cascade_and_restrict() {
    assert_eq!(
        parsed("REVOKE GRANT OPTION FOR SELECT ON t FROM r CASCADE"),
        AccessStatement::Revoke(RevokeSpec {
            privileges: vec![privilege("SELECT", &[])],
            target: objects("table", &["T"]),
            grantees: vec!["r".to_string()],
            grant_option_only: true,
            cascade: true,
            granted_by: None,
        })
    );
    match parsed("REVOKE ALL ON t FROM a, b GRANTED BY boss RESTRICT") {
        AccessStatement::Revoke(spec) => {
            assert!(!spec.cascade && !spec.grant_option_only);
            assert_eq!(spec.granted_by.as_deref(), Some("boss"));
            assert_eq!(spec.grantees, vec!["a".to_string(), "b".to_string()]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_grant_without_on_is_a_role_grant() {
    assert_eq!(
        parsed("GRANT readers, \"Writers\" TO ann, ben WITH ADMIN OPTION GRANTED BY boss"),
        AccessStatement::GrantRole {
            roles: vec!["readers".to_string(), "Writers".to_string()],
            members: vec!["ann".to_string(), "ben".to_string()],
            admin_option: true,
            granted_by: Some("boss".to_string()),
        }
    );
    // A role may be named like a privilege: without ON it is a role.
    assert_eq!(
        parsed("GRANT select TO ann"),
        AccessStatement::GrantRole {
            roles: vec!["select".to_string()],
            members: vec!["ann".to_string()],
            admin_option: false,
            granted_by: None,
        }
    );
    assert_eq!(
        parsed("REVOKE ADMIN OPTION FOR readers FROM ann"),
        AccessStatement::RevokeRole {
            roles: vec!["readers".to_string()],
            members: vec!["ann".to_string()],
            admin_option_only: true,
        }
    );
    assert_eq!(
        parsed("REVOKE readers FROM ann CASCADE"),
        AccessStatement::RevokeRole {
            roles: vec!["readers".to_string()],
            members: vec!["ann".to_string()],
            admin_option_only: false,
        }
    );
}

#[test]
fn set_and_reset_role_and_session_authorization() {
    assert_eq!(parsed("SET ROLE app"), AccessStatement::SetRole(Some("app".to_string())));
    assert_eq!(parsed("set session role \"App\";"), AccessStatement::SetRole(Some("App".to_string())));
    assert_eq!(parsed("SET ROLE 'app'"), AccessStatement::SetRole(Some("app".to_string())));
    assert_eq!(parsed("SET ROLE NONE"), AccessStatement::SetRole(None));
    assert_eq!(parsed("RESET ROLE"), AccessStatement::SetRole(None));
    assert_eq!(
        parsed("SET SESSION AUTHORIZATION app"),
        AccessStatement::SetSessionAuthorization(Some("app".to_string()))
    );
    assert_eq!(
        parsed("SET SESSION AUTHORIZATION DEFAULT"),
        AccessStatement::SetSessionAuthorization(None)
    );
    assert_eq!(
        parsed("RESET SESSION AUTHORIZATION"),
        AccessStatement::SetSessionAuthorization(None)
    );
    for sql in ["SET LOCAL ROLE app", "SET LOCAL SESSION AUTHORIZATION app"] {
        let error = parse(sql, &Upper).unwrap_err().to_string();
        assert!(error.contains(crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER), "{sql}: {error}");
    }
}

#[test]
fn alter_owner_and_rename_role() {
    assert_eq!(
        parsed("ALTER TABLE IF EXISTS sales.deals OWNER TO app"),
        AccessStatement::AlterOwner {
            kind: "table".to_string(),
            name: "SALES.DEALS".to_string(),
            kind_word: "table".to_string(),
            new_owner: "app".to_string(),
        }
    );
    for (sql, kind, word, name) in [
        ("ALTER VIEW v OWNER TO app", "table", "view", "V"),
        ("ALTER MATERIALIZED VIEW mv OWNER TO app", "table", "materialized view", "MV"),
        ("ALTER SEQUENCE s OWNER TO app", "sequence", "sequence", "s"),
        ("ALTER SCHEMA s OWNER TO app", "schema", "schema", "s"),
        ("ALTER FUNCTION f(int) OWNER TO app", "function", "function", "f"),
        ("ALTER PROCEDURE p OWNER TO app", "function", "procedure", "p"),
        ("ALTER DATABASE d OWNER TO app", "database", "database", "d"),
    ] {
        assert_eq!(
            parsed(sql),
            AccessStatement::AlterOwner {
                kind: kind.to_string(),
                name: name.to_string(),
                kind_word: word.to_string(),
                new_owner: "app".to_string(),
            },
            "{sql}"
        );
    }
    assert_eq!(
        parsed("ALTER ROLE app RENAME TO app2"),
        AccessStatement::RenameRole {
            name: "app".to_string(),
            new_name: "app2".to_string(),
        }
    );
    assert_eq!(
        parsed("ALTER USER app RENAME TO app2"),
        AccessStatement::RenameRole {
            name: "app".to_string(),
            new_name: "app2".to_string(),
        }
    );
}

#[test]
fn everything_else_is_left_to_the_sql_parser() {
    for sql in [
        "SELECT 1",
        "SET search_path TO app",
        "SET statement_timeout = 100",
        "SET role_cache = on",
        "RESET ALL",
        "RESET search_path",
        "ALTER TABLE t ADD COLUMN c INT",
        "ALTER TABLE t RENAME TO u",
        "ALTER ROLE app WITH LOGIN",
        "ALTER ROLE app PASSWORD 'secret'",
        "ALTER SEQUENCE s RESTART WITH 5",
        // MySQL's spellings carry `*`, `@` and quotes this grammar has no place for.
        "GRANT ALL PRIVILEGES ON *.* TO 'app'@'%'",
        "GRANT SELECT ON db.* TO app@localhost",
    ] {
        assert_eq!(parse(sql, &Upper).unwrap(), None, "{sql}");
        assert!(!is_access_statement(sql), "{sql}");
    }
}

#[test]
fn what_is_recognised_but_not_implemented_is_refused_by_name() {
    for sql in [
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO app",
        "GRANT USAGE ON TYPE mood TO app",
        "GRANT USAGE ON LANGUAGE plpgsql TO app",
        "GRANT ALL ON TABLESPACE fast TO app",
    ] {
        let error = parse(sql, &Upper).unwrap_err().to_string();
        assert!(error.contains(crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER), "{sql}: {error}");
        assert!(is_access_statement(sql), "{sql}");
    }
}

#[test]
fn a_malformed_statement_is_a_syntax_error_not_a_guess() {
    for sql in [
        "GRANT SELECT ON t",
        "GRANT SELECT ON t TO",
        "GRANT SELECT (id ON t TO r",
        "GRANT SELECT ON t TO r WITH GRANT",
        "REVOKE SELECT ON t TO r",
        "GRANT readers TO ann WITH ADMIN",
        "GRANT SELECT ON t TO r extra words",
        "REVOKE GRANT OPTION FOR readers FROM ann",
    ] {
        assert!(parse(sql, &Upper).is_err(), "`{sql}` must be refused");
    }
}

#[test]
fn security_definer_is_refused_on_a_routine_definition_only() {
    let refused = [
        "CREATE FUNCTION f() RETURNS INT SECURITY DEFINER AS $$ SELECT 1 $$ LANGUAGE sql",
        "CREATE FUNCTION f() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql SECURITY DEFINER",
        "create or replace function f() returns int language sql security definer as $body$ select 1 $body$",
        "CREATE PROCEDURE p() LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
        "ALTER FUNCTION f() SECURITY DEFINER",
    ];
    for sql in refused {
        let error = refuse_security_definer(sql).unwrap_err().to_string();
        assert!(error.contains("SECURITY DEFINER"), "{sql}: {error}");
        assert!(error.contains(crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER), "{sql}: {error}");
    }
    let allowed = [
        "CREATE FUNCTION f() RETURNS INT SECURITY INVOKER AS $$ SELECT 1 $$ LANGUAGE sql",
        "CREATE FUNCTION f() RETURNS TEXT AS $$ SELECT 'SECURITY DEFINER' $$ LANGUAGE sql",
        "CREATE TABLE notes (body TEXT DEFAULT 'security definer')",
        "CREATE TABLE \"security definer\" (id INT)",
        "INSERT INTO notes VALUES ('SECURITY DEFINER')",
        "SELECT 'CREATE FUNCTION f() SECURITY DEFINER'",
    ];
    for sql in allowed {
        refuse_security_definer(sql).unwrap_or_else(|e| panic!("`{sql}` must be allowed: {e}"));
    }
}
