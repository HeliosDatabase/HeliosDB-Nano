//! Tree-sitter driver: take a source string + language tag, return a
//! parsed tree.
//!
//! Two grammar dispatch lanes coexist:
//!
//! 1. **Static**, built-in grammars compiled into the binary
//!    (`Language::Rust`, `Language::Python`, …) — same shape as
//!    phase 1.
//! 2. **Dynamic**, registered at runtime via [`register_grammar`].
//!    Grammars live in a process-static `RwLock<HashMap<String,
//!    tree_sitter::Language>>` keyed by the language tag the user
//!    will pass through `_hdb_code_files.lang`. The registry
//!    consumes already-built `tree_sitter::Language` values, so the
//!    consumer chooses the loader (a wasm runtime such as
//!    `wasmtime` paired with `tree_sitter::WasmStore`, or
//!    dynamically-linked shared libraries) without dragging
//!    additional weight into Nano's default build.
//!
//! When a parse request arrives the dispatcher consults the dynamic
//! registry first (so an admin can override a built-in if they want
//! a newer grammar version) and falls back to the static set.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

use crate::{Error, Result};

/// Languages the static build knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Markdown,
    Sql,
}

impl Language {
    /// Parse the language string as it appears in a user row.
    ///
    /// Resolution order: built-in canonical names → built-in aliases →
    /// dynamic registry. A dynamic name shadows a built-in only if it
    /// was registered with one of the canonical strings.
    pub fn from_lang_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Language::Rust),
            "python" | "py" => Some(Language::Python),
            "typescript" | "ts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "javascript" | "js" | "mjs" | "cjs" => Some(Language::JavaScript),
            "go" => Some(Language::Go),
            "markdown" | "md" => Some(Language::Markdown),
            "sql" => Some(Language::Sql),
            _ => None,
        }
    }

    /// Canonical lower-case name, stable across releases.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Go => "go",
            Language::Markdown => "markdown",
            Language::Sql => "sql",
        }
    }
}

// ----------------------------------------------------------------------
// Dynamic registry
// ----------------------------------------------------------------------

fn registry() -> &'static RwLock<HashMap<String, tree_sitter::Language>> {
    static R: OnceLock<RwLock<HashMap<String, tree_sitter::Language>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a grammar under a language tag. Subsequent calls to
/// [`parse_by_name`] (and SQL queries that hit `_hdb_code_files.lang`
/// matching `name`) will resolve through this entry first, falling
/// back to the static set if absent.
///
/// Common loader patterns:
///
/// ```ignore
/// // Pattern 1: dynamically-linked native grammar.
/// let lib = libloading::Library::new("/opt/grammars/cobol.so")?;
/// let lang_fn: libloading::Symbol<unsafe extern "C" fn() -> tree_sitter::Language> =
///     unsafe { lib.get(b"tree_sitter_cobol")? };
/// register_grammar("cobol", unsafe { lang_fn() });
///
/// // Pattern 2: WASM grammar via tree-sitter's WasmStore. Requires
/// // tree-sitter to be built with the `wasm` feature and a
/// // `wasmtime`-backed engine; pluggable at the consumer's choice.
/// let wasm_bytes = std::fs::read("/opt/grammars/cobol.wasm")?;
/// let engine = wasmtime::Engine::default();
/// let mut store = tree_sitter::WasmStore::new(&engine)?;
/// let lang = store.load_language("cobol", &wasm_bytes)?;
/// register_grammar("cobol", lang);
/// ```
///
/// Idempotent: re-registering the same tag overwrites the previous
/// entry. Returns the previous registration if any.
pub fn register_grammar(
    name: impl Into<String>,
    grammar: tree_sitter::Language,
) -> Option<tree_sitter::Language> {
    let mut m = registry().write();
    m.insert(name.into(), grammar)
}

/// Drop a registered grammar. Returns the entry that was removed, or
/// `None` if it wasn't registered.
pub fn unregister_grammar(name: &str) -> Option<tree_sitter::Language> {
    let mut m = registry().write();
    m.remove(name)
}

/// Snapshot the registered language tags. Useful for diagnostics
/// and for the SQL-callable `hdb_code.list_grammars()` view.
pub fn registered_grammars() -> Vec<String> {
    let m = registry().read();
    let mut v: Vec<String> = m.keys().cloned().collect();
    v.sort();
    v
}

fn registered_grammar(name: &str) -> Option<tree_sitter::Language> {
    let m = registry().read();
    m.get(name).cloned()
}

// ----------------------------------------------------------------------
// Parse entry points
// ----------------------------------------------------------------------

/// Parse `source` under `lang`. The static grammar dispatch.
pub fn parse(lang: Language, source: &str) -> Result<tree_sitter::Tree> {
    do_parse(grammar_for(lang), source)
}

/// Parse `source` under a language tag. Consults the dynamic registry
/// first, then the canonical [`Language`] table. Returns
/// `Error::query_execution` if no grammar is found.
pub fn parse_by_name(lang_name: &str, source: &str) -> Result<tree_sitter::Tree> {
    if let Some(g) = registered_grammar(lang_name) {
        return do_parse(g, source);
    }
    if let Some(builtin) = Language::from_lang_str(lang_name) {
        return parse(builtin, source);
    }
    Err(Error::query_execution(format!(
        "no tree-sitter grammar registered for language '{lang_name}' \
         (try register_grammar(name, lang))"
    )))
}

fn do_parse(ts_lang: tree_sitter::Language, source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
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
        Language::TypeScript | Language::JavaScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Markdown => tree_sitter_md::LANGUAGE.into(),
        Language::Sql => tree_sitter_sequel::LANGUAGE.into(),
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

    #[test]
    fn parse_by_name_falls_back_to_builtin() {
        let src = "fn main() {}";
        let tree = parse_by_name("rust", src).expect("rust builtin");
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn parse_by_name_uses_registry_first() {
        // Register tree-sitter-rust under a custom name; parsing
        // should succeed with `source_file` as the root kind.
        let lang_name = "rust_alias_for_test";
        let prior = register_grammar(lang_name, tree_sitter_rust::LANGUAGE.into());
        assert!(prior.is_none());

        let tree = parse_by_name(lang_name, "fn main() {}").expect("aliased grammar");
        assert_eq!(tree.root_node().kind(), "source_file");

        let removed = unregister_grammar(lang_name);
        assert!(removed.is_some());
    }

    #[test]
    fn parse_by_name_unknown_errors() {
        let err = parse_by_name("definitely_unknown_grammar", "...").expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("no tree-sitter grammar registered"), "got: {msg}");
    }

    #[test]
    fn registry_overrides_builtin() {
        // Register a Python grammar under "rust" — proves the dynamic
        // registry wins. (Don't ship this in production!)
        let prior = register_grammar("rust", tree_sitter_python::LANGUAGE.into());
        let tree = parse_by_name("rust", "def x():\n    pass\n").expect("registered overrides");
        assert_eq!(tree.root_node().kind(), "module");
        // Restore.
        if let Some(p) = prior {
            register_grammar("rust", p);
        } else {
            unregister_grammar("rust");
        }
    }

    #[test]
    fn registered_grammars_lists_entries() {
        register_grammar("test_listing_a", tree_sitter_rust::LANGUAGE.into());
        register_grammar("test_listing_b", tree_sitter_python::LANGUAGE.into());
        let names = registered_grammars();
        assert!(names.contains(&"test_listing_a".to_string()));
        assert!(names.contains(&"test_listing_b".to_string()));
        unregister_grammar("test_listing_a");
        unregister_grammar("test_listing_b");
    }
}
