use crate::session::{IsolationLevel, SessionId};
use crate::{Config, EmbeddedDatabase, Error, Result, Tuple, Value};
use std::sync::{mpsc, Arc};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}

impl Family {
    fn query(self, db: &EmbeddedDatabase, session: SessionId, sql: &str, parameters: &[Value]) -> Result<Vec<Tuple>> {
        match self {
            Self::Text => db.query_with_columns_for_session(session, sql).map(|(rows, _)| rows),
            Self::Params => db.query_params_for_session(session, sql, parameters),
        }
    }
}

fn require(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::internal(message))
    }
}

fn fixture(history: bool) -> Result<Arc<EmbeddedDatabase>> {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = history;
    let db = Arc::new(EmbeddedDatabase::with_config(config)?);
    db.storage.debug_set_table_lock_timeout(Duration::from_secs(5));
    db.execute("CREATE TABLE a_noise (id INT PRIMARY KEY)")?;
    db.execute("CREATE TABLE z_selected (id INT PRIMARY KEY, payload TEXT)")?;
    let visible = db.storage.catalog().list_tables()?;
    require(
        visible.iter().position(|name| name == "a_noise") < visible.iter().position(|name| name == "z_selected"),
        "fixture requires the real visible enumeration to encounter noise before the selected table",
    )?;
    Ok(db)
}

fn names(rows: Vec<Tuple>) -> Result<Vec<String>> {
    rows.into_iter()
        .map(|row| match row.values.first() {
            Some(Value::String(name)) => Ok(name.clone()),
            other => Err(Error::internal(format!(
                "column-name result has wrong value: {other:?}"
            ))),
        })
        .collect()
}

fn overlap(history: bool, family: Family, disjunction: bool) -> Result<()> {
    let db = fixture(history)?;
    let session = db.create_session("columns_scope", IsolationLevel::ReadCommitted)?;
    db.begin_transaction_for_session(session)?;
    let (reached_tx, reached_rx) = mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = mpsc::sync_channel(0);
    let reader_db = Arc::clone(&db);
    let reader = std::thread::spawn(move || -> Result<_> {
        super::next_after_columns_schema("z_selected", move || {
            reached_tx
                .send(())
                .map_err(|_| Error::internal("columns parent closed before real schema read"))?;
            resume_rx
                .recv_timeout(Duration::from_secs(20))
                .map_err(|_| Error::internal("columns writer did not release schema read"))?;
            Ok(())
        })?;
        let (sql, parameters) = match (family, disjunction) {
            (Family::Text, false) => (
                "SELECT column_name FROM information_schema.columns WHERE table_name='z_selected' ORDER BY ordinal_position",
                Vec::new(),
            ),
            (Family::Params, false) => (
                "SELECT column_name FROM information_schema.columns WHERE table_name=$1 ORDER BY ordinal_position",
                vec![Value::String("z_selected".into())],
            ),
            (Family::Text, true) => (
                "SELECT column_name FROM information_schema.columns WHERE table_name='z_selected' OR table_name='a_noise' ORDER BY table_name,ordinal_position",
                Vec::new(),
            ),
            (Family::Params, true) => (
                "SELECT column_name FROM information_schema.columns WHERE table_name=$1 OR table_name=$2 ORDER BY table_name,ordinal_position",
                vec![Value::String("z_selected".into()), Value::String("a_noise".into())],
            ),
        };
        let result = family.query(&reader_db, session, sql, &parameters);
        let rolled_back = reader_db.rollback_transaction_for_session(session);
        Ok((result, rolled_back))
    });
    let reached = reached_rx.recv_timeout(Duration::from_secs(20));
    let changed = if reached.is_ok() {
        db.execute("ALTER TABLE a_noise ADD COLUMN moved INT DEFAULT 7")
    } else {
        Err(Error::internal(format!(
            "reader never reached actual selected schema: {reached:?}"
        )))
    };
    let released = resume_tx.send(());
    let joined = reader.join().map_err(|_| Error::internal("columns reader panicked"));
    // No assertions or propagated writer error can strand the real reader.
    let (result, rollback) = joined??;
    changed?;
    require(released.is_ok(), "columns reader disappeared before overlap release")?;
    rollback?;
    require(
        db.storage.catalog().get_table_schema("a_noise")?.columns.len() == 2,
        "public unrelated ALTER did not complete",
    )?;
    if disjunction {
        let error = result
            .err()
            .ok_or_else(|| Error::internal("OR query lost its real noise-table layout dependency"))?;
        require(
            crate::storage::layout_epochs::is_layout_changed(&error),
            format!("wrong OR overlap error: {error:?}"),
        )?;
    } else {
        require(
            names(result?)? == ["id", "payload"],
            "selected table column rows changed under unrelated DDL",
        )?;
    }
    // The same public session is reusable after its explicit rollback.
    require(
        names(family.query(&db, session,
            "SELECT column_name FROM information_schema.columns WHERE table_name='z_selected' ORDER BY ordinal_position", &[])?)?
            == ["id", "payload"],
        "session did not recover after rollback",
    )?;
    db.destroy_session(session)?;
    eprintln!("catalog-columns-scope-cell overlap history={history} family={family:?} disjunction={disjunction} actual_schema=true unrelated_ddl=true rollback=true");
    Ok(())
}

fn complete(cells: Vec<Result<()>>) -> Result<()> {
    let errors: Vec<_> = cells
        .into_iter()
        .filter_map(|result| result.err().map(|error| format!("{error:?}")))
        .collect();
    require(errors.is_empty(), errors.join("\n"))
}

#[test]
fn an_exact_table_name_filter_does_not_depend_on_an_unrelated_layout() -> Result<()> {
    let mut cells = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            cells.push(overlap(history, family, false));
        }
    }
    complete(cells)
}

#[test]
fn a_disjunction_keeps_its_real_layout_dependencies() -> Result<()> {
    let mut cells = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            cells.push(overlap(history, family, true));
        }
    }
    complete(cells)
}

fn visibility(history: bool, family: Family) -> Result<()> {
    let db = fixture(history)?;
    db.execute("CREATE SCHEMA extra")?;
    db.execute("CREATE TABLE extra.z_selected (other INT)")?;
    let owner = db.create_session("columns_temp_owner", IsolationLevel::ReadCommitted)?;
    let reader = db.create_session("columns_temp_reader", IsolationLevel::ReadCommitted)?;
    db.execute_for_session(owner, "CREATE TEMP TABLE hidden_columns (secret INT)")?;
    let (sql, parameters) = match family {
        Family::Text => (
            "SELECT c.column_name FROM information_schema.columns AS c WHERE 'z_selected'=c.table_name AND c.table_schema='public' AND c.column_name='payload'",
            Vec::new(),
        ),
        Family::Params => (
            "SELECT c.column_name FROM information_schema.columns AS c WHERE $1=c.table_name AND c.table_schema=$2 AND c.column_name=$3",
            vec![Value::String("z_selected".into()), Value::String("public".into()), Value::String("payload".into())],
        ),
    };
    require(
        names(family.query(&db, reader, sql, &parameters)?)? == ["payload"],
        "residual/alias/reversed equality changed",
    )?;
    require(
        names(family.query(&db, reader,
            "SELECT column_name FROM information_schema.columns WHERE table_name='z_selected' ORDER BY table_schema,ordinal_position", &[])?)?
            == ["other", "id", "payload"],
        "bare table_name filter lost an identically named visible table in another schema",
    )?;
    require(
        family
            .query(
                &db,
                reader,
                "SELECT column_name FROM information_schema.columns WHERE table_name='hidden_columns'",
                &[],
            )?
            .is_empty(),
        "exact table_name filter exposed another session's temporary schema",
    )?;
    require(
        names(family.query(
            &db,
            owner,
            "SELECT column_name FROM information_schema.columns WHERE table_name='hidden_columns'",
            &[],
        )?)? == ["secret"],
        "exact filter hid the owning session's visible temporary table",
    )?;
    require(
        family
            .query(
                &db,
                reader,
                "SELECT column_name FROM information_schema.columns WHERE table_name='absent_columns'",
                &[],
            )?
            .is_empty(),
        "absent exact filter returned unrelated rows",
    )?;
    db.destroy_session(owner)?;
    db.destroy_session(reader)?;
    eprintln!("catalog-columns-scope-cell visibility history={history} family={family:?} residual=true schemas=true temp_hidden=true temp_owner=true absent=true");
    Ok(())
}

#[test]
fn exact_filters_preserve_residuals_schema_names_and_session_visibility() -> Result<()> {
    let mut cells = Vec::new();
    for history in [false, true] {
        for family in [Family::Text, Family::Params] {
            cells.push(visibility(history, family));
        }
    }
    complete(cells)
}
