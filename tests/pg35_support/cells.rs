//! Checked common result cells; unsupported wire types are never discarded.
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::error::Error;
use tokio_postgres::types::{FromSql, Type};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Cell {
    Null,
    Int(i64),
    Decimal(f64),
    Text(String),
    Bool(bool),
}
pub(super) type Rows = Vec<Vec<Cell>>;
pub(super) fn int(v: i64) -> Cell {
    Cell::Int(v)
}
pub(super) fn text(v: impl Into<String>) -> Cell {
    Cell::Text(v.into())
}

fn finite(value: f64) -> Result<Cell, Box<dyn Error + Send + Sync>> {
    if value.is_finite() {
        Ok(Cell::Decimal(value))
    } else {
        Err("nonfinite benchmark numeric result".into())
    }
}

fn numeric(raw: &[u8]) -> Result<Cell, Box<dyn Error + Send + Sync>> {
    if raw.len() < 8 {
        return Err("short PostgreSQL NUMERIC header".into());
    }
    let signed = |at| i16::from_be_bytes([raw[at], raw[at + 1]]);
    let unsigned = |at| u16::from_be_bytes([raw[at], raw[at + 1]]);
    let count = usize::try_from(signed(0))?;
    let weight = i32::from(signed(2));
    let sign = unsigned(4);
    let scale = unsigned(6);
    if raw.len() != 8 + count * 2 || !matches!(sign, 0 | 0x4000) || scale > 16383 {
        return Err("malformed/unsupported PostgreSQL NUMERIC".into());
    }
    let mut value = 0.0;
    for i in 0..count {
        let digit = unsigned(8 + i * 2);
        if digit >= 10000 {
            return Err("invalid base10000 NUMERIC digit".into());
        }
        value += f64::from(digit) * 10000f64.powi(weight - i32::try_from(i)?);
    }
    finite(if sign == 0x4000 { -value } else { value })
}

impl<'a> FromSql<'a> for Cell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        match *ty {
            Type::INT2 => Ok(Self::Int(i64::from(i16::from_sql(ty, raw)?))),
            Type::INT4 => Ok(Self::Int(i64::from(i32::from_sql(ty, raw)?))),
            Type::INT8 => Ok(Self::Int(i64::from_sql(ty, raw)?)),
            Type::FLOAT4 => finite(f64::from(f32::from_sql(ty, raw)?)),
            Type::FLOAT8 => finite(f64::from_sql(ty, raw)?),
            Type::NUMERIC => numeric(raw),
            Type::BOOL => Ok(Self::Bool(bool::from_sql(ty, raw)?)),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
                Ok(Self::Text(std::str::from_utf8(raw)?.into()))
            }
            _ => Err(format!("unsupported PostgreSQL result type {ty}").into()),
        }
    }
    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn Error + Send + Sync>> {
        Ok(Self::Null)
    }
    fn accepts(ty: &Type) -> bool {
        matches!(
            *ty,
            Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::NUMERIC
                | Type::BOOL
                | Type::TEXT
                | Type::VARCHAR
                | Type::BPCHAR
                | Type::NAME
                | Type::UNKNOWN
        )
    }
}

pub(super) fn nano(db: &EmbeddedDatabase, sql: &str) -> Rows {
    db.query(sql, &[])
        .unwrap_or_else(|error| panic!("Nano oracle {sql}: {error}"))
        .into_iter()
        .map(|row| {
            row.values
                .into_iter()
                .map(|value| match value {
                    Value::Null => Cell::Null,
                    Value::Boolean(v) => Cell::Bool(v),
                    Value::Int2(v) => int(i64::from(v)),
                    Value::Int4(v) => int(i64::from(v)),
                    Value::Int8(v) => int(v),
                    Value::Float4(v) => finite(f64::from(v)).unwrap(),
                    Value::Float8(v) => finite(v).unwrap(),
                    Value::Numeric(v) => finite(v.parse().expect("valid Nano numeric result")).unwrap(),
                    Value::String(v) => text(v),
                    other => panic!("unsupported Nano result value {other:?} for {sql}"),
                })
                .collect()
        })
        .collect()
}

pub(super) fn pg(pg: &crate::PgClient, sql: &str) -> Rows {
    pg.rt
        .block_on(pg.client.query(sql, &[]))
        .unwrap_or_else(|error| panic!("PG oracle {sql}: {error}"))
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|column| {
                    row.try_get::<_, Cell>(column)
                        .unwrap_or_else(|error| panic!("PG oracle {sql} column{column}: {error}"))
                })
                .collect()
        })
        .collect()
}

fn compare_cell(a: &Cell, b: &Cell) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |value: &Cell| match value {
        Cell::Null => 0,
        Cell::Int(_) | Cell::Decimal(_) => 1,
        Cell::Text(_) => 2,
        Cell::Bool(_) => 3,
    };
    let order = rank(a).cmp(&rank(b));
    if order != Ordering::Equal {
        return order;
    }
    match (a, b) {
        (Cell::Int(x), Cell::Int(y)) => x.cmp(y),
        (Cell::Decimal(x), Cell::Decimal(y)) => x.total_cmp(y),
        (Cell::Int(x), Cell::Decimal(y)) => (*x as f64).total_cmp(y),
        (Cell::Decimal(x), Cell::Int(y)) => x.total_cmp(&(*y as f64)),
        (Cell::Text(x), Cell::Text(y)) => x.cmp(y),
        (Cell::Bool(x), Cell::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

fn normalize(rows: &mut Rows) {
    rows.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b) {
            let order = compare_cell(x, y);
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        a.len().cmp(&b.len())
    });
}

pub(super) fn assert_rows(mut actual: Rows, mut expected: Rows, ordered: bool, label: &str) {
    if !ordered {
        normalize(&mut actual);
        normalize(&mut expected);
    }
    assert_eq!(actual.len(), expected.len(), "{label}: row count");
    for (row, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(actual.len(), expected.len(), "{label}: row{row} width");
        for (column, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            let equal = match (actual, expected) {
                (Cell::Int(x), Cell::Decimal(y)) => (*x as f64 - *y).abs() <= 1e-12,
                (Cell::Decimal(x), Cell::Int(y)) => (*x - *y as f64).abs() <= 1e-12,
                (Cell::Decimal(x), Cell::Decimal(y)) => (*x - *y).abs() <= 1e-12,
                _ => actual == expected,
            };
            assert!(
                equal,
                "{label}: row{row} col{column}: actual{actual:?} expected{expected:?}"
            );
        }
    }
}

pub(super) fn both(
    db: &EmbeddedDatabase,
    pg_client: &crate::PgClient,
    sql: &str,
    expected: Rows,
    ordered: bool,
) -> u64 {
    assert_rows(nano(db, sql), expected.clone(), ordered, &format!("Nano {sql}"));
    assert_rows(pg(pg_client, sql), expected, ordered, &format!("PG {sql}"));
    2
}
