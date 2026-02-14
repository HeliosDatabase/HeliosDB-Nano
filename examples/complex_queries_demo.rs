//! Complex SQL Query Demonstration
//!
//! This example demonstrates advanced SQL features in HeliosDB Lite:
//! - GROUP BY with aggregation
//! - HAVING clauses
//! - INNER JOINs
//! - Multiple aggregate functions
//! - COUNT(DISTINCT)

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn main() -> Result<()> {
    println!("=== HeliosDB Lite Complex Query Demonstration ===\n");

    let db = EmbeddedDatabase::new_in_memory()?;

    // Setup demonstration data
    setup_demo_data(&db)?;

    // Demonstration 1: GROUP BY with Aggregates
    println!("--- Demonstration 1: Department Statistics ---");
    demo_group_by(&db)?;

    // Demonstration 2: HAVING Clause
    println!("\n--- Demonstration 2: Filtering Aggregated Results with HAVING ---");
    demo_having_clause(&db)?;

    // Demonstration 3: JOIN Operations
    println!("\n--- Demonstration 3: Joining Tables ---");
    demo_joins(&db)?;

    // Demonstration 4: Advanced Aggregates
    println!("\n--- Demonstration 4: Advanced Aggregates ---");
    demo_advanced_aggregates(&db)?;

    // Demonstration 5: Multi-Column GROUP BY
    println!("\n--- Demonstration 5: Multi-Column Grouping ---");
    demo_multi_column_grouping(&db)?;

    println!("\n=== All Demonstrations Complete ===");
    Ok(())
}

fn setup_demo_data(db: &EmbeddedDatabase) -> Result<()> {
    println!("Setting up demonstration data...\n");

    // Create employees table
    db.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (1, 'Alice', 'Engineering', 100000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (2, 'Bob', 'Engineering', 120000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (3, 'Charlie', 'Sales', 80000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (4, 'Dave', 'Sales', 90000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (5, 'Eve', 'Engineering', 110000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (6, 'Frank', 'Marketing', 70000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (7, 'Grace', 'Engineering', 95000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (8, 'Hank', 'Sales', 85000)")?;

    // Create departments table
    db.execute("CREATE TABLE departments (dept_id INT PRIMARY KEY, dept_name TEXT, location TEXT)")?;
    db.execute("INSERT INTO departments (dept_id, dept_name, location) VALUES (1, 'Engineering', 'San Francisco')")?;
    db.execute("INSERT INTO departments (dept_id, dept_name, location) VALUES (2, 'Sales', 'New York')")?;
    db.execute("INSERT INTO departments (dept_id, dept_name, location) VALUES (3, 'Marketing', 'Austin')")?;

    // Create sales table
    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, region TEXT, product TEXT, amount INT)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (1, 'North', 'Widget', 100)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (2, 'North', 'Widget', 150)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (3, 'North', 'Gadget', 200)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (4, 'South', 'Widget', 300)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (5, 'South', 'Gadget', 250)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (6, 'East', 'Widget', 180)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (7, 'East', 'Gadget', 220)")?;

    println!("Data setup complete!");
    Ok(())
}

fn demo_group_by(db: &EmbeddedDatabase) -> Result<()> {
    println!("Query: SELECT dept, COUNT(*), AVG(salary) FROM employees GROUP BY dept\n");

    let results = db.query(
        "SELECT dept, COUNT(*), AVG(salary) FROM employees GROUP BY dept",
        &[]
    )?;

    println!("Results:");
    println!("{:<15} {:<10} {:<15}", "Department", "Count", "Avg Salary");
    println!("{:-<45}", "");

    for row in &results {
        let dept = match row.get(0).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };
        let count = match row.get(1).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };
        let avg_salary = match row.get(2).unwrap() {
            Value::Float8(f) => *f,
            _ => 0.0,
        };

        println!("{:<15} {:<10} ${:<14.2}", dept, count, avg_salary);
    }

    println!("\nTotal departments: {}", results.len());
    Ok(())
}

fn demo_having_clause(db: &EmbeddedDatabase) -> Result<()> {
    println!("Query: SELECT dept, COUNT(*), AVG(salary) FROM employees");
    println!("       GROUP BY dept HAVING COUNT(*) > 2\n");

    let results = db.query(
        "SELECT dept, COUNT(*), AVG(salary) FROM employees GROUP BY dept HAVING COUNT(*) > 2",
        &[]
    )?;

    println!("Results (departments with more than 2 employees):");
    println!("{:<15} {:<10} {:<15}", "Department", "Count", "Avg Salary");
    println!("{:-<45}", "");

    for row in &results {
        let dept = match row.get(0).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };
        let count = match row.get(1).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };
        let avg_salary = match row.get(2).unwrap() {
            Value::Float8(f) => *f,
            _ => 0.0,
        };

        println!("{:<15} {:<10} ${:<14.2}", dept, count, avg_salary);
    }

    println!("\nFiltered to {} departments", results.len());
    Ok(())
}

fn demo_joins(db: &EmbeddedDatabase) -> Result<()> {
    // First, create a simplified employees table for joining
    db.execute("CREATE TABLE emp_dept (emp_id INT PRIMARY KEY, emp_name TEXT, department_id INT)")?;
    db.execute("INSERT INTO emp_dept (emp_id, emp_name, department_id) VALUES (1, 'Alice', 1)")?;
    db.execute("INSERT INTO emp_dept (emp_id, emp_name, department_id) VALUES (2, 'Bob', 1)")?;
    db.execute("INSERT INTO emp_dept (emp_id, emp_name, department_id) VALUES (3, 'Charlie', 2)")?;
    db.execute("INSERT INTO emp_dept (emp_id, emp_name, department_id) VALUES (4, 'Dave', 2)")?;
    db.execute("INSERT INTO emp_dept (emp_id, emp_name, department_id) VALUES (5, 'Frank', 3)")?;

    println!("Query: SELECT * FROM emp_dept INNER JOIN departments");
    println!("       ON emp_dept.department_id = departments.dept_id\n");

    let results = db.query(
        "SELECT * FROM emp_dept INNER JOIN departments ON emp_dept.department_id = departments.dept_id",
        &[]
    )?;

    println!("Results (Employee-Department Join):");
    println!("{:<12} {:<15} {:<15} {:<15}", "Emp Name", "Dept ID", "Dept Name", "Location");
    println!("{:-<65}", "");

    for row in &results {
        // emp_dept: emp_id, emp_name, department_id (3 columns)
        // departments: dept_id, dept_name, location (3 columns)
        // Total: 6 columns
        let emp_name = match row.get(1).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };
        let dept_id = match row.get(2).unwrap() {
            Value::Int4(i) => *i,
            _ => 0,
        };
        let dept_name = match row.get(4).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };
        let location = match row.get(5).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };

        println!("{:<12} {:<15} {:<15} {:<15}", emp_name, dept_id, dept_name, location);
    }

    println!("\nTotal joined rows: {}", results.len());
    Ok(())
}

fn demo_advanced_aggregates(db: &EmbeddedDatabase) -> Result<()> {
    println!("Query: SELECT COUNT(*), MIN(salary), MAX(salary), AVG(salary), SUM(salary)");
    println!("       FROM employees\n");

    let results = db.query(
        "SELECT COUNT(*), MIN(salary), MAX(salary), AVG(salary), SUM(salary) FROM employees",
        &[]
    )?;

    println!("Results (All Employees):");
    println!("{:<10} {:<12} {:<12} {:<15} {:<15}", "Count", "Min Salary", "Max Salary", "Avg Salary", "Total Salary");
    println!("{:-<70}", "");

    if let Some(row) = results.first() {
        let count = match row.get(0).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };
        let min_sal = match row.get(1).unwrap() {
            Value::Int4(i) => *i,
            _ => 0,
        };
        let max_sal = match row.get(2).unwrap() {
            Value::Int4(i) => *i,
            _ => 0,
        };
        let avg_sal = match row.get(3).unwrap() {
            Value::Float8(f) => *f,
            _ => 0.0,
        };
        let total_sal = match row.get(4).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };

        println!("{:<10} ${:<11} ${:<11} ${:<14.2} ${:<14}",
            count, min_sal, max_sal, avg_sal, total_sal);
    }

    // COUNT(DISTINCT) demo
    println!("\nQuery: SELECT COUNT(*), COUNT(DISTINCT dept) FROM employees\n");

    let results = db.query(
        "SELECT COUNT(*), COUNT(DISTINCT dept) FROM employees",
        &[]
    )?;

    if let Some(row) = results.first() {
        let total = match row.get(0).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };
        let distinct_depts = match row.get(1).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };

        println!("Total Employees: {}", total);
        println!("Distinct Departments: {}", distinct_depts);
    }

    Ok(())
}

fn demo_multi_column_grouping(db: &EmbeddedDatabase) -> Result<()> {
    println!("Query: SELECT region, product, SUM(amount) FROM sales");
    println!("       GROUP BY region, product\n");

    let results = db.query(
        "SELECT region, product, SUM(amount) FROM sales GROUP BY region, product",
        &[]
    )?;

    println!("Results (Sales by Region and Product):");
    println!("{:<12} {:<12} {:<15}", "Region", "Product", "Total Sales");
    println!("{:-<45}", "");

    for row in &results {
        let region = match row.get(0).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };
        let product = match row.get(1).unwrap() {
            Value::String(s) => s.clone(),
            _ => "Unknown".to_string(),
        };
        let total = match row.get(2).unwrap() {
            Value::Int8(i) => *i,
            _ => 0,
        };

        println!("{:<12} {:<12} ${:<14}", region, product, total);
    }

    println!("\nTotal region-product combinations: {}", results.len());
    Ok(())
}
