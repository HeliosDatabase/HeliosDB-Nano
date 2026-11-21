//! Running a parsed script against an interface, counting, classifying the failures,
//! and the result document, the page and the baseline check built from the counts.

use super::engines::Engine;
use super::script::{compare, render_rows, Kind, Mismatch, Record, SortMode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub timed_out: u64,
    pub crashed: u64,
}

impl Counts {
    pub fn add(&mut self, other: &Counts) {
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self.timed_out += other.timed_out;
        self.crashed += other.crashed;
    }

    pub fn total(&self) -> u64 {
        self.passed + self.failed + self.skipped + self.timed_out + self.crashed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Example {
    pub file: String,
    pub line: usize,
    pub sql: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureClass {
    pub count: u64,
    pub example: Example,
}

/// What one script produced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOutcome {
    pub counts: Counts,
    pub classes: BTreeMap<String, FailureClass>,
}

impl FileOutcome {
    fn fail(&mut self, class: String, file: &str, record: &Record, detail: String) {
        self.counts.failed += 1;
        let entry = self.classes.entry(class).or_insert_with(|| FailureClass {
            count: 0,
            example: Example {
                file: file.to_string(),
                line: record.line,
                sql: clip(&record.sql, 400),
                detail: clip(&detail, 400),
            },
        });
        entry.count += 1;
    }

    pub fn merge(&mut self, other: &FileOutcome) {
        self.counts.add(&other.counts);
        for (class, theirs) in &other.classes {
            match self.classes.get_mut(class) {
                Some(mine) => mine.count += theirs.count,
                None => {
                    self.classes.insert(class.clone(), theirs.clone());
                }
            }
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// An error message with its variable parts taken out, so that one cause is one class:
/// quoted text and numbers become placeholders, whitespace is collapsed, and the text
/// is cut to a fixed length.
pub fn normalise_error(message: &str) -> String {
    let mut out = String::new();
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                for d in chars.by_ref() {
                    if d == c {
                        break;
                    }
                }
                out.push('?');
            }
            d if d.is_ascii_digit() => {
                while chars.peek().is_some_and(|n| n.is_ascii_digit() || *n == '.') {
                    chars.next();
                }
                out.push('N');
            }
            w if w.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            other => out.push(other),
        }
    }
    clip(out.trim(), 90)
}

fn mismatch_class(m: &Mismatch, sort: SortMode, values: &[String], expected: &[String]) -> (String, String) {
    match m {
        Mismatch::ColumnCount(d) => ("wrong result: number of columns".to_string(), d.clone()),
        Mismatch::ValueCount { expected, got } => (
            "wrong result: number of values".to_string(),
            format!("{got} values returned, {expected} expected"),
        ),
        Mismatch::Values {
            index,
            expected: want,
            got,
        } => {
            let mut a: Vec<&String> = values.iter().collect();
            let mut b: Vec<&String> = expected.iter().collect();
            a.sort();
            b.sort();
            let class = if sort == SortMode::NoSort && a == b {
                "wrong result: the right values in another order"
            } else if got == "NULL" || want == "NULL" {
                "wrong result: NULL on one side only"
            } else {
                "wrong result: different values"
            };
            (class.to_string(), format!("value {index}: got {got}, expected {want}"))
        }
        Mismatch::Hash { expected, got } => (
            "wrong result: digest differs, same number of values".to_string(),
            format!("got {got}, expected {expected}"),
        ),
        Mismatch::Label { label } => (
            "wrong result: differs from the earlier result under the same label".to_string(),
            format!("label {label}"),
        ),
    }
}

/// Run every record of a script. `skip` names records (by line) that an earlier attempt
/// could not get through, with the count they go to (`timed_out` or `crashed`);
/// `on_record` is told each record's line before it starts.
pub fn run_script(
    engine: &mut dyn Engine,
    engine_label: &str,
    file: &str,
    records: &[Record],
    skip: &HashMap<usize, String>,
    on_record: &mut dyn FnMut(usize),
) -> FileOutcome {
    let mut out = FileOutcome::default();
    let mut hash_threshold = 0usize;
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut halted = false;
    for record in records {
        let countable = matches!(record.kind, Kind::Statement { .. } | Kind::Query { .. });
        if halted || !record.applies_to(engine_label) {
            if countable {
                out.counts.skipped += 1;
            }
            continue;
        }
        if let Some(kind) = skip.get(&record.line) {
            if countable {
                if kind == "crashed" {
                    out.counts.crashed += 1;
                } else {
                    out.counts.timed_out += 1;
                }
            }
            continue;
        }
        on_record(record.line);
        match &record.kind {
            Kind::HashThreshold(n) => hash_threshold = *n,
            Kind::Halt => halted = true,
            Kind::Statement { expect_ok } => match (engine.statement(&record.sql), expect_ok) {
                (Ok(()), true) | (Err(_), false) => out.counts.passed += 1,
                (Err(e), true) => out.fail(format!("statement refused: {}", normalise_error(&e)), file, record, e),
                (Ok(()), false) => out.fail(
                    "statement accepted where the script expects an error".to_string(),
                    file,
                    record,
                    String::new(),
                ),
            },
            Kind::Query {
                types,
                sort,
                label,
                expected,
            } => match engine.query(&record.sql) {
                Err(e) => out.fail(format!("query refused: {}", normalise_error(&e)), file, record, e),
                Ok(rows) => match render_rows(&rows, types, *sort) {
                    Err(d) => {
                        let (class, detail) = mismatch_class(&Mismatch::ColumnCount(d), *sort, &[], expected);
                        out.fail(class, file, record, detail);
                    }
                    Ok(values) => match compare(&values, expected, label.as_deref(), hash_threshold, &mut labels) {
                        Ok(()) => out.counts.passed += 1,
                        Err(m) => {
                            let (class, detail) = mismatch_class(&m, *sort, &values, expected);
                            out.fail(class, file, record, detail);
                        }
                    },
                },
            },
        }
    }
    out
}

/// A record an attempt could not get through: the process ran out of time on it, or died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incident {
    pub file: String,
    pub line: usize,
    /// `timed_out` or `crashed`
    pub kind: String,
    pub sql: String,
    pub detail: String,
}

/// The result document of one run through one interface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub interface: String,
    pub engine_label: String,
    pub corpus: String,
    pub source_revision: String,
    pub files: u64,
    pub totals: Counts,
    pub directories: BTreeMap<String, Counts>,
    /// Every failure class, largest first.
    pub failure_classes: Vec<(String, FailureClass)>,
    pub incidents: Vec<Incident>,
}

impl RunResult {
    /// Fold per-file outcomes (keyed by path relative to the corpus root) into the document.
    pub fn build(
        interface: &str,
        engine_label: &str,
        corpus: &str,
        source_revision: &str,
        files: &BTreeMap<String, FileOutcome>,
        incidents: Vec<Incident>,
    ) -> Self {
        let mut all = FileOutcome::default();
        let mut directories: BTreeMap<String, Counts> = BTreeMap::new();
        for (path, outcome) in files {
            all.merge(outcome);
            let dir = match path.rfind('/') {
                Some(at) => path.get(..at).unwrap_or(".").to_string(),
                None => ".".to_string(),
            };
            directories.entry(dir).or_default().add(&outcome.counts);
        }
        let mut failure_classes: Vec<(String, FailureClass)> = all.classes.into_iter().collect();
        failure_classes.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
        Self {
            interface: interface.to_string(),
            engine_label: engine_label.to_string(),
            corpus: corpus.to_string(),
            source_revision: source_revision.to_string(),
            files: files.len() as u64,
            totals: all.counts,
            directories,
            failure_classes,
            incidents,
        }
    }
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        "n/a".to_string()
    } else {
        format!("{:.2}%", part as f64 * 100.0 / whole as f64)
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// The page, built from result documents only.
pub fn render_page(results: &[RunResult]) -> String {
    let mut p = String::new();
    p.push_str("# SQL Logic Test results\n\n");
    p.push_str(
        "Generated by `scripts/slt_run.sh` from the result documents next to this page. \
         Do not edit by hand.\n\n\
         The corpus is the public SQL Logic Test suite, unchanged. Each record is a statement \
         that must succeed or fail, or a query whose values must match. A record counts as \
         *skipped* when the corpus marks it as not for the engine label the interface runs \
         under, *timed out* when it did not finish within the per-record limit, and \
         *crashed* when the process running its file died on it. The percentage is passed \
         over passed plus failed plus timed out plus crashed.\n\n",
    );
    for r in results {
        p.push_str(&format!(
            "## Interface `{}` (engine label `{}`)\n\n",
            r.interface, r.engine_label
        ));
        p.push_str(&format!(
            "Corpus: {}. Source revision: `{}`. Files: {}.\n\n",
            r.corpus, r.source_revision, r.files
        ));
        p.push_str("| Directory | Passed | Failed | Skipped | Timed out | Crashed | Pass rate |\n");
        p.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
        let mut rows: Vec<(&str, &Counts)> = r.directories.iter().map(|(d, c)| (d.as_str(), c)).collect();
        rows.push(("**all**", &r.totals));
        for (dir, c) in rows {
            p.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                dir,
                c.passed,
                c.failed,
                c.skipped,
                c.timed_out,
                c.crashed,
                percent(c.passed, c.total() - c.skipped)
            ));
        }
        p.push_str("\n### The ten largest failure classes\n\n");
        if r.failure_classes.is_empty() {
            p.push_str("None.\n\n");
        }
        for (n, (class, f)) in r.failure_classes.iter().take(10).enumerate() {
            p.push_str(&format!("{}. **{}**: {} records\n\n", n + 1, cell(class), f.count));
            p.push_str(&format!("   Example: `{}` line {}\n\n", f.example.file, f.example.line));
            p.push_str("   ```sql\n");
            for line in f.example.sql.lines() {
                p.push_str(&format!("   {line}\n"));
            }
            p.push_str("   ```\n\n");
            if !f.example.detail.is_empty() {
                p.push_str(&format!("   {}\n\n", f.example.detail.replace('\n', " ")));
            }
        }
        if r.failure_classes.len() > 10 {
            let rest: u64 = r.failure_classes.iter().skip(10).map(|(_, f)| f.count).sum();
            p.push_str(&format!(
                "{} further classes hold {} records; the result document lists all of them.\n\n",
                r.failure_classes.len() - 10,
                rest
            ));
        }
        p.push_str("### Records that timed out or crashed\n\n");
        if r.incidents.is_empty() {
            p.push_str("None.\n\n");
        }
        for i in r.incidents.iter().take(50) {
            p.push_str(&format!(
                "- {} at `{}` line {}: `{}` {}\n",
                i.kind,
                i.file,
                i.line,
                cell(&clip(&i.sql, 160)),
                cell(&i.detail)
            ));
        }
        if r.incidents.len() > 50 {
            p.push_str(&format!(
                "- and {} more in the result document\n",
                r.incidents.len() - 50
            ));
        }
        p.push('\n');
    }
    p
}

/// Pass counts per interface and directory that a later run must not go below.
pub type Baseline = BTreeMap<String, BTreeMap<String, u64>>;

/// Every directory whose pass count fell below the baseline, or that the baseline has
/// and the run lacks. Empty means the check passes.
pub fn check_baseline(baseline: &Baseline, result: &RunResult) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(dirs) = baseline.get(&result.interface) {
        for (dir, want) in dirs {
            match result.directories.get(dir) {
                None => problems.push(format!(
                    "{} {dir}: in the baseline with {want} passed, missing from the run",
                    result.interface
                )),
                Some(c) if c.passed < *want => problems.push(format!(
                    "{} {dir}: {} passed, the baseline has {want}",
                    result.interface, c.passed
                )),
                Some(_) => {}
            }
        }
    }
    problems
}

/// Raise the baseline to a run's pass counts. A directory never goes down.
pub fn raise_baseline(baseline: &mut Baseline, result: &RunResult) {
    let dirs = baseline.entry(result.interface.clone()).or_default();
    for (dir, c) in &result.directories {
        let slot = dirs.entry(dir.clone()).or_insert(0);
        if c.passed > *slot {
            *slot = c.passed;
        }
    }
}
