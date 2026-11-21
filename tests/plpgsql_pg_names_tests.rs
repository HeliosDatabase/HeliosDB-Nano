#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]
//! PL/pgSQL written the PostgreSQL way: parameters and variables as bare
//! names (bound as parameters, never written into the statement), the
//! ambiguity error, qualified names, records, `SELECT … INTO [STRICT]`,
//! `RETURNING … INTO`, `FOUND`, `GET DIAGNOSTICS`, `RAISE`, `EXCEPTION`
//! blocks and dynamic SQL with `USING`.

use heliosdb_nano::{EmbeddedDatabase, Error, Value};

fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, owner TEXT, balance INT)")
        .unwrap();
    db.execute("INSERT INTO acct VALUES (1, 'ann', 100), (2, 'bob', 50), (3, 'cy', 0)")
        .unwrap();
    db.execute("CREATE TABLE log (seq INT, note TEXT)").unwrap();
    db
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        other => int(other).to_string(),
    }
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows[0].values[0].clone()
}

fn notes(db: &EmbeddedDatabase) -> Vec<String> {
    db.query("SELECT seq, note FROM log ORDER BY seq", &[])
        .unwrap()
        .iter()
        .map(|r| format!("{}:{}", int(&r.values[0]), text(&r.values[1])))
        .collect()
}

fn sqlstate(e: &Error) -> String {
    e.raised_sqlstate().unwrap_or("-").to_string()
}

fn call_err(db: &EmbeddedDatabase, sql: &str) -> Error {
    match db.execute(sql) {
        Ok(_) => panic!("`{sql}` must fail"),
        Err(e) => e,
    }
}

#[test]
fn bare_names_in_expressions() {
    let db = db();
    db.execute(
        "CREATE FUNCTION tenfold(x INT) RETURNS INT LANGUAGE plpgsql AS $$ \
         DECLARE factor INT := 10; total INT; \
         BEGIN \
           total := x * factor; \
           IF total > 40 THEN total := total + 1; END IF; \
           WHILE total < 20 LOOP total := total + x; END LOOP; \
           RETURN total; \
         END $$",
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT tenfold(5)")), 51);
    assert_eq!(int(&scalar(&db, "SELECT tenfold(1)")), 20);
    // An unquoted name matches its declaration in any case.
    db.execute(
        "CREATE FUNCTION shout(Word TEXT) RETURNS TEXT LANGUAGE plpgsql AS $$ \
         DECLARE Suffix TEXT := '!'; BEGIN RETURN upper(WORD) || suffix; END $$",
    )
    .unwrap();
    assert_eq!(text(&scalar(&db, "SELECT shout('hey')")), "HEY!");
}

#[test]
fn bare_names_in_each_embedded_statement_kind() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE move_money(src INT, dst INT, amount INT) LANGUAGE plpgsql AS $$ \
         DECLARE seen INT; \
         BEGIN \
           UPDATE acct SET balance = balance - amount WHERE acct.id = src; \
           UPDATE acct SET balance = balance + amount WHERE acct.id = dst; \
           SELECT count(*) INTO seen FROM acct WHERE balance >= amount; \
           INSERT INTO log VALUES (seen, 'moved'); \
           DELETE FROM acct WHERE balance = 0 AND acct.id <> dst; \
         END $$",
    )
    .unwrap();
    db.execute("CALL move_money(1, 2, 30)").unwrap();
    let rows = db.query("SELECT id, balance FROM acct ORDER BY id", &[]).unwrap();
    let got: Vec<(i64, i64)> = rows.iter().map(|r| (int(&r.values[0]), int(&r.values[1]))).collect();
    assert_eq!(got, [(1, 70), (2, 80)]);
    assert_eq!(notes(&db), ["2:moved"]);
    // The bound-parameter call family runs the same body.
    db.execute_params(
        "CALL move_money($1, $2, $3)",
        &[Value::Int4(2), Value::Int4(1), Value::Int4(5)],
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 1")), 75);
}

#[test]
fn a_variable_is_data_never_statement_text() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE remember(what TEXT) LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO log VALUES (1, what); UPDATE log SET note = what || '!' WHERE note = what; END $$",
    )
    .unwrap();
    let hostile = "'; DROP TABLE acct; --";
    db.execute_params("CALL remember($1)", &[Value::String(hostile.to_string())])
        .unwrap();
    assert_eq!(notes(&db), [format!("1:{hostile}!")]);
    assert_eq!(int(&scalar(&db, "SELECT count(*) FROM acct")), 3, "the table is still there");
    // Also as a literal argument, quotes and all.
    db.execute("CALL remember('it''s; DELETE FROM acct')").unwrap();
    assert_eq!(int(&scalar(&db, "SELECT count(*) FROM acct")), 3);
    assert_eq!(notes(&db).len(), 2);
}

#[test]
fn a_name_that_is_a_variable_and_a_column_is_ambiguous() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE clash(id INT) LANGUAGE plpgsql AS $$ \
         BEGIN UPDATE acct SET balance = 0 WHERE id = 1; END $$",
    )
    .unwrap();
    let err = call_err(&db, "CALL clash(1)");
    assert_eq!(sqlstate(&err), "42702", "{err}");
    assert!(err.to_string().contains("\"id\" is ambiguous"), "{err}");
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 1")), 100, "nothing ran");

    // Qualified, both are reachable: table.column and routine.variable.
    db.execute(
        "CREATE PROCEDURE no_clash(id INT) LANGUAGE plpgsql AS $$ \
         BEGIN UPDATE acct SET balance = balance + 1 WHERE acct.id = no_clash.id; END $$",
    )
    .unwrap();
    db.execute("CALL no_clash(2)").unwrap();
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 2")), 51);

    // A block label qualifies a variable the same way.
    db.execute(
        "CREATE FUNCTION labelled() RETURNS INT LANGUAGE plpgsql AS $$ \
         <<outer_block>> DECLARE balance INT := 7; got INT; \
         BEGIN SELECT acct.balance + outer_block.balance INTO got FROM acct WHERE acct.id = 1; RETURN got; END $$",
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT labelled()")), 107);

    // The target of an INSERT is not in scope of its VALUES: no ambiguity.
    db.execute(
        "CREATE PROCEDURE add_note(seq INT, note TEXT) LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO log VALUES (seq, note); END $$",
    )
    .unwrap();
    db.execute("CALL add_note(4, 'four')").unwrap();
    assert_eq!(notes(&db), ["4:four"]);
}

#[test]
fn the_dollar_spelling_keeps_working_and_keeps_its_column_rule() {
    let db = db();
    // A body written for the older rule: `$name` is the variable, and a bare
    // name that is also a column stays the column.
    db.execute(
        "CREATE PROCEDURE old_style(id INT, bonus INT) LANGUAGE plpgsql AS $$ \
         BEGIN UPDATE acct SET balance = balance + $bonus WHERE id = $id; \
               UPDATE acct SET balance = balance + $2 WHERE id = $1; END $$",
    )
    .unwrap();
    db.execute("CALL old_style(3, 5)").unwrap();
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 3")), 10);
    assert_eq!(int(&scalar(&db, "SELECT sum(balance) FROM acct")), 160);
    // Both spellings in one body.
    db.execute(
        "CREATE FUNCTION mixed(a INT, b INT) RETURNS INT LANGUAGE plpgsql AS $$ \
         BEGIN RETURN $a * 100 + b; END $$",
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT mixed(4, 2)")), 402);
}

#[test]
fn select_into_strict_and_not_strict() {
    let db = db();
    db.execute(
        "CREATE FUNCTION owner_of(wanted INT) RETURNS TEXT LANGUAGE plpgsql AS $$ \
         DECLARE who TEXT := 'unset'; \
         BEGIN SELECT owner INTO who FROM acct WHERE acct.id = wanted; \
               IF NOT FOUND THEN RETURN 'nobody'; END IF; RETURN who; END $$",
    )
    .unwrap();
    assert_eq!(text(&scalar(&db, "SELECT owner_of(2)")), "bob");
    assert_eq!(text(&scalar(&db, "SELECT owner_of(9)")), "nobody");

    db.execute(
        "CREATE FUNCTION strict_owner(low INT) RETURNS TEXT LANGUAGE plpgsql AS $$ \
         DECLARE who TEXT; \
         BEGIN SELECT owner INTO STRICT who FROM acct WHERE acct.id >= low; RETURN who; END $$",
    )
    .unwrap();
    assert_eq!(text(&scalar(&db, "SELECT strict_owner(3)")), "cy");
    let none = db.query("SELECT strict_owner(9)", &[]).unwrap_err();
    assert_eq!(sqlstate(&none), "P0002", "{none}");
    let many = db.query("SELECT strict_owner(1)", &[]).unwrap_err();
    assert_eq!(sqlstate(&many), "P0003", "{many}");

    // Without STRICT and without a row the targets become NULL.
    db.execute(
        "CREATE FUNCTION nulled() RETURNS TEXT LANGUAGE plpgsql AS $$ \
         DECLARE who TEXT := 'before'; \
         BEGIN SELECT owner INTO who FROM acct WHERE acct.id = 99; RETURN coalesce(who, 'was null'); END $$",
    )
    .unwrap();
    assert_eq!(text(&scalar(&db, "SELECT nulled()")), "was null");
}

#[test]
fn records_hold_a_row_and_give_its_fields() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE audit() LANGUAGE plpgsql AS $$ \
         DECLARE r RECORD; one acct%ROWTYPE; n INT := 0; \
         BEGIN \
           FOR r IN SELECT id, owner, balance FROM acct ORDER BY id LOOP \
             n := n + 1; \
             INSERT INTO log VALUES (r.id * 10 + n, r.owner || '=' || r.balance); \
           END LOOP; \
           SELECT * INTO one FROM acct WHERE acct.id = 2; \
           INSERT INTO log VALUES (one.id + 100, one.owner); \
         END $$",
    )
    .unwrap();
    db.execute("CALL audit()").unwrap();
    assert_eq!(notes(&db), ["11:ann=100", "22:bob=50", "33:cy=0", "102:bob"]);

    db.execute(
        "CREATE PROCEDURE bad_field() LANGUAGE plpgsql AS $$ \
         DECLARE r RECORD; \
         BEGIN FOR r IN SELECT id FROM acct LOOP INSERT INTO log VALUES (r.nope, 'x'); END LOOP; END $$",
    )
    .unwrap();
    let err = call_err(&db, "CALL bad_field()");
    assert_eq!(sqlstate(&err), "42703", "{err}");
}

#[test]
fn returning_into_takes_the_changed_row() {
    let db = db();
    db.execute("CREATE TABLE ticket (id SERIAL PRIMARY KEY, title TEXT)").unwrap();
    db.execute(
        "CREATE PROCEDURE open_ticket(title TEXT) LANGUAGE plpgsql AS $$ \
         DECLARE new_id INT; old_balance INT; \
         BEGIN \
           INSERT INTO ticket (title) VALUES (open_ticket.title) RETURNING id INTO new_id; \
           INSERT INTO log VALUES (new_id, 'opened'); \
           UPDATE acct SET balance = balance + 1 WHERE acct.id = 1 RETURNING balance INTO old_balance; \
           INSERT INTO log VALUES (old_balance, 'balance'); \
         END $$",
    )
    .unwrap();
    db.execute("CALL open_ticket('first')").unwrap();
    db.execute("CALL open_ticket('second')").unwrap();
    assert_eq!(notes(&db), ["1:opened", "2:opened", "101:balance", "102:balance"]);
}

#[test]
fn found_and_row_count_follow_each_statement_kind() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE probe() LANGUAGE plpgsql AS $$ \
         DECLARE n INT; x INT; \
         BEGIN \
           INSERT INTO log VALUES (0, 'start:' || FOUND); \
           UPDATE acct SET balance = balance WHERE acct.id > 1; \
           GET DIAGNOSTICS n = ROW_COUNT; \
           INSERT INTO log VALUES (1, 'update:' || FOUND || ':' || n); \
           UPDATE acct SET balance = balance WHERE acct.id > 100; \
           GET DIAGNOSTICS n = ROW_COUNT; \
           INSERT INTO log VALUES (2, 'update none:' || FOUND || ':' || n); \
           DELETE FROM acct WHERE acct.id = 3; \
           INSERT INTO log VALUES (3, 'delete:' || FOUND); \
           PERFORM 1 FROM acct WHERE acct.id = 3; \
           INSERT INTO log VALUES (4, 'perform:' || FOUND); \
           SELECT balance INTO x FROM acct WHERE acct.id = 1; \
           INSERT INTO log VALUES (5, 'select into:' || FOUND); \
           FOR x IN SELECT id FROM acct WHERE acct.id > 100 LOOP NULL; END LOOP; \
           INSERT INTO log VALUES (6, 'empty loop:' || FOUND); \
           FOR x IN SELECT id FROM acct LOOP NULL; END LOOP; \
           INSERT INTO log VALUES (7, 'loop:' || FOUND); \
         END $$",
    )
    .unwrap();
    db.execute("CALL probe()").unwrap();
    assert_eq!(
        notes(&db),
        [
            "0:start:false",
            "1:update:true:2",
            "2:update none:false:0",
            "3:delete:true",
            "4:perform:false",
            "5:select into:true",
            "6:empty loop:false",
            "7:loop:true",
        ]
    );
}

#[test]
fn raise_formats_its_message_and_carries_its_sqlstate() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE complain(kind INT, who TEXT) LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF kind = 1 THEN RAISE EXCEPTION 'no account for % (%%, code %)', who, kind + 41; END IF; \
           IF kind = 2 THEN RAISE EXCEPTION 'custom' USING ERRCODE = 'AB123', DETAIL = 'about ' || who, HINT = 'try again'; END IF; \
           IF kind = 3 THEN RAISE unique_violation USING MESSAGE = 'duplicate ' || who; END IF; \
           IF kind = 4 THEN RAISE SQLSTATE '22012'; END IF; \
           IF kind = 5 THEN RAISE EXCEPTION 'named code' USING ERRCODE = 'check_violation'; END IF; \
           IF kind = 6 THEN RAISE 'short form %', who; END IF; \
           IF kind = 7 THEN RAISE EXCEPTION 'too % few %', who; END IF; \
           RAISE NOTICE 'only a notice for %', who; \
           INSERT INTO log VALUES (kind, 'after the notice'); \
         END $$",
    )
    .unwrap();
    let e = call_err(&db, "CALL complain(1, 'zed')");
    assert_eq!(sqlstate(&e), "P0001");
    assert_eq!(e.to_string(), "no account for zed (%, code 42)");
    let e = call_err(&db, "CALL complain(2, 'zed')");
    assert_eq!(sqlstate(&e), "AB123");
    match &e {
        Error::Raised { detail, hint, message, .. } => {
            assert_eq!(message, "custom");
            assert_eq!(detail.as_deref(), Some("about zed"));
            assert_eq!(hint.as_deref(), Some("try again"));
        }
        other => panic!("{other:?}"),
    }
    let e = call_err(&db, "CALL complain(3, 'zed')");
    assert_eq!((sqlstate(&e), e.to_string()), ("23505".to_string(), "duplicate zed".to_string()));
    assert_eq!(sqlstate(&call_err(&db, "CALL complain(4, 'zed')")), "22012");
    assert_eq!(sqlstate(&call_err(&db, "CALL complain(5, 'zed')")), "23514");
    let e = call_err(&db, "CALL complain(6, 'zed')");
    assert_eq!((sqlstate(&e), e.to_string()), ("P0001".to_string(), "short form zed".to_string()));
    assert_eq!(sqlstate(&call_err(&db, "CALL complain(7, 'zed')")), "42601");
    // A notice does not stop the routine.
    db.execute("CALL complain(0, 'zed')").unwrap();
    assert_eq!(notes(&db), ["0:after the notice"]);
    // In a function too.
    db.execute(
        "CREATE FUNCTION must_be_positive(n INT) RETURNS INT LANGUAGE plpgsql AS $$ \
         BEGIN IF n <= 0 THEN RAISE EXCEPTION '% is not positive', n USING ERRCODE = '22023'; END IF; RETURN n; END $$",
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT must_be_positive(3)")), 3);
    let e = db.query("SELECT must_be_positive(-2)", &[]).unwrap_err();
    assert_eq!(sqlstate(&e), "22023", "{e}");
    assert!(e.to_string().contains("-2 is not positive"), "{e}");
}

#[test]
fn an_exception_block_catches_and_undoes_its_own_statements() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE guarded(kind INT) LANGUAGE plpgsql AS $$ \
         DECLARE x INT; \
         BEGIN \
           INSERT INTO log VALUES (1, 'before the block'); \
           BEGIN \
             INSERT INTO log VALUES (2, 'inside, undone when caught'); \
             IF kind = 1 THEN INSERT INTO acct VALUES (1, 'dup', 0); END IF; \
             IF kind = 2 THEN x := 1 / (kind - 2); END IF; \
             IF kind = 3 THEN SELECT balance INTO STRICT x FROM acct WHERE acct.id = 99; END IF; \
             IF kind = 4 THEN SELECT balance INTO STRICT x FROM acct; END IF; \
             IF kind = 5 THEN RAISE EXCEPTION 'mine' USING ERRCODE = 'ZZ001'; END IF; \
             IF kind = 6 THEN RAISE EXCEPTION 'not handled here' USING ERRCODE = 'ZZ999'; END IF; \
             INSERT INTO log VALUES (3, 'inside, kept'); \
           EXCEPTION \
             WHEN unique_violation THEN INSERT INTO log VALUES (10, 'unique:' || SQLSTATE); \
             WHEN division_by_zero THEN INSERT INTO log VALUES (11, 'div:' || SQLSTATE); \
             WHEN NO_DATA_FOUND OR TOO_MANY_ROWS THEN INSERT INTO log VALUES (12, 'rows:' || SQLSTATE); \
             WHEN SQLSTATE 'ZZ001' THEN INSERT INTO log VALUES (13, 'mine:' || SQLERRM); \
           END; \
           INSERT INTO log VALUES (20, 'after the block'); \
         END $$",
    )
    .unwrap();
    let run = |kind: i32| -> Vec<String> {
        db.execute("DELETE FROM log").unwrap();
        db.execute(&format!("CALL guarded({kind})")).unwrap();
        notes(&db)
    };
    assert_eq!(run(0), ["1:before the block", "2:inside, undone when caught", "3:inside, kept", "20:after the block"]);
    assert_eq!(run(1), ["1:before the block", "10:unique:23505", "20:after the block"]);
    assert_eq!(run(2), ["1:before the block", "11:div:22012", "20:after the block"]);
    assert_eq!(run(3), ["1:before the block", "12:rows:P0002", "20:after the block"]);
    assert_eq!(run(4), ["1:before the block", "12:rows:P0003", "20:after the block"]);
    assert_eq!(run(5), ["1:before the block", "13:mine:mine", "20:after the block"]);
    // An error no handler names leaves the block and undoes the whole call.
    db.execute("DELETE FROM log").unwrap();
    let e = call_err(&db, "CALL guarded(6)");
    assert_eq!(sqlstate(&e), "ZZ999");
    assert!(notes(&db).is_empty(), "the failed CALL left nothing: {:?}", notes(&db));
    assert_eq!(int(&scalar(&db, "SELECT count(*) FROM acct")), 3);
}

#[test]
fn others_catches_and_a_bare_raise_raises_again() {
    let db = db();
    db.execute(
        "CREATE FUNCTION safe_div(a INT, b INT) RETURNS INT LANGUAGE plpgsql AS $$ \
         BEGIN RETURN a / b; EXCEPTION WHEN OTHERS THEN RETURN -1; END $$",
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT safe_div(6, 3)")), 2);
    assert_eq!(int(&scalar(&db, "SELECT safe_div(6, 0)")), -1);

    db.execute(
        "CREATE PROCEDURE note_and_fail() LANGUAGE plpgsql AS $$ \
         BEGIN \
           BEGIN RAISE EXCEPTION 'inner %', 7 USING ERRCODE = 'ZZ007', HINT = 'h'; \
           EXCEPTION WHEN OTHERS THEN RAISE; END; \
         END $$",
    )
    .unwrap();
    let e = call_err(&db, "CALL note_and_fail()");
    assert_eq!((sqlstate(&e), e.to_string()), ("ZZ007".to_string(), "inner 7".to_string()));
    db.execute("CREATE PROCEDURE lone_raise() LANGUAGE plpgsql AS $$ BEGIN RAISE; END $$")
        .unwrap();
    assert_eq!(sqlstate(&call_err(&db, "CALL lone_raise()")), "0Z002");
}

#[test]
fn dynamic_sql_binds_its_using_values() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE dyn(tbl TEXT, who TEXT, amount INT) LANGUAGE plpgsql AS $$ \
         DECLARE got INT; n INT; \
         BEGIN \
           EXECUTE 'UPDATE ' || tbl || ' SET balance = balance + $2 WHERE owner = $1' USING who, amount; \
           GET DIAGNOSTICS n = ROW_COUNT; \
           EXECUTE 'SELECT balance FROM acct WHERE owner = $1' INTO got USING who; \
           INSERT INTO log VALUES (n, who || ':' || coalesce(got, -1)); \
         END $$",
    )
    .unwrap();
    db.execute("CALL dyn('acct', 'ann', 11)").unwrap();
    assert_eq!(notes(&db), ["1:ann:111"]);
    // A hostile value is compared as a value: no row matches, nothing is dropped.
    db.execute("DELETE FROM log").unwrap();
    db.execute_params(
        "CALL dyn('acct', $1, 1)",
        &[Value::String("'; DROP TABLE acct; --".to_string())],
    )
    .unwrap();
    assert_eq!(notes(&db), ["0:'; DROP TABLE acct; --:-1"]);
    assert_eq!(int(&scalar(&db, "SELECT count(*) FROM acct")), 3);

    db.execute(
        "CREATE FUNCTION dyn_strict(low INT) RETURNS INT LANGUAGE plpgsql AS $$ \
         DECLARE got INT; \
         BEGIN EXECUTE 'SELECT id FROM acct WHERE id >= $1' INTO STRICT got USING low; RETURN got; END $$",
    )
    .unwrap();
    assert_eq!(int(&scalar(&db, "SELECT dyn_strict(3)")), 3);
    assert_eq!(sqlstate(&db.query("SELECT dyn_strict(1)", &[]).unwrap_err()), "P0003");
    assert_eq!(sqlstate(&db.query("SELECT dyn_strict(8)", &[]).unwrap_err()), "P0002");
    db.execute("CREATE PROCEDURE dyn_null() LANGUAGE plpgsql AS $$ DECLARE c TEXT; BEGIN EXECUTE c; END $$")
        .unwrap();
    assert_eq!(sqlstate(&call_err(&db, "CALL dyn_null()")), "22004");
}

#[test]
fn a_routine_inside_a_transaction_block_follows_the_block() {
    let db = db();
    db.execute(
        "CREATE PROCEDURE spend(who INT, amount INT) LANGUAGE plpgsql AS $$ \
         DECLARE left_over INT; \
         BEGIN \
           UPDATE acct SET balance = balance - amount WHERE acct.id = who RETURNING balance INTO left_over; \
           IF left_over < 0 THEN RAISE EXCEPTION 'account % would go to %', who, left_over USING ERRCODE = '23514'; END IF; \
         END $$",
    )
    .unwrap();
    db.begin().unwrap();
    db.execute("CALL spend(1, 40)").unwrap();
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 1")), 60);
    db.rollback().unwrap();
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 1")), 100);
    let e = call_err(&db, "CALL spend(2, 80)");
    assert_eq!(sqlstate(&e), "23514");
    assert_eq!(e.to_string(), "account 2 would go to -30");
    assert_eq!(int(&scalar(&db, "SELECT balance FROM acct WHERE id = 2")), 50, "the failed CALL changed nothing");
}
