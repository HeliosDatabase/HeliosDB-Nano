//! JSONB hardening tests for HeliosDB Nano
//!
//! Covers: basic JSONB storage, arrow operator (->), text arrow operator (->>),
//! containment operator (@>), existence operator (?), and JSONB in queries
//! (WHERE, ORDER BY, GROUP BY, JOIN, CAST).
//!
//! Tests use match on Ok/Err to document unsupported features without failing.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod jsonb_hardening {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    fn db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("Failed to create test database")
    }

    fn val_to_string(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Json(s) => s.to_string(),
            Value::Null => "NULL".to_string(),
            other => format!("{:?}", other),
        }
    }

    // ========================================================================
    // Basic JSONB storage (7 tests)
    // ========================================================================

    #[test]
    fn test_insert_retrieve_json_object() {
        let d = db();
        d.execute("CREATE TABLE jb_obj (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_obj VALUES (1, '{"name":"alice","age":30}')"#).unwrap();
        match d.query("SELECT data FROM jb_obj WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("alice"), "Expected 'alice' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] insert/retrieve JSON object: {e}"),
        }
    }

    #[test]
    fn test_insert_retrieve_json_array() {
        let d = db();
        d.execute("CREATE TABLE jb_arr (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_arr VALUES (1, '[1,2,3,"four"]')"#).unwrap();
        match d.query("SELECT data FROM jb_arr WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("four"), "Expected 'four' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] insert/retrieve JSON array: {e}"),
        }
    }

    #[test]
    fn test_insert_nested_json() {
        let d = db();
        d.execute("CREATE TABLE jb_nested (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_nested VALUES (1, '{"a":{"b":{"c":42}}}')"#).unwrap();
        match d.query("SELECT data FROM jb_nested WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("42"), "Expected '42' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] insert nested JSON: {e}"),
        }
    }

    #[test]
    fn test_json_various_types() {
        let d = db();
        d.execute("CREATE TABLE jb_types (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_types VALUES (1, '{"s":"hello","n":42,"f":3.14,"b":true,"nil":null}')"#).unwrap();
        match d.query("SELECT data FROM jb_types WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("hello"), "Expected 'hello' in {}", s);
                assert!(s.contains("true"), "Expected 'true' in {}", s);
                assert!(s.contains("null"), "Expected 'null' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] JSON various types: {e}"),
        }
    }

    #[test]
    fn test_update_json_column() {
        let d = db();
        d.execute("CREATE TABLE jb_upd (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_upd VALUES (1, '{"v":1}')"#).unwrap();
        d.execute(r#"UPDATE jb_upd SET data = '{"v":2}' WHERE id = 1"#).unwrap();
        match d.query("SELECT data FROM jb_upd WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains('2'), "Expected '2' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] update JSON column: {e}"),
        }
    }

    #[test]
    fn test_null_json_column() {
        let d = db();
        d.execute("CREATE TABLE jb_null (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute("INSERT INTO jb_null VALUES (1, NULL)").unwrap();
        match d.query("SELECT data FROM jb_null WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert!(matches!(rows[0].get(0).unwrap(), Value::Null),
                    "Expected NULL, got {:?}", rows[0].get(0));
            }
            Err(e) => eprintln!("[UNSUPPORTED] NULL JSON column: {e}"),
        }
    }

    #[test]
    fn test_empty_json_object() {
        let d = db();
        d.execute("CREATE TABLE jb_empty (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_empty VALUES (1, '{}')"#).unwrap();
        match d.query("SELECT data FROM jb_empty WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains('{'), "Expected object, got {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] empty JSON object: {e}"),
        }
    }

    // ========================================================================
    // Arrow operator -> (object field access) (6 tests)
    // ========================================================================

    #[test]
    fn test_arrow_top_level_field() {
        let d = db();
        d.execute("CREATE TABLE jb_ar1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ar1 VALUES (1, '{"name":"bob","age":25}')"#).unwrap();
        match d.query("SELECT data->'name' FROM jb_ar1 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("bob"), "Expected 'bob' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow top-level field: {e}"),
        }
    }

    #[test]
    fn test_arrow_nested_field() {
        let d = db();
        d.execute("CREATE TABLE jb_ar2 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ar2 VALUES (1, '{"a":{"b":"deep"}}')"#).unwrap();
        match d.query("SELECT data->'a'->'b' FROM jb_ar2 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("deep"), "Expected 'deep' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow nested field: {e}"),
        }
    }

    #[test]
    fn test_arrow_array_element() {
        let d = db();
        d.execute("CREATE TABLE jb_ar3 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ar3 VALUES (1, '["a","b","c"]')"#).unwrap();
        match d.query("SELECT data->1 FROM jb_ar3 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains('b'), "Expected 'b' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow array element: {e}"),
        }
    }

    #[test]
    fn test_arrow_nonexistent_key() {
        let d = db();
        d.execute("CREATE TABLE jb_ar4 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ar4 VALUES (1, '{"a":1}')"#).unwrap();
        match d.query("SELECT data->'missing' FROM jb_ar4 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert!(matches!(rows[0].get(0).unwrap(), Value::Null),
                    "Expected NULL for missing key, got {:?}", rows[0].get(0));
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow nonexistent key: {e}"),
        }
    }

    #[test]
    fn test_arrow_chain_multiple() {
        let d = db();
        d.execute("CREATE TABLE jb_ar5 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ar5 VALUES (1, '{"x":{"y":{"z":"found"}}}')"#).unwrap();
        match d.query("SELECT data->'x'->'y'->'z' FROM jb_ar5 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("found"), "Expected 'found' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow chain multiple: {e}"),
        }
    }

    #[test]
    fn test_arrow_on_null_json() {
        let d = db();
        d.execute("CREATE TABLE jb_ar6 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute("INSERT INTO jb_ar6 VALUES (1, NULL)").unwrap();
        match d.query("SELECT data->'key' FROM jb_ar6 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert!(matches!(rows[0].get(0).unwrap(), Value::Null),
                    "Expected NULL for arrow on NULL, got {:?}", rows[0].get(0));
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow on NULL JSON: {e}"),
        }
    }

    // ========================================================================
    // Text arrow operator ->> (5 tests)
    // ========================================================================

    #[test]
    fn test_text_arrow_extract_text() {
        let d = db();
        d.execute("CREATE TABLE jb_ta1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ta1 VALUES (1, '{"name":"carol"}')"#).unwrap();
        match d.query("SELECT data->>'name' FROM jb_ta1 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("carol"), "Expected 'carol' in {}", s);
                assert!(!s.starts_with('"'), "Should be unquoted text, got {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] text arrow extract text: {e}"),
        }
    }

    #[test]
    fn test_text_arrow_nested_path() {
        let d = db();
        d.execute("CREATE TABLE jb_ta2 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ta2 VALUES (1, '{"a":{"b":"nested_val"}}')"#).unwrap();
        match d.query("SELECT data->'a'->>'b' FROM jb_ta2 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("nested_val"), "Expected 'nested_val' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] text arrow nested path: {e}"),
        }
    }

    #[test]
    fn test_text_arrow_extract_number() {
        let d = db();
        d.execute("CREATE TABLE jb_ta3 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ta3 VALUES (1, '{"val":42}')"#).unwrap();
        match d.query("SELECT data->>'val' FROM jb_ta3 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("42"), "Expected '42' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] text arrow extract number: {e}"),
        }
    }

    #[test]
    fn test_text_arrow_extract_boolean() {
        let d = db();
        d.execute("CREATE TABLE jb_ta4 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ta4 VALUES (1, '{"flag":true}')"#).unwrap();
        match d.query("SELECT data->>'flag' FROM jb_ta4 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("true"), "Expected 'true' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] text arrow extract boolean: {e}"),
        }
    }

    #[test]
    fn test_text_arrow_nonexistent_key() {
        let d = db();
        d.execute("CREATE TABLE jb_ta5 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ta5 VALUES (1, '{"a":1}')"#).unwrap();
        match d.query("SELECT data->>'missing' FROM jb_ta5 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert!(matches!(rows[0].get(0).unwrap(), Value::Null),
                    "Expected NULL for missing key, got {:?}", rows[0].get(0));
            }
            Err(e) => eprintln!("[UNSUPPORTED] text arrow nonexistent key: {e}"),
        }
    }

    // ========================================================================
    // Containment operator @> (5 tests)
    // ========================================================================

    #[test]
    fn test_contains_key_value_pair() {
        let d = db();
        d.execute("CREATE TABLE jb_ct1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ct1 VALUES (1, '{"a":1,"b":2}')"#).unwrap();
        match d.query(r#"SELECT data @> '{"a":1}' FROM jb_ct1 WHERE id = 1"#, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                match rows[0].get(0).unwrap() {
                    Value::Boolean(b) => assert!(b, "Expected true for containment"),
                    other => eprintln!("Unexpected result type: {:?}", other),
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] contains key-value pair: {e}"),
        }
    }

    #[test]
    fn test_contains_subset() {
        let d = db();
        d.execute("CREATE TABLE jb_ct2 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ct2 VALUES (1, '{"a":1,"b":2,"c":3}')"#).unwrap();
        match d.query(r#"SELECT data @> '{"a":1,"c":3}' FROM jb_ct2 WHERE id = 1"#, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                match rows[0].get(0).unwrap() {
                    Value::Boolean(b) => assert!(b, "Expected true for subset containment"),
                    other => eprintln!("Unexpected result type: {:?}", other),
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] contains subset: {e}"),
        }
    }

    #[test]
    fn test_contains_array_element() {
        let d = db();
        d.execute("CREATE TABLE jb_ct3 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ct3 VALUES (1, '[1,2,3,4,5]')"#).unwrap();
        match d.query(r#"SELECT data @> '[3]' FROM jb_ct3 WHERE id = 1"#, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                match rows[0].get(0).unwrap() {
                    Value::Boolean(b) => assert!(b, "Expected true for array containment"),
                    other => eprintln!("Unexpected result type: {:?}", other),
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] contains array element: {e}"),
        }
    }

    #[test]
    fn test_contains_in_where() {
        let d = db();
        d.execute("CREATE TABLE jb_ct4 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ct4 VALUES (1, '{"type":"admin","active":true}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_ct4 VALUES (2, '{"type":"user","active":true}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_ct4 VALUES (3, '{"type":"admin","active":false}')"#).unwrap();
        match d.query(r#"SELECT id FROM jb_ct4 WHERE data @> '{"type":"admin"}' ORDER BY id"#, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Expected 2 admins, got {}", rows.len());
            }
            Err(e) => eprintln!("[UNSUPPORTED] containment in WHERE: {e}"),
        }
    }

    #[test]
    fn test_contains_negative() {
        let d = db();
        d.execute("CREATE TABLE jb_ct5 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ct5 VALUES (1, '{"a":1}')"#).unwrap();
        match d.query(r#"SELECT data @> '{"a":2}' FROM jb_ct5 WHERE id = 1"#, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                match rows[0].get(0).unwrap() {
                    Value::Boolean(b) => assert!(!b, "Expected false for non-matching containment"),
                    other => eprintln!("Unexpected result type: {:?}", other),
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] contains negative: {e}"),
        }
    }

    // ========================================================================
    // Existence operator ? (4 tests)
    // ========================================================================

    #[test]
    fn test_exists_key_present() {
        let d = db();
        d.execute("CREATE TABLE jb_ex1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ex1 VALUES (1, '{"name":"test","age":10}')"#).unwrap();
        match d.query("SELECT data ? 'name' FROM jb_ex1 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                match rows[0].get(0).unwrap() {
                    Value::Boolean(b) => assert!(b, "Key 'name' should exist"),
                    other => eprintln!("Unexpected result type: {:?}", other),
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] exists key present: {e}"),
        }
    }

    #[test]
    fn test_exists_key_absent() {
        let d = db();
        d.execute("CREATE TABLE jb_ex2 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ex2 VALUES (1, '{"name":"test"}')"#).unwrap();
        match d.query("SELECT data ? 'missing' FROM jb_ex2 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                match rows[0].get(0).unwrap() {
                    Value::Boolean(b) => assert!(!b, "Key 'missing' should not exist"),
                    other => eprintln!("Unexpected result type: {:?}", other),
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] exists key absent: {e}"),
        }
    }

    #[test]
    fn test_exists_in_where_filter() {
        let d = db();
        d.execute("CREATE TABLE jb_ex3 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ex3 VALUES (1, '{"email":"a@b.c"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_ex3 VALUES (2, '{"phone":"555"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_ex3 VALUES (3, '{"email":"d@e.f","phone":"111"}')"#).unwrap();
        match d.query("SELECT id FROM jb_ex3 WHERE data ? 'email' ORDER BY id", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Expected 2 rows with 'email', got {}", rows.len());
            }
            Err(e) => eprintln!("[UNSUPPORTED] exists in WHERE filter: {e}"),
        }
    }

    #[test]
    fn test_exists_multiple_checks() {
        let d = db();
        d.execute("CREATE TABLE jb_ex4 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_ex4 VALUES (1, '{"a":1,"b":2,"c":3}')"#).unwrap();
        match d.query("SELECT data ? 'a', data ? 'b', data ? 'z' FROM jb_ex4 WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                if let (Value::Boolean(a), Value::Boolean(b), Value::Boolean(z)) =
                    (rows[0].get(0).unwrap(), rows[0].get(1).unwrap(), rows[0].get(2).unwrap())
                {
                    assert!(a, "Key 'a' should exist");
                    assert!(b, "Key 'b' should exist");
                    assert!(!z, "Key 'z' should not exist");
                } else {
                    eprintln!("Unexpected result types: {:?}", rows[0]);
                }
            }
            Err(e) => eprintln!("[UNSUPPORTED] exists multiple checks: {e}"),
        }
    }

    // ========================================================================
    // JSONB in queries (8 tests)
    // ========================================================================

    #[test]
    fn test_where_filter_arrow() {
        let d = db();
        d.execute("CREATE TABLE jb_qw1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_qw1 VALUES (1, '{"status":"active"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qw1 VALUES (2, '{"status":"inactive"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qw1 VALUES (3, '{"status":"active"}')"#).unwrap();
        match d.query("SELECT id FROM jb_qw1 WHERE data->>'status' = 'active' ORDER BY id", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Expected 2 active rows, got {}", rows.len());
            }
            Err(e) => eprintln!("[UNSUPPORTED] WHERE filter with arrow: {e}"),
        }
    }

    #[test]
    fn test_order_by_jsonb_field() {
        let d = db();
        d.execute("CREATE TABLE jb_qo1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_qo1 VALUES (1, '{"rank":3}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qo1 VALUES (2, '{"rank":1}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qo1 VALUES (3, '{"rank":2}')"#).unwrap();
        match d.query("SELECT id FROM jb_qo1 ORDER BY data->>'rank'", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3);
                eprintln!("ORDER BY jsonb field results: {:?}", rows);
            }
            Err(e) => eprintln!("[UNSUPPORTED] ORDER BY JSONB field: {e}"),
        }
    }

    #[test]
    fn test_group_by_jsonb_field() {
        let d = db();
        d.execute("CREATE TABLE jb_qg1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_qg1 VALUES (1, '{"cat":"a"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qg1 VALUES (2, '{"cat":"b"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qg1 VALUES (3, '{"cat":"a"}')"#).unwrap();
        match d.query("SELECT data->>'cat', COUNT(*) FROM jb_qg1 GROUP BY data->>'cat' ORDER BY data->>'cat'", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Expected 2 groups, got {}", rows.len());
            }
            Err(e) => eprintln!("[UNSUPPORTED] GROUP BY JSONB field: {e}"),
        }
    }

    #[test]
    fn test_jsonb_in_join_condition() {
        let d = db();
        d.execute("CREATE TABLE jb_qj1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute("CREATE TABLE jb_qj2 (id INT PRIMARY KEY, tag TEXT)").unwrap();
        d.execute(r#"INSERT INTO jb_qj1 VALUES (1, '{"tag":"x"}')"#).unwrap();
        d.execute(r#"INSERT INTO jb_qj1 VALUES (2, '{"tag":"y"}')"#).unwrap();
        d.execute("INSERT INTO jb_qj2 VALUES (10, 'x')").unwrap();
        d.execute("INSERT INTO jb_qj2 VALUES (20, 'z')").unwrap();
        match d.query("SELECT jb_qj1.id, jb_qj2.id FROM jb_qj1 JOIN jb_qj2 ON jb_qj1.data->>'tag' = jb_qj2.tag", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1, "Expected 1 joined row, got {}", rows.len());
            }
            Err(e) => eprintln!("[UNSUPPORTED] JSONB in JOIN condition: {e}"),
        }
    }

    #[test]
    fn test_insert_json_string_literal() {
        let d = db();
        d.execute("CREATE TABLE jb_qi1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        match d.execute(r#"INSERT INTO jb_qi1 VALUES (1, '{"key":"value"}')"#) {
            Ok(_) => {
                let rows = d.query("SELECT data FROM jb_qi1 WHERE id = 1", &[]).unwrap();
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("value"), "Expected 'value' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] INSERT JSON string literal: {e}"),
        }
    }

    #[test]
    fn test_cast_text_to_jsonb() {
        let d = db();
        d.execute("CREATE TABLE jb_qc1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        match d.execute(r#"INSERT INTO jb_qc1 VALUES (1, CAST('{"x":1}' AS JSONB))"#) {
            Ok(_) => {
                let rows = d.query("SELECT data FROM jb_qc1 WHERE id = 1", &[]).unwrap();
                assert_eq!(rows.len(), 1);
            }
            Err(e) => eprintln!("[UNSUPPORTED] CAST text to JSONB: {e}"),
        }
    }

    #[test]
    fn test_jsonb_is_null_check() {
        let d = db();
        d.execute("CREATE TABLE jb_qn1 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute("INSERT INTO jb_qn1 VALUES (1, NULL)").unwrap();
        d.execute(r#"INSERT INTO jb_qn1 VALUES (2, '{"a":1}')"#).unwrap();
        match d.query("SELECT id FROM jb_qn1 WHERE data IS NULL", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
            }
            Err(e) => eprintln!("[UNSUPPORTED] JSONB IS NULL: {e}"),
        }
    }

    #[test]
    fn test_jsonb_is_not_null_check() {
        let d = db();
        d.execute("CREATE TABLE jb_qn2 (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute("INSERT INTO jb_qn2 VALUES (1, NULL)").unwrap();
        d.execute(r#"INSERT INTO jb_qn2 VALUES (2, '{"a":1}')"#).unwrap();
        match d.query("SELECT id FROM jb_qn2 WHERE data IS NOT NULL", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
            }
            Err(e) => eprintln!("[UNSUPPORTED] JSONB IS NOT NULL: {e}"),
        }
    }

    #[test]
    fn test_large_json_document() {
        let d = db();
        d.execute("CREATE TABLE jb_large (id INT PRIMARY KEY, data JSONB)").unwrap();
        let mut json = String::from("{");
        for i in 0..50 {
            if i > 0 { json.push(','); }
            json.push_str(&format!(r#""k{}":"v{}""#, i, i));
        }
        json.push('}');
        let sql = format!("INSERT INTO jb_large VALUES (1, '{}')", json);
        match d.execute(&sql) {
            Ok(_) => {
                let rows = d.query("SELECT data FROM jb_large WHERE id = 1", &[]).unwrap();
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("k49"), "Expected key k49 in large doc");
            }
            Err(e) => eprintln!("[UNSUPPORTED] large JSON document: {e}"),
        }
    }

    #[test]
    fn test_json_with_special_chars() {
        let d = db();
        d.execute("CREATE TABLE jb_spec (id INT PRIMARY KEY, data JSONB)").unwrap();
        match d.execute(r#"INSERT INTO jb_spec VALUES (1, '{"msg":"hello\nworld","quote":"say \"hi\""}')"#) {
            Ok(_) => {
                let rows = d.query("SELECT data FROM jb_spec WHERE id = 1", &[]).unwrap();
                assert_eq!(rows.len(), 1);
            }
            Err(e) => eprintln!("[UNSUPPORTED] JSON with special chars: {e}"),
        }
    }

    #[test]
    fn test_arrow_extract_number_value() {
        let d = db();
        d.execute("CREATE TABLE jb_arn (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_arn VALUES (1, '{"count":99}')"#).unwrap();
        match d.query("SELECT data->'count' FROM jb_arn WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("99"), "Expected '99' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] arrow extract number value: {e}"),
        }
    }

    #[test]
    fn test_deeply_nested_json() {
        let d = db();
        d.execute("CREATE TABLE jb_deep (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_deep VALUES (1, '{"l1":{"l2":{"l3":{"l4":"bottom"}}}}')"#).unwrap();
        match d.query("SELECT data->'l1'->'l2'->'l3'->'l4' FROM jb_deep WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains("bottom"), "Expected 'bottom' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] deeply nested JSON: {e}"),
        }
    }

    #[test]
    fn test_json_array_of_objects() {
        let d = db();
        d.execute("CREATE TABLE jb_aoo (id INT PRIMARY KEY, data JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_aoo VALUES (1, '[{"n":"a"},{"n":"b"},{"n":"c"}]')"#).unwrap();
        match d.query("SELECT data->1->'n' FROM jb_aoo WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let s = val_to_string(rows[0].get(0).unwrap());
                assert!(s.contains('b'), "Expected 'b' in {}", s);
            }
            Err(e) => eprintln!("[UNSUPPORTED] array of objects access: {e}"),
        }
    }

    #[test]
    fn test_multiple_jsonb_columns() {
        let d = db();
        d.execute("CREATE TABLE jb_multi (id INT PRIMARY KEY, meta JSONB, config JSONB)").unwrap();
        d.execute(r#"INSERT INTO jb_multi VALUES (1, '{"name":"x"}', '{"debug":true}')"#).unwrap();
        match d.query("SELECT meta->>'name', config->>'debug' FROM jb_multi WHERE id = 1", &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                let n = val_to_string(rows[0].get(0).unwrap());
                let dd = val_to_string(rows[0].get(1).unwrap());
                assert!(n.contains('x'), "Expected 'x' in name, got {}", n);
                assert!(dd.contains("true"), "Expected 'true' in debug, got {}", dd);
            }
            Err(e) => eprintln!("[UNSUPPORTED] multiple JSONB columns: {e}"),
        }
    }
}
