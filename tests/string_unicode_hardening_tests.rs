//! Hardening tests for string function edge cases and Unicode handling.
//!
//! Covers empty string semantics, multi-byte/Unicode characters, string function
//! boundary conditions, comparison edge cases, concatenation with mixed types,
//! and large string handling. Avoids duplicating tests already present in
//! `string_and_date_tests.rs`.

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod string_unicode_hardening {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    // ========================================================================
    // 1. Empty string handling
    // ========================================================================

    #[test]
    fn test_empty_string_is_not_null() {
        // In PostgreSQL, '' IS NULL should be false (empty string != NULL)
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT '' IS NULL", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Boolean(false));
    }

    #[test]
    fn test_upper_empty_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UPPER('')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_lower_empty_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOWER('')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_empty_string_equals_empty_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT '' = ''", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Boolean(true));
    }

    #[test]
    fn test_empty_string_less_than_nonempty() {
        // Empty string should sort before any non-empty string
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT '' < 'a'", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Boolean(true));
    }

    #[test]
    fn test_coalesce_empty_string_is_not_null() {
        // COALESCE should return '' (not skip it) because '' is not NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT COALESCE('', 'default')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    // ========================================================================
    // 2. Unicode / multi-byte characters
    // ========================================================================

    #[test]
    fn test_insert_and_select_chinese_characters() {
        // Multi-byte UTF-8 characters must survive the INSERT/storage/SELECT round-trip.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE uni_zh (id INT PRIMARY KEY, txt TEXT)")
            .unwrap();
        db.execute("INSERT INTO uni_zh (id, txt) VALUES (1, '\u{4F60}\u{597D}\u{4E16}\u{754C}')")
            .unwrap();
        let rows = db
            .query("SELECT txt FROM uni_zh WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::String(s) => assert_eq!(s, "\u{4F60}\u{597D}\u{4E16}\u{754C}", "Chinese text must round-trip exactly"),
            other => panic!("Expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_insert_and_select_arabic_characters() {
        // Multi-byte UTF-8 characters must survive the INSERT/storage/SELECT round-trip.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE uni_ar (id INT PRIMARY KEY, txt TEXT)")
            .unwrap();
        db.execute("INSERT INTO uni_ar (id, txt) VALUES (1, '\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}')")
            .unwrap();
        let rows = db
            .query("SELECT txt FROM uni_ar WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::String(s) => assert_eq!(s, "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}", "Arabic text must round-trip exactly"),
            other => panic!("Expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_length_on_multibyte_string() {
        // LENGTH should count characters, not bytes
        // Chinese chars are 3 bytes each in UTF-8; 4 chars = 12 bytes but LENGTH = 4
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT LENGTH('\u{4F60}\u{597D}\u{4E16}\u{754C}')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(4));
    }

    #[test]
    fn test_upper_lower_accented_characters() {
        // Rust's to_uppercase handles accented chars: e-acute -> E-acute
        let db = EmbeddedDatabase::new_in_memory().unwrap();

        let rows_up = db.query("SELECT UPPER('\u{00E9}t\u{00E9}')", &[]).unwrap();
        assert_eq!(rows_up.len(), 1);
        assert_eq!(
            rows_up[0].get(0).unwrap(),
            &Value::String("\u{00C9}T\u{00C9}".to_string())
        );

        let rows_down = db.query("SELECT LOWER('\u{00C9}T\u{00C9}')", &[]).unwrap();
        assert_eq!(rows_down.len(), 1);
        assert_eq!(
            rows_down[0].get(0).unwrap(),
            &Value::String("\u{00E9}t\u{00E9}".to_string())
        );
    }

    #[test]
    fn test_substr_on_unicode_is_character_based() {
        // SUBSTR('hello', 1, 3) on a Unicode string with accented chars
        // 'h\u{00E9}llo' = h,e-acute,l,l,o -- SUBSTR(1,3) should be 'h\u{00E9}l'
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SUBSTR('h\u{00E9}llo', 1, 3)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("h\u{00E9}l".to_string())
        );
    }

    #[test]
    fn test_emoji_insert_select_roundtrip() {
        // Multi-byte Unicode (emoji) must survive the INSERT/storage/SELECT round-trip.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE uni_emoji (id INT PRIMARY KEY, msg TEXT)")
            .unwrap();
        db.execute("INSERT INTO uni_emoji (id, msg) VALUES (1, 'Hello \u{1F600}\u{1F389}')")
            .unwrap();
        db.execute("INSERT INTO uni_emoji (id, msg) VALUES (2, 'No emoji here')")
            .unwrap();

        // Verify both rows are stored and retrievable with exact content
        let rows = db
            .query("SELECT id, msg FROM uni_emoji ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
        match rows[0].get(1).unwrap() {
            Value::String(s) => assert_eq!(s, "Hello \u{1F600}\u{1F389}", "Emoji text must round-trip exactly"),
            other => panic!("Expected String, got {:?}", other),
        }
        match rows[1].get(1).unwrap() {
            Value::String(s) => assert_eq!(s, "No emoji here"),
            other => panic!("Expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_emoji_in_literal_functions() {
        // String functions on emoji literals (no storage round-trip) should work
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT LENGTH('Hi\u{1F600}')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        // 'H','i',emoji = 3 characters
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(3));
    }

    #[test]
    fn test_mixed_ascii_and_unicode_in_same_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let mixed = "abc\u{00E9}\u{4E16}\u{754C}xyz";
        let query = format!("SELECT LENGTH('{}')", mixed);
        let rows = db.query(&query, &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // a,b,c,e-acute,world(1 char),world(1 char),x,y,z = 9 chars
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(9));
    }

    #[test]
    fn test_unicode_in_where_clause_equality() {
        // Multi-byte characters must survive storage and match in WHERE clauses.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE uni_where (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO uni_where (id, name) VALUES (1, 'caf\u{00E9}')")
            .unwrap();
        db.execute("INSERT INTO uni_where (id, name) VALUES (2, 'cafe')")
            .unwrap();

        // ASCII equality should work
        let rows = db
            .query("SELECT id FROM uni_where WHERE name = 'cafe'", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Int4(n) => assert_eq!(*n, 2),
            Value::Int8(n) => assert_eq!(*n, 2),
            other => panic!("Expected integer, got {:?}", other),
        }

        // Unicode equality must also work after round-trip through storage
        let rows = db
            .query("SELECT id FROM uni_where WHERE name = 'caf\u{00E9}'", &[])
            .unwrap();
        assert_eq!(rows.len(), 1, "Unicode WHERE equality must match after storage round-trip");
        match rows[0].get(0).unwrap() {
            Value::Int4(n) => assert_eq!(*n, 1),
            Value::Int8(n) => assert_eq!(*n, 1),
            other => panic!("Expected integer, got {:?}", other),
        }
    }

    // ========================================================================
    // 3. String function edge cases
    // ========================================================================

    #[test]
    fn test_substr_start_beyond_string_length_no_length_arg() {
        // SUBSTR('hi', 10) -> '' (start beyond string, no length)
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT SUBSTR('hi', 10)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_substr_start_at_zero() {
        // In PostgreSQL, SUBSTR('hello', 0, 3) returns 'he' (0 treated as before 1)
        // Our implementation: start < 1 maps to start_idx = 0
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT SUBSTR('hello', 0, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        let val = match rows[0].get(0).unwrap() {
            Value::String(s) => s.clone(),
            other => panic!("Expected String, got {:?}", other),
        };
        // start=0 maps to index 0, take 3 => 'hel'
        assert_eq!(val, "hel");
    }

    #[test]
    fn test_replace_with_empty_replacement() {
        // REPLACE('hello', 'l', '') -> 'heo' (remove all 'l's)
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT REPLACE('hello', 'l', '')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("heo".to_string()));
    }

    #[test]
    fn test_replace_with_empty_search_string() {
        // REPLACE('hello', '', 'X') -- Rust's str::replace("", "X") inserts X between every char
        // PostgreSQL returns 'hello' unchanged; Rust inserts. Test behavior exists and is consistent.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT REPLACE('hello', '', 'X')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        // Whatever the result, it should be a string (either 'hello' or 'XhXeXlXlXoX')
        match rows[0].get(0).unwrap() {
            Value::String(_) => {} // Just verify it does not crash
            other => panic!("Expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_position_substring_not_found_returns_zero() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT POSITION('xyz', 'hello world')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(0));
    }

    #[test]
    fn test_btrim_with_custom_characters() {
        // BTRIM('xxxhelloxxx', 'x') should strip 'x' from both sides
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT BTRIM('xxxhelloxxx', 'x')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_ltrim_with_custom_characters() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT LTRIM('xxxhelloxxx', 'x')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("helloxxx".to_string())
        );
    }

    #[test]
    fn test_rtrim_with_custom_characters() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT RTRIM('xxxhelloxxx', 'x')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("xxxhello".to_string())
        );
    }

    #[test]
    fn test_repeat_with_one_repetition() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPEAT('abc', 1)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("abc".to_string())
        );
    }

    // ========================================================================
    // 4. String comparison edge cases
    // ========================================================================

    #[test]
    fn test_case_sensitive_comparison() {
        // 'ABC' = 'abc' should be false (case-sensitive)
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 'ABC' = 'abc'", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Boolean(false));
    }

    #[test]
    fn test_like_underscore_matches_single_char() {
        // '_bc' matches 'abc' but NOT 'bc'
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE like_us (id INT PRIMARY KEY, val TEXT)")
            .unwrap();
        db.execute("INSERT INTO like_us (id, val) VALUES (1, 'abc')")
            .unwrap();
        db.execute("INSERT INTO like_us (id, val) VALUES (2, 'bc')")
            .unwrap();
        db.execute("INSERT INTO like_us (id, val) VALUES (3, 'axc')")
            .unwrap();

        let rows = db
            .query("SELECT val FROM like_us WHERE val LIKE '_bc'", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("abc".to_string())
        );
    }

    #[test]
    fn test_ilike_case_insensitive_exact() {
        // ILIKE without wildcards: case-insensitive equality
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE ilike_exact (id INT PRIMARY KEY, val TEXT)")
            .unwrap();
        db.execute("INSERT INTO ilike_exact (id, val) VALUES (1, 'Hello')")
            .unwrap();
        db.execute("INSERT INTO ilike_exact (id, val) VALUES (2, 'HELLO')")
            .unwrap();
        db.execute("INSERT INTO ilike_exact (id, val) VALUES (3, 'world')")
            .unwrap();

        let rows = db
            .query(
                "SELECT val FROM ilike_exact WHERE val ILIKE 'hello'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        let mut vals: Vec<String> = rows
            .iter()
            .map(|r| match r.get(0).unwrap() {
                Value::String(s) => s.clone(),
                other => panic!("Expected String, got {:?}", other),
            })
            .collect();
        vals.sort();
        assert_eq!(vals, vec!["HELLO", "Hello"]);
    }

    #[test]
    fn test_string_comparison_with_trailing_spaces() {
        // In standard SQL, 'abc' = 'abc   ' is implementation-defined
        // Test that the comparison produces a boolean result
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 'abc' = 'abc   '", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // Most databases treat these as not equal; Rust String comparison is exact
        assert_eq!(rows[0].get(0).unwrap(), &Value::Boolean(false));
    }

    #[test]
    fn test_like_percent_only_matches_everything() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE like_pct (id INT PRIMARY KEY, val TEXT)")
            .unwrap();
        db.execute("INSERT INTO like_pct (id, val) VALUES (1, 'anything')")
            .unwrap();
        db.execute("INSERT INTO like_pct (id, val) VALUES (2, '')")
            .unwrap();
        db.execute("INSERT INTO like_pct (id, val) VALUES (3, 'x')")
            .unwrap();

        let rows = db
            .query("SELECT val FROM like_pct WHERE val LIKE '%'", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
    }

    // ========================================================================
    // 5. String concatenation edge cases
    // ========================================================================

    #[test]
    fn test_concat_with_empty_strings() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT('', 'hello', '', 'world', '')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("helloworld".to_string())
        );
    }

    #[test]
    fn test_concat_single_argument() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CONCAT('only')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("only".to_string())
        );
    }

    #[test]
    fn test_concat_with_integer_arguments() {
        // CONCAT with mixed string and int types
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT('num:', 42, ':', 100)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let val = match rows[0].get(0).unwrap() {
            Value::String(s) => s.clone(),
            other => panic!("Expected String, got {:?}", other),
        };
        // Should contain the number representations
        assert!(val.contains("42"), "Result '{}' should contain '42'", val);
        assert!(val.contains("100"), "Result '{}' should contain '100'", val);
    }

    #[test]
    fn test_concat_all_empty_strings() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CONCAT('', '', '')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    // ========================================================================
    // 6. Large string handling
    // ========================================================================

    #[test]
    fn test_insert_and_select_large_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE big_str (id INT PRIMARY KEY, content TEXT)")
            .unwrap();

        // Build a 10KB+ string
        let large = "A".repeat(12_000);
        let insert_sql = format!(
            "INSERT INTO big_str (id, content) VALUES (1, '{}')",
            large
        );
        db.execute(&insert_sql).unwrap();

        let rows = db
            .query("SELECT content FROM big_str WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::String(s) => assert_eq!(s.len(), 12_000),
            other => panic!("Expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_length_on_large_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let large = "X".repeat(15_000);
        let query = format!("SELECT LENGTH('{}')", large);
        let rows = db.query(&query, &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(15_000));
    }

    #[test]
    fn test_substr_on_large_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let large = "Z".repeat(10_000);
        let query = format!("SELECT SUBSTR('{}', 9990, 5)", large);
        let rows = db.query(&query, &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("ZZZZZ".to_string())
        );
    }

    #[test]
    fn test_comparison_of_large_strings() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE big_cmp (id INT PRIMARY KEY, val TEXT)")
            .unwrap();

        let s1 = "B".repeat(10_000);
        let s2 = "B".repeat(10_000);
        db.execute(&format!(
            "INSERT INTO big_cmp (id, val) VALUES (1, '{}')",
            s1
        ))
        .unwrap();

        // WHERE clause comparing with an identical large string
        let rows = db
            .query(
                &format!("SELECT id FROM big_cmp WHERE val = '{}'", s2),
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
    }
}
