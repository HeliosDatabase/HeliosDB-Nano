#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
#![allow(clippy::assert_is_empty)]
//! The registry as a whole: every registered view has a producer, and every
//! row a producer returns has the width its schema declares.

use super::DictContext;
use crate::sql::phase3::SystemViewRegistry;
use crate::EmbeddedDatabase;

/// The one view that refuses by design (it says what to use instead).
const REFUSES_BY_DESIGN: &[&str] = &["pg_compare_branches"];

fn database_with_objects() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE rt_parent (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
        .unwrap();
    db.execute("CREATE TABLE rt_child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES rt_parent(id), n INTEGER DEFAULT 1)")
        .unwrap();
    db.execute("CREATE INDEX rt_child_n ON rt_child (n)").unwrap();
    db.execute("CREATE VIEW rt_view AS SELECT id FROM rt_child").unwrap();
    db.execute("CREATE SEQUENCE rt_seq").unwrap();
    db.execute("CREATE FUNCTION rt_fn(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    db.execute("INSERT INTO rt_parent VALUES (1, 'a')").unwrap();
    db
}

/// A registered view with no producer arm used to answer with zero rows. It is
/// an error now, so this test fails for a view registered without a producer.
#[test]
fn every_registered_view_has_a_producer() {
    let db = database_with_objects();
    let registry = SystemViewRegistry::shared();
    let ctx = DictContext::for_engine(&db.storage);
    let mut views = registry.list_views();
    views.sort_unstable();
    assert!(views.len() >= 77, "the registry lost views: {}", views.len());
    for view in views {
        match registry.execute(view, &ctx) {
            Ok(rows) => {
                let width = registry.get_schema(view).expect("schema").columns.len();
                for row in &rows {
                    assert_eq!(
                        row.values.len(),
                        width,
                        "{view}: a row has {} values for {width} declared columns",
                        row.values.len()
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                assert!(
                    !message.contains("has no producer"),
                    "{view} is registered without a producer: {message}"
                );
                assert!(
                    REFUSES_BY_DESIGN.contains(&view),
                    "{view} failed on a database it should describe: {message}"
                );
            }
        }
    }
}

#[test]
fn a_name_that_is_not_registered_is_an_error_not_an_empty_answer() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let ctx = DictContext::for_engine(&db.storage);
    let error = SystemViewRegistry::shared()
        .execute("hdb_no_such_view", &ctx)
        .expect_err("an unregistered name");
    assert!(error.to_string().contains("Unknown system view"), "{error}");
}

/// Every dictionary name is spelled the way the planner resolves names:
/// lowercase, at most one schema qualifier.
#[test]
fn registered_names_are_resolvable_spellings() {
    for view in SystemViewRegistry::shared().list_views() {
        assert_eq!(view, view.to_lowercase(), "{view}");
        assert!(view.matches('.').count() <= 1, "{view}");
        assert!(super::is_dictionary_name(view));
    }
}
