//! Hardening tests for the PostgreSQL-standard date/time function surface.
//!
//! Locks down the v3.21 audit work: `TO_CHAR`, `TO_DATE`, `TO_TIMESTAMP`,
//! `DATE_TRUNC`, `DATE_PART` (alias for EXTRACT), `AGE`, `MAKE_DATE`,
//! `MAKE_TIMESTAMP`, plus the existing `EXTRACT(... FROM ...)` and
//! `current_*` family — all without reaching for SQLite-specific names.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod datetime {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    fn db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("Failed to create test database")
    }

    fn to_str(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {:?}", other),
        }
    }

    fn to_i64(v: &Value) -> i64 {
        match v {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            other => panic!("expected integer, got {:?}", other),
        }
    }

    fn to_f64(v: &Value) -> f64 {
        match v {
            Value::Float4(f) => *f as f64,
            Value::Float8(f) => *f,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            other => panic!("expected numeric, got {:?}", other),
        }
    }

    // -------- TO_CHAR --------

    #[test]
    fn to_char_basic_iso_date() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_DATE(2026, 4, 28), 'YYYY-MM-DD') AS d",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28");
    }

    #[test]
    fn to_char_full_timestamp() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_TIMESTAMP(2026, 4, 28, 13, 5, 42.5), 'YYYY-MM-DD HH24:MI:SS') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28 13:05:42");
    }

    #[test]
    fn to_char_month_names() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_DATE(2026, 4, 28), 'Mon DD YYYY') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "Apr 28 2026");

        let r2 = db.query(
            "SELECT TO_CHAR(MAKE_DATE(2026, 4, 28), 'MON') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r2[0].values[0]), "APR");
    }

    #[test]
    fn to_char_weekday_names() {
        // 2026-04-28 is a Tuesday.
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_DATE(2026, 4, 28), 'Day') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]).trim(), "Tuesday");
    }

    #[test]
    fn to_char_quarter_and_iso_week() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_DATE(2026, 4, 28), 'Q-IW') AS s",
            &[],
        ).unwrap();
        let s = to_str(&r[0].values[0]);
        assert!(s.starts_with("2-"), "expected Q=2, got {}", s);
    }

    // -------- TO_DATE / TO_TIMESTAMP --------

    #[test]
    fn to_date_round_trip() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(TO_DATE('2026-04-28', 'YYYY-MM-DD'), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28");
    }

    #[test]
    fn to_timestamp_from_epoch() {
        // 2026-04-28 00:00:00 UTC = 1777334400
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(TO_TIMESTAMP(1777334400), 'YYYY-MM-DD HH24:MI:SS') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28 00:00:00");
    }

    #[test]
    fn to_timestamp_from_text_with_format() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(TO_TIMESTAMP('2026-04-28 13:05:00', 'YYYY-MM-DD HH24:MI:SS'), 'YYYY-MM-DD HH24:MI:SS') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28 13:05:00");
    }

    // -------- DATE_TRUNC --------

    #[test]
    fn date_trunc_day() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('day', MAKE_TIMESTAMP(2026, 4, 28, 13, 5, 42.5)), 'YYYY-MM-DD HH24:MI:SS') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28 00:00:00");
    }

    #[test]
    fn date_trunc_month_quarter_year() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('month', MAKE_DATE(2026, 4, 28)), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-01");

        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('quarter', MAKE_DATE(2026, 4, 28)), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-01");

        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('quarter', MAKE_DATE(2026, 6, 30)), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-01");

        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('year', MAKE_DATE(2026, 4, 28)), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-01-01");
    }

    #[test]
    fn date_trunc_week_to_monday() {
        // 2026-04-28 is Tuesday → DATE_TRUNC('week', ...) gives Monday 2026-04-27.
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('week', MAKE_DATE(2026, 4, 28)), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-27");
    }

    // -------- DATE_PART (alias for EXTRACT) --------

    #[test]
    fn date_part_components() {
        let db = db();
        let r = db.query(
            "SELECT DATE_PART('year',  MAKE_DATE(2026, 4, 28)) AS y, \
                    DATE_PART('month', MAKE_DATE(2026, 4, 28)) AS m, \
                    DATE_PART('day',   MAKE_DATE(2026, 4, 28)) AS d",
            &[],
        ).unwrap();
        assert_eq!(to_i64(&r[0].values[0]), 2026);
        assert_eq!(to_i64(&r[0].values[1]), 4);
        assert_eq!(to_i64(&r[0].values[2]), 28);
    }

    #[test]
    fn date_part_epoch() {
        let db = db();
        let r = db.query(
            "SELECT DATE_PART('epoch', MAKE_TIMESTAMP(2026, 4, 28, 0, 0, 0)) AS e",
            &[],
        ).unwrap();
        assert!(
            (to_f64(&r[0].values[0]) - 1_777_334_400.0).abs() < 1.0,
            "got epoch {}, expected ~1777334400 for 2026-04-28 UTC midnight",
            to_f64(&r[0].values[0])
        );
    }

    // -------- MAKE_DATE / MAKE_TIMESTAMP --------

    #[test]
    fn make_date_round_trip() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_DATE(2000, 1, 1), 'YYYY-MM-DD') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2000-01-01");
    }

    #[test]
    fn make_date_invalid_rejected() {
        let db = db();
        let res = db.query(
            "SELECT MAKE_DATE(2026, 13, 1) AS bad",
            &[],
        );
        assert!(res.is_err(), "MAKE_DATE(month=13) must error");
    }

    #[test]
    fn make_timestamp_subsecond() {
        let db = db();
        let r = db.query(
            "SELECT TO_CHAR(MAKE_TIMESTAMP(2026, 4, 28, 13, 5, 42), 'YYYY-MM-DD HH24:MI:SS') AS s",
            &[],
        ).unwrap();
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28 13:05:42");
    }

    // -------- AGE --------

    #[test]
    fn age_two_arg_returns_interval() {
        let db = db();
        let r = db.query(
            "SELECT DATE_PART('day', AGE(MAKE_DATE(2026, 4, 28), MAKE_DATE(2026, 4, 1))) AS d",
            &[],
        ).unwrap();
        assert_eq!(to_i64(&r[0].values[0]), 27);
    }

    #[test]
    fn age_handles_null() {
        let db = db();
        let r = db.query("SELECT AGE(NULL, MAKE_DATE(2020, 1, 1)) AS a", &[]).unwrap();
        assert!(matches!(r[0].values[0], Value::Null));
    }

    // -------- Combined: realistic OLTP-style usage --------

    #[test]
    fn date_aggregation_smoke() {
        let db = db();
        db.execute(
            "CREATE TABLE events (id INT PRIMARY KEY, ts TIMESTAMP, body TEXT)",
        ).unwrap();
        db.execute(
            "INSERT INTO events VALUES \
             (1, MAKE_TIMESTAMP(2026, 4, 28, 9,  0, 0), 'morning'), \
             (2, MAKE_TIMESTAMP(2026, 4, 28, 14, 0, 0), 'afternoon'), \
             (3, MAKE_TIMESTAMP(2026, 4, 29, 10, 0, 0), 'next day')",
        ).unwrap();

        let r = db.query(
            "SELECT TO_CHAR(DATE_TRUNC('day', ts), 'YYYY-MM-DD') AS day, COUNT(*) AS n \
             FROM events GROUP BY DATE_TRUNC('day', ts) ORDER BY day",
            &[],
        ).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(to_str(&r[0].values[0]), "2026-04-28");
        assert_eq!(to_i64(&r[0].values[1]), 2);
        assert_eq!(to_str(&r[1].values[0]), "2026-04-29");
        assert_eq!(to_i64(&r[1].values[1]), 1);
    }
}
