//! The lists of the feature-truth tests: `tests/feature_truth/<name>.txt`.
//!
//! A list file has sections (`[name]`) and one entry per line: fields separated by
//! ` | `, the first being the key. `#` starts a comment line. The lists are data so that
//! a package that finishes something removes one line and nothing else.

use std::collections::BTreeMap;

/// One line of a list: the key and the fields after it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub fields: Vec<String>,
}

impl Entry {
    pub fn field(&self, n: usize) -> &str {
        self.fields.get(n).map(String::as_str).unwrap_or("")
    }
}

pub fn path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/feature_truth")
        .join(format!("{name}.txt"))
}

/// Section name -> entries, in file order. Panics on a line outside a section, on a
/// key listed twice in one section, and on an entry with fewer than `min_fields` fields
/// after the key (every entry names at least its owner).
pub fn load(name: &str, min_fields: usize) -> BTreeMap<String, Vec<Entry>> {
    let file = path(name);
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let mut out: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let mut section: Option<String> = None;
    let mut seen = std::collections::BTreeSet::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(name.to_string());
            out.entry(name.to_string()).or_default();
            continue;
        }
        let Some(section) = &section else {
            panic!("{}:{}: an entry before any [section]", file.display(), n + 1)
        };
        let mut parts = line.split(" | ").map(|p| p.trim().to_string());
        let key = parts.next().unwrap_or_default();
        let fields: Vec<String> = parts.collect();
        assert!(
            fields.len() >= min_fields && fields.iter().take(min_fields).all(|f| !f.is_empty()),
            "{}:{}: `{key}` needs {min_fields} more field(s) (owner first)",
            file.display(),
            n + 1
        );
        assert!(
            seen.insert((section.clone(), key.clone())),
            "{}:{}: `{key}` is listed twice in [{section}]",
            file.display(),
            n + 1
        );
        out.entry(section.clone()).or_default().push(Entry { key, fields });
    }
    out
}

/// The entries of one section; the section must exist in the file.
pub fn section<'a>(lists: &'a BTreeMap<String, Vec<Entry>>, name: &str) -> &'a [Entry] {
    lists
        .get(name)
        .unwrap_or_else(|| panic!("the list file has no [{name}] section"))
}
