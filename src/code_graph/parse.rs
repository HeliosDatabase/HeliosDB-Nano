//! Tree-sitter driver: take a source string + language tag, return a
//! parsed tree. Thin wrapper so callers can stay language-agnostic.

use crate::{Error, Result};

/// Languages the phase 1 MVP understands.
///
/// Additional languages (TypeScript, Go, SQL, Markdown) ship in phase 2
/// — their grammar crates are gated by the same `code-graph` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
}

impl Language {
    /// Parse the language string as it appears in a user row. Accepts
    /// canonical names, a few common aliases, and the same strings
    /// that appear in the `lang` column of `_hdb_code_files`.
    pub fn from_lang_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Language::Rust),
            "python" | "py" => Some(Language::Python),
            _ => None,
        }
    }

    /// Canonical lower-case name. Stable across releases; used as the
    /// `lang` value written into `_hdb_code_files`.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
        }
    }
}

/// Parse `source` under `lang`. Errors surface as `Error::query_execution`
/// so callers can surface them through the regular result-set channel.
pub fn parse(lang: Language, source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    let ts_lang = grammar_for(lang);
    parser
        .set_language(&ts_lang)
        .map_err(|e| Error::query_execution(format!("tree-sitter set_language failed: {e}")))?;
    parser
        .parse(source, None)
        .ok_or_else(|| Error::query_execution("tree-sitter parse returned None"))
}

fn grammar_for(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_parses_smoke() {
        let src = "fn main() { println!(\"hi\"); }";
        let tree = parse(Language::Rust, src).expect("parse");
        assert!(tree.root_node().kind() == "source_file");
    }

    #[test]
    fn python_parses_smoke() {
        let src = "def main():\n    print('hi')\n";
        let tree = parse(Language::Python, src).expect("parse");
        assert!(tree.root_node().kind() == "module");
    }

    #[test]
    fn unknown_language_str_returns_none() {
        assert!(Language::from_lang_str("cobol").is_none());
        assert_eq!(Language::from_lang_str("RS"), Some(Language::Rust));
    }
}
