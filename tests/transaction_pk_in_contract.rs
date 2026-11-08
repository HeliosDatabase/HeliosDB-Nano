//! Contract retained when transaction-owned mutations select PK-IN candidates.
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Result, Tuple, Value};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

impl Family {
    fn write(self, db: &EmbeddedDatabase, text: &str, bound: &str, values: &[Value]) -> Result<(u64, Vec<Tuple>)> {
        match self {
            Self::Text => db.execute_returning(text),
            Self::Params => db.execute_params_returning(bound, values),
        }
    }
}

fn db(history: bool) -> Result<EmbeddedDatabase> {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = history;
    let db = EmbeddedDatabase::with_config(config)?;
    db.execute("CREATE TABLE parent (id BIGINT PRIMARY KEY)")?;
    db.execute("INSERT INTO parent VALUES (1)")?;
    db.execute(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT CHECK(v >= 0), u INT UNIQUE, pid BIGINT REFERENCES parent(id))",
    )?;
    db.execute("INSERT INTO t VALUES (1,10,101,1),(2,20,102,1),(3,30,103,1),(4,40,104,1)")?;
    Ok(db)
}

fn ints(rows: Vec<Tuple>) -> Result<Vec<Vec<i64>>> {
    let mut result: Vec<Vec<i64>> = rows
        .into_iter()
        .map(|row| {
            row.values
                .into_iter()
                .map(|value| match value {
                    Value::Int2(n) => Ok(i64::from(n)),
                    Value::Int4(n) => Ok(i64::from(n)),
                    Value::Int8(n) => Ok(n),
                    other => Err(Error::storage(format!("expected integer contract field: {other:?}"))),
                })
                .collect()
        })
        .collect::<Result<_>>()?;
    result.sort();
    Ok(result)
}

fn image(db: &EmbeddedDatabase) -> Result<Vec<Vec<i64>>> {
    ints(db.query("SELECT id,v,u,pid FROM t ORDER BY id", &[])?)
}

fn staged_changes(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("INSERT INTO t VALUES (10,100,110,1)")?;
    db.execute("UPDATE t SET id=11 WHERE id=1")?;
    db.execute("DELETE FROM t WHERE id=2")?;
    db.execute("SAVEPOINT keep")?;
    db.execute("UPDATE t SET id=14 WHERE id=4")?;
    db.execute("ROLLBACK TO SAVEPOINT keep")?;
    Ok(())
}

fn update_in(family: Family, db: &EmbeddedDatabase) -> Result<()> {
    let (count, returned) = family.write(
        db,
        "UPDATE t SET v=v+7 WHERE id IN (2,3,10,11,3,99) RETURNING id,v,u,pid",
        "UPDATE t SET v=v+$1 WHERE id IN ($2,$3,$4,$5,$6,$7) RETURNING id,v,u,pid",
        &[7, 2, 3, 10, 11, 3, 99].map(Value::Int4),
    )?;
    assert_eq!(count, 3);
    assert_eq!(
        ints(returned)?,
        vec![vec![3, 37, 103, 1], vec![10, 107, 110, 1], vec![11, 17, 101, 1]]
    );
    let (count, returned) = family.write(
        db,
        "DELETE FROM t WHERE id IN (4,10,10,98) RETURNING id",
        "DELETE FROM t WHERE id IN ($1,$2,$3,$4) RETURNING id",
        &[4, 10, 10, 98].map(Value::Int4),
    )?;
    assert_eq!(count, 2);
    assert_eq!(ints(returned)?, vec![vec![4], vec![10]]);
    assert_eq!(image(db)?, vec![vec![3, 37, 103, 1], vec![11, 17, 101, 1]]);
    Ok(())
}

#[test]
fn staged_rows_pk_changes_tombstones_and_savepoints_are_not_lost() -> Result<()> {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for commit in [false, true] {
                let db = db(history)?;
                let before = image(&db)?;
                db.begin()?;
                staged_changes(&db)?;
                update_in(family, &db)?;
                if commit {
                    db.commit()?;
                } else {
                    db.rollback()?;
                }
                assert_eq!(
                    image(&db)?,
                    if commit {
                        vec![vec![3, 37, 103, 1], vec![11, 17, 101, 1]]
                    } else {
                        before
                    }
                );
                eprintln!("transaction-pk-in-cell staged history={history} family={family:?} commit={commit}");
            }
        }
    }
    Ok(())
}

#[test]
fn implicit_owner_duplicates_bound_values_and_absent_keys_keep_exact_counts() -> Result<()> {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let db = db(history)?;
            let (count, returned) = family.write(
                &db,
                "UPDATE t SET v=v+1 WHERE id IN (1,2,2,99) RETURNING id,v",
                "UPDATE t SET v=v+$1 WHERE id IN ($2,$3,$4,$5) RETURNING id,v",
                &[1, 1, 2, 2, 99].map(Value::Int4),
            )?;
            assert_eq!(count, 2);
            assert_eq!(ints(returned)?, vec![vec![1, 11], vec![2, 21]]);
            let (count, returned) = family.write(
                &db,
                "DELETE FROM t WHERE id IN (3,3,99) RETURNING id",
                "DELETE FROM t WHERE id IN ($1,$2,$3) RETURNING id",
                &[3, 3, 99].map(Value::Int4),
            )?;
            assert_eq!(count, 1);
            assert_eq!(ints(returned)?, vec![vec![3]]);
            let (count, returned) = family.write(
                &db,
                "UPDATE t SET v=99 WHERE id IN (88,99) RETURNING id",
                "UPDATE t SET v=99 WHERE id IN ($1,$2) RETURNING id",
                &[88, 99].map(Value::Int4),
            )?;
            assert_eq!(count, 0);
            assert!(returned.is_empty());
            assert_eq!(
                image(&db)?,
                vec![vec![1, 11, 101, 1], vec![2, 21, 102, 1], vec![4, 40, 104, 1]]
            );
            eprintln!("transaction-pk-in-cell implicit history={history} family={family:?}");
        }
    }
    Ok(())
}

#[test]
fn late_real_constraints_abort_and_restore_the_complete_owner() -> Result<()> {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for (kind, text, bound) in [
                (
                    "check",
                    "UPDATE t SET v=-1 WHERE id IN (1,2,10)",
                    "UPDATE t SET v=-1 WHERE id IN ($1,$2,$3)",
                ),
                (
                    "unique",
                    "UPDATE t SET u=777 WHERE id IN (1,2,10)",
                    "UPDATE t SET u=777 WHERE id IN ($1,$2,$3)",
                ),
                (
                    "foreign",
                    "UPDATE t SET pid=999 WHERE id IN (1,2,10)",
                    "UPDATE t SET pid=999 WHERE id IN ($1,$2,$3)",
                ),
            ] {
                let db = db(history)?;
                let before = image(&db)?;
                db.begin()?;
                db.execute("INSERT INTO t VALUES (10,100,110,1)")?;
                let error = family
                    .write(&db, text, bound, &[1, 2, 10].map(Value::Int4))
                    .err()
                    .ok_or_else(|| Error::storage("real constraint unexpectedly accepted PK-IN update"))?;
                let message = error.to_string().to_ascii_lowercase();
                assert!(
                    message.contains(kind) || kind == "unique" && message.contains("duplicate"),
                    "{kind}: {error}"
                );
                db.rollback()?;
                assert_eq!(image(&db)?, before);
                eprintln!("transaction-pk-in-cell constraint history={history} family={family:?} kind={kind}");
            }
        }
    }
    Ok(())
}

#[test]
fn branches_keep_inherited_and_staged_rows_on_their_own_level() -> Result<()> {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for commit in [false, true] {
                let db = db(history)?;
                let before = image(&db)?;
                db.create_branch("in_child")?;
                db.switch_branch("in_child")?;
                db.begin()?;
                staged_changes(&db)?;
                update_in(family, &db)?;
                if commit {
                    db.commit()?;
                } else {
                    db.rollback()?;
                }
                assert_eq!(
                    image(&db)?,
                    if commit {
                        vec![vec![3, 37, 103, 1], vec![11, 17, 101, 1]]
                    } else {
                        before.clone()
                    }
                );
                db.switch_branch("main")?;
                assert_eq!(image(&db)?, before);
                eprintln!("transaction-pk-in-cell branch history={history} family={family:?} commit={commit}");
            }
        }
    }
    Ok(())
}

#[test]
fn composite_absent_indexes_and_null_predicates_retain_the_scan_contract() -> Result<()> {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let db = db(history)?;
            db.execute("CREATE TABLE composite (a INT,b INT,v INT,PRIMARY KEY(a,b))")?;
            db.execute("INSERT INTO composite VALUES (1,1,10),(1,2,20),(2,1,30)")?;
            let (count, returned) = family.write(
                &db,
                "UPDATE composite SET v=v+1 WHERE a IN (1,1,99) RETURNING a,b,v",
                "UPDATE composite SET v=v+1 WHERE a IN ($1,$2,$3) RETURNING a,b,v",
                &[1, 1, 99].map(Value::Int4),
            )?;
            assert_eq!(count, 2);
            assert_eq!(ints(returned)?, vec![vec![1, 1, 11], vec![1, 2, 21]]);
            db.execute("CREATE TABLE no_pk (id INT,v INT)")?;
            db.execute("INSERT INTO no_pk VALUES (1,10),(1,20),(2,30)")?;
            let (count, returned) = family.write(
                &db,
                "DELETE FROM no_pk WHERE id IN (1,1,99) RETURNING v",
                "DELETE FROM no_pk WHERE id IN ($1,$2,$3) RETURNING v",
                &[1, 1, 99].map(Value::Int4),
            )?;
            assert_eq!(count, 2);
            assert_eq!(ints(returned)?, vec![vec![10], vec![20]]);
            let (count, _) = family.write(
                &db,
                "UPDATE t SET v=99 WHERE id NOT IN (1,NULL)",
                "UPDATE t SET v=99 WHERE id NOT IN ($1,$2)",
                &[Value::Int4(1), Value::Null],
            )?;
            assert_eq!(count, 0);
            let (count, returned) = family.write(
                &db,
                "UPDATE t SET v=v+1 WHERE id IN (1,NULL,99) RETURNING id,v",
                "UPDATE t SET v=v+1 WHERE id IN ($1,$2,$3) RETURNING id,v",
                &[Value::Int4(1), Value::Null, Value::Int4(99)],
            )?;
            assert_eq!(count, 1);
            assert_eq!(ints(returned)?, vec![vec![1, 11]]);
            eprintln!("transaction-pk-in-cell fallback history={history} family={family:?}");
        }
    }
    Ok(())
}

#[test]
fn session_owners_join_the_supplied_transaction_for_both_families() -> Result<()> {
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for commit in [false, true] {
                let db = db(history)?;
                let before = image(&db)?;
                let sid = db.create_session("pk_in", heliosdb_nano::session::IsolationLevel::ReadCommitted)?;
                db.begin_transaction_for_session(sid)?;
                db.execute_for_session(sid, "INSERT INTO t VALUES (10,100,110,1)")?;
                let (count, returned) = match family {
                    Family::Text => db.execute_returning_for_session(
                        sid,
                        "UPDATE t SET v=v+1 WHERE id IN (1,10,10,99) RETURNING id,v",
                    )?,
                    Family::Params => db.execute_params_returning_for_session(
                        sid,
                        "UPDATE t SET v=v+$1 WHERE id IN ($2,$3,$4,$5) RETURNING id,v",
                        &[1, 1, 10, 10, 99].map(Value::Int4),
                    )?,
                };
                assert_eq!(count, 2);
                assert_eq!(ints(returned)?, vec![vec![1, 11], vec![10, 101]]);
                if commit {
                    db.commit_transaction_for_session(sid)?;
                } else {
                    db.rollback_transaction_for_session(sid)?;
                }
                let after = image(&db)?;
                if commit {
                    assert_eq!(
                        after,
                        vec![
                            vec![1, 11, 101, 1],
                            vec![2, 20, 102, 1],
                            vec![3, 30, 103, 1],
                            vec![4, 40, 104, 1],
                            vec![10, 101, 110, 1]
                        ]
                    );
                } else {
                    assert_eq!(after, before);
                }
                db.destroy_session(sid)?;
                eprintln!("transaction-pk-in-cell session history={history} family={family:?} commit={commit}");
            }
        }
    }
    Ok(())
}

#[test]
#[allow(deprecated)] // The public RAII owner, rather than BEGIN's global slot, is the contract under test.
fn raii_owner_keeps_staged_pk_in_updates_until_its_own_settlement() -> Result<()> {
    for history in [false, true] {
        for commit in [false, true] {
            let db = db(history)?;
            let before = image(&db)?;
            let tx = db.begin_transaction()?;
            tx.execute("INSERT INTO t VALUES (10,100,110,1)")?;
            assert_eq!(tx.execute("UPDATE t SET v=v+1 WHERE id IN (1,10,10,99)")?, 2);
            assert_eq!(
                ints(tx.query("SELECT id,v FROM t WHERE id IN (1,10) ORDER BY id", &[])?)?,
                vec![vec![1, 11], vec![10, 101]]
            );
            if commit {
                tx.commit()?;
            } else {
                tx.rollback()?;
            }
            if commit {
                assert_eq!(
                    image(&db)?,
                    vec![
                        vec![1, 11, 101, 1],
                        vec![2, 20, 102, 1],
                        vec![3, 30, 103, 1],
                        vec![4, 40, 104, 1],
                        vec![10, 101, 110, 1]
                    ]
                );
            } else {
                assert_eq!(image(&db)?, before);
            }
            eprintln!("transaction-pk-in-cell raii history={history} commit={commit}");
        }
    }
    Ok(())
}
