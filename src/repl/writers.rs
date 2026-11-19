//! Result writers for script mode: one per output format.
//!
//! `repl -c` and `repl -f` run statements with no terminal in front of them, so
//! what they print is for a program to read. Each [`OutputFormat`] has one
//! writer here. None of them colours its output, and the typed ones (JSON) take
//! the type from the [`Value`], not from the text the table would show.
//!
//! The table writer is the shell's own table, byte for byte.

use super::formatter;
use super::OutputFormat;
use crate::{Column, DataType, Schema, Tuple, Value};
use std::io::{self, Write};

/// What a writer is told besides the rows.
#[derive(Debug, Clone, Default)]
pub struct WriterOptions {
    /// The text a NULL is written as; `None` is the format's own (empty for
    /// the delimited formats, `NULL` for the table and vertical ones, `null`
    /// in JSON).
    pub null: Option<String>,
    /// No header row and no row-count footer.
    pub tuples_only: bool,
}

/// Writes the rows of one statement.
pub trait ResultWriter {
    /// Write `rows`, whose columns are named `columns`, to `out`.
    ///
    /// # Errors
    ///
    /// The error of the underlying writer.
    fn write_rows(&mut self, out: &mut dyn Write, columns: &[String], rows: &[Tuple]) -> io::Result<()>;
}

/// The writer of `format`.
pub fn writer_for(format: OutputFormat, options: WriterOptions) -> Box<dyn ResultWriter> {
    match format {
        OutputFormat::Table => Box::new(TableWriter { options }),
        OutputFormat::Csv => Box::new(DelimitedWriter {
            options,
            kind: Delimited::Csv,
        }),
        OutputFormat::Tsv => Box::new(DelimitedWriter {
            options,
            kind: Delimited::Tsv,
        }),
        OutputFormat::Json => Box::new(JsonWriter { lines: false }),
        OutputFormat::Jsonl => Box::new(JsonWriter { lines: true }),
        OutputFormat::Vertical => Box::new(VerticalWriter { options }),
        OutputFormat::Markdown => Box::new(MarkdownWriter { options }),
    }
}

/// A value as plain text, with no colour. NULL is the caller's to spell.
pub fn plain_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::Float4(f) => f.to_string(),
        Value::Float8(f) => f.to_string(),
        Value::Numeric(n) => n.clone(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => bytes_text(b),
        Value::Uuid(u) => u.to_string(),
        Value::Timestamp(t) => t.to_string(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S").to_string(),
        Value::Interval(micros) => interval_text(*micros),
        Value::Json(j) => j.clone(),
        Value::Array(_) | Value::Vector(_) => json_text(value),
        Value::DictRef { dict_id } => format!("<dict:{dict_id}>"),
        Value::CasRef { hash } => format!("<cas:{}>", hex(hash.get(..8).unwrap_or(&[]))),
        Value::ColumnarRef => "<columnar>".to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn bytes_text(bytes: &[u8]) -> String {
    format!("\\x{}", hex(bytes))
}

fn interval_text(microseconds: i64) -> String {
    let negative = microseconds < 0;
    let magnitude = microseconds.unsigned_abs();
    let total_secs = magnitude / 1_000_000;
    let (hours, mins, secs) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    let micros = magnitude % 1_000_000;
    let sign = if negative { "-" } else { "" };
    if micros != 0 {
        format!("{sign}{hours:02}:{mins:02}:{secs:02}.{micros:06}")
    } else {
        format!("{sign}{hours:02}:{mins:02}:{secs:02}")
    }
}

/// A float as a JSON number; a value JSON cannot hold (NaN, infinity) as a string.
fn float_json(shortest: &str) -> serde_json::Value {
    shortest
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or_else(
            || serde_json::Value::String(shortest.to_string()),
            serde_json::Value::Number,
        )
}

/// A value as typed JSON: integers and floats are numbers, NUMERIC a string
/// (so it stays lossless), bytea `\x` hex, JSON embedded, arrays and vectors
/// arrays, timestamps ISO 8601, NULL null.
pub fn json_value(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Null => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Int2(i) => J::from(*i),
        Value::Int4(i) => J::from(*i),
        Value::Int8(i) => J::from(*i),
        // The shortest text that reads back as the same f32 (0.1f32 is 0.1, not 0.10000000149011612).
        Value::Float4(f) => float_json(&f.to_string()),
        Value::Float8(f) => float_json(&f.to_string()),
        Value::Numeric(n) => J::String(n.clone()),
        Value::String(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(bytes_text(b)),
        Value::Uuid(u) => J::String(u.to_string()),
        Value::Timestamp(t) => J::String(t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
        Value::Date(d) => J::String(d.format("%Y-%m-%d").to_string()),
        Value::Time(t) => J::String(t.format("%H:%M:%S%.f").to_string()),
        Value::Interval(micros) => J::String(interval_text(*micros)),
        Value::Json(text) => serde_json::from_str(text).unwrap_or_else(|_| J::String(text.clone())),
        Value::Array(items) => J::Array(items.iter().map(json_value).collect()),
        Value::Vector(items) => J::Array(items.iter().map(|f| float_json(&f.to_string())).collect()),
        other => J::String(plain_text(other)),
    }
}

fn json_text(value: &Value) -> String {
    json_value(value).to_string()
}

/// One row as a JSON object, keys in column order (a `serde_json::Map` would sort them).
fn json_object(columns: &[String], row: &Tuple) -> String {
    let mut out = String::from("{");
    for (index, name) in columns.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::Value::String(name.clone()).to_string());
        out.push(':');
        out.push_str(&json_text(row.values.get(index).unwrap_or(&Value::Null)));
    }
    out.push('}');
    out
}

/// A cell as text: the value, or the null text.
fn cell(value: &Value, null: &str) -> String {
    if matches!(value, Value::Null) {
        null.to_string()
    } else {
        plain_text(value)
    }
}

struct TableWriter {
    options: WriterOptions,
}

impl ResultWriter for TableWriter {
    fn write_rows(&mut self, out: &mut dyn Write, columns: &[String], rows: &[Tuple]) -> io::Result<()> {
        let schema = Schema::new(
            columns
                .iter()
                .map(|name| Column::new(name.clone(), DataType::Text))
                .collect(),
        );
        let tuples_only = self.options.tuples_only;
        let text =
            formatter::format_results_with(rows, &schema, self.options.null.as_deref(), !tuples_only, !tuples_only);
        writeln!(out, "\n{text}")
    }
}

#[derive(Clone, Copy)]
enum Delimited {
    Csv,
    Tsv,
}

struct DelimitedWriter {
    options: WriterOptions,
    kind: Delimited,
}

/// RFC 4180: a field with a comma, a quote or a line break is quoted, a quote inside doubled.
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// The text format of `COPY`: a tab, a line break or a backslash is escaped.
fn tsv_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

impl ResultWriter for DelimitedWriter {
    fn write_rows(&mut self, out: &mut dyn Write, columns: &[String], rows: &[Tuple]) -> io::Result<()> {
        let (escape, separator): (fn(&str) -> String, &str) = match self.kind {
            Delimited::Csv => (csv_field, ","),
            Delimited::Tsv => (tsv_field, "\t"),
        };
        let null = self.options.null.as_deref().unwrap_or("");
        if !self.options.tuples_only {
            let header: Vec<String> = columns.iter().map(|name| escape(name)).collect();
            writeln!(out, "{}", header.join(separator))?;
        }
        for row in rows {
            let fields: Vec<String> = (0..columns.len())
                .map(|index| escape(&cell(row.values.get(index).unwrap_or(&Value::Null), null)))
                .collect();
            writeln!(out, "{}", fields.join(separator))?;
        }
        Ok(())
    }
}

struct JsonWriter {
    lines: bool,
}

impl ResultWriter for JsonWriter {
    fn write_rows(&mut self, out: &mut dyn Write, columns: &[String], rows: &[Tuple]) -> io::Result<()> {
        if self.lines {
            for row in rows {
                writeln!(out, "{}", json_object(columns, row))?;
            }
            return Ok(());
        }
        let objects: Vec<String> = rows.iter().map(|row| json_object(columns, row)).collect();
        writeln!(out, "[{}]", objects.join(","))
    }
}

struct VerticalWriter {
    options: WriterOptions,
}

impl ResultWriter for VerticalWriter {
    fn write_rows(&mut self, out: &mut dyn Write, columns: &[String], rows: &[Tuple]) -> io::Result<()> {
        if rows.is_empty() {
            return writeln!(out, "(0 rows)");
        }
        let null = self.options.null.as_deref().unwrap_or("NULL");
        let width = columns.iter().map(|name| name.chars().count()).max().unwrap_or(0);
        for (number, row) in rows.iter().enumerate() {
            writeln!(out, "-[ RECORD {} ]", number + 1)?;
            for (index, name) in columns.iter().enumerate() {
                let value = cell(row.values.get(index).unwrap_or(&Value::Null), null);
                writeln!(out, "{name:<width$} | {value}")?;
            }
        }
        Ok(())
    }
}

struct MarkdownWriter {
    options: WriterOptions,
}

/// A cell of a Markdown table: a pipe is escaped and a line break becomes `<br>`.
fn markdown_cell(text: &str) -> String {
    text.replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

impl ResultWriter for MarkdownWriter {
    fn write_rows(&mut self, out: &mut dyn Write, columns: &[String], rows: &[Tuple]) -> io::Result<()> {
        let null = self.options.null.as_deref().unwrap_or("");
        if !self.options.tuples_only {
            let header: Vec<String> = columns.iter().map(|name| markdown_cell(name)).collect();
            writeln!(out, "| {} |", header.join(" | "))?;
            writeln!(out, "|{}|", vec![" --- "; columns.len()].join("|"))?;
        }
        for row in rows {
            let fields: Vec<String> = (0..columns.len())
                .map(|index| markdown_cell(&cell(row.values.get(index).unwrap_or(&Value::Null), null)))
                .collect();
            writeln!(out, "| {} |", fields.join(" | "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn render(format: OutputFormat, options: WriterOptions, columns: &[&str], rows: Vec<Vec<Value>>) -> String {
        let columns: Vec<String> = columns.iter().map(|c| (*c).to_string()).collect();
        let rows: Vec<Tuple> = rows.into_iter().map(Tuple::new).collect();
        let mut out = Vec::new();
        writer_for(format, options)
            .write_rows(&mut out, &columns, &rows)
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn csv_quotes_a_comma_a_quote_and_a_newline() {
        let text = render(
            OutputFormat::Csv,
            WriterOptions::default(),
            &["a", "b,c"],
            vec![vec![Value::String("x,\"y\"\nz".into()), Value::Null]],
        );
        assert_eq!(text, "a,\"b,c\"\n\"x,\"\"y\"\"\nz\",\n");
    }

    #[test]
    fn csv_null_text_and_tuples_only() {
        let options = WriterOptions {
            null: Some("-".into()),
            tuples_only: true,
        };
        let text = render(
            OutputFormat::Csv,
            options,
            &["a"],
            vec![vec![Value::Null], vec![Value::Int4(1)]],
        );
        assert_eq!(text, "-\n1\n");
    }

    #[test]
    fn tsv_escapes_tab_newline_and_backslash() {
        let text = render(
            OutputFormat::Tsv,
            WriterOptions::default(),
            &["a"],
            vec![vec![Value::String("x\ty\nz\\".into())]],
        );
        assert_eq!(text, "a\nx\\ty\\nz\\\\\n");
    }

    #[test]
    fn json_is_typed() {
        let text = render(
            OutputFormat::Json,
            WriterOptions::default(),
            &["i", "f", "n", "s", "b", "j", "v", "z", "nan", "x"],
            vec![vec![
                Value::Int4(1),
                Value::Float4(0.1),
                Value::Numeric("12345678901234567890.5".into()),
                Value::String("é\"".into()),
                Value::Bytes(vec![0xde, 0xad]),
                Value::Json("{\"a\":[1,2]}".into()),
                Value::Vector(vec![0.5, 1.0]),
                Value::Null,
                Value::Float8(f64::NAN),
                Value::Boolean(true),
            ]],
        );
        assert_eq!(
            text,
            "[{\"i\":1,\"f\":0.1,\"n\":\"12345678901234567890.5\",\"s\":\"é\\\"\",\"b\":\"\\\\xdead\",\"j\":{\"a\":[1,2]},\"v\":[0.5,1.0],\"z\":null,\"nan\":\"NaN\",\"x\":true}]\n"
        );
    }

    #[test]
    fn json_keeps_column_order_and_empty_result_is_an_empty_array() {
        let text = render(
            OutputFormat::Json,
            WriterOptions::default(),
            &["b", "a"],
            vec![vec![Value::Int4(2), Value::Int4(1)]],
        );
        assert_eq!(text, "[{\"b\":2,\"a\":1}]\n");
        assert_eq!(
            render(OutputFormat::Json, WriterOptions::default(), &["a"], vec![]),
            "[]\n"
        );
    }

    #[test]
    fn jsonl_is_one_object_per_row() {
        let text = render(
            OutputFormat::Jsonl,
            WriterOptions::default(),
            &["a"],
            vec![vec![Value::Int4(1)], vec![Value::Int4(2)]],
        );
        assert_eq!(text, "{\"a\":1}\n{\"a\":2}\n");
    }

    #[test]
    fn markdown_escapes_pipes_and_newlines() {
        let text = render(
            OutputFormat::Markdown,
            WriterOptions::default(),
            &["a", "b"],
            vec![vec![Value::String("x|y".into()), Value::String("l1\nl2".into())]],
        );
        assert_eq!(text, "| a | b |\n| --- | --- |\n| x\\|y | l1<br>l2 |\n");
    }

    #[test]
    fn vertical_pads_names_and_numbers_records() {
        let text = render(
            OutputFormat::Vertical,
            WriterOptions::default(),
            &["id", "name"],
            vec![
                vec![Value::Int4(1), Value::String("a".into())],
                vec![Value::Int4(2), Value::Null],
            ],
        );
        assert_eq!(
            text,
            "-[ RECORD 1 ]\nid   | 1\nname | a\n-[ RECORD 2 ]\nid   | 2\nname | NULL\n"
        );
        assert_eq!(
            render(OutputFormat::Vertical, WriterOptions::default(), &["a"], vec![]),
            "(0 rows)\n"
        );
    }

    #[test]
    fn plain_text_has_no_colour_and_spells_each_type() {
        assert_eq!(plain_text(&Value::Boolean(true)), "true");
        assert_eq!(plain_text(&Value::Bytes(vec![1, 255])), "\\x01ff");
        assert_eq!(plain_text(&Value::Interval(3_661_000_000)), "01:01:01");
        assert_eq!(plain_text(&Value::Interval(-1_500_000)), "-00:00:01.500000");
    }
}
