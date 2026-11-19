//! The terminal renderer of the shell: bounded tables, expanded mode, `\pset`.
//!
//! The renderer takes the terminal's width and height as arguments, so none of
//! this needs a terminal. A pipe with nothing asked for gets the shell's old
//! table, byte for byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::repl::display::{
    char_width, render, render_expanded, render_table, text_width, DisplaySettings, Expanded, Terminal,
};
use heliosdb_nano::repl::{format_results, MetaCommand, OutputFormat};
use heliosdb_nano::{Column, DataType, Schema, Tuple, Value};

fn names(columns: &[&str]) -> Vec<String> {
    columns.iter().map(|c| (*c).to_string()).collect()
}

fn schema(columns: &[String]) -> Schema {
    Schema::new(columns.iter().map(|c| Column::new(c.clone(), DataType::Text)).collect())
}

fn ints(rows: &[&[i32]]) -> Vec<Tuple> {
    rows.iter()
        .map(|row| Tuple::new(row.iter().map(|v| Value::Int4(*v)).collect()))
        .collect()
}

#[test]
fn two_hundred_columns_elide_with_marker() {
    let columns: Vec<String> = (0..200).map(|i| format!("column_{i}")).collect();
    let row = Tuple::new((0..200).map(Value::Int4).collect());
    let rendered = render_table(&DisplaySettings::default(), &columns, &[row.clone(), row], 120);
    for line in rendered.text.lines() {
        assert!(text_width(line) <= 120, "{} wide: {line}", text_width(line));
    }
    assert!(rendered.text.contains("…"), "{}", rendered.text);
    assert!(rendered.text.contains("column_0"), "{}", rendered.text);
    assert!(!rendered.text.contains("column_199"), "{}", rendered.text);
    assert!(rendered.text.contains("columns not shown"), "{}", rendered.text);
    assert!(rendered.text.contains("(2 rows)"), "{}", rendered.text);
}

#[test]
fn a_table_that_fits_has_no_marker() {
    let rendered = render_table(&DisplaySettings::default(), &names(&["a", "b"]), &ints(&[&[1, 2]]), 80);
    assert!(!rendered.text.contains("not shown"), "{}", rendered.text);
    assert!(!rendered.text.contains('…'), "{}", rendered.text);
}

#[test]
fn maxrows_prints_head_tail_and_footer_only_when_set() {
    let rows: Vec<Tuple> = (0..100).map(|i| Tuple::new(vec![Value::Int4(i)])).collect();
    let columns = names(&["n"]);

    let capped = DisplaySettings {
        maxrows: 10,
        ..DisplaySettings::default()
    };
    let rendered = render_table(&capped, &columns, &rows, 80);
    assert_eq!(rendered.rows_formatted, 10);
    assert!(
        rendered
            .text
            .contains("(100 rows; the first 5 and the last 10 are shown)")
            || rendered
                .text
                .contains("(100 rows; the first 5 and the last 5 are shown)"),
        "{}",
        rendered.text
    );
    for shown in ["0", "4", "95", "99"] {
        assert!(
            rendered.text.lines().any(|l| l.trim_matches(&['│', ' '][..]) == shown),
            "{shown}: {}",
            rendered.text
        );
    }
    for hidden in ["5", "50", "94"] {
        assert!(
            !rendered.text.lines().any(|l| l.trim_matches(&['│', ' '][..]) == hidden),
            "{hidden}: {}",
            rendered.text
        );
    }

    let whole = render_table(&DisplaySettings::default(), &columns, &rows, 80);
    assert_eq!(whole.rows_formatted, 100);
    assert!(whole.text.contains("(100 rows)"), "no cap, no cap footer");
    assert!(!whole.text.contains("are shown"));
}

#[test]
fn a_capped_result_does_not_format_the_rows_it_hides() {
    // 100,000 rows by 10 columns, capped at 40: only 40 rows are formatted.
    let rows: Vec<Tuple> = (0..100_000)
        .map(|i| Tuple::new((0..10).map(|c| Value::Int4(i * 10 + c)).collect()))
        .collect();
    let columns: Vec<String> = (0..10).map(|c| format!("c{c}")).collect();
    let settings = DisplaySettings {
        maxrows: 40,
        ..DisplaySettings::default()
    };
    let rendered = render_table(&settings, &columns, &rows, 200);
    assert_eq!(rendered.rows_formatted, 40);
    let expanded = render_expanded(&settings, &columns, &rows);
    assert_eq!(expanded.rows_formatted, 40);
}

#[test]
fn non_terminal_path_is_unbounded_and_unchanged() {
    let columns: Vec<String> = (0..200).map(|i| format!("c{i}")).collect();
    let rows: Vec<Tuple> = (0..60)
        .map(|r| Tuple::new((0..200).map(|c| Value::Int4(r * 200 + c)).collect()))
        .collect();
    let rendered = render(&DisplaySettings::default(), &columns, &rows, Terminal::not_a_terminal());
    assert_eq!(rendered.text, format_results(&rows, &schema(&columns)));
    assert_eq!(rendered.rows_formatted, 60);
    assert!(!rendered.text.contains("not shown"));
    // The old table has a rule between every data row.
    assert!(rendered.text.matches('├').count() >= 60);
}

#[test]
fn a_pipe_obeys_a_setting_the_user_asked_for() {
    let mut settings = DisplaySettings::default();
    settings.pset("format", Some("csv")).unwrap();
    let rendered = render(
        &settings,
        &names(&["a", "b"]),
        &ints(&[&[1, 2]]),
        Terminal::not_a_terminal(),
    );
    assert_eq!(rendered.text, "a,b\n1,2");
}

#[test]
fn expanded_prints_one_block_per_row() {
    let settings = DisplaySettings {
        expanded: Expanded::On,
        ..DisplaySettings::default()
    };
    let rendered = render(
        &settings,
        &names(&["id", "name"]),
        &[
            Tuple::new(vec![Value::Int4(1), Value::String("ann".into())]),
            Tuple::new(vec![Value::Int4(2), Value::String("bob".into())]),
        ],
        Terminal::of(80, 24),
    );
    assert_eq!(
        rendered.text,
        "-[ RECORD 1 ]\nid   | 1\nname | ann\n-[ RECORD 2 ]\nid   | 2\nname | bob"
    );
}

#[test]
fn expanded_auto_switches_only_when_too_wide() {
    let settings = DisplaySettings {
        expanded: Expanded::Auto,
        ..DisplaySettings::default()
    };
    let narrow = render(&settings, &names(&["a", "b"]), &ints(&[&[1, 2]]), Terminal::of(80, 24));
    assert!(narrow.text.contains('│'), "a table: {}", narrow.text);
    assert!(!narrow.text.contains("RECORD"));

    let columns: Vec<String> = (0..30).map(|i| format!("a_long_column_name_{i}")).collect();
    let row = Tuple::new((0..30).map(Value::Int4).collect());
    let wide = render(&settings, &columns, &[row], Terminal::of(80, 24));
    assert!(wide.text.starts_with("-[ RECORD 1 ]"), "{}", wide.text);
}

#[test]
fn pset_null_applies_to_table_and_vertical() {
    let mut settings = DisplaySettings::default();
    settings.pset("null", Some("(null)")).unwrap();
    let rows = vec![Tuple::new(vec![Value::Null, Value::Int4(1)])];
    let columns = names(&["a", "b"]);
    let table = render(&settings, &columns, &rows, Terminal::of(80, 24));
    assert!(table.text.contains("(null)"), "{}", table.text);
    assert!(!table.text.contains("NULL"));
    settings.expanded = Expanded::On;
    let vertical = render(&settings, &columns, &rows, Terminal::of(80, 24));
    assert!(vertical.text.contains("a | (null)"), "{}", vertical.text);
}

#[test]
fn cjk_and_emoji_cells_keep_alignment() {
    assert_eq!(char_width('a'), 1);
    assert_eq!(char_width('日'), 2);
    assert_eq!(char_width('😀'), 2);
    let rows = vec![
        Tuple::new(vec![Value::String("ascii".into()), Value::Int4(1)]),
        Tuple::new(vec![Value::String("日本語のテキスト".into()), Value::Int4(22)]),
        Tuple::new(vec![Value::String("emoji 😀😀".into()), Value::Int4(333)]),
    ];
    let rendered = render_table(&DisplaySettings::default(), &names(&["label", "n"]), &rows, 80);
    let widths: Vec<usize> = rendered
        .text
        .lines()
        .take_while(|l| !l.starts_with('('))
        .map(text_width)
        .collect();
    assert!(widths.len() >= 6, "{}", rendered.text);
    assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}\n{}", rendered.text);
}

#[test]
fn explain_plan_lines_are_not_boxed_per_row() {
    let rows: Vec<Tuple> = (0..19)
        .map(|i| Tuple::new(vec![Value::String(format!("  plan node {i}"))]))
        .collect();
    let rendered = render_table(&DisplaySettings::default(), &names(&["QUERY PLAN"]), &rows, 120);
    let rules = rendered.text.lines().filter(|l| l.contains('─')).count();
    assert_eq!(rules, 3, "top, under the header, bottom:\n{}", rendered.text);
    assert_eq!(rendered.text.lines().filter(|l| l.contains("plan node")).count(), 19);
}

#[test]
fn numbers_are_right_aligned_and_text_left() {
    let rows = vec![
        Tuple::new(vec![Value::Int4(1), Value::String("x".into())]),
        Tuple::new(vec![Value::Int4(1000), Value::String("yyyy".into())]),
    ];
    let rendered = render_table(&DisplaySettings::default(), &names(&["n", "label"]), &rows, 80);
    assert!(rendered.text.contains("│    1 │ x     │"), "{}", rendered.text);
    assert!(rendered.text.contains("│ 1000 │ yyyy  │"), "{}", rendered.text);
}

#[test]
fn a_wide_cell_is_cut_with_a_mark() {
    let settings = DisplaySettings {
        maxwidth: 10,
        ..DisplaySettings::default()
    };
    let rows = vec![Tuple::new(vec![Value::String("abcdefghijklmnopqrstuvwxyz".into())])];
    let rendered = render_table(&settings, &names(&["s"]), &rows, 80);
    assert!(rendered.text.contains("abcdefghi…"), "{}", rendered.text);
    assert!(!rendered.text.contains("jklmn"));
}

#[test]
fn a_multi_line_cell_stays_inside_its_row() {
    let rows = vec![Tuple::new(vec![Value::String("one\ntwo".into()), Value::Int4(5)])];
    let rendered = render_table(&DisplaySettings::default(), &names(&["text", "n"]), &rows, 80);
    let lines: Vec<&str> = rendered.text.lines().collect();
    assert!(lines.iter().any(|l| l.contains("│ one  │")), "{}", rendered.text);
    assert!(lines.iter().any(|l| l.contains("│ two  │")), "{}", rendered.text);
    let widths: Vec<usize> = lines
        .iter()
        .take_while(|l| !l.starts_with('('))
        .map(|l| text_width(l))
        .collect();
    assert!(widths.iter().all(|w| *w == widths[0]), "{}", rendered.text);
}

#[test]
fn tuples_only_drops_header_and_footer() {
    let settings = DisplaySettings {
        tuples_only: true,
        ..DisplaySettings::default()
    };
    let rendered = render_table(&settings, &names(&["a"]), &ints(&[&[7]]), 80);
    assert!(!rendered.text.contains('a') || rendered.text.contains('7'));
    assert!(!rendered.text.contains("(1 row)"), "{}", rendered.text);
    assert!(rendered.text.contains('7'));
}

#[test]
fn border_styles() {
    let rows = ints(&[&[1, 2]]);
    let columns = names(&["a", "b"]);
    let mut settings = DisplaySettings::default();
    settings.pset("border", Some("1")).unwrap();
    let one = render_table(&settings, &columns, &rows, 80);
    assert!(one.text.contains(" a | b"), "{}", one.text);
    assert!(!one.text.contains('┌'));
    settings.pset("border", Some("0")).unwrap();
    let zero = render_table(&settings, &columns, &rows, 80);
    assert!(!zero.text.contains('|') && !zero.text.contains('│'), "{}", zero.text);
}

#[test]
fn pset_validates_its_options() {
    let mut settings = DisplaySettings::default();
    assert!(settings.pset("border", Some("5")).is_err());
    assert!(settings.pset("maxrows", Some("many")).is_err());
    assert!(settings.pset("format", Some("xml")).is_err());
    assert!(settings.pset("nonsense", Some("1")).is_err());
    assert!(!settings.touched, "a refused setting changes nothing");
    settings.pset("format", Some("json")).unwrap();
    assert_eq!(settings.format, OutputFormat::Json);
    assert!(settings.touched);
    settings.pset("pager", Some("off")).unwrap();
    assert!(!settings.pager);
    settings.pset("pager", None).unwrap();
    assert!(settings.pager, "no value toggles");
}

#[test]
fn the_commands_parse() {
    assert_eq!(MetaCommand::parse("\\x"), Some(MetaCommand::Expanded(None)));
    assert_eq!(
        MetaCommand::parse("\\x on"),
        Some(MetaCommand::Expanded(Some("on".into())))
    );
    assert_eq!(
        MetaCommand::parse("\\x auto"),
        Some(MetaCommand::Expanded(Some("auto".into())))
    );
    assert_eq!(
        MetaCommand::parse("\\pset null '(null)'"),
        Some(MetaCommand::Pset {
            option: "null".into(),
            value: Some("(null)".into())
        })
    );
    assert_eq!(
        MetaCommand::parse("\\pset border 1"),
        Some(MetaCommand::Pset {
            option: "border".into(),
            value: Some("1".into())
        })
    );
    assert_eq!(MetaCommand::parse("\\t"), Some(MetaCommand::TuplesOnly(None)));
    assert_eq!(
        MetaCommand::parse("\\t off"),
        Some(MetaCommand::TuplesOnly(Some(false)))
    );
    assert_eq!(MetaCommand::parse("\\a"), Some(MetaCommand::Aligned));
    assert_eq!(MetaCommand::parse("\\pset"), None);
}
