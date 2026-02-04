use heliosdb_lite::EmbeddedDatabase;

fn main() -> heliosdb_lite::Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create a sales table
    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, region TEXT, amount INT)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (1, 'North', 100)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (2, 'South', 200)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (3, 'North', 150)")?;

    // First, test without HAVING
    println!("=== Query without HAVING ===");
    let results1 = db.query("SELECT region, SUM(amount) FROM sales GROUP BY region", &[])?;
    println!("Results: {} rows", results1.len());
    for (i, row) in results1.iter().enumerate() {
        println!("Row {}: {:?}", i, row);
    }

    // Then test with HAVING
    println!("\n=== Query with HAVING ===");
    let results2 = db.query(
        "SELECT region, SUM(amount) FROM sales GROUP BY region HAVING SUM(amount) > 200",
        &[]
    )?;
    println!("Results: {} rows", results2.len());
    for (i, row) in results2.iter().enumerate() {
        println!("Row {}: {:?}", i, row);
    }

    Ok(())
}
