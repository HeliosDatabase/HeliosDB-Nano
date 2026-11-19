//! Row-level-security READ policies apply to every relation a DML statement's
//! subqueries read.
//!
//! GH#32 made UPDATE / DELETE accept a subquery in WHERE, but ran it on a plain
//! storage executor that never went through the RLS plan rewrite. With a tenant
//! context and a policy that shows 2 of `docs`' 4 rows, `DELETE FROM plain
//! WHERE id IN (SELECT id FROM docs)` deleted the rows matching the hidden docs
//! too, and the same held for correlated subqueries, a RETURNING subquery and
//! an `INSERT … VALUES ((SELECT … FROM docs))`.
//!
//! Each shape runs through the text family (`execute` / `query`) and the
//! parameter family (`execute_params` / `query_params`, with a `$1` inside the
//! protected read), and a no-context control must still see every row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, deprecated)]

use heliosdb_nano::tenant::{IsolationMode, RLSCommand, TenantContext};
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

/// `docs`: alice owns 1 and 3 (a1, a3); bob owns 2, carol 4.
/// `plain`: no policy, ids 1..4 valued p1..p4. `sink`: empty, no policy.
fn db(with_context: bool) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT NOT NULL, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'alice', 'a1'), (2, 'bob', 'b2'), (3, 'alice', 'a3'), (4, 'carol', 'c4')")
        .unwrap();
    db.execute("CREATE TABLE plain (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO plain VALUES (1, 'p1'), (2, 'p2'), (3, 'p3'), (4, 'p4')")
        .unwrap();
    db.execute("CREATE TABLE sink (id INT, body TEXT)").unwrap();
    let tenant = db
        .tenant_manager
        .register_tenant("acme".to_string(), IsolationMode::SharedSchema);
    db.tenant_manager.create_rls_policy(
        "docs".to_string(),
        "p_owner".to_string(),
        "alice reads her own docs".to_string(),
        RLSCommand::Select,
        "owner = 'alice'".to_string(),
        None,
    );
    if with_context {
        db.tenant_manager.set_current_context(TenantContext {
            tenant_id: tenant.id,
            user_id: "alice".to_string(),
            roles: vec!["member".to_string()],
            isolation_mode: IsolationMode::SharedSchema,
        });
    }
    db
}

fn render(rows: &[Tuple]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|t| {
            t.values
                .iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    out.sort();
    out
}

fn strs(xs: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = xs.iter().map(|s| (*s).to_string()).collect();
    v.sort();
    v
}

/// Run a write; `{p}` is `0` (text) or `$1` bound to 0 (params).
fn exec(db: &EmbeddedDatabase, params: bool, template: &str) -> u64 {
    let out = if params {
        db.execute_params(&template.replace("{p}", "$1"), &[Value::Int4(0)])
    } else {
        db.execute(&template.replace("{p}", "0"))
    };
    out.unwrap_or_else(|e| panic!("[params={params}] `{template}`: {e}"))
}

fn query(db: &EmbeddedDatabase, params: bool, template: &str) -> Vec<String> {
    let out = if params {
        db.query_params(&template.replace("{p}", "$1"), &[Value::Int4(0)])
    } else {
        db.query(&template.replace("{p}", "0"), &[])
    };
    render(&out.unwrap_or_else(|e| panic!("[params={params}] `{template}`: {e}")))
}

/// A scalar subquery in RETURNING or in a VALUES row is not supported on every
/// route yet: the statement is then refused, which leaks nothing. Where it
/// runs, it must see only the rows the policy shows.
fn unsupported(error: &heliosdb_nano::Error) -> bool {
    error.to_string().contains("without materialisation")
}

fn query_if_supported(db: &EmbeddedDatabase, params: bool, template: &str) -> Option<Vec<String>> {
    let out = if params {
        db.query_params(&template.replace("{p}", "$1"), &[Value::Int4(0)])
    } else {
        db.query(&template.replace("{p}", "0"), &[])
    };
    match out {
        Ok(rows) => Some(render(&rows)),
        Err(e) if unsupported(&e) => None,
        Err(e) => panic!("[params={params}] `{template}`: {e}"),
    }
}

fn exec_if_supported(db: &EmbeddedDatabase, params: bool, template: &str) -> Option<u64> {
    let out = if params {
        db.execute_params(&template.replace("{p}", "$1"), &[Value::Int4(0)])
    } else {
        db.execute(&template.replace("{p}", "0"))
    };
    match out {
        Ok(n) => Some(n),
        Err(e) if unsupported(&e) => None,
        Err(e) => panic!("[params={params}] `{template}`: {e}"),
    }
}

fn plain(db: &EmbeddedDatabase) -> Vec<String> {
    render(&db.query("SELECT id, v FROM plain", &[]).unwrap())
}

/// (statement, rows written under the policy, plain afterwards under the policy,
/// rows written without a context)
const WRITES: &[(&str, u64, &[&str], u64)] = &[
    (
        "UPDATE plain SET v = 'x' WHERE id IN (SELECT id FROM docs WHERE id > {p})",
        2,
        &["1|x", "2|p2", "3|x", "4|p4"],
        4,
    ),
    (
        "DELETE FROM plain WHERE id IN (SELECT id FROM docs WHERE id > {p})",
        2,
        &["2|p2", "4|p4"],
        4,
    ),
    (
        "DELETE FROM plain WHERE id NOT IN (SELECT id FROM docs WHERE id > {p})",
        2,
        &["1|p1", "3|p3"],
        0,
    ),
    (
        "UPDATE plain SET v = 'x' WHERE EXISTS (SELECT 1 FROM docs d WHERE d.id = plain.id AND d.id > {p})",
        2,
        &["1|x", "2|p2", "3|x", "4|p4"],
        4,
    ),
    (
        "DELETE FROM plain WHERE NOT EXISTS (SELECT 1 FROM docs d WHERE d.id = plain.id AND d.id > {p})",
        2,
        &["1|p1", "3|p3"],
        0,
    ),
    (
        "DELETE FROM plain AS t WHERE t.id IN (SELECT d.id FROM docs d WHERE d.id = t.id AND d.id > {p})",
        2,
        &["2|p2", "4|p4"],
        4,
    ),
    (
        "UPDATE plain SET v = 'x' WHERE id = (SELECT max(id) FROM docs WHERE id > {p})",
        1,
        &["1|p1", "2|p2", "3|x", "4|p4"],
        1,
    ),
    (
        "DELETE FROM plain WHERE id > (SELECT count(*) FROM docs WHERE id > {p})",
        2,
        &["1|p1", "2|p2"],
        0,
    ),
    (
        "UPDATE plain SET v = 'x' WHERE EXISTS (SELECT 1 FROM docs WHERE id > {p} AND owner <> 'alice')",
        0,
        &["1|p1", "2|p2", "3|p3", "4|p4"],
        4,
    ),
    (
        "UPDATE plain SET v = (SELECT body FROM docs d WHERE d.id = plain.id AND d.id > {p})",
        4,
        &["1|a1", "2|NULL", "3|a3", "4|NULL"],
        4,
    ),
];

#[test]
fn dml_where_and_set_subqueries_see_only_visible_rows() {
    for &(sql, n, after, n_control) in WRITES {
        for params in [false, true] {
            let d = db(true);
            assert_eq!(exec(&d, params, sql), n, "[params={params}] {sql}");
            assert_eq!(plain(&d), strs(after), "[params={params}] {sql}");
            let c = db(false);
            assert_eq!(exec(&c, params, sql), n_control, "[control params={params}] {sql}");
        }
    }
}

#[test]
fn dml_subqueries_inside_a_transaction() {
    for params in [false, true] {
        let d = db(true);
        d.execute("BEGIN").unwrap();
        assert_eq!(
            exec(
                &d,
                params,
                "DELETE FROM plain WHERE id IN (SELECT id FROM docs WHERE id > {p})"
            ),
            2
        );
        assert_eq!(
            exec(
                &d,
                params,
                "UPDATE plain SET v = 'x' WHERE EXISTS (SELECT 1 FROM docs d WHERE d.id = plain.id AND d.id > {p})"
            ),
            0
        );
        d.execute("COMMIT").unwrap();
        assert_eq!(plain(&d), strs(&["2|p2", "4|p4"]));
    }
}

#[test]
fn returning_subqueries_see_only_visible_rows() {
    for params in [false, true] {
        let d = db(true);
        if let Some(rows) = query_if_supported(
            &d,
            params,
            "UPDATE plain SET v = 'x' WHERE id >= 3 RETURNING id, (SELECT body FROM docs d WHERE d.id = 4 AND d.id > {p})",
        ) {
            assert_eq!(rows, strs(&["3|NULL", "4|NULL"]), "[params={params}] UPDATE RETURNING");
        }
        if let Some(rows) = query_if_supported(
            &d,
            params,
            "DELETE FROM plain WHERE id = 2 RETURNING (SELECT count(*) FROM docs WHERE id > {p})",
        ) {
            assert_eq!(rows, strs(&["2"]), "[params={params}] DELETE RETURNING");
        }
        if let Some(rows) = query_if_supported(
            &d,
            params,
            "INSERT INTO sink VALUES (9, 'z') RETURNING (SELECT count(*) FROM docs WHERE id > {p})",
        ) {
            assert_eq!(rows, strs(&["2"]), "[params={params}] INSERT RETURNING");
        }
        let c = db(false);
        if let Some(rows) = query_if_supported(
            &c,
            params,
            "DELETE FROM plain WHERE id = 2 RETURNING (SELECT count(*) FROM docs WHERE id > {p})",
        ) {
            assert_eq!(rows, strs(&["4"]), "[control params={params}]");
        }
    }
}

#[test]
fn insert_values_subquery_sees_only_visible_rows() {
    for params in [false, true] {
        let d = db(true);
        let written = exec_if_supported(
            &d,
            params,
            "INSERT INTO sink VALUES (2, (SELECT body FROM docs WHERE id = 2 AND id > {p})), \
             (3, (SELECT body FROM docs WHERE id = 3 AND id > {p})), \
             (5, (SELECT count(*) FROM docs WHERE id > {p}))",
        );
        let stored = render(&d.query("SELECT id, body FROM sink", &[]).unwrap());
        match written {
            Some(n) => {
                assert_eq!(n, 3);
                assert_eq!(stored, strs(&["2|NULL", "3|a3", "5|2"]), "[params={params}]");
            }
            None => assert!(stored.is_empty(), "[params={params}] a refused INSERT wrote {stored:?}"),
        }
        let c = db(false);
        if exec_if_supported(
            &c,
            params,
            "INSERT INTO sink VALUES (2, (SELECT body FROM docs WHERE id = 2 AND id > {p}))",
        )
        .is_some()
        {
            assert_eq!(
                render(&c.query("SELECT id, body FROM sink", &[]).unwrap()),
                strs(&["2|b2"]),
                "[control params={params}]"
            );
        }
    }
}
