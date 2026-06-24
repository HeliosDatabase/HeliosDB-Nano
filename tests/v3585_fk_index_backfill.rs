//! Regression for the Pagila `film_list` 0-rows bug (Any2HeliosDB migration).
//!
//! `ALTER TABLE … ADD FOREIGN KEY` auto-creates an ART index on the FK
//! column(s) for lookups, but it was registered EMPTY — it was never
//! backfilled from rows already in the table. Bulk migration loads the data
//! first and adds FKs afterwards, so the FK column's index ended up empty;
//! the planner then answered `WHERE fk_col = …` (and FK-column joins) from that
//! empty index and silently returned zero rows. This was most visible on
//! composite-PK tables (`film_category`, `film_actor`) whose multi-table views
//! (`film_list`) came back empty even though the base data was correct.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int8(n) => *n,
        Value::Int4(n) => *n as i64,
        Value::Int2(n) => *n as i64,
        Value::Numeric(s) => s.parse().expect("numeric count"),
        other => panic!("expected an integer count, got {:?}", other),
    }
}

#[test]
fn add_fk_after_load_backfills_index() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE cat (category_id INT4 PRIMARY KEY)")?;
    db.execute("CREATE TABLE film (film_id INT4 PRIMARY KEY)")?;
    // Composite PK with the FK column (category_id) as the NON-leading member,
    // exactly like Pagila's film_category(film_id, category_id).
    db.execute(
        "CREATE TABLE film_category (film_id INT4, category_id INT4, PRIMARY KEY (film_id, category_id))",
    )?;

    for i in 1..=5 {
        db.execute(&format!("INSERT INTO cat VALUES ({i})"))?;
    }
    for f in 1..=100 {
        db.execute(&format!("INSERT INTO film VALUES ({f})"))?;
    }
    // Load the child rows BEFORE the FK exists (the bulk-migration order).
    // category_id = (film % 5) + 1, so category_id = 1 for film in {5,10,…,100} = 20 rows.
    for f in 1..=100 {
        let c = (f % 5) + 1;
        db.execute(&format!("INSERT INTO film_category VALUES ({f}, {c})"))?;
    }

    // Add the FK on the non-leading PK column AFTER the data is present.
    db.execute(
        "ALTER TABLE film_category ADD CONSTRAINT fc_cat_fk \
         FOREIGN KEY (category_id) REFERENCES cat (category_id)",
    )?;

    // Equality lookup on the FK column must see the pre-existing rows.
    let cnt = db.query("SELECT count(*) FROM film_category WHERE category_id = 1", &[])?;
    assert_eq!(as_i64(&cnt[0].values[0]), 20, "FK-column equality must see pre-existing rows");

    // The leading composite-PK column must still resolve too (a second FK
    // should not disturb it either).
    db.execute(
        "ALTER TABLE film_category ADD CONSTRAINT fc_film_fk \
         FOREIGN KEY (film_id) REFERENCES film (film_id)",
    )?;
    let cnt2 = db.query("SELECT count(*) FROM film_category WHERE category_id = 1", &[])?;
    assert_eq!(as_i64(&cnt2[0].values[0]), 20, "second FK must not break the first FK's index");

    // The join that previously returned 0 (film_list-style) now returns all rows.
    let j = db.query(
        "SELECT count(*) FROM cat c JOIN film_category fc ON c.category_id = fc.category_id",
        &[],
    )?;
    assert_eq!(as_i64(&j[0].values[0]), 100, "FK-column join must return every matching row");
    Ok(())
}
