//! Result formatting and pretty printing

use crate::{Tuple, Schema, Value, DataType};
use prettytable::{Table, Row, Cell, format};
use colored::Colorize;

/// Format query results as a pretty table
pub fn format_results(tuples: &[Tuple], schema: &Schema) -> String {
    if tuples.is_empty() {
        return format!("{}", "(0 rows)".dimmed());
    }

    let mut table = Table::new();

    // Use a clean format
    table.set_format(*format::consts::FORMAT_BOX_CHARS);

    // Add header row
    let header: Vec<Cell> = schema.columns.iter()
        .map(|col| Cell::new(&col.name).style_spec("Fb"))
        .collect();
    table.add_row(Row::new(header));

    // Add data rows
    for tuple in tuples {
        let cells: Vec<Cell> = tuple.values.iter()
            .map(|val| Cell::new(&format_value(val)))
            .collect();
        table.add_row(Row::new(cells));
    }

    let mut output = String::new();
    output.push_str(&table.to_string());
    output.push('\n');
    output.push_str(&format!("({} row{})", tuples.len(), if tuples.len() == 1 { "" } else { "s" }).dimmed().to_string());

    output
}

/// Format a single value for display
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".dimmed().to_string(),
        Value::Boolean(b) => if *b { "true".green().to_string() } else { "false".red().to_string() },
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::Float4(f) => format_float(*f as f64),
        Value::Float8(f) => format_float(*f),
        Value::Numeric(n) => n.clone(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("\\x{}", hex::encode(b)),
        Value::Timestamp(d) => d.to_string(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S").to_string(),
        Value::Json(j) => j.to_string(),
        Value::Int2(i) => i.to_string(),
        Value::Uuid(u) => u.to_string(),
        Value::Array(arr) => format!("{:?}", arr),
        Value::Vector(vec) => format!("{:?}", vec),
        // Storage references (should be resolved before display, but show debug info if not)
        Value::DictRef { dict_id } => format!("<dict:{}>", dict_id).dimmed().to_string(),
        Value::CasRef { hash } => format!("<cas:{}>", hex::encode(&hash[..8])).dimmed().to_string(),
        Value::ColumnarRef => "<columnar>".dimmed().to_string(),
    }
}

/// Format floating point numbers nicely
fn format_float(f: f64) -> String {
    if f.abs() < 1e-10 && f != 0.0 {
        format!("{:.2e}", f)
    } else if f.fract() == 0.0 && f.abs() < 1e10 {
        format!("{:.0}", f)
    } else {
        format!("{:.6}", f).trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Format query execution time
pub fn format_timing(duration_secs: f64) -> String {
    if duration_secs < 0.001 {
        format!("({:.3} ms)", duration_secs * 1000.0).dimmed().to_string()
    } else if duration_secs < 1.0 {
        format!("({:.2} ms)", duration_secs * 1000.0).dimmed().to_string()
    } else {
        format!("({:.3} s)", duration_secs).dimmed().to_string()
    }
}

/// Format error message
pub fn format_error(error: &str) -> String {
    format!("{}: {}", "ERROR".red().bold(), error)
}

/// Format a schema for display
pub fn format_schema(schema: &Schema) -> String {
    let mut output = String::new();

    for (i, column) in schema.columns.iter().enumerate() {
        if i > 0 {
            output.push_str(", ");
        }
        output.push_str(&format!(
            "{} {}{}",
            column.name.green(),
            format_datatype(&column.data_type).yellow(),
            if column.nullable { "" } else { " NOT NULL" }
        ));
    }

    output
}

/// Format a data type for display
fn format_datatype(dt: &DataType) -> String {
    match dt {
        DataType::Int2 => "SMALLINT".to_string(),
        DataType::Int4 => "INT".to_string(),
        DataType::Int8 => "BIGINT".to_string(),
        DataType::Float4 => "REAL".to_string(),
        DataType::Float8 => "DOUBLE".to_string(),
        DataType::Text => "TEXT".to_string(),
        DataType::Varchar(n) => {
            if let Some(len) = n {
                format!("VARCHAR({})", len)
            } else {
                "VARCHAR".to_string()
            }
        },
        DataType::Boolean => "BOOLEAN".to_string(),
        DataType::Date => "DATE".to_string(),
        DataType::Time => "TIME".to_string(),
        DataType::Timestamp => "TIMESTAMP".to_string(),
        DataType::Timestamptz => "TIMESTAMPTZ".to_string(),
        DataType::Interval => "INTERVAL".to_string(),
        DataType::Bytea => "BYTEA".to_string(),
        DataType::Uuid => "UUID".to_string(),
        DataType::Json => "JSON".to_string(),
        DataType::Jsonb => "JSONB".to_string(),
        DataType::Numeric => "NUMERIC".to_string(),
        DataType::Char(n) => format!("CHAR({})", n),
        DataType::Array(inner) => format!("{}[]", format_datatype(inner)),
        DataType::Vector(dim) => format!("VECTOR({})", dim),
    }
}

// Hex encoding dependency
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}
