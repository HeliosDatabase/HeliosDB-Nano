use heliosdb_nano::{Config, EmbeddedDatabase, Error, Result, Tuple, Value};
use rocksdb::{IteratorMode, ReadOptions};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

impl Family {
    fn execute(self, db: &EmbeddedDatabase, sql: &str) -> Result<u64> {
        match self {
            Self::Text => db.execute(sql),
            Self::Params => db.execute_params(sql, &[]),
        }
    }

    fn query(self, db: &EmbeddedDatabase, sql: &str) -> Result<Vec<Tuple>> {
        match self {
            Self::Text => db.query(sql, &[]),
            Self::Params => db.query_params(sql, &[]),
        }
    }
}

fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::internal(message))
    }
}

fn fixture(history: bool) -> Result<EmbeddedDatabase> {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = history;
    let db = EmbeddedDatabase::with_config(config)?;
    db.execute("CREATE TABLE refd_table (id INT PRIMARY KEY)")?;
    db.execute("CREATE TABLE healthy_parent (id INT PRIMARY KEY)")?;
    db.execute("INSERT INTO healthy_parent VALUES (7)")?;
    db.execute("CREATE TABLE trig_table (id INT PRIMARY KEY, fk INT, h INT)")?;
    // CREATE persists FK definitions but does not install their optional
    // lookup graphs. ALTER owns the actual registration/backfill path, so this
    // fixture proves that CASCADE retires a graph which really existed.
    db.execute(
        "ALTER TABLE trig_table ADD CONSTRAINT gone_fk FOREIGN KEY (fk) \
         REFERENCES refd_table(id) DEFERRABLE INITIALLY DEFERRED",
    )?;
    db.execute("ALTER TABLE trig_table ADD CONSTRAINT kept_fk FOREIGN KEY (h) REFERENCES healthy_parent(id)")?;
    db.execute("INSERT INTO trig_table VALUES (0, NULL, 7)")?;
    require(
        db.storage.art_indexes().get_fk_info("gone_fk").is_some()
            && db.storage.art_indexes().get_fk_info("kept_fk").is_some(),
        "fixture did not register both actual constraint-named FK indexes",
    )?;
    Ok(db)
}

fn image(db: &EmbeddedDatabase) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut options = ReadOptions::default();
    options.set_total_order_seek(true);
    db.storage
        .db()
        .iterator_opt(IteratorMode::Start, options)
        .map(|entry| {
            entry
                .map(|(key, value)| (key.to_vec(), value.to_vec()))
                .map_err(|error| Error::storage(error.to_string()))
        })
        .collect()
}

fn ids(rows: &[Tuple]) -> Vec<Value> {
    rows.iter().filter_map(|row| row.values.first().cloned()).collect()
}

fn complete(cases: Vec<Result<()>>) -> Result<()> {
    let errors: Vec<String> = cases
        .into_iter()
        .filter_map(|result| result.err().map(|error| format!("{error:?}")))
        .collect();
    require(errors.is_empty(), errors.join("\n"))
}

fn cascade_case(history: bool, family: Family, commit: bool) -> Result<()> {
    let db = fixture(history)?;
    family.execute(&db, "BEGIN")?;
    family.execute(&db, "INSERT INTO trig_table VALUES (1, 999, 7)")?;
    family.execute(&db, "DROP TABLE refd_table CASCADE")?;
    family.execute(&db, if commit { "COMMIT" } else { "ROLLBACK" })?;
    // Nano's documented DDL contract is immediate: ROLLBACK undoes its
    // staged DML, while the completed DROP and FK removal remain durable.
    require(
        !db.storage.catalog().table_exists("refd_table")?,
        "dropped parent returned",
    )?;
    let constraints = db.storage.catalog().load_table_constraints("trig_table")?;
    require(
        constraints.foreign_keys.len() == 1
            && constraints
                .foreign_keys
                .iter()
                .all(|fk| fk.name == "kept_fk" && fk.references_table == "healthy_parent"),
        "CASCADE did not remove only its incoming FK",
    )?;
    let expected = if commit {
        vec![Value::Int4(0), Value::Int4(1)]
    } else {
        vec![Value::Int4(0)]
    };
    require(
        ids(&family.query(&db, "SELECT id FROM trig_table ORDER BY id")?) == expected,
        "CASCADE child/DML settlement mismatch",
    )?;
    require(
        db.storage.art_indexes().get_fk_info("gone_fk").is_none(),
        "retired FK ART index survives",
    )?;
    require(
        db.storage.art_indexes().get_fk_info("kept_fk").is_some(),
        "unrelated FK ART index was retired",
    )?;
    family.execute(&db, "INSERT INTO trig_table VALUES (2, 888, 7)")?;
    family.execute(&db, "CREATE TABLE refd_table (id INT PRIMARY KEY)")?;
    family.execute(&db, "INSERT INTO trig_table VALUES (3, 777, 7)")?;
    let error = family
        .execute(&db, "INSERT INTO trig_table VALUES (4, 777, 999)")
        .err()
        .ok_or_else(|| Error::internal("unrelated FK stopped enforcing"))?;
    require(
        matches!(&error, Error::ConstraintViolation(_)),
        format!("wrong unrelated FK error: {error:?}"),
    )?;
    let raw = db
        .storage
        .get(&b"table_constraints:trig_table".to_vec())?
        .ok_or_else(|| Error::internal("child constraints missing"))?;
    let expected = bincode::serialize(&constraints).map_err(|error| Error::storage(error.to_string()))?;
    require(raw == expected, "cache and durable child constraints disagree")
}

#[test]
fn deferred_cascade_removes_only_its_fk_and_preserves_child_settlement() -> Result<()> {
    let mut cases = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for commit in [false, true] {
                let result = cascade_case(history, family, commit);
                eprintln!(
                    "drop-table-fk-cell cascade history={history} family={family:?} commit={commit} ok={}",
                    result.is_ok()
                );
                cases.push(result);
            }
        }
    }
    complete(cases)
}

fn restrict_case(history: bool, family: Family, explicit: bool) -> Result<()> {
    let db = fixture(history)?;
    let before = image(&db)?;
    let sequence = db.storage.db().latest_sequence_number();
    let error = family
        .execute(
            &db,
            if explicit {
                "DROP TABLE refd_table RESTRICT"
            } else {
                "DROP TABLE refd_table"
            },
        )
        .err()
        .ok_or_else(|| Error::internal("RESTRICT dropped a referenced parent"))?;
    require(
        error
            .to_string()
            .contains("cannot be dropped because some objects depend"),
        format!("wrong RESTRICT error: {error:?}"),
    )?;
    require(
        image(&db)? == before && db.storage.db().latest_sequence_number() == sequence,
        "RESTRICT wrote native state",
    )?;
    require(
        db.storage.catalog().table_exists("refd_table")?
            && db
                .storage
                .catalog()
                .load_table_constraints("trig_table")?
                .foreign_keys
                .len()
                == 2,
        "RESTRICT changed catalog",
    )?;
    family.execute(&db, "BEGIN")?;
    family.execute(&db, "INSERT INTO trig_table VALUES (1, 999, 7)")?;
    family.execute(&db, "ROLLBACK")?;
    require(
        ids(&family.query(&db, "SELECT id FROM trig_table ORDER BY id")?) == vec![Value::Int4(0)],
        "rollback control leaked child row",
    )
}

#[test]
fn default_and_explicit_restrict_refuse_before_any_native_effect() -> Result<()> {
    let mut cases = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            for explicit in [false, true] {
                let result = restrict_case(history, family, explicit);
                eprintln!(
                    "drop-table-fk-cell restrict history={history} family={family:?} explicit={explicit} ok={}",
                    result.is_ok()
                );
                cases.push(result);
            }
        }
    }
    complete(cases)
}

fn target_set_case(history: bool, family: Family) -> Result<()> {
    let db = fixture(history)?;
    family.execute(&db, "DROP TABLE refd_table, trig_table")?;
    require(
        !db.storage.catalog().table_exists("refd_table")? && !db.storage.catalog().table_exists("trig_table")?,
        "multi target DROP incomplete",
    )?;
    require(
        db.storage.catalog().table_exists("healthy_parent")?,
        "multi target DROP reached unrelated parent",
    )?;
    family.execute(
        &db,
        "CREATE TABLE self_ref (id INT PRIMARY KEY, p INT REFERENCES self_ref(id))",
    )?;
    family.execute(&db, "DROP TABLE self_ref")?;
    require(
        !db.storage.catalog().table_exists("self_ref")?,
        "self-reference blocked own DROP",
    )
}

#[test]
fn dependencies_inside_the_drop_set_and_self_references_need_no_cascade() -> Result<()> {
    let mut cases = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let result = target_set_case(history, family);
            eprintln!(
                "drop-table-fk-cell target_set history={history} family={family:?} ok={}",
                result.is_ok()
            );
            cases.push(result);
        }
    }
    complete(cases)
}

fn branch_case(history: bool, family: Family) -> Result<()> {
    let db = fixture(history)?;
    db.execute("CREATE BRANCH b AS OF NOW")?;
    db.with_request_branch_scope("b", || family.query(&db, "SELECT id FROM trig_table"))?;
    let before = image(&db)?;
    let sequence = db.storage.db().latest_sequence_number();
    let error = db
        .with_request_branch_scope("b", || family.execute(&db, "DROP TABLE refd_table CASCADE"))
        .err()
        .ok_or_else(|| Error::internal("branch incoming CASCADE was admitted"))?;
    require(
        error
            .to_string()
            .contains("DROP TABLE CASCADE with incoming foreign keys"),
        format!("wrong branch refusal: {error:?}"),
    )?;
    require(
        image(&db)? == before && db.storage.db().latest_sequence_number() == sequence,
        "branch refusal changed raw state",
    )?;
    require(
        db.storage.catalog().table_exists("refd_table")?
            && db
                .storage
                .catalog()
                .load_table_constraints("trig_table")?
                .foreign_keys
                .len()
                == 2,
        "branch refusal changed main",
    )?;
    db.with_request_branch_scope("b", || {
        family.execute(&db, "CREATE TABLE isolated (id INT PRIMARY KEY)")?;
        family.execute(&db, "DROP TABLE isolated CASCADE")?;
        require(
            !db.storage.catalog().table_exists("isolated")?,
            "ordinary branch CASCADE stopped working",
        )
    })
}

#[test]
fn branch_incoming_cascade_is_explicitly_refused_without_touching_main() -> Result<()> {
    let mut cases = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            let result = branch_case(history, family);
            eprintln!(
                "drop-table-fk-cell branch history={history} family={family:?} ok={}",
                result.is_ok()
            );
            cases.push(result);
        }
    }
    complete(cases)
}
