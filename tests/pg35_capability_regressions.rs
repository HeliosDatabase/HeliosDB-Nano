use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn pg35_refresh_matview_if_not_exists_uses_existing_mv_path() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE customers (id INT PRIMARY KEY, region TEXT, spend INT)")?;
    db.execute("INSERT INTO customers VALUES (1, 'north', 10)")?;
    db.execute("INSERT INTO customers VALUES (2, 'north', 20)")?;
    db.execute("INSERT INTO customers VALUES (3, 'south', 30)")?;

    db.execute(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS bench_mv AS \
         SELECT region, COUNT(*) as cnt FROM customers GROUP BY region",
    )?;
    db.execute(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS bench_mv AS \
         SELECT region, COUNT(*) as cnt FROM customers GROUP BY region",
    )?;
    db.execute("REFRESH MATERIALIZED VIEW bench_mv")?;

    let rows = db.query("SELECT * FROM bench_mv", &[])?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn pg35_recursive_cte_category_shape() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE categories (id INT PRIMARY KEY, parent_id INT, name TEXT)")?;
    db.execute("INSERT INTO categories VALUES (1, NULL, 'root')")?;
    db.execute("INSERT INTO categories VALUES (2, 1, 'child')")?;
    db.execute("INSERT INTO categories VALUES (3, 2, 'leaf')")?;

    let rows = db.query(
        "WITH RECURSIVE cat_tree(id, parent_id, depth) AS (\
           SELECT id, parent_id, 1 FROM categories WHERE parent_id IS NULL \
           UNION ALL \
           SELECT c.id, c.parent_id, ct.depth + 1 \
           FROM categories c JOIN cat_tree ct ON c.parent_id = ct.id \
         ) SELECT COUNT(*) FROM cat_tree",
        &[],
    )?;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int8(3));
    Ok(())
}

#[test]
fn pg35_recursive_cte_benchmark_no_alias_shape() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE categories (cat_id INT PRIMARY KEY, name TEXT, parent_id INT)")?;
    db.execute("INSERT INTO categories VALUES (1, 'root', NULL)")?;
    db.execute("INSERT INTO categories VALUES (2, 'child', 1)")?;
    db.execute("INSERT INTO categories VALUES (3, 'leaf', 2)")?;

    let rows = db.query(
        "WITH RECURSIVE cat_tree AS (\
            SELECT cat_id, name, parent_id, 0 AS depth FROM categories WHERE parent_id IS NULL \
            UNION ALL \
            SELECT c.cat_id, c.name, c.parent_id, ct.depth + 1 FROM categories c JOIN cat_tree ct ON c.parent_id = ct.cat_id\
         ) SELECT * FROM cat_tree",
        &[],
    )?;

    assert_eq!(rows.len(), 3);
    Ok(())
}

#[test]
fn pg35_prepared_statement_category_shape_through_query() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, amount INT)")?;
    db.execute("INSERT INTO orders VALUES (1, 10, 100)")?;
    db.execute("INSERT INTO orders VALUES (2, 20, 200)")?;

    db.query(
        "PREPARE order_lookup(INT) AS SELECT amount FROM orders WHERE id = $1",
        &[],
    )?;
    let rows = db.query("EXECUTE order_lookup(2)", &[])?;
    db.query("DEALLOCATE order_lookup", &[])?;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int4(200));
    Ok(())
}

#[test]
fn pg35_work_mem_set_show_reset_category_shape() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.query("SET work_mem = '8MB'", &[])?;
    let rows = db.query("SHOW work_mem", &[])?;
    db.query("RESET work_mem", &[])?;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::String("8MB".to_string()));
    Ok(())
}
