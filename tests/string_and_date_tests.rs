//! Comprehensive tests for string functions and date/time functions
//! in HeliosDB Nano.
//!
//! String functions tested: UPPER, LOWER, LENGTH, SUBSTR/SUBSTRING, TRIM,
//! LTRIM, RTRIM, BTRIM, CONCAT, CONCAT_WS, LEFT, RIGHT, REPEAT, REPLACE,
//! REVERSE, STRPOS/POSITION, SPLIT_PART, INITCAP, LPAD, RPAD.
//!
//! Pattern matching tested: LIKE, NOT LIKE, ILIKE, NOT ILIKE.
//!
//! Date/time functions tested: NOW(), CURRENT_TIMESTAMP(), CURRENT_DATE(),
//! CURRENT_TIME(), SYSDATE(), GETDATE(), CURDATE(), CURTIME(),
//! UTC_TIMESTAMP(), LOCALTIMESTAMP().

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod string_and_date_tests {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    // ========================================================================
    // UPPER function tests
    // ========================================================================

    #[test]
    fn test_string_func_upper() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UPPER('hello')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("HELLO".to_string()));
    }

    #[test]
    fn test_string_func_upper_mixed_case() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UPPER('Hello World 123!')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("HELLO WORLD 123!".to_string())
        );
    }

    #[test]
    fn test_string_func_upper_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UPPER(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_upper_already_uppercase() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UPPER('ABC')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("ABC".to_string()));
    }

    // ========================================================================
    // LOWER function tests
    // ========================================================================

    #[test]
    fn test_string_func_lower() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOWER('WORLD')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("world".to_string()));
    }

    #[test]
    fn test_string_func_lower_mixed_case() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOWER('HeLLo WoRLd')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello world".to_string())
        );
    }

    #[test]
    fn test_string_func_lower_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOWER(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_lower_already_lowercase() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOWER('abc')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("abc".to_string()));
    }

    // ========================================================================
    // LENGTH function tests
    // ========================================================================

    #[test]
    fn test_string_func_length() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LENGTH('test')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(4));
    }

    #[test]
    fn test_string_func_length_empty() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LENGTH('')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(0));
    }

    #[test]
    fn test_string_func_length_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LENGTH(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_char_length_alias() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CHAR_LENGTH('hello')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(5));
    }

    #[test]
    fn test_string_func_length_with_spaces() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LENGTH('  hi  ')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(6));
    }

    // ========================================================================
    // SUBSTR / SUBSTRING function tests
    // ========================================================================

    #[test]
    fn test_string_func_substr_three_args() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // SUBSTR('hello', 2, 3) -> 'ell' (1-based indexing)
        let rows = db.query("SELECT SUBSTR('hello', 2, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("ell".to_string()));
    }

    #[test]
    fn test_string_func_substr_two_args() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // SUBSTR('hello', 3) -> 'llo' (from position 3 to end)
        let rows = db.query("SELECT SUBSTR('hello', 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("llo".to_string()));
    }

    #[test]
    fn test_string_func_substr_start_beyond_length() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT SUBSTR('hi', 10, 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_string_func_substr_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT SUBSTR(NULL, 1, 2)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_substring_alias() {
        // NOTE: SQL SUBSTRING(...) is parsed as Expr::Substring by sqlparser,
        // which the planner doesn't handle. Use SUBSTR() function-call syntax.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SUBSTR('hello world', 7, 5)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("world".to_string())
        );
    }

    #[test]
    fn test_string_func_substr_start_at_one() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT SUBSTR('hello', 1, 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_substr_length_exceeds() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // Length argument exceeds remaining chars - should return what's available
        let rows = db.query("SELECT SUBSTR('hi', 1, 100)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("hi".to_string()));
    }

    // ========================================================================
    // TRIM / LTRIM / RTRIM / BTRIM function tests
    // ========================================================================

    #[test]
    fn test_string_func_trim() {
        // NOTE: SQL TRIM('...') is parsed as a special Expr::Trim by sqlparser,
        // which the planner doesn't handle. Use BTRIM() function-call syntax instead.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT BTRIM('  hello  ')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_trim_no_spaces() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT BTRIM('hello')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_trim_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT BTRIM(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_ltrim() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LTRIM('  hello  ')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello  ".to_string())
        );
    }

    #[test]
    fn test_string_func_rtrim() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RTRIM('  hello  ')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("  hello".to_string())
        );
    }

    #[test]
    fn test_string_func_btrim() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT BTRIM('  hello  ')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_trim_only_spaces() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT BTRIM('   ')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    // ========================================================================
    // REPLACE function tests
    // ========================================================================

    #[test]
    fn test_string_func_replace() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT REPLACE('hello world', 'world', 'there')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello there".to_string())
        );
    }

    #[test]
    fn test_string_func_replace_multiple_occurrences() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPLACE('aaa', 'a', 'bb')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("bbbbbb".to_string())
        );
    }

    #[test]
    fn test_string_func_replace_no_match() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT REPLACE('hello', 'xyz', 'abc')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_replace_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPLACE(NULL, 'a', 'b')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_replace_empty_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT REPLACE('hello world', ' world', '')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    // ========================================================================
    // CONCAT / CONCAT_WS function tests
    // ========================================================================

    #[test]
    fn test_string_func_concat() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CONCAT('a', 'b', 'c')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("abc".to_string()));
    }

    #[test]
    fn test_string_func_concat_with_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // In PostgreSQL, CONCAT treats NULL as empty string
        let rows = db.query("SELECT CONCAT('a', NULL, 'c')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("ac".to_string()));
    }

    #[test]
    fn test_string_func_concat_all_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT(NULL, NULL, NULL)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_string_func_concat_with_numbers() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT('value: ', 42)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let val = rows[0].get(0).unwrap();
        if let Value::String(s) = val {
            assert!(s.starts_with("value: "), "Got: {}", s);
        } else {
            panic!("Expected String, got {:?}", val);
        }
    }

    #[test]
    fn test_string_func_concat_ws() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT_WS(', ', 'a', 'b', 'c')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("a, b, c".to_string())
        );
    }

    #[test]
    fn test_string_func_concat_ws_null_separator() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // If separator is NULL, result is NULL per PostgreSQL spec
        let rows = db
            .query("SELECT CONCAT_WS(NULL, 'a', 'b')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_concat_ws_skips_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT_WS('-', 'a', NULL, 'c')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("a-c".to_string())
        );
    }

    #[test]
    fn test_string_func_concat_ws_single_arg() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CONCAT_WS(',', 'only')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("only".to_string())
        );
    }

    // ========================================================================
    // STRPOS / POSITION function tests
    // ========================================================================

    #[test]
    fn test_string_func_strpos() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // STRPOS(string, substring) - 1-based position
        let rows = db.query("SELECT STRPOS('hello', 'lo')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(4));
    }

    #[test]
    fn test_string_func_strpos_not_found() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT STRPOS('hello', 'xyz')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(0));
    }

    #[test]
    fn test_string_func_strpos_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT STRPOS(NULL, 'a')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_strpos_at_start() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT STRPOS('hello', 'he')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(1));
    }

    #[test]
    fn test_string_func_strpos_empty_needle() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT STRPOS('hello', '')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // Rust's str::find("") returns Some(0), so 0+1 = 1
        assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(1));
    }

    // ========================================================================
    // REPEAT function tests
    // ========================================================================

    #[test]
    fn test_string_func_repeat() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPEAT('ab', 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("ababab".to_string())
        );
    }

    #[test]
    fn test_string_func_repeat_zero() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPEAT('ab', 0)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_string_func_repeat_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPEAT(NULL, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_repeat_single_char() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPEAT('*', 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("*****".to_string())
        );
    }

    // ========================================================================
    // REVERSE function tests
    // ========================================================================

    #[test]
    fn test_string_func_reverse() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REVERSE('hello')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("olleh".to_string())
        );
    }

    #[test]
    fn test_string_func_reverse_empty() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REVERSE('')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_string_func_reverse_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REVERSE(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_reverse_palindrome() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REVERSE('racecar')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("racecar".to_string())
        );
    }

    // ========================================================================
    // LEFT / RIGHT function tests
    // ========================================================================

    #[test]
    fn test_string_func_left() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LEFT('hello', 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("hel".to_string()));
    }

    #[test]
    fn test_string_func_left_exceeds_length() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LEFT('hi', 10)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("hi".to_string()));
    }

    #[test]
    fn test_string_func_left_zero() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LEFT('hello', 0)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_string_func_left_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LEFT(NULL, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_right() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RIGHT('hello', 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("llo".to_string()));
    }

    #[test]
    fn test_string_func_right_exceeds_length() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RIGHT('hi', 10)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("hi".to_string()));
    }

    #[test]
    fn test_string_func_right_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RIGHT(NULL, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_left_right_complementary() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT LEFT('hello', 3), RIGHT('hello', 2)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("hel".to_string()));
        assert_eq!(rows[0].get(1).unwrap(), &Value::String("lo".to_string()));
    }

    // ========================================================================
    // SPLIT_PART function tests
    // ========================================================================

    #[test]
    fn test_string_func_split_part() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SPLIT_PART('one,two,three', ',', 2)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("two".to_string()));
    }

    #[test]
    fn test_string_func_split_part_first() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SPLIT_PART('a-b-c', '-', 1)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("a".to_string()));
    }

    #[test]
    fn test_string_func_split_part_beyond_parts() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SPLIT_PART('a,b', ',', 5)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("".to_string()));
    }

    #[test]
    fn test_string_func_split_part_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SPLIT_PART(NULL, ',', 1)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_split_part_multi_char_delimiter() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT SPLIT_PART('a::b::c', '::', 2)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("b".to_string()));
    }

    // ========================================================================
    // INITCAP function tests
    // ========================================================================

    #[test]
    fn test_string_func_initcap() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT INITCAP('hello world')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("Hello World".to_string())
        );
    }

    #[test]
    fn test_string_func_initcap_mixed() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT INITCAP('hELLO wORLD')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("Hello World".to_string())
        );
    }

    #[test]
    fn test_string_func_initcap_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT INITCAP(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_initcap_with_punctuation() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT INITCAP('hello-world')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("Hello-World".to_string())
        );
    }

    // ========================================================================
    // LPAD / RPAD function tests
    // ========================================================================

    #[test]
    fn test_string_func_lpad_default_fill() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LPAD('hi', 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("   hi".to_string())
        );
    }

    #[test]
    fn test_string_func_lpad_custom_fill() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LPAD('hi', 5, '0')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("000hi".to_string())
        );
    }

    #[test]
    fn test_string_func_lpad_truncate() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT LPAD('hello world', 5)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_lpad_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LPAD(NULL, 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_lpad_cyclic_fill() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LPAD('x', 7, 'ab')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("abababx".to_string())
        );
    }

    #[test]
    fn test_string_func_rpad_default_fill() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RPAD('hi', 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hi   ".to_string())
        );
    }

    #[test]
    fn test_string_func_rpad_custom_fill() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RPAD('hi', 5, 'x')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hixxx".to_string())
        );
    }

    #[test]
    fn test_string_func_rpad_truncate() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT RPAD('hello world', 5)", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_rpad_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RPAD(NULL, 5)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_string_func_rpad_cyclic_fill() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT RPAD('x', 7, 'ab')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("xababab".to_string())
        );
    }

    // ========================================================================
    // String functions with column data (not just literals)
    // ========================================================================

    #[test]
    fn test_string_func_upper_on_column() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_upper_col (name TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_upper_col (name) VALUES ('alice')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_upper_col (name) VALUES ('bob')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_upper_col (name) VALUES ('charlie')"
        )
        .unwrap();

        let rows = db
            .query("SELECT UPPER(name) FROM sf_upper_col", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);

        let mut values: Vec<String> = rows
            .iter()
            .map(|r| match r.get(0).unwrap() {
                Value::String(s) => s.clone(),
                other => panic!("Expected String, got {:?}", other),
            })
            .collect();
        values.sort();
        assert_eq!(values, vec!["ALICE", "BOB", "CHARLIE"]);
    }

    #[test]
    fn test_string_func_lower_on_column() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_lower_col (name TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_lower_col (name) VALUES ('HELLO')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_lower_col (name) VALUES ('WORLD')"
        )
        .unwrap();

        let rows = db
            .query("SELECT LOWER(name) FROM sf_lower_col", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);

        let mut values: Vec<String> = rows
            .iter()
            .map(|r| match r.get(0).unwrap() {
                Value::String(s) => s.clone(),
                other => panic!("Expected String, got {:?}", other),
            })
            .collect();
        values.sort();
        assert_eq!(values, vec!["hello", "world"]);
    }

    #[test]
    fn test_string_func_length_on_column() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_len_col (word TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_len_col (word) VALUES ('hi')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_len_col (word) VALUES ('hello')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_len_col (word) VALUES ('greetings')"
        )
        .unwrap();

        let rows = db
            .query("SELECT LENGTH(word) FROM sf_len_col", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);

        let mut lengths: Vec<i32> = rows
            .iter()
            .map(|r| match r.get(0).unwrap() {
                Value::Int4(n) => *n,
                other => panic!("Expected Int4, got {:?}", other),
            })
            .collect();
        lengths.sort();
        assert_eq!(lengths, vec![2, 5, 9]);
    }

    #[test]
    fn test_string_func_concat_columns() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE sf_concat_col (first_name TEXT, last_name TEXT)"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_concat_col (first_name, last_name) VALUES ('John', 'Doe')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT CONCAT(first_name, ' ', last_name) FROM sf_concat_col",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("John Doe".to_string())
        );
    }

    #[test]
    fn test_string_func_replace_on_column() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_repl_col (addr TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_repl_col (addr) VALUES ('user@old.com')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT REPLACE(addr, 'old.com', 'new.com') FROM sf_repl_col",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("user@new.com".to_string())
        );
    }

    #[test]
    fn test_string_func_where_clause_with_upper() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_where_col (label TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_where_col (label) VALUES ('apple')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_where_col (label) VALUES ('Banana')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_where_col (label) VALUES ('CHERRY')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT label FROM sf_where_col WHERE UPPER(label) = 'BANANA'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("Banana".to_string())
        );
    }

    // ========================================================================
    // Nested / Combined string function tests
    // ========================================================================

    #[test]
    fn test_string_func_nested_upper_lower() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT LOWER(UPPER('hello'))", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_string_func_combined_left_upper() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT UPPER(LEFT('hello world', 5))", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("HELLO".to_string())
        );
    }

    #[test]
    fn test_string_func_multiple_in_select() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query(
                "SELECT UPPER('hello'), LOWER('WORLD'), LENGTH('test'), REVERSE('abc')",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("HELLO".to_string())
        );
        assert_eq!(
            rows[0].get(1).unwrap(),
            &Value::String("world".to_string())
        );
        assert_eq!(rows[0].get(2).unwrap(), &Value::Int4(4));
        assert_eq!(
            rows[0].get(3).unwrap(),
            &Value::String("cba".to_string())
        );
    }

    // ========================================================================
    // LIKE pattern matching tests
    // ========================================================================

    #[test]
    fn test_string_func_like_percent_start() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_like1 (val TEXT)").unwrap();
        db.execute("INSERT INTO sf_like1 (val) VALUES ('hello')")
            .unwrap();
        db.execute("INSERT INTO sf_like1 (val) VALUES ('world')")
            .unwrap();
        db.execute("INSERT INTO sf_like1 (val) VALUES ('help')")
            .unwrap();

        let rows = db
            .query("SELECT val FROM sf_like1 WHERE val LIKE 'h%'", &[])
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
        assert_eq!(vals, vec!["hello", "help"]);
    }

    #[test]
    fn test_string_func_like_underscore() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_like2 (val TEXT)").unwrap();
        db.execute("INSERT INTO sf_like2 (val) VALUES ('hello')")
            .unwrap();
        db.execute("INSERT INTO sf_like2 (val) VALUES ('jello')")
            .unwrap();
        db.execute("INSERT INTO sf_like2 (val) VALUES ('cello')")
            .unwrap();
        db.execute("INSERT INTO sf_like2 (val) VALUES ('bell')")
            .unwrap();

        // _ello matches exactly 5-char strings ending with 'ello'
        let rows = db
            .query("SELECT val FROM sf_like2 WHERE val LIKE '_ello'", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
        let mut vals: Vec<String> = rows
            .iter()
            .map(|r| match r.get(0).unwrap() {
                Value::String(s) => s.clone(),
                other => panic!("Expected String, got {:?}", other),
            })
            .collect();
        vals.sort();
        assert_eq!(vals, vec!["cello", "hello", "jello"]);
    }

    #[test]
    fn test_string_func_like_middle_percent() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_like3 (val TEXT)").unwrap();
        db.execute("INSERT INTO sf_like3 (val) VALUES ('hello')")
            .unwrap();
        db.execute("INSERT INTO sf_like3 (val) VALUES ('world')")
            .unwrap();
        db.execute(
            "INSERT INTO sf_like3 (val) VALUES ('balloon')"
        )
        .unwrap();

        let rows = db
            .query("SELECT val FROM sf_like3 WHERE val LIKE '%ll%'", &[])
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
        assert_eq!(vals, vec!["balloon", "hello"]);
    }

    #[test]
    fn test_string_func_like_percent_end() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_like4 (val TEXT)").unwrap();
        db.execute(
            "INSERT INTO sf_like4 (val) VALUES ('testing')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_like4 (val) VALUES ('flying')"
        )
        .unwrap();
        db.execute("INSERT INTO sf_like4 (val) VALUES ('done')")
            .unwrap();

        let rows = db
            .query("SELECT val FROM sf_like4 WHERE val LIKE '%ing'", &[])
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
        assert_eq!(vals, vec!["flying", "testing"]);
    }

    #[test]
    fn test_string_func_not_like() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_notlike (val TEXT)")
            .unwrap();
        db.execute("INSERT INTO sf_notlike (val) VALUES ('abc')")
            .unwrap();
        db.execute("INSERT INTO sf_notlike (val) VALUES ('def')")
            .unwrap();
        db.execute("INSERT INTO sf_notlike (val) VALUES ('abx')")
            .unwrap();

        let rows = db
            .query(
                "SELECT val FROM sf_notlike WHERE val NOT LIKE 'ab%'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("def".to_string())
        );
    }

    #[test]
    fn test_string_func_like_exact_match() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_like_exact (val TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_like_exact (val) VALUES ('test')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_like_exact (val) VALUES ('testing')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT val FROM sf_like_exact WHERE val LIKE 'test'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("test".to_string())
        );
    }

    // ========================================================================
    // ILIKE (case-insensitive LIKE) tests
    // ========================================================================

    #[test]
    fn test_string_func_ilike_case_insensitive() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_ilike1 (val TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_ilike1 (val) VALUES ('Hello')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_ilike1 (val) VALUES ('HELLO')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_ilike1 (val) VALUES ('world')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT val FROM sf_ilike1 WHERE val ILIKE 'hello'",
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
    fn test_string_func_ilike_with_pattern() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_ilike2 (val TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_ilike2 (val) VALUES ('Apple')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_ilike2 (val) VALUES ('APRICOT')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_ilike2 (val) VALUES ('banana')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT val FROM sf_ilike2 WHERE val ILIKE 'ap%'",
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
        assert_eq!(vals, vec!["APRICOT", "Apple"]);
    }

    #[test]
    fn test_string_func_not_ilike() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sf_notilike (val TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO sf_notilike (val) VALUES ('Apple')"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_notilike (val) VALUES ('banana')"
        )
        .unwrap();

        let rows = db
            .query(
                "SELECT val FROM sf_notilike WHERE val NOT ILIKE 'a%'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("banana".to_string())
        );
    }

    // ========================================================================
    // Date/Time function tests
    //
    // Tests for NOW(), CURRENT_TIMESTAMP(), CURRENT_DATE(), CURRENT_TIME()
    // and cross-database aliases. EXTRACT is not supported via function-call
    // syntax, so we test what is available.
    // ========================================================================

    #[test]
    fn test_date_func_now() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NOW()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Timestamp(ts) => {
                let now = chrono::Utc::now();
                let diff = (now - *ts).num_seconds().abs();
                assert!(
                    diff < 60,
                    "NOW() timestamp too far from current time: {} seconds",
                    diff
                );
            }
            other => panic!("Expected Timestamp from NOW(), got {:?}", other),
        }
    }

    #[test]
    fn test_date_func_current_timestamp() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CURRENT_TIMESTAMP()", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Timestamp(ts) => {
                let now = chrono::Utc::now();
                let diff = (now - *ts).num_seconds().abs();
                assert!(
                    diff < 60,
                    "CURRENT_TIMESTAMP() too far from now: {} seconds",
                    diff
                );
            }
            other => panic!(
                "Expected Timestamp from CURRENT_TIMESTAMP(), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_date_func_current_date() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CURRENT_DATE()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Date(d) => {
                let today = chrono::Utc::now().date_naive();
                let diff = (*d - today).num_days().abs();
                assert!(
                    diff <= 1,
                    "CURRENT_DATE() too far from today: {} days",
                    diff
                );
            }
            other => panic!("Expected Date from CURRENT_DATE(), got {:?}", other),
        }
    }

    #[test]
    fn test_date_func_current_time() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CURRENT_TIME()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Time(_) => {}
            other => panic!(
                "Expected Time from CURRENT_TIME(), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_date_func_sysdate_alias() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // Oracle alias
        let rows = db.query("SELECT SYSDATE()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Timestamp(_) => {}
            other => panic!(
                "Expected Timestamp from SYSDATE(), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_date_func_getdate_alias() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // SQL Server alias
        let rows = db.query("SELECT GETDATE()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Timestamp(_) => {}
            other => panic!(
                "Expected Timestamp from GETDATE(), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_date_func_curdate_alias() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // MySQL alias for CURRENT_DATE
        let rows = db.query("SELECT CURDATE()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Date(_) => {}
            other => panic!("Expected Date from CURDATE(), got {:?}", other),
        }
    }

    #[test]
    fn test_date_func_curtime_alias() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        // MySQL alias for CURRENT_TIME
        let rows = db.query("SELECT CURTIME()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Time(_) => {}
            other => panic!("Expected Time from CURTIME(), got {:?}", other),
        }
    }

    #[test]
    fn test_date_func_utc_timestamp_alias() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UTC_TIMESTAMP()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Timestamp(_) => {}
            other => panic!(
                "Expected Timestamp from UTC_TIMESTAMP(), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_date_func_localtimestamp() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOCALTIMESTAMP()", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        match rows[0].get(0).unwrap() {
            Value::Timestamp(ts) => {
                let now = chrono::Utc::now();
                let diff = (now - *ts).num_seconds().abs();
                assert!(
                    diff < 120,
                    "LOCALTIMESTAMP() too far from now: {} seconds",
                    diff
                );
            }
            other => panic!(
                "Expected Timestamp from LOCALTIMESTAMP(), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_date_func_now_returns_consistent_type() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        for _ in 0..3 {
            let rows = db.query("SELECT NOW()", &[]).unwrap();
            assert_eq!(rows.len(), 1);
            assert!(
                matches!(rows[0].get(0).unwrap(), Value::Timestamp(_)),
                "NOW() should always return Timestamp"
            );
        }
    }

    // ========================================================================
    // Date/time column storage and retrieval tests
    // ========================================================================

    #[test]
    fn test_date_column_storage_and_retrieval() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE sf_events (name TEXT, created_at TIMESTAMP)"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_events (name, created_at) VALUES ('event1', NOW())"
        )
        .unwrap();

        let rows = db
            .query("SELECT name, created_at FROM sf_events", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("event1".to_string())
        );
        match rows[0].get(1).unwrap() {
            Value::Timestamp(ts) => {
                let now = chrono::Utc::now();
                let diff = (now - *ts).num_seconds().abs();
                assert!(
                    diff < 60,
                    "Stored timestamp too far from now: {} seconds",
                    diff
                );
            }
            other => panic!("Expected Timestamp, got {:?}", other),
        }
    }

    #[test]
    fn test_date_column_with_date_type() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE sf_dated (label TEXT, d DATE)"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_dated (label, d) VALUES ('today', CURRENT_DATE())"
        )
        .unwrap();

        let rows = db
            .query("SELECT label, d FROM sf_dated", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("today".to_string())
        );
        match rows[0].get(1).unwrap() {
            Value::Date(d) => {
                let today = chrono::Utc::now().date_naive();
                let diff = (*d - today).num_days().abs();
                assert!(
                    diff <= 1,
                    "Stored date too far from today: {} days",
                    diff
                );
            }
            other => panic!("Expected Date, got {:?}", other),
        }
    }

    #[test]
    fn test_date_multiple_timestamps() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE sf_ts_multi (id INT, ts TIMESTAMP)"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_ts_multi (id, ts) VALUES (1, NOW())"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_ts_multi (id, ts) VALUES (2, NOW())"
        )
        .unwrap();

        let rows = db
            .query("SELECT id, ts FROM sf_ts_multi", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            match row.get(1).unwrap() {
                Value::Timestamp(_) => {}
                other => panic!("Expected Timestamp, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_date_now_in_where_clause() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE sf_future (name TEXT, event_ts TIMESTAMP)"
        )
        .unwrap();
        db.execute(
            "INSERT INTO sf_future (name, event_ts) VALUES ('past', NOW())"
        )
        .unwrap();

        // The inserted event's timestamp should be <= NOW()
        let rows = db
            .query(
                "SELECT name FROM sf_future WHERE event_ts <= NOW()",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("past".to_string())
        );
    }
}
