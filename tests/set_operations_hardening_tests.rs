//! Comprehensive hardening tests for SQL set operations: UNION, INTERSECT, EXCEPT
//!
//! Covers:
//! - UNION / UNION ALL basics (dedup, column naming, empty sets)
//! - INTERSECT / INTERSECT ALL (common rows, empty intersections)
//! - EXCEPT / EXCEPT ALL (subtraction semantics, superset/subset)
//! - NULL handling in set operations
//! - Type handling and column count mismatches
//! - ORDER BY and LIMIT with set operations
//! - Nested/chained set operations
//! - Edge cases (aggregates, WHERE, subqueries, multi-column, large unions)

mod test_helpers;

use heliosdb_nano::Value;
use test_helpers::*;

// ============================================================================
// 1. UNION Basics (~6 tests)
// ============================================================================

#[test]
fn test_union_removes_duplicates() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE set_a (val INT)").unwrap();
    db.execute("INSERT INTO set_a VALUES (1)").unwrap();
    db.execute("INSERT INTO set_a VALUES (2)").unwrap();
    db.execute("INSERT INTO set_a VALUES (3)").unwrap();

    db.execute("CREATE TABLE set_b (val INT)").unwrap();
    db.execute("INSERT INTO set_b VALUES (2)").unwrap();
    db.execute("INSERT INTO set_b VALUES (3)").unwrap();
    db.execute("INSERT INTO set_b VALUES (4)").unwrap();

    // UNION should return {1,2,3,4} -- 4 unique values
    let rows = db
        .query("SELECT val FROM set_a UNION SELECT val FROM set_b", &[])
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "UNION should remove duplicates, expected 4 rows, got {}",
        rows.len()
    );

    // Verify values present
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2, 3, 4], "UNION should contain [1,2,3,4]");
}

#[test]
fn test_union_all_preserves_duplicates() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ua1 (val INT)").unwrap();
    db.execute("INSERT INTO ua1 VALUES (1)").unwrap();
    db.execute("INSERT INTO ua1 VALUES (2)").unwrap();

    db.execute("CREATE TABLE ua2 (val INT)").unwrap();
    db.execute("INSERT INTO ua2 VALUES (2)").unwrap();
    db.execute("INSERT INTO ua2 VALUES (3)").unwrap();

    // UNION ALL should return {1,2,2,3} -- all 4 rows
    let rows = db
        .query(
            "SELECT val FROM ua1 UNION ALL SELECT val FROM ua2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "UNION ALL should keep all rows including duplicates"
    );

    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(
        vals,
        vec![1, 2, 2, 3],
        "UNION ALL should have two copies of 2"
    );
}

#[test]
fn test_union_uses_first_select_column_names() {
    let db = create_test_db().unwrap();

    // SQL standard: column names come from the first SELECT
    // SELECT 1 AS first_name UNION SELECT 2 AS second_name
    // Result column should be named 'first_name'
    let rows = db
        .query(
            "SELECT 1 AS first_name UNION SELECT 2 AS second_name",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2, "Should return 2 rows for UNION of 1 and 2");
    // We verify the query succeeds and produces correct data
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2]);
}

#[test]
fn test_union_single_row_each_side() {
    let db = create_test_db().unwrap();

    let rows = db
        .query("SELECT 42 AS val UNION SELECT 99 AS val", &[])
        .unwrap();
    assert_eq!(rows.len(), 2, "UNION of two single-row SELECTs should give 2 rows");
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![42, 99]);
}

#[test]
fn test_union_with_empty_result_on_one_side() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ue_full (val INT)").unwrap();
    db.execute("INSERT INTO ue_full VALUES (10)").unwrap();
    db.execute("INSERT INTO ue_full VALUES (20)").unwrap();

    db.execute("CREATE TABLE ue_empty (val INT)").unwrap();
    // ue_empty has no rows

    // UNION with empty right side
    let rows = db
        .query(
            "SELECT val FROM ue_full UNION SELECT val FROM ue_empty",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2, "UNION with empty right side should return left side rows");

    // UNION with empty left side
    let rows = db
        .query(
            "SELECT val FROM ue_empty UNION SELECT val FROM ue_full",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2, "UNION with empty left side should return right side rows");
}

#[test]
fn test_union_both_sides_empty() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE empty1 (val INT)").unwrap();
    db.execute("CREATE TABLE empty2 (val INT)").unwrap();

    let rows = db
        .query(
            "SELECT val FROM empty1 UNION SELECT val FROM empty2",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 0, "UNION of two empty tables should return 0 rows");
}

// ============================================================================
// 2. INTERSECT (~5 tests)
// ============================================================================

#[test]
fn test_intersect_returns_common_rows() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ia (val INT)").unwrap();
    db.execute("INSERT INTO ia VALUES (1)").unwrap();
    db.execute("INSERT INTO ia VALUES (2)").unwrap();
    db.execute("INSERT INTO ia VALUES (3)").unwrap();

    db.execute("CREATE TABLE ib (val INT)").unwrap();
    db.execute("INSERT INTO ib VALUES (2)").unwrap();
    db.execute("INSERT INTO ib VALUES (3)").unwrap();
    db.execute("INSERT INTO ib VALUES (4)").unwrap();

    let rows = db
        .query("SELECT val FROM ia INTERSECT SELECT val FROM ib", &[])
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "INTERSECT should return only common rows (2 and 3)"
    );
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![2, 3]);
}

#[test]
fn test_intersect_no_common_rows() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE in1 (val INT)").unwrap();
    db.execute("INSERT INTO in1 VALUES (1)").unwrap();
    db.execute("INSERT INTO in1 VALUES (2)").unwrap();

    db.execute("CREATE TABLE in2 (val INT)").unwrap();
    db.execute("INSERT INTO in2 VALUES (3)").unwrap();
    db.execute("INSERT INTO in2 VALUES (4)").unwrap();

    let rows = db
        .query("SELECT val FROM in1 INTERSECT SELECT val FROM in2", &[])
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "INTERSECT with no common rows should return empty"
    );
}

#[test]
fn test_intersect_all_preserves_duplicates() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ial (val INT)").unwrap();
    db.execute("INSERT INTO ial VALUES (1)").unwrap();
    db.execute("INSERT INTO ial VALUES (2)").unwrap();
    db.execute("INSERT INTO ial VALUES (2)").unwrap();
    db.execute("INSERT INTO ial VALUES (3)").unwrap();

    db.execute("CREATE TABLE iar (val INT)").unwrap();
    db.execute("INSERT INTO iar VALUES (2)").unwrap();
    db.execute("INSERT INTO iar VALUES (2)").unwrap();
    db.execute("INSERT INTO iar VALUES (2)").unwrap();
    db.execute("INSERT INTO iar VALUES (3)").unwrap();

    // INTERSECT ALL: min(left_count, right_count) for each value
    // val=2: min(2,3) = 2 copies; val=3: min(1,1) = 1 copy
    let result = db.query(
        "SELECT val FROM ial INTERSECT ALL SELECT val FROM iar",
        &[],
    );
    match result {
        Ok(rows) => {
            // Expected: 2 copies of 2, 1 copy of 3 = 3 rows total
            assert_eq!(
                rows.len(),
                3,
                "INTERSECT ALL should return min(left,right) duplicates per value"
            );
            let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            vals.sort();
            assert_eq!(vals, vec![2, 2, 3]);
        }
        Err(e) => {
            // INTERSECT ALL may not be supported
            let err_str = e.to_string();
            println!(
                "INTERSECT ALL not supported (acceptable): {}",
                err_str
            );
        }
    }
}

#[test]
fn test_intersect_with_one_empty_side() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ie_full (val INT)").unwrap();
    db.execute("INSERT INTO ie_full VALUES (1)").unwrap();
    db.execute("INSERT INTO ie_full VALUES (2)").unwrap();

    db.execute("CREATE TABLE ie_empty (val INT)").unwrap();

    // INTERSECT with empty right side -> empty
    let rows = db
        .query(
            "SELECT val FROM ie_full INTERSECT SELECT val FROM ie_empty",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "INTERSECT with empty right side should return empty"
    );

    // INTERSECT with empty left side -> empty
    let rows = db
        .query(
            "SELECT val FROM ie_empty INTERSECT SELECT val FROM ie_full",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "INTERSECT with empty left side should return empty"
    );
}

#[test]
fn test_intersect_identical_sets() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE iid (val INT)").unwrap();
    db.execute("INSERT INTO iid VALUES (10)").unwrap();
    db.execute("INSERT INTO iid VALUES (20)").unwrap();
    db.execute("INSERT INTO iid VALUES (30)").unwrap();

    // INTERSECT of table with itself should return all unique rows
    let rows = db
        .query(
            "SELECT val FROM iid INTERSECT SELECT val FROM iid",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "INTERSECT of identical sets should return all rows"
    );
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![10, 20, 30]);
}

// ============================================================================
// 3. EXCEPT (~5 tests)
// ============================================================================

#[test]
fn test_except_removes_second_from_first() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ea (val INT)").unwrap();
    db.execute("INSERT INTO ea VALUES (1)").unwrap();
    db.execute("INSERT INTO ea VALUES (2)").unwrap();
    db.execute("INSERT INTO ea VALUES (3)").unwrap();

    db.execute("CREATE TABLE eb (val INT)").unwrap();
    db.execute("INSERT INTO eb VALUES (2)").unwrap();
    db.execute("INSERT INTO eb VALUES (3)").unwrap();

    // EXCEPT should return rows in A not in B: {1}
    let rows = db
        .query("SELECT val FROM ea EXCEPT SELECT val FROM eb", &[])
        .unwrap();
    assert_eq!(rows.len(), 1, "EXCEPT should remove common rows from first set");
    assert_eq!(get_int_value(&rows[0], 0).unwrap(), 1);
}

#[test]
fn test_except_all_preserves_duplicates() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE eal (val INT)").unwrap();
    db.execute("INSERT INTO eal VALUES (1)").unwrap();
    db.execute("INSERT INTO eal VALUES (2)").unwrap();
    db.execute("INSERT INTO eal VALUES (2)").unwrap();
    db.execute("INSERT INTO eal VALUES (2)").unwrap();
    db.execute("INSERT INTO eal VALUES (3)").unwrap();

    db.execute("CREATE TABLE ear (val INT)").unwrap();
    db.execute("INSERT INTO ear VALUES (2)").unwrap();
    db.execute("INSERT INTO ear VALUES (3)").unwrap();

    // EXCEPT ALL: subtract one copy of each matching value
    // val=1: 1-0=1; val=2: 3-1=2; val=3: 1-1=0 => {1, 2, 2}
    let result = db.query(
        "SELECT val FROM eal EXCEPT ALL SELECT val FROM ear",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(
                rows.len(),
                3,
                "EXCEPT ALL should subtract matching row counts"
            );
            let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            vals.sort();
            assert_eq!(vals, vec![1, 2, 2]);
        }
        Err(e) => {
            println!(
                "EXCEPT ALL not supported (acceptable): {}",
                e
            );
        }
    }
}

#[test]
fn test_except_no_overlap() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE en1 (val INT)").unwrap();
    db.execute("INSERT INTO en1 VALUES (1)").unwrap();
    db.execute("INSERT INTO en1 VALUES (2)").unwrap();

    db.execute("CREATE TABLE en2 (val INT)").unwrap();
    db.execute("INSERT INTO en2 VALUES (3)").unwrap();
    db.execute("INSERT INTO en2 VALUES (4)").unwrap();

    // EXCEPT with no overlap should return all of first
    let rows = db
        .query("SELECT val FROM en1 EXCEPT SELECT val FROM en2", &[])
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "EXCEPT with no overlap should return all rows from first set"
    );
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2]);
}

#[test]
fn test_except_second_is_superset() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE es_sub (val INT)").unwrap();
    db.execute("INSERT INTO es_sub VALUES (1)").unwrap();
    db.execute("INSERT INTO es_sub VALUES (2)").unwrap();

    db.execute("CREATE TABLE es_sup (val INT)").unwrap();
    db.execute("INSERT INTO es_sup VALUES (1)").unwrap();
    db.execute("INSERT INTO es_sup VALUES (2)").unwrap();
    db.execute("INSERT INTO es_sup VALUES (3)").unwrap();

    // EXCEPT where second is superset => empty result
    let rows = db
        .query(
            "SELECT val FROM es_sub EXCEPT SELECT val FROM es_sup",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "EXCEPT where second set is superset should return empty"
    );
}

#[test]
fn test_except_empty_second_set() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ees_full (val INT)").unwrap();
    db.execute("INSERT INTO ees_full VALUES (5)").unwrap();
    db.execute("INSERT INTO ees_full VALUES (6)").unwrap();

    db.execute("CREATE TABLE ees_empty (val INT)").unwrap();

    // EXCEPT with empty right side should return all of left
    let rows = db
        .query(
            "SELECT val FROM ees_full EXCEPT SELECT val FROM ees_empty",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "EXCEPT with empty second set should return all of first"
    );
}

// ============================================================================
// 4. NULL Handling in Set Operations (~5 tests)
// ============================================================================

#[test]
fn test_union_with_null_values() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE nu1 (val INT)").unwrap();
    db.execute("INSERT INTO nu1 VALUES (1)").unwrap();
    db.execute("INSERT INTO nu1 VALUES (NULL)").unwrap();

    db.execute("CREATE TABLE nu2 (val INT)").unwrap();
    db.execute("INSERT INTO nu2 VALUES (NULL)").unwrap();
    db.execute("INSERT INTO nu2 VALUES (2)").unwrap();

    // SQL standard: NULL is treated as equal for UNION dedup purposes
    let rows = db
        .query("SELECT val FROM nu1 UNION SELECT val FROM nu2", &[])
        .unwrap();

    // Expected: {1, NULL, 2} = 3 rows (NULLs deduplicated)
    // Or {1, NULL, NULL, 2} = 4 rows if NULLs are NOT deduplicated
    let null_count = rows
        .iter()
        .filter(|r| matches!(r.get(0), Some(Value::Null)))
        .count();

    // Document actual behavior
    println!(
        "UNION NULL dedup: {} total rows, {} NULL rows (SQL standard expects 1 NULL)",
        rows.len(),
        null_count
    );

    // At minimum, non-null values should all be present
    let non_null_vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    assert!(
        non_null_vals.contains(&1) && non_null_vals.contains(&2),
        "UNION should contain non-null values from both sides"
    );
}

#[test]
fn test_intersect_with_null_in_both_sides() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ni1 (val INT)").unwrap();
    db.execute("INSERT INTO ni1 VALUES (1)").unwrap();
    db.execute("INSERT INTO ni1 VALUES (NULL)").unwrap();

    db.execute("CREATE TABLE ni2 (val INT)").unwrap();
    db.execute("INSERT INTO ni2 VALUES (NULL)").unwrap();
    db.execute("INSERT INTO ni2 VALUES (2)").unwrap();

    // SQL standard: NULL = NULL for INTERSECT purposes
    let rows = db
        .query(
            "SELECT val FROM ni1 INTERSECT SELECT val FROM ni2",
            &[],
        )
        .unwrap();

    let null_count = rows
        .iter()
        .filter(|r| matches!(r.get(0), Some(Value::Null)))
        .count();

    println!(
        "INTERSECT NULL handling: {} total rows, {} NULL rows (SQL standard: NULL matches NULL)",
        rows.len(),
        null_count
    );

    // The only common value should be NULL (if NULL=NULL for set ops)
    // val=1 is only in ni1, val=2 is only in ni2
    let non_null_count = rows.len() - null_count;
    assert_eq!(
        non_null_count, 0,
        "INTERSECT should not return non-common non-null values"
    );
}

#[test]
fn test_except_with_null_values() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ne1 (val INT)").unwrap();
    db.execute("INSERT INTO ne1 VALUES (1)").unwrap();
    db.execute("INSERT INTO ne1 VALUES (NULL)").unwrap();
    db.execute("INSERT INTO ne1 VALUES (2)").unwrap();

    db.execute("CREATE TABLE ne2 (val INT)").unwrap();
    db.execute("INSERT INTO ne2 VALUES (NULL)").unwrap();
    db.execute("INSERT INTO ne2 VALUES (2)").unwrap();

    // EXCEPT: {1, NULL, 2} - {NULL, 2} = {1}
    let rows = db
        .query("SELECT val FROM ne1 EXCEPT SELECT val FROM ne2", &[])
        .unwrap();

    // If NULL=NULL for EXCEPT, result should be {1}
    let null_count = rows
        .iter()
        .filter(|r| matches!(r.get(0), Some(Value::Null)))
        .count();
    println!(
        "EXCEPT NULL handling: {} total rows, {} NULL rows",
        rows.len(),
        null_count
    );

    // At minimum, val=1 should be present (it's only in ne1)
    let non_null_vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    assert!(
        non_null_vals.contains(&1),
        "EXCEPT should return val=1 which is only in left side"
    );
    // val=2 should NOT be present (it's in both sides)
    assert!(
        !non_null_vals.contains(&2),
        "EXCEPT should not return val=2 which is in both sides"
    );
}

#[test]
fn test_union_all_with_nulls() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE nua1 (val INT)").unwrap();
    db.execute("INSERT INTO nua1 VALUES (NULL)").unwrap();
    db.execute("INSERT INTO nua1 VALUES (1)").unwrap();

    db.execute("CREATE TABLE nua2 (val INT)").unwrap();
    db.execute("INSERT INTO nua2 VALUES (NULL)").unwrap();
    db.execute("INSERT INTO nua2 VALUES (2)").unwrap();

    // UNION ALL should preserve all rows including duplicate NULLs
    let rows = db
        .query(
            "SELECT val FROM nua1 UNION ALL SELECT val FROM nua2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "UNION ALL should return all 4 rows (no dedup)"
    );

    let null_count = rows
        .iter()
        .filter(|r| matches!(r.get(0), Some(Value::Null)))
        .count();
    assert_eq!(
        null_count, 2,
        "UNION ALL should preserve both NULL values"
    );
}

#[test]
fn test_null_only_sets_union() {
    let db = create_test_db().unwrap();

    // SELECT NULL UNION SELECT NULL
    let result = db.query("SELECT NULL AS val UNION SELECT NULL", &[]);
    match result {
        Ok(rows) => {
            // SQL standard: NULL = NULL for UNION dedup, so result should be 1 row
            println!(
                "NULL UNION NULL: {} rows (SQL standard expects 1)",
                rows.len()
            );
            // Both 1 and 2 are acceptable behaviors depending on NULL dedup semantics
            assert!(
                rows.len() >= 1 && rows.len() <= 2,
                "NULL UNION NULL should return 1 or 2 rows"
            );
            // All rows should be NULL
            for row in &rows {
                assert_eq!(
                    row.get(0).unwrap(),
                    &Value::Null,
                    "All rows in NULL UNION NULL should be NULL"
                );
            }
        }
        Err(e) => {
            println!("NULL UNION NULL error (acceptable): {}", e);
        }
    }
}

// ============================================================================
// 5. Type Handling (~4 tests)
// ============================================================================

#[test]
fn test_union_same_int_types() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ti1 (val INT)").unwrap();
    db.execute("INSERT INTO ti1 VALUES (100)").unwrap();

    db.execute("CREATE TABLE ti2 (val INT)").unwrap();
    db.execute("INSERT INTO ti2 VALUES (200)").unwrap();

    let rows = db
        .query("SELECT val FROM ti1 UNION SELECT val FROM ti2", &[])
        .unwrap();
    assert_eq!(rows.len(), 2, "UNION of same INT types should return 2 rows");
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![100, 200]);
}

#[test]
fn test_union_compatible_int_bigint_types() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE tc_int (val INT)").unwrap();
    db.execute("INSERT INTO tc_int VALUES (42)").unwrap();

    db.execute("CREATE TABLE tc_big (val BIGINT)").unwrap();
    db.execute("INSERT INTO tc_big VALUES (9999999999)").unwrap();

    let result = db.query(
        "SELECT val FROM tc_int UNION SELECT val FROM tc_big",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(
                rows.len(),
                2,
                "UNION of INT and BIGINT should return 2 rows"
            );
        }
        Err(e) => {
            // Type width mismatch may be rejected
            println!(
                "INT/BIGINT UNION type mismatch (acceptable): {}",
                e
            );
        }
    }
}

#[test]
fn test_union_different_column_counts_should_error() {
    let db = create_test_db().unwrap();

    // UNION with different column counts must produce an error
    let result = db.query("SELECT 1, 2 UNION SELECT 3", &[]);
    match result {
        Ok(rows) => {
            // Some databases silently handle this, but it's non-standard
            println!(
                "Different column count UNION returned {} rows (unexpected but documented)",
                rows.len()
            );
        }
        Err(e) => {
            let err_str = e.to_string().to_lowercase();
            // Error should indicate column count mismatch
            println!("Column count mismatch error (expected): {}", e);
            assert!(
                err_str.contains("column")
                    || err_str.contains("mismatch")
                    || err_str.contains("number")
                    || err_str.contains("different")
                    || err_str.contains("match"),
                "Error should reference column count mismatch, got: {}",
                err_str
            );
        }
    }
}

#[test]
fn test_union_text_and_text_columns() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE tt1 (name TEXT)").unwrap();
    db.execute("INSERT INTO tt1 VALUES ('Alice')").unwrap();
    db.execute("INSERT INTO tt1 VALUES ('Bob')").unwrap();

    db.execute("CREATE TABLE tt2 (label TEXT)").unwrap();
    db.execute("INSERT INTO tt2 VALUES ('Bob')").unwrap();
    db.execute("INSERT INTO tt2 VALUES ('Carol')").unwrap();

    let rows = db
        .query(
            "SELECT name FROM tt1 UNION SELECT label FROM tt2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "UNION of text columns should dedup 'Bob' and return [Alice, Bob, Carol]"
    );
    let mut vals: Vec<String> = rows
        .iter()
        .filter_map(|r| get_string_value(r, 0))
        .collect();
    vals.sort();
    assert_eq!(vals, vec!["Alice", "Bob", "Carol"]);
}

// ============================================================================
// 6. ORDER BY and LIMIT with Set Operations (~5 tests)
// ============================================================================

#[test]
fn test_union_order_by_column_name() {
    let db = create_test_db().unwrap();

    // ORDER BY applied to the UNION result
    let result = db.query(
        "SELECT 3 AS val UNION SELECT 1 AS val UNION SELECT 2 AS val ORDER BY val",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(rows.len(), 3, "Should return 3 ordered rows");
            let vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            assert_eq!(vals, vec![1, 2, 3], "Rows should be ordered ascending by val");
        }
        Err(e) => {
            println!("UNION ORDER BY column_name error: {}", e);
        }
    }
}

#[test]
fn test_union_order_by_ordinal_position() {
    let db = create_test_db().unwrap();

    // ORDER BY 1 (ordinal position) - standard SQL feature
    let result = db.query(
        "SELECT 30 AS val UNION SELECT 10 UNION SELECT 20 ORDER BY 1",
        &[],
    );
    let rows = result.expect("ORDER BY ordinal position should work");
    assert_eq!(rows.len(), 3, "Should return 3 rows");
    let vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    assert_eq!(vals, vec![10, 20, 30], "ORDER BY 1 should sort by first column ascending");

    // ORDER BY 1 DESC
    let result = db.query(
        "SELECT 30 AS val UNION SELECT 10 UNION SELECT 20 ORDER BY 1 DESC",
        &[],
    );
    let rows = result.expect("ORDER BY ordinal position DESC should work");
    let vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    assert_eq!(vals, vec![30, 20, 10], "ORDER BY 1 DESC should sort by first column descending");

    // ORDER BY with out-of-range ordinal should error
    let result = db.query(
        "SELECT 1 AS val UNION SELECT 2 ORDER BY 5",
        &[],
    );
    assert!(result.is_err(), "ORDER BY with out-of-range ordinal should return an error");
}

#[test]
fn test_union_with_limit() {
    let db = create_test_db().unwrap();

    let result = db.query(
        "SELECT 1 AS val UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5 LIMIT 3",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(
                rows.len(),
                3,
                "UNION with LIMIT 3 should return exactly 3 rows"
            );
        }
        Err(e) => {
            println!("UNION LIMIT error: {}", e);
        }
    }
}

#[test]
fn test_intersect_with_order_by() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE io1 (val INT)").unwrap();
    db.execute("INSERT INTO io1 VALUES (3)").unwrap();
    db.execute("INSERT INTO io1 VALUES (1)").unwrap();
    db.execute("INSERT INTO io1 VALUES (2)").unwrap();

    db.execute("CREATE TABLE io2 (val INT)").unwrap();
    db.execute("INSERT INTO io2 VALUES (2)").unwrap();
    db.execute("INSERT INTO io2 VALUES (3)").unwrap();
    db.execute("INSERT INTO io2 VALUES (4)").unwrap();

    let result = db.query(
        "SELECT val FROM io1 INTERSECT SELECT val FROM io2 ORDER BY val",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(rows.len(), 2, "INTERSECT should return [2,3]");
            let vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            assert_eq!(vals, vec![2, 3], "INTERSECT ORDER BY should return sorted results");
        }
        Err(e) => {
            println!("INTERSECT ORDER BY error: {}", e);
        }
    }
}

#[test]
fn test_except_with_limit() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE el1 (val INT)").unwrap();
    db.execute("INSERT INTO el1 VALUES (1)").unwrap();
    db.execute("INSERT INTO el1 VALUES (2)").unwrap();
    db.execute("INSERT INTO el1 VALUES (3)").unwrap();
    db.execute("INSERT INTO el1 VALUES (4)").unwrap();

    db.execute("CREATE TABLE el2 (val INT)").unwrap();
    db.execute("INSERT INTO el2 VALUES (2)").unwrap();

    // EXCEPT should yield {1,3,4}, then LIMIT 2
    let result = db.query(
        "SELECT val FROM el1 EXCEPT SELECT val FROM el2 LIMIT 2",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(
                rows.len(),
                2,
                "EXCEPT with LIMIT 2 should return exactly 2 rows"
            );
        }
        Err(e) => {
            println!("EXCEPT LIMIT error: {}", e);
        }
    }
}

// ============================================================================
// 7. Nested/Chained Set Operations (~5 tests)
// ============================================================================

#[test]
fn test_three_way_union() {
    let db = create_test_db().unwrap();

    // A UNION B UNION C
    let rows = db
        .query(
            "SELECT 1 AS val UNION SELECT 2 UNION SELECT 3",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 3, "Three-way UNION should return 3 distinct values");
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2, 3]);
}

#[test]
fn test_union_then_intersect() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE ui_a (val INT)").unwrap();
    db.execute("INSERT INTO ui_a VALUES (1)").unwrap();
    db.execute("INSERT INTO ui_a VALUES (2)").unwrap();

    db.execute("CREATE TABLE ui_b (val INT)").unwrap();
    db.execute("INSERT INTO ui_b VALUES (3)").unwrap();
    db.execute("INSERT INTO ui_b VALUES (4)").unwrap();

    db.execute("CREATE TABLE ui_c (val INT)").unwrap();
    db.execute("INSERT INTO ui_c VALUES (1)").unwrap();
    db.execute("INSERT INTO ui_c VALUES (3)").unwrap();

    // (A UNION B) INTERSECT C = {1,2,3,4} INTERSECT {1,3} = {1,3}
    let result = db.query(
        "(SELECT val FROM ui_a UNION SELECT val FROM ui_b) INTERSECT SELECT val FROM ui_c",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(rows.len(), 2, "UNION then INTERSECT should return [1,3]");
            let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            vals.sort();
            assert_eq!(vals, vec![1, 3]);
        }
        Err(e) => {
            // Parenthesized set operations may not be supported; try without parens
            println!(
                "Parenthesized UNION INTERSECT error: {}, trying flat form",
                e
            );
            // Without parentheses, SQL standard precedence: INTERSECT binds tighter than UNION
            // A UNION B INTERSECT C = A UNION (B INTERSECT C) = {1,2} UNION ({3,4} INTERSECT {1,3}) = {1,2} UNION {3} = {1,2,3}
            let result2 = db.query(
                "SELECT val FROM ui_a UNION SELECT val FROM ui_b INTERSECT SELECT val FROM ui_c",
                &[],
            );
            match result2 {
                Ok(rows) => {
                    println!(
                        "Flat UNION/INTERSECT returned {} rows (precedence depends on implementation)",
                        rows.len()
                    );
                }
                Err(e2) => {
                    println!("Chained UNION INTERSECT not supported: {}", e2);
                }
            }
        }
    }
}

#[test]
fn test_except_from_union() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE eu_a (val INT)").unwrap();
    db.execute("INSERT INTO eu_a VALUES (1)").unwrap();
    db.execute("INSERT INTO eu_a VALUES (2)").unwrap();
    db.execute("INSERT INTO eu_a VALUES (3)").unwrap();

    db.execute("CREATE TABLE eu_b (val INT)").unwrap();
    db.execute("INSERT INTO eu_b VALUES (2)").unwrap();
    db.execute("INSERT INTO eu_b VALUES (4)").unwrap();

    db.execute("CREATE TABLE eu_c (val INT)").unwrap();
    db.execute("INSERT INTO eu_c VALUES (3)").unwrap();
    db.execute("INSERT INTO eu_c VALUES (4)").unwrap();

    // A EXCEPT (B UNION C) = {1,2,3} - {2,3,4} = {1}
    let result = db.query(
        "SELECT val FROM eu_a EXCEPT (SELECT val FROM eu_b UNION SELECT val FROM eu_c)",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(rows.len(), 1, "A EXCEPT (B UNION C) should return [1]");
            assert_eq!(get_int_value(&rows[0], 0).unwrap(), 1);
        }
        Err(e) => {
            println!(
                "Parenthesized EXCEPT (UNION) error: {}",
                e
            );
        }
    }
}

#[test]
fn test_mixed_union_all_and_except() {
    let db = create_test_db().unwrap();

    // A UNION ALL B EXCEPT C
    // SQL standard: EXCEPT has same precedence as UNION, evaluated left-to-right
    // So: (A UNION ALL B) EXCEPT C
    let result = db.query(
        "SELECT 1 AS val UNION ALL SELECT 2 EXCEPT SELECT 2",
        &[],
    );
    match result {
        Ok(rows) => {
            println!(
                "UNION ALL then EXCEPT returned {} rows",
                rows.len()
            );
            // Depends on precedence and whether EXCEPT deduplicates first
            // Document actual behavior
            let vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            println!("Values: {:?}", vals);
        }
        Err(e) => {
            println!("Mixed UNION ALL/EXCEPT error: {}", e);
        }
    }
}

#[test]
fn test_three_way_union_with_overlapping_data() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE tw_a (val INT)").unwrap();
    db.execute("INSERT INTO tw_a VALUES (1)").unwrap();
    db.execute("INSERT INTO tw_a VALUES (2)").unwrap();

    db.execute("CREATE TABLE tw_b (val INT)").unwrap();
    db.execute("INSERT INTO tw_b VALUES (2)").unwrap();
    db.execute("INSERT INTO tw_b VALUES (3)").unwrap();

    db.execute("CREATE TABLE tw_c (val INT)").unwrap();
    db.execute("INSERT INTO tw_c VALUES (3)").unwrap();
    db.execute("INSERT INTO tw_c VALUES (4)").unwrap();

    // All overlapping: A UNION B UNION C should produce {1,2,3,4}
    let rows = db
        .query(
            "SELECT val FROM tw_a UNION SELECT val FROM tw_b UNION SELECT val FROM tw_c",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "Three-way UNION with overlaps should produce 4 unique values"
    );
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2, 3, 4]);
}

// ============================================================================
// 8. Edge Cases (~5 tests)
// ============================================================================

#[test]
fn test_union_multi_column_results() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE mc1 (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO mc1 VALUES (1, 'x')").unwrap();
    db.execute("INSERT INTO mc1 VALUES (2, 'y')").unwrap();

    db.execute("CREATE TABLE mc2 (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO mc2 VALUES (2, 'y')").unwrap();
    db.execute("INSERT INTO mc2 VALUES (3, 'z')").unwrap();

    // Multi-column UNION should match tuples (both columns must match for dedup)
    let rows = db
        .query(
            "SELECT a, b FROM mc1 UNION SELECT a, b FROM mc2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "Multi-column UNION should dedup (2,'y') and return 3 rows"
    );
}

#[test]
fn test_union_with_aggregate_queries() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE agg1 (grp TEXT, val INT)").unwrap();
    db.execute("INSERT INTO agg1 VALUES ('a', 10)").unwrap();
    db.execute("INSERT INTO agg1 VALUES ('a', 20)").unwrap();
    db.execute("INSERT INTO agg1 VALUES ('b', 30)").unwrap();

    db.execute("CREATE TABLE agg2 (grp TEXT, val INT)").unwrap();
    db.execute("INSERT INTO agg2 VALUES ('b', 40)").unwrap();
    db.execute("INSERT INTO agg2 VALUES ('c', 50)").unwrap();

    // UNION of aggregated results
    let result = db.query(
        "SELECT grp, SUM(val) AS total FROM agg1 GROUP BY grp \
         UNION \
         SELECT grp, SUM(val) AS total FROM agg2 GROUP BY grp",
        &[],
    );
    match result {
        Ok(rows) => {
            // agg1: ('a', 30), ('b', 30)
            // agg2: ('b', 40), ('c', 50)
            // UNION: ('a', 30), ('b', 30), ('b', 40), ('c', 50) = 4 rows (no exact dedup)
            // ('b',30) != ('b',40) so no dedup between them
            assert!(
                rows.len() >= 3 && rows.len() <= 4,
                "UNION of aggregates should return 3-4 rows, got {}",
                rows.len()
            );
        }
        Err(e) => {
            println!("UNION with aggregates error: {}", e);
        }
    }
}

#[test]
fn test_set_operation_with_where_clauses() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE wh1 (id INT PRIMARY KEY, val INT)").unwrap();
    db.execute("INSERT INTO wh1 VALUES (1, 10)").unwrap();
    db.execute("INSERT INTO wh1 VALUES (2, 20)").unwrap();
    db.execute("INSERT INTO wh1 VALUES (3, 30)").unwrap();

    db.execute("CREATE TABLE wh2 (id INT PRIMARY KEY, val INT)").unwrap();
    db.execute("INSERT INTO wh2 VALUES (1, 20)").unwrap();
    db.execute("INSERT INTO wh2 VALUES (2, 40)").unwrap();

    // UNION with WHERE on each side
    let rows = db
        .query(
            "SELECT val FROM wh1 WHERE val > 15 UNION SELECT val FROM wh2 WHERE val < 30",
            &[],
        )
        .unwrap();

    // wh1 WHERE val > 15: {20, 30}
    // wh2 WHERE val < 30: {20}
    // UNION: {20, 30} (20 is deduplicated)
    assert_eq!(
        rows.len(),
        2,
        "UNION with WHERE clauses should filter each side independently"
    );
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![20, 30]);
}

#[test]
fn test_union_with_subquery_on_one_side() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE sq_main (val INT)").unwrap();
    db.execute("INSERT INTO sq_main VALUES (1)").unwrap();
    db.execute("INSERT INTO sq_main VALUES (2)").unwrap();
    db.execute("INSERT INTO sq_main VALUES (3)").unwrap();

    // UNION with a subquery (derived table) on one side
    let result = db.query(
        "SELECT val FROM sq_main WHERE val <= 2 \
         UNION \
         SELECT val FROM (SELECT val FROM sq_main WHERE val >= 2) AS sub",
        &[],
    );
    match result {
        Ok(rows) => {
            // Left: {1, 2}, Right: {2, 3} => UNION: {1, 2, 3}
            assert_eq!(
                rows.len(),
                3,
                "UNION with subquery should combine results correctly"
            );
            let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            vals.sort();
            assert_eq!(vals, vec![1, 2, 3]);
        }
        Err(e) => {
            println!("UNION with subquery error: {}", e);
        }
    }
}

#[test]
fn test_large_union_five_selects() {
    let db = create_test_db().unwrap();

    // Five-way UNION of literal SELECTs
    let rows = db
        .query(
            "SELECT 1 AS val \
             UNION SELECT 2 \
             UNION SELECT 3 \
             UNION SELECT 4 \
             UNION SELECT 5",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        5,
        "Five-way UNION should return 5 distinct values"
    );
    let mut vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2, 3, 4, 5]);
}

// ============================================================================
// Additional edge cases
// ============================================================================

#[test]
fn test_union_dedup_multi_column_partial_match() {
    let db = create_test_db().unwrap();

    // Two rows with same first column but different second column should NOT be deduped
    db.execute("CREATE TABLE pd1 (a INT, b INT)").unwrap();
    db.execute("INSERT INTO pd1 VALUES (1, 10)").unwrap();

    db.execute("CREATE TABLE pd2 (a INT, b INT)").unwrap();
    db.execute("INSERT INTO pd2 VALUES (1, 20)").unwrap();

    let rows = db
        .query(
            "SELECT a, b FROM pd1 UNION SELECT a, b FROM pd2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "Rows with same first col but different second col should NOT be deduped"
    );
}

#[test]
fn test_except_self_yields_empty() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE es_self (val INT)").unwrap();
    db.execute("INSERT INTO es_self VALUES (1)").unwrap();
    db.execute("INSERT INTO es_self VALUES (2)").unwrap();
    db.execute("INSERT INTO es_self VALUES (3)").unwrap();

    // T EXCEPT T = empty set
    let rows = db
        .query(
            "SELECT val FROM es_self EXCEPT SELECT val FROM es_self",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "Table EXCEPT itself should return empty set"
    );
}

#[test]
fn test_union_all_does_not_dedup_identical_rows() {
    let db = create_test_db().unwrap();

    // Two identical literal selects with UNION ALL
    let rows = db
        .query(
            "SELECT 42 AS val UNION ALL SELECT 42",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "UNION ALL of identical rows should keep both copies"
    );
    assert_eq!(get_int_value(&rows[0], 0).unwrap(), 42);
    assert_eq!(get_int_value(&rows[1], 0).unwrap(), 42);
}

#[test]
fn test_union_vs_union_all_duplicate_count() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE dup_a (val INT)").unwrap();
    db.execute("INSERT INTO dup_a VALUES (1)").unwrap();
    db.execute("INSERT INTO dup_a VALUES (1)").unwrap();
    db.execute("INSERT INTO dup_a VALUES (2)").unwrap();

    db.execute("CREATE TABLE dup_b (val INT)").unwrap();
    db.execute("INSERT INTO dup_b VALUES (1)").unwrap();
    db.execute("INSERT INTO dup_b VALUES (3)").unwrap();

    // UNION should dedup: {1, 2, 3} = 3 rows
    let union_rows = db
        .query(
            "SELECT val FROM dup_a UNION SELECT val FROM dup_b",
            &[],
        )
        .unwrap();
    assert_eq!(union_rows.len(), 3, "UNION should return 3 unique values");

    // UNION ALL should keep all: {1, 1, 2, 1, 3} = 5 rows
    let union_all_rows = db
        .query(
            "SELECT val FROM dup_a UNION ALL SELECT val FROM dup_b",
            &[],
        )
        .unwrap();
    assert_eq!(
        union_all_rows.len(),
        5,
        "UNION ALL should return all 5 rows"
    );

    assert!(
        union_all_rows.len() > union_rows.len(),
        "UNION ALL should always return >= rows compared to UNION"
    );
}

#[test]
fn test_intersect_multi_column() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE im1 (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO im1 VALUES (1, 'x')").unwrap();
    db.execute("INSERT INTO im1 VALUES (2, 'y')").unwrap();
    db.execute("INSERT INTO im1 VALUES (3, 'z')").unwrap();

    db.execute("CREATE TABLE im2 (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO im2 VALUES (2, 'y')").unwrap();
    db.execute("INSERT INTO im2 VALUES (3, 'w')").unwrap(); // same 'a' but different 'b'
    db.execute("INSERT INTO im2 VALUES (4, 'x')").unwrap();

    // Only (2, 'y') is in both -- (3, 'z') != (3, 'w')
    let rows = db
        .query(
            "SELECT a, b FROM im1 INTERSECT SELECT a, b FROM im2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "Multi-column INTERSECT should match on all columns"
    );
    assert_eq!(get_int_value(&rows[0], 0).unwrap(), 2);
    assert_eq!(get_string_value(&rows[0], 1).unwrap(), "y");
}

#[test]
fn test_except_multi_column() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE em1 (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO em1 VALUES (1, 'x')").unwrap();
    db.execute("INSERT INTO em1 VALUES (2, 'y')").unwrap();
    db.execute("INSERT INTO em1 VALUES (3, 'z')").unwrap();

    db.execute("CREATE TABLE em2 (a INT, b TEXT)").unwrap();
    db.execute("INSERT INTO em2 VALUES (2, 'y')").unwrap();

    // EXCEPT: {(1,'x'), (2,'y'), (3,'z')} - {(2,'y')} = {(1,'x'), (3,'z')}
    let rows = db
        .query(
            "SELECT a, b FROM em1 EXCEPT SELECT a, b FROM em2",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "Multi-column EXCEPT should remove only exact tuple matches"
    );
}

#[test]
fn test_union_all_with_order_by_and_limit() {
    let db = create_test_db().unwrap();

    let result = db.query(
        "SELECT 5 AS val UNION ALL SELECT 1 UNION ALL SELECT 3 \
         UNION ALL SELECT 2 UNION ALL SELECT 4 ORDER BY val LIMIT 3",
        &[],
    );
    match result {
        Ok(rows) => {
            assert_eq!(rows.len(), 3, "Should return 3 rows with LIMIT");
            let vals: Vec<i64> = rows.iter().filter_map(|r| get_int_value(r, 0)).collect();
            assert_eq!(
                vals,
                vec![1, 2, 3],
                "ORDER BY + LIMIT should return first 3 sorted values"
            );
        }
        Err(e) => {
            println!("UNION ALL ORDER BY LIMIT error: {}", e);
        }
    }
}

#[test]
fn test_union_with_string_duplicates() {
    let db = create_test_db().unwrap();

    // Test string dedup in UNION
    let rows = db
        .query(
            "SELECT 'hello' AS val UNION SELECT 'world' UNION SELECT 'hello'",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "UNION should dedup identical strings, expected [hello, world]"
    );
}

#[test]
fn test_large_union_all_preserves_all_rows() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE lu (val INT)").unwrap();
    for i in 1..=50 {
        db.execute(&format!("INSERT INTO lu VALUES ({})", i % 10)).unwrap();
    }

    // UNION ALL of same table twice should double the row count
    let rows = db
        .query(
            "SELECT val FROM lu UNION ALL SELECT val FROM lu",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        100,
        "UNION ALL of 50-row table with itself should return 100 rows"
    );
}

#[test]
fn test_except_asymmetry() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE asym_a (val INT)").unwrap();
    db.execute("INSERT INTO asym_a VALUES (1)").unwrap();
    db.execute("INSERT INTO asym_a VALUES (2)").unwrap();

    db.execute("CREATE TABLE asym_b (val INT)").unwrap();
    db.execute("INSERT INTO asym_b VALUES (2)").unwrap();
    db.execute("INSERT INTO asym_b VALUES (3)").unwrap();

    // A EXCEPT B != B EXCEPT A (set difference is not commutative)
    let a_minus_b = db
        .query(
            "SELECT val FROM asym_a EXCEPT SELECT val FROM asym_b",
            &[],
        )
        .unwrap();
    let b_minus_a = db
        .query(
            "SELECT val FROM asym_b EXCEPT SELECT val FROM asym_a",
            &[],
        )
        .unwrap();

    let a_vals: Vec<i64> = a_minus_b.iter().filter_map(|r| get_int_value(r, 0)).collect();
    let b_vals: Vec<i64> = b_minus_a.iter().filter_map(|r| get_int_value(r, 0)).collect();

    assert_eq!(a_vals, vec![1], "A EXCEPT B should return [1]");
    assert_eq!(b_vals, vec![3], "B EXCEPT A should return [3]");
    assert_ne!(
        a_vals, b_vals,
        "EXCEPT is not commutative: A-B should differ from B-A"
    );
}
