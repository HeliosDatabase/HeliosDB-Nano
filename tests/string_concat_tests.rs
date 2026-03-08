//! Tests for the string concatenation operator (||).
//!
//! PostgreSQL uses `||` for string concatenation:
//!   'hello' || ' ' || 'world' = 'hello world'
//!
//! SQL standard: NULL || anything = NULL.
//! Non-string types are cast to their string representation.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod string_concat_tests {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    #[test]
    fn test_basic_string_concat() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 'hello' || ' world'", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("hello world".to_string())
        );
    }

    #[test]
    fn test_multiple_concat() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 'a' || 'b' || 'c'", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("abc".to_string())
        );
    }

    #[test]
    fn test_concat_with_null_right() {
        // SQL standard: 'text' || NULL = NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 'text' || NULL", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_concat_with_null_left() {
        // SQL standard: NULL || 'text' = NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NULL || 'text'", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_concat_null_null() {
        // SQL standard: NULL || NULL = NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NULL || NULL", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_concat_with_integer() {
        // PostgreSQL casts non-string types to text: 'count: ' || 42
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 'count: ' || 42", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("count: 42".to_string())
        );
    }

    #[test]
    fn test_concat_with_columns() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE users (first_name TEXT, last_name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES ('John', 'Doe')").unwrap();
        db.execute("INSERT INTO users VALUES ('Jane', 'Smith')").unwrap();

        let rows = db
            .query("SELECT first_name || ' ' || last_name AS full_name FROM users ORDER BY first_name", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
        let mut names: Vec<String> = rows.iter().map(|r| {
            match r.get(0).unwrap() {
                Value::String(s) => s.clone(),
                other => panic!("Expected String, got {:?}", other),
            }
        }).collect();
        names.sort();
        assert_eq!(names, vec!["Jane Smith", "John Doe"]);
    }

    #[test]
    fn test_concat_empty_string() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT '' || 'text'", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("text".to_string())
        );
    }

    #[test]
    fn test_concat_in_where_clause() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE items (name TEXT, suffix TEXT)").unwrap();
        db.execute("INSERT INTO items VALUES ('test', '_value')").unwrap();
        db.execute("INSERT INTO items VALUES ('other', '_thing')").unwrap();

        let rows = db
            .query("SELECT name FROM items WHERE name || suffix = 'test_value'", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("test".to_string())
        );
    }

    #[test]
    fn test_concat_column_with_null() {
        // When a column value is NULL, concatenation with || should return NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE str1 (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO str1 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT val || ' suffix' FROM str1", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }
}
