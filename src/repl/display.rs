//! Readable results at the terminal: expanded mode, `\pset`, bounded tables, a pager.
//!
//! When stdout is a terminal (or the user has asked for something with `\x`,
//! `\pset`, `\t`), results are drawn here: one rule under the header and none
//! between data rows, numbers right-aligned, wide cells cut, columns that do not
//! fit left out with a marker, a long result capped on request. When stdout is a
//! pipe and nothing was asked for, [`render`] returns exactly what
//! `formatter::format_results` prints, which is what the legacy SQLite SDK reads.
//!
//! Every function takes the terminal's width and height as arguments, so the
//! tests need no terminal.

use super::formatter;
use super::writers::{plain_text, writer_for, WriterOptions};
use super::{OutputFormat, ReplConfig};
use crate::{Column, DataType, Schema, Tuple, Value};
use std::io::{IsTerminal, Write};

/// The mark that replaces what was cut.
pub const ELLIPSIS: &str = "…";

/// `\x`: one block per row instead of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expanded {
    /// Always a table.
    Off,
    /// Always one block per row.
    On,
    /// One block per row only when the table is wider than the terminal.
    Auto,
}

/// The session's display settings, seeded from [`ReplConfig`], changed by `\x`,
/// `\pset`, `\t` and `\a`.
#[derive(Debug, Clone)]
pub struct DisplaySettings {
    /// How rows are written.
    pub format: OutputFormat,
    /// `\x`.
    pub expanded: Expanded,
    /// The text a NULL is shown as; `None` is the format's own.
    pub null: Option<String>,
    /// 0 no lines, 1 a rule under the header, 2 a full box.
    pub border: u8,
    /// Rows shown (the first half and the last half); 0 shows all.
    pub maxrows: usize,
    /// Widest a cell is drawn, in terminal columns; 0 draws every cell whole.
    pub maxwidth: usize,
    /// `maxwidth` was set with `\pset`; otherwise it is the configuration's
    /// and applies only at a terminal.
    pub maxwidth_explicit: bool,
    /// Send a result taller than the screen through `$PAGER`.
    pub pager: bool,
    /// No header and no footer.
    pub tuples_only: bool,
    /// `\a`: aligned (the default) or unaligned (`|`-separated) table output.
    pub aligned: bool,
    /// The user changed a setting: the settings apply to a pipe too.
    pub touched: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            format: OutputFormat::Table,
            expanded: Expanded::Off,
            null: None,
            border: 2,
            maxrows: 0,
            maxwidth: 0,
            maxwidth_explicit: false,
            pager: true,
            tuples_only: false,
            aligned: true,
            touched: false,
        }
    }
}

impl DisplaySettings {
    /// The settings a session starts with.
    pub fn from_config(config: &ReplConfig) -> Self {
        Self {
            format: config.output_format,
            null: (config.null_display != "NULL").then(|| config.null_display.clone()),
            maxwidth: config.max_column_width,
            ..Self::default()
        }
    }

    /// `\pset <option> [value]`: change one setting; the message to print.
    ///
    /// # Errors
    ///
    /// A message naming what was wrong with the option or its value.
    pub fn pset(&mut self, option: &str, value: Option<&str>) -> Result<String, String> {
        let on_off = |value: Option<&str>, current: bool| -> Result<bool, String> {
            match value.map(str::to_ascii_lowercase).as_deref() {
                None => Ok(!current),
                Some("on" | "true" | "1" | "yes") => Ok(true),
                Some("off" | "false" | "0" | "no") => Ok(false),
                Some(other) => Err(format!("expected on or off, got '{other}'")),
            }
        };
        let number = |value: Option<&str>| -> Result<usize, String> {
            value
                .ok_or_else(|| "a number is required".to_string())?
                .parse::<usize>()
                .map_err(|_| "a non-negative number is required".to_string())
        };
        let message = match option.to_ascii_lowercase().as_str() {
            "format" => {
                let name = value.ok_or_else(|| "a format is required: table, csv, tsv, json, jsonl, vertical, markdown".to_string())?;
                self.format = parse_format(name)?;
                format!("Output format is {name}.")
            }
            "null" => {
                self.null = value.map(str::to_string);
                match &self.null {
                    Some(text) => format!("Null display is \"{text}\"."),
                    None => "Null display is the default.".to_string(),
                }
            }
            "border" => {
                let n = number(value)?;
                if n > 2 {
                    return Err("border is 0, 1 or 2".to_string());
                }
                self.border = u8::try_from(n).unwrap_or(2);
                format!("Border style is {n}.")
            }
            "maxrows" => {
                self.maxrows = number(value)?;
                if self.maxrows == 0 {
                    "Every row is shown.".to_string()
                } else {
                    format!("At most {} rows are shown.", self.maxrows)
                }
            }
            "maxwidth" => {
                self.maxwidth = number(value)?;
                self.maxwidth_explicit = true;
                if self.maxwidth == 0 {
                    "Cells are shown whole.".to_string()
                } else {
                    format!("Cells are cut at {} columns.", self.maxwidth)
                }
            }
            "pager" => {
                self.pager = on_off(value, self.pager)?;
                format!("Pager is {}.", if self.pager { "on" } else { "off" })
            }
            "tuples_only" | "t" => {
                self.tuples_only = on_off(value, self.tuples_only)?;
                format!("Tuples only is {}.", if self.tuples_only { "on" } else { "off" })
            }
            "aligned" | "a" => {
                self.aligned = on_off(value, self.aligned)?;
                format!("Output format is {}.", if self.aligned { "aligned" } else { "unaligned" })
            }
            "expanded" | "x" => {
                self.expanded = parse_expanded(value, self.expanded)?;
                format!("Expanded display is {}.", expanded_name(self.expanded))
            }
            other => {
                return Err(format!(
                    "unknown option '{other}': format, null, border, maxrows, maxwidth, pager, tuples_only, aligned, expanded"
                ))
            }
        };
        self.touched = true;
        Ok(message)
    }
}

/// The name of an expanded mode.
pub fn expanded_name(mode: Expanded) -> &'static str {
    match mode {
        Expanded::Off => "off",
        Expanded::On => "on",
        Expanded::Auto => "auto",
    }
}

/// `\x [on|off|auto]`; with no argument the mode toggles between on and off.
///
/// # Errors
///
/// A message when the word is none of the three.
pub fn parse_expanded(value: Option<&str>, current: Expanded) -> Result<Expanded, String> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(if current == Expanded::Off {
            Expanded::On
        } else {
            Expanded::Off
        }),
        Some("on" | "true" | "1") => Ok(Expanded::On),
        Some("off" | "false" | "0") => Ok(Expanded::Off),
        Some("auto") => Ok(Expanded::Auto),
        Some(other) => Err(format!("expected on, off or auto, got '{other}'")),
    }
}

/// An output format by name.
///
/// # Errors
///
/// A message naming the formats.
pub fn parse_format(name: &str) -> Result<OutputFormat, String> {
    match name.to_ascii_lowercase().as_str() {
        "table" | "aligned" => Ok(OutputFormat::Table),
        "csv" => Ok(OutputFormat::Csv),
        "tsv" => Ok(OutputFormat::Tsv),
        "json" => Ok(OutputFormat::Json),
        "jsonl" => Ok(OutputFormat::Jsonl),
        "vertical" | "expanded" => Ok(OutputFormat::Vertical),
        "markdown" | "md" => Ok(OutputFormat::Markdown),
        other => Err(format!(
            "unknown format '{other}': table, csv, tsv, json, jsonl, vertical, markdown"
        )),
    }
}

/// What the display is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    /// Stdout is a terminal.
    pub is_terminal: bool,
    /// Its width in columns (`usize::MAX` when it is not a terminal).
    pub width: usize,
    /// Its height in rows.
    pub height: usize,
}

impl Terminal {
    /// Ask the process's stdout.
    pub fn detect() -> Self {
        if !std::io::stdout().is_terminal() {
            return Self::not_a_terminal();
        }
        let (width, height) =
            terminal_size::terminal_size().map_or((80, 24), |(w, h)| (usize::from(w.0), usize::from(h.0)));
        Self {
            is_terminal: true,
            width,
            height,
        }
    }

    /// A pipe or a file: no width limit, no height.
    pub fn not_a_terminal() -> Self {
        Self {
            is_terminal: false,
            width: usize::MAX,
            height: usize::MAX,
        }
    }

    /// A terminal of this size.
    pub fn of(width: usize, height: usize) -> Self {
        Self {
            is_terminal: true,
            width,
            height,
        }
    }
}

/// A rendered result.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The text, without a trailing newline.
    pub text: String,
    /// How many rows were formatted (a capped result formats only what it shows).
    pub rows_formatted: usize,
}

/// Terminal columns a character takes: 0 for a combining mark or joiner, 2 for
/// a wide (CJK, fullwidth, emoji) character, else 1.
pub fn char_width(ch: char) -> usize {
    let c = ch as u32;
    if c == 0 || (0x0300..=0x036F).contains(&c) || (0x200B..=0x200F).contains(&c) || c == 0xFE0F || c == 0x20E3 {
        return 0;
    }
    if c < 0x20 || (0x7F..0xA0).contains(&c) {
        return 0;
    }
    let wide = (0x1100..=0x115F).contains(&c)
        || (0x2E80..=0x303E).contains(&c)
        || (0x3041..=0x33FF).contains(&c)
        || (0x3400..=0x4DBF).contains(&c)
        || (0x4E00..=0x9FFF).contains(&c)
        || (0xA000..=0xA4CF).contains(&c)
        || (0xAC00..=0xD7A3).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
        || (0xFE30..=0xFE6F).contains(&c)
        || (0xFF00..=0xFF60).contains(&c)
        || (0xFFE0..=0xFFE6).contains(&c)
        || (0x1F300..=0x1F64F).contains(&c)
        || (0x1F900..=0x1F9FF).contains(&c)
        || (0x1FA70..=0x1FAFF).contains(&c)
        || (0x20000..=0x3FFFD).contains(&c);
    if wide {
        2
    } else {
        1
    }
}

/// Terminal columns a string takes.
pub fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// `text` cut to at most `max` columns, with a mark where it was cut.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 || text_width(text) <= max {
        return text.to_string();
    }
    let room = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > room {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push_str(ELLIPSIS);
    out
}

fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(text_width(text)));
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

fn is_numeric(value: &Value) -> bool {
    matches!(
        value,
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Float4(_) | Value::Float8(_) | Value::Numeric(_)
    )
}

fn cell_text(value: &Value, null: &str, maxwidth: usize) -> String {
    let text = if matches!(value, Value::Null) {
        null.to_string()
    } else {
        plain_text(value)
    };
    // A line break would break the box; the lines are kept apart by the caller,
    // so only each line is cut.
    if text.contains('\n') {
        text.split('\n')
            .map(|line| truncate(line.trim_end_matches('\r'), maxwidth))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        truncate(&text, maxwidth)
    }
}

/// How many rows of `total` a cap of `maxrows` shows from the head and the tail.
fn head_and_tail(total: usize, maxrows: usize) -> (usize, usize) {
    if maxrows == 0 || total <= maxrows {
        return (total, 0);
    }
    let head = maxrows.div_ceil(2);
    (head, maxrows - head)
}

/// Draw `rows` as a table no wider than `width` columns.
pub fn render_table(settings: &DisplaySettings, columns: &[String], rows: &[Tuple], width: usize) -> Rendered {
    let total = rows.len();
    if total == 0 {
        return Rendered {
            text: "(0 rows)".to_string(),
            rows_formatted: 0,
        };
    }
    let null = settings.null.as_deref().unwrap_or("NULL");
    let (head, tail) = head_and_tail(total, settings.maxrows);
    let shown: Vec<&Tuple> = rows.iter().take(head).chain(rows.iter().skip(total - tail)).collect();
    let capped = head + tail < total;

    // Cells of the shown rows only: a capped result never formats the rows it hides.
    let mut grid: Vec<Vec<String>> = shown
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| cell_text(row.values.get(index).unwrap_or(&Value::Null), null, settings.maxwidth))
                .collect()
        })
        .collect();
    let numeric: Vec<bool> = (0..columns.len())
        .map(|index| {
            shown
                .iter()
                .filter_map(|row| row.values.get(index))
                .find(|value| !matches!(value, Value::Null))
                .is_some_and(is_numeric)
        })
        .collect();
    let names: Vec<String> = columns.iter().map(|name| truncate(name, settings.maxwidth)).collect();

    // The gap row of a capped result, as wide as the table.
    let gap_at = head;
    if capped {
        grid.insert(gap_at, vec![ELLIPSIS.to_string(); columns.len()]);
    }

    let widths: Vec<usize> = (0..columns.len())
        .map(|index| {
            grid.iter()
                .filter_map(|row| {
                    row.get(index)
                        .map(|cell| cell.split('\n').map(text_width).max().unwrap_or(0))
                })
                .chain(std::iter::once(text_width(names.get(index).map_or("", String::as_str))))
                .max()
                .unwrap_or(0)
        })
        .collect();

    // Leave out the columns on the right that do not fit; say so with a marker column.
    let (border_cost, edge_cost): (usize, usize) = match settings.border {
        0 => (1, 0),
        1 => (3, 0),
        _ => (3, 4),
    };
    let table_width = |count: usize| -> usize {
        let cells: usize = widths.iter().take(count).sum();
        cells + border_cost * count.saturating_sub(1) + edge_cost
    };
    let mut visible = columns.len();
    let marker_cost = border_cost + 1;
    if table_width(visible) > width {
        while visible > 1 && table_width(visible) + marker_cost > width {
            visible -= 1;
        }
    }
    let hidden = columns.len() - visible;

    let mut out = String::new();
    let line_for = |cells: &[String], right: &[bool]| -> Vec<String> {
        // One physical line per line of the tallest cell.
        let height = cells.iter().map(|cell| cell.split('\n').count()).max().unwrap_or(1);
        (0..height)
            .map(|line_no| {
                let mut parts: Vec<String> = (0..visible)
                    .map(|index| {
                        let text = cells
                            .get(index)
                            .and_then(|cell| cell.split('\n').nth(line_no))
                            .unwrap_or("");
                        pad(
                            text,
                            widths.get(index).copied().unwrap_or(0),
                            right.get(index).copied().unwrap_or(false),
                        )
                    })
                    .collect();
                if hidden > 0 {
                    parts.push(ELLIPSIS.to_string());
                }
                match settings.border {
                    0 => parts.join(" "),
                    1 => parts.join(" | "),
                    _ => format!("│ {} │", parts.join(" │ ")),
                }
            })
            .collect()
    };
    let rule = |left: &str, mid: &str, right: &str| -> String {
        let mut segments: Vec<String> = (0..visible)
            .map(|index| "─".repeat(widths.get(index).copied().unwrap_or(0) + 2))
            .collect();
        if hidden > 0 {
            segments.push("─".repeat(3));
        }
        format!("{left}{}{right}", segments.join(mid))
    };
    let no_right: Vec<bool> = vec![false; columns.len()];

    if !settings.tuples_only {
        if settings.border == 2 {
            out.push_str(&rule("┌", "┬", "┐"));
            out.push('\n');
        }
        out.push_str(&line_for(&names, &no_right).join("\n"));
        out.push('\n');
        match settings.border {
            2 => out.push_str(&rule("├", "┼", "┤")),
            1 => {
                let segments: Vec<String> = (0..visible)
                    .map(|index| "-".repeat(widths.get(index).copied().unwrap_or(0) + 2))
                    .chain((hidden > 0).then(|| "---".to_string()))
                    .collect();
                out.push_str(&segments.join("+"));
            }
            _ => {
                let segments: Vec<String> = (0..visible)
                    .map(|index| "-".repeat(widths.get(index).copied().unwrap_or(0)))
                    .collect();
                out.push_str(&segments.join(" "));
            }
        }
        out.push('\n');
    } else if settings.border == 2 {
        out.push_str(&rule("┌", "┬", "┐"));
        out.push('\n');
    }
    for row in &grid {
        out.push_str(&line_for(row, &numeric).join("\n"));
        out.push('\n');
    }
    if settings.border == 2 {
        out.push_str(&rule("└", "┴", "┘"));
        out.push('\n');
    }
    if !settings.tuples_only {
        if capped {
            out.push_str(&format!(
                "({total} rows; the first {head} and the last {tail} are shown)"
            ));
        } else {
            out.push_str(&format!("({total} row{})", if total == 1 { "" } else { "s" }));
        }
        if hidden > 0 {
            out.push_str(&format!(
                "\n({hidden} column{} not shown; \\x shows one block per row)",
                if hidden == 1 { "" } else { "s" }
            ));
        }
    } else {
        while out.ends_with('\n') {
            out.pop();
        }
    }
    Rendered {
        text: out,
        rows_formatted: shown.len(),
    }
}

/// The `\a` unaligned form: fields separated by `|`, no box.
pub fn render_unaligned(settings: &DisplaySettings, columns: &[String], rows: &[Tuple]) -> Rendered {
    let total = rows.len();
    let (head, tail) = head_and_tail(total, settings.maxrows);
    let null = settings.null.as_deref().unwrap_or("NULL");
    let mut lines = Vec::new();
    if !settings.tuples_only {
        lines.push(columns.join("|"));
    }
    let shown = rows.iter().take(head).chain(rows.iter().skip(total - tail));
    let mut formatted = 0;
    for row in shown {
        formatted += 1;
        let cells: Vec<String> = (0..columns.len())
            .map(|index| {
                let value = row.values.get(index).unwrap_or(&Value::Null);
                if matches!(value, Value::Null) {
                    null.to_string()
                } else {
                    plain_text(value)
                }
            })
            .collect();
        lines.push(cells.join("|"));
    }
    if !settings.tuples_only {
        if head + tail < total {
            lines.push(format!(
                "({total} rows; the first {head} and the last {tail} are shown)"
            ));
        } else {
            lines.push(format!("({total} row{})", if total == 1 { "" } else { "s" }));
        }
    }
    Rendered {
        text: lines.join("\n"),
        rows_formatted: formatted,
    }
}

/// One block per row, `name | value`, capped by `maxrows` like a table.
pub fn render_expanded(settings: &DisplaySettings, columns: &[String], rows: &[Tuple]) -> Rendered {
    let total = rows.len();
    let (head, tail) = head_and_tail(total, settings.maxrows);
    let shown: Vec<Tuple> = rows
        .iter()
        .take(head)
        .chain(rows.iter().skip(total - tail))
        .cloned()
        .collect();
    let options = WriterOptions {
        null: settings.null.clone(),
        tuples_only: settings.tuples_only,
    };
    let mut bytes = Vec::new();
    // A Vec cannot fail to be written.
    let _ = writer_for(OutputFormat::Vertical, options).write_rows(&mut bytes, columns, &shown);
    let mut text = String::from_utf8_lossy(&bytes).trim_end().to_string();
    if head + tail < total {
        text.push_str(&format!(
            "\n({total} rows; the first {head} and the last {tail} are shown)"
        ));
    }
    Rendered {
        text,
        rows_formatted: shown.len(),
    }
}

/// Draw a result for `terminal` under `settings`.
///
/// On a pipe with nothing asked for this is `formatter::format_results`, byte
/// for byte.
pub fn render(settings: &DisplaySettings, columns: &[String], rows: &[Tuple], terminal: Terminal) -> Rendered {
    // The configured cell limit is for a terminal; a pipe gets whole cells unless asked.
    let limited;
    let settings = if !terminal.is_terminal && !settings.maxwidth_explicit && settings.maxwidth > 0 {
        limited = DisplaySettings {
            maxwidth: 0,
            ..settings.clone()
        };
        &limited
    } else {
        settings
    };
    if !terminal.is_terminal && !settings.touched {
        let schema = Schema::new(
            columns
                .iter()
                .map(|name| Column::new(name.clone(), DataType::Text))
                .collect(),
        );
        return Rendered {
            text: formatter::format_results(rows, &schema),
            rows_formatted: rows.len(),
        };
    }
    if settings.format != OutputFormat::Table {
        let options = WriterOptions {
            null: settings.null.clone(),
            tuples_only: settings.tuples_only,
        };
        let mut bytes = Vec::new();
        let _ = writer_for(settings.format, options).write_rows(&mut bytes, columns, rows);
        return Rendered {
            text: String::from_utf8_lossy(&bytes).trim_end().to_string(),
            rows_formatted: rows.len(),
        };
    }
    let expanded = match settings.expanded {
        Expanded::On => true,
        Expanded::Off => false,
        Expanded::Auto => {
            // Wider than the terminal when drawn whole?
            let whole = DisplaySettings {
                maxrows: 0,
                ..settings.clone()
            };
            let probe = render_table(
                &whole,
                columns,
                &rows.iter().take(200).cloned().collect::<Vec<_>>(),
                usize::MAX,
            );
            probe.text.lines().map(text_width).max().unwrap_or(0) > terminal.width
        }
    };
    if expanded {
        render_expanded(settings, columns, rows)
    } else if !settings.aligned {
        render_unaligned(settings, columns, rows)
    } else {
        render_table(settings, columns, rows, terminal.width)
    }
}

/// Print `text` (and the newline after it), through `$PAGER` when `terminal`
/// is a terminal, the settings allow it and the text is taller than the screen.
///
/// Returns whether a pager showed it; a pager that cannot be started leaves
/// the text to be printed directly, so this prints it itself in that case too.
pub fn print_paged(text: &str, settings: &DisplaySettings, terminal: Terminal) -> bool {
    let lines = text.lines().count();
    if terminal.is_terminal && settings.pager && lines + 1 >= terminal.height && spawn_pager(text) {
        return true;
    }
    println!("{text}");
    false
}

fn spawn_pager(text: &str) -> bool {
    use std::process::{Command, Stdio};
    let command = std::env::var("PAGER")
        .ok()
        .filter(|pager| !pager.trim().is_empty())
        .unwrap_or_else(|| "less -FRSX".to_string());
    let Ok(mut child) = Command::new("sh").arg("-c").arg(&command).stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A pager that quits early closes the pipe: not an error of ours.
        let _ = stdin.write_all(text.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    child.wait().is_ok()
}
