#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! v04.5 — PL/pgSQL procedures and functions take the right branch.
//!
//! Every procedural expression (IF/ELSIF/WHILE/CASE conditions, EXIT/CONTINUE
//! WHEN, `:=` values, FOR bounds, DECLARE defaults, RETURN) is evaluated as
//! `SELECT <text>` after `$`-sigil interpolation. Before, the parser kept the
//! expression as a string literal of its own text, so every IF took ELSE, WHILE
//! never iterated, `i := 0` stored the text '0', `EXIT WHEN` ended the whole
//! routine and numeric FOR did not parse.
//!
//! Variables and parameters are written with the mandatory `$` sigil (bare
//! names are brief v04.6). Every test asserts unconditionally, on the text
//! family (`execute`) and, where a CALL takes an argument, the params family
//! (`execute_params`).

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

fn col(db: &EmbeddedDatabase, sql: &str) -> Vec<Value> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|r| r.values[0].clone())
        .collect()
}

fn texts(v: &[&str]) -> Vec<Value> {
    v.iter().map(|s| Value::String((*s).to_string())).collect()
}

fn ints(v: &[i32]) -> Vec<Value> {
    v.iter().map(|i| Value::Int4(*i)).collect()
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(i) => i64::from(*i),
        Value::Int4(i) => i64::from(*i),
        Value::Int8(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn run(db: &EmbeddedDatabase, sql: &str) {
    db.execute(sql).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
}

#[test]
fn if_elsif_else_takes_each_branch_on_both_families() {
    let db = db();
    run(&db, "CREATE TABLE br (n INT, label TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_branch(n INT) LANGUAGE plpgsql AS $$
         BEGIN
           IF $n > 10 THEN INSERT INTO br VALUES ($n, 'big');
           ELSIF $n > 5 THEN INSERT INTO br VALUES ($n, 'medium');
           ELSEIF $n > 0 THEN INSERT INTO br VALUES ($n, 'small');
           ELSE INSERT INTO br VALUES ($n, 'else');
           END IF;
         END $$",
    );
    run(&db, "CALL p_branch(20)");
    run(&db, "CALL p_branch(7)");
    db.execute_params("CALL p_branch($1)", &[Value::Int4(3)]).unwrap();
    db.execute_params("CALL p_branch($1)", &[Value::Int4(-1)]).unwrap();
    assert_eq!(
        col(&db, "SELECT label FROM br ORDER BY n"),
        texts(&["else", "small", "medium", "big"])
    );
}

#[test]
fn if_true_and_null_conditions() {
    let db = db();
    run(&db, "CREATE TABLE it (v TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_true() LANGUAGE plpgsql AS $$
         BEGIN
           IF TRUE THEN INSERT INTO it VALUES ('true'); END IF;
           IF NULL THEN INSERT INTO it VALUES ('null'); ELSE INSERT INTO it VALUES ('null is not true'); END IF;
         END $$",
    );
    run(&db, "CALL p_true()");
    assert_eq!(
        col(&db, "SELECT v FROM it ORDER BY v"),
        texts(&["null is not true", "true"])
    );
}

#[test]
fn a_non_boolean_condition_is_an_error_not_false() {
    let db = db();
    run(&db, "CREATE TABLE nb (v TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_nb() LANGUAGE plpgsql AS $$
         BEGIN IF 'abc' THEN INSERT INTO nb VALUES ('then'); ELSE INSERT INTO nb VALUES ('else'); END IF; END $$",
    );
    let err = db
        .execute("CALL p_nb()")
        .expect_err("a text condition must fail")
        .to_string();
    assert!(err.contains("must be boolean"), "got: {err}");
    assert!(col(&db, "SELECT v FROM nb").is_empty());
}

#[test]
fn while_loop_with_a_counter_and_typed_assignment() {
    let db = db();
    run(&db, "CREATE TABLE wl (v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_while(lim INT) LANGUAGE plpgsql AS $$
         DECLARE i INT := 0;
         BEGIN
           WHILE $i < $lim LOOP
             i := $i + 1;
             INSERT INTO wl VALUES ($i);
           END LOOP;
         END $$",
    );
    run(&db, "CALL p_while(3)");
    db.execute_params("CALL p_while($1)", &[Value::Int4(2)]).unwrap();
    assert_eq!(col(&db, "SELECT v FROM wl ORDER BY v"), ints(&[1, 1, 2, 2, 3]));
}

#[test]
fn loop_exit_when_false_then_true_and_the_statement_after_the_loop_runs() {
    let db = db();
    run(&db, "CREATE TABLE lx (v TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_exit() LANGUAGE plpgsql AS $$
         DECLARE i INT := 0;
         BEGIN
           LOOP
             i := $i + 1;
             INSERT INTO lx VALUES ('iter ' || $i);
             EXIT WHEN 1 = 2;
             EXIT WHEN $i >= 2;
           END LOOP;
           INSERT INTO lx VALUES ('after loop');
         END $$",
    );
    run(&db, "CALL p_exit()");
    assert_eq!(
        col(&db, "SELECT v FROM lx ORDER BY v"),
        texts(&["after loop", "iter 1", "iter 2"])
    );
}

#[test]
fn labelled_exit_and_continue_reach_the_outer_loop() {
    let db = db();
    run(&db, "CREATE TABLE lb (i INT, j INT)");
    run(
        &db,
        "CREATE PROCEDURE p_label() LANGUAGE plpgsql AS $$
         BEGIN
           <<outer>>
           FOR i IN 1..3 LOOP
             FOR j IN 1..3 LOOP
               CONTINUE outer WHEN $j = 2 AND $i = 1;
               EXIT outer WHEN $i = 3;
               INSERT INTO lb VALUES ($i, $j);
             END LOOP;
           END LOOP outer;
           INSERT INTO lb VALUES (99, 99);
         END $$",
    );
    run(&db, "CALL p_label()");
    let rows = db.query("SELECT i, j FROM lb ORDER BY i, j", &[]).unwrap();
    let got: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (as_i64(&r.values[0]), as_i64(&r.values[1])))
        .collect();
    assert_eq!(got, vec![(1, 1), (2, 1), (2, 2), (2, 3), (99, 99)]);
}

#[test]
fn an_exit_naming_an_unknown_label_is_an_error() {
    let db = db();
    run(
        &db,
        "CREATE PROCEDURE p_badlabel() LANGUAGE plpgsql AS $$
         BEGIN LOOP EXIT nowhere; END LOOP; END $$",
    );
    let err = db.execute("CALL p_badlabel()").expect_err("unknown label").to_string();
    assert!(err.contains("nowhere"), "got: {err}");
}

#[test]
fn numeric_for_loops_range_by_and_reverse() {
    let db = db();
    run(&db, "CREATE TABLE fl (tag TEXT, v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_for() LANGUAGE plpgsql AS $$
         BEGIN
           FOR i IN 1..3 LOOP INSERT INTO fl VALUES ('range', $i); END LOOP;
           FOR i IN 1..6 BY 2 LOOP INSERT INTO fl VALUES ('by', $i); END LOOP;
           FOR i IN REVERSE 3..1 LOOP INSERT INTO fl VALUES ('reverse', $i); END LOOP;
           FOR i IN 5..1 LOOP INSERT INTO fl VALUES ('empty', $i); END LOOP;
         END $$",
    );
    run(&db, "CALL p_for()");
    assert_eq!(
        col(&db, "SELECT v FROM fl WHERE tag = 'range' ORDER BY v"),
        ints(&[1, 2, 3])
    );
    assert_eq!(
        col(&db, "SELECT v FROM fl WHERE tag = 'by' ORDER BY v"),
        ints(&[1, 3, 5])
    );
    assert_eq!(
        col(&db, "SELECT v FROM fl WHERE tag = 'reverse' ORDER BY v"),
        ints(&[1, 2, 3])
    );
    assert!(col(&db, "SELECT v FROM fl WHERE tag = 'empty'").is_empty());
}

#[test]
fn for_over_a_query_runs_its_body_per_row() {
    let db = db();
    run(&db, "CREATE TABLE src (id INT)");
    run(&db, "INSERT INTO src VALUES (10), (20), (30)");
    run(&db, "CREATE TABLE dst (v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_forq(minv INT) LANGUAGE plpgsql AS $$
         BEGIN
           FOR x IN SELECT id FROM src WHERE id >= $minv ORDER BY id LOOP
             INSERT INTO dst VALUES ($x + 1);
           END LOOP;
         END $$",
    );
    run(&db, "CALL p_forq(20)");
    assert_eq!(col(&db, "SELECT v FROM dst ORDER BY v"), ints(&[21, 31]));
}

#[test]
fn case_statement_searched_and_simple() {
    let db = db();
    run(&db, "CREATE TABLE cs (n INT, kind TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_case(n INT) LANGUAGE plpgsql AS $$
         BEGIN
           CASE
             WHEN $n < 0 THEN INSERT INTO cs VALUES ($n, 'searched negative');
             WHEN $n = 0 THEN INSERT INTO cs VALUES ($n, 'searched zero');
             ELSE INSERT INTO cs VALUES ($n, 'searched positive');
           END CASE;
           CASE $n
             WHEN 1, 2 THEN INSERT INTO cs VALUES ($n, 'simple one or two');
             WHEN 0 THEN INSERT INTO cs VALUES ($n, 'simple zero');
             ELSE INSERT INTO cs VALUES ($n, 'simple other');
           END CASE;
         END $$",
    );
    run(&db, "CALL p_case(0)");
    db.execute_params("CALL p_case($1)", &[Value::Int4(2)]).unwrap();
    run(&db, "CALL p_case(-5)");
    assert_eq!(
        col(&db, "SELECT kind FROM cs ORDER BY n, kind"),
        texts(&[
            "searched negative",
            "simple other",
            "searched zero",
            "simple zero",
            "searched positive",
            "simple one or two",
        ])
    );
}

#[test]
fn a_case_statement_with_no_match_and_no_else_raises_case_not_found() {
    let db = db();
    run(
        &db,
        "CREATE PROCEDURE p_case_nf(n INT) LANGUAGE plpgsql AS $$
         BEGIN CASE $n WHEN 1 THEN NULL; END CASE; END $$",
    );
    run(&db, "CALL p_case_nf(1)");
    let err = db.execute("CALL p_case_nf(2)").expect_err("no branch").to_string();
    assert!(err.contains("case not found"), "got: {err}");
}

#[test]
fn assignments_and_declare_defaults_are_evaluated_and_cast() {
    let db = db();
    run(&db, "CREATE TABLE asg (tag TEXT, v TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_asg() LANGUAGE plpgsql AS $$
         DECLARE
           v INT := 2 + 3;
           t TEXT;
           s VARCHAR(10) DEFAULT 'x' || 'y';
         BEGIN
           v := $v + 1;
           t := 40 + 2;
           INSERT INTO asg VALUES ('v', CAST($v AS TEXT));
           INSERT INTO asg VALUES ('t', $t);
           INSERT INTO asg VALUES ('s', $s);
           INSERT INTO asg VALUES ('t is text', CAST($t || '!' AS TEXT));
         END $$",
    );
    run(&db, "CALL p_asg()");
    assert_eq!(
        col(&db, "SELECT v FROM asg ORDER BY tag"),
        texts(&["xy", "42", "42!", "6"])
    );
}

#[test]
fn expressions_scan_whole_words_and_skip_literals() {
    // A condition with 'THEN' inside a string, a variable named `status_end`
    // (it used to end the expression at its `END`), and a `;` inside a string
    // literal in a body INSERT.
    let db = db();
    run(&db, "CREATE TABLE sc (v TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_scan(word TEXT) LANGUAGE plpgsql AS $$
         DECLARE status_end INT := 7;
         BEGIN
           IF $word = 'THENCE' THEN INSERT INTO sc VALUES ('matched THENCE; really'); END IF;
           IF $status_end = 7 THEN INSERT INTO sc VALUES ('status_end read'); END IF;
           INSERT INTO sc VALUES (CASE WHEN $status_end > 1 THEN 'case end ok' ELSE 'no' END);
         END $$",
    );
    run(&db, "CALL p_scan('THENCE')");
    assert_eq!(
        col(&db, "SELECT v FROM sc ORDER BY v"),
        texts(&["case end ok", "matched THENCE; really", "status_end read"])
    );
}

#[test]
fn set_search_path_inside_a_body_is_a_statement_not_an_assignment() {
    let db = db();
    run(&db, "CREATE TABLE sp (v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_sp() LANGUAGE plpgsql AS $$
         BEGIN SET search_path = public; INSERT INTO sp VALUES (1); END $$",
    );
    run(&db, "CALL p_sp()");
    assert_eq!(col(&db, "SELECT v FROM sp"), ints(&[1]));
}

#[test]
fn perform_runs_the_query_and_discards_the_result() {
    let db = db();
    run(&db, "CREATE TABLE pf (v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_perform() LANGUAGE plpgsql AS $$
         BEGIN PERFORM 1 + 1; PERFORM count(*) FROM pf; INSERT INTO pf VALUES (5); END $$",
    );
    run(&db, "CALL p_perform()");
    assert_eq!(col(&db, "SELECT v FROM pf"), ints(&[5]));
}

#[test]
fn a_subquery_condition() {
    let db = db();
    run(&db, "CREATE TABLE ex (id INT)");
    run(&db, "INSERT INTO ex VALUES (1)");
    run(&db, "CREATE TABLE exlog (v TEXT)");
    run(
        &db,
        "CREATE PROCEDURE p_exists(k INT) LANGUAGE plpgsql AS $$
         BEGIN
           IF EXISTS (SELECT 1 FROM ex WHERE id = $1) THEN INSERT INTO exlog VALUES ('found ' || $1);
           ELSE INSERT INTO exlog VALUES ('missing ' || $1); END IF;
         END $$",
    );
    run(&db, "CALL p_exists(1)");
    db.execute_params("CALL p_exists($1)", &[Value::Int4(2)]).unwrap();
    assert_eq!(
        col(&db, "SELECT v FROM exlog ORDER BY v"),
        texts(&["found 1", "missing 2"])
    );
}

#[test]
fn a_function_uses_control_flow_and_loops() {
    let db = db();
    run(
        &db,
        "CREATE FUNCTION f_core_sum(n INT) RETURNS INT LANGUAGE plpgsql AS $$
         DECLARE total INT := 0;
         BEGIN
           FOR i IN 1..$n LOOP
             CONTINUE WHEN $i = 2;
             total := $total + $i;
           END LOOP;
           IF $total > 100 THEN RETURN -1; END IF;
           RETURN $total;
         END $$",
    );
    let v = db.query("SELECT f_core_sum(4)", &[]).unwrap();
    assert_eq!(as_i64(&v[0].values[0]), 8);
    let v = db.query("SELECT f_core_sum(20)", &[]).unwrap();
    assert_eq!(as_i64(&v[0].values[0]), -1);
}

#[test]
fn recursion_is_bounded_by_udf_max_call_depth() {
    let mut config = Config::in_memory();
    config.session.udf_max_call_depth = 8;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    run(
        &db,
        "CREATE FUNCTION f_core_fact(n INT) RETURNS INT LANGUAGE plpgsql AS $$
         BEGIN
           IF $n <= 1 THEN RETURN 1; END IF;
           RETURN $n * f_core_fact($n - 1);
         END $$",
    );
    let v = db.query("SELECT f_core_fact(5)", &[]).unwrap();
    assert_eq!(as_i64(&v[0].values[0]), 120);
    let err = db
        .query("SELECT f_core_fact(50)", &[])
        .expect_err("too deep")
        .to_string();
    assert!(err.contains("depth limit (8)"), "got: {err}");
}

#[test]
fn max_loop_iterations_stops_a_runaway_loop() {
    let mut config = Config::in_memory();
    config.procedural.max_loop_iterations = 50;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    run(&db, "CREATE TABLE rl (v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_runaway() LANGUAGE plpgsql AS $$ BEGIN LOOP NULL; END LOOP; END $$",
    );
    let err = db.execute("CALL p_runaway()").expect_err("runaway").to_string();
    assert!(err.contains("max_loop_iterations"), "got: {err}");
    run(
        &db,
        "CREATE PROCEDURE p_bounded() LANGUAGE plpgsql AS $$
         BEGIN FOR i IN 1..50 LOOP INSERT INTO rl VALUES ($i); END LOOP; END $$",
    );
    run(&db, "CALL p_bounded()");
    assert_eq!(col(&db, "SELECT count(*) FROM rl").first().map(as_i64), Some(50));
}

#[test]
fn ten_thousand_iterations_wall_time() {
    // Measurement for brief v04.7 (each expression is one `SELECT` through the
    // engine): printed, not asserted beyond completing.
    let db = db();
    run(&db, "CREATE TABLE tt (v INT)");
    run(
        &db,
        "CREATE PROCEDURE p_tenk() LANGUAGE plpgsql AS $$
         DECLARE i INT := 0;
         BEGIN
           WHILE $i < 10000 LOOP
             i := $i + 1;
           END LOOP;
           INSERT INTO tt VALUES ($i);
         END $$",
    );
    let started = std::time::Instant::now();
    run(&db, "CALL p_tenk()");
    eprintln!("v04.5 10k-iteration loop: {:?}", started.elapsed());
    assert_eq!(col(&db, "SELECT v FROM tt"), ints(&[10000]));
}
