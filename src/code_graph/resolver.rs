//! Phase 1 in-file resolver: given a file's symbols + unresolved
//! textual refs, try to rebind each ref to a symbol index.
//!
//! Phase 2 extends this with imports tables and cross-file matching.
//! Resolution failure is NOT fatal — we record `resolution =
//! 'unresolved'` so downstream consumers can still use the dangling
//! edge for grep-style queries.

use super::symbols::{Symbol, SymbolRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRef {
    pub from_idx: usize,
    /// `Some(idx)` if we rebound to a symbol in the same file;
    /// `None` if the name was not found.
    pub to_idx: Option<usize>,
    pub to_name: String,
    pub kind_str: &'static str,
    pub line: u32,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Exact,
    Heuristic,
    Unresolved,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Exact => "exact",
            Resolution::Heuristic => "heuristic",
            Resolution::Unresolved => "unresolved",
        }
    }
}

/// Resolve every ref against the symbol list (same file only).
///
/// Strategy:
/// 1. If the ref's textual target matches a symbol's `name` exactly
///    AND only one symbol has that name → `Exact`.
/// 2. Multiple matches on `name` → take the first, mark `Heuristic`.
/// 3. Otherwise → `Unresolved`.
pub fn resolve_in_file(symbols: &[Symbol], refs: &[SymbolRef]) -> Vec<ResolvedRef> {
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        // Strip trailing `()` and any qualifier — we match on the rightmost segment.
        let bare = last_segment(&r.to_name);
        let matches: Vec<usize> = symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name == bare)
            .map(|(i, _)| i)
            .collect();
        let (to_idx, resolution) = match matches.len() {
            0 => (None, Resolution::Unresolved),
            1 => (Some(matches[0]), Resolution::Exact),
            _ => (Some(matches[0]), Resolution::Heuristic),
        };
        out.push(ResolvedRef {
            from_idx: r.from_idx,
            to_idx,
            to_name: r.to_name.clone(),
            kind_str: r.kind.as_str(),
            line: r.line,
            resolution,
        });
    }
    out
}

fn last_segment(name: &str) -> &str {
    // Strip trailing `()` if present, then split on `.` or `::`.
    let bare = name.trim_end_matches(')');
    let bare = bare.split('(').next().unwrap_or(bare);
    if let Some(idx) = bare.rfind("::") {
        return &bare[idx + 2..];
    }
    if let Some(idx) = bare.rfind('.') {
        return &bare[idx + 1..];
    }
    bare
}

/// Scope-chain rebinder (FR-7 §189): for every unresolved CALLS /
/// REFERENCES ref, consult the file's IMPORTS edges. If exactly one
/// import path ends in the unresolved bare name (after the right
/// language-specific separator), promote the ref's `to_name` to the
/// fully-qualified import path so the cross-file rebinder can hit
/// it.  Resolution is upgraded from `Unresolved` / `Heuristic` to
/// `Exact` only when the import match is unambiguous.
///
/// Recognised separators:
/// * Rust  — `::`  (`use foo::bar;`)
/// * Python / TS / Go — `.` (`from foo import bar` becomes
///   `foo.bar` once the extractor canonicalises imports; for
///   pre-canonicalised imports we also accept the bare name as a
///   trailing match.)
pub fn rebind_via_imports(refs: &mut [ResolvedRef]) {
    // Snapshot every IMPORTS to_name (skip already-resolved /
    // already-imports rows).  Treat IMPORTS as scope members of
    // the file: a single matching import wins.
    let imports: Vec<String> = refs
        .iter()
        .filter(|r| r.kind_str == "IMPORTS")
        .map(|r| r.to_name.clone())
        .collect();
    if imports.is_empty() {
        return;
    }
    for r in refs.iter_mut() {
        if r.kind_str == "IMPORTS" {
            continue;
        }
        if matches!(r.resolution, Resolution::Exact) {
            continue;
        }
        let bare = last_segment(&r.to_name).to_string();
        if bare.is_empty() {
            continue;
        }
        let mut candidates: Vec<&str> = Vec::new();
        for imp in &imports {
            if matches_import(imp, &bare) {
                candidates.push(imp.as_str());
            }
        }
        match candidates.len() {
            0 => {}
            1 => {
                r.to_name = candidates[0].to_string();
                r.resolution = Resolution::Exact;
            }
            _ => {
                // Ambiguous: keep heuristic but still rewrite to the
                // first candidate so the cross-file pass has a
                // qualified name to chase.
                r.to_name = candidates[0].to_string();
                if matches!(r.resolution, Resolution::Unresolved) {
                    r.resolution = Resolution::Heuristic;
                }
            }
        }
    }
}

fn matches_import(import_path: &str, bare: &str) -> bool {
    if import_path == bare {
        return true;
    }
    if let Some(stripped) = import_path.strip_suffix(bare) {
        let prev = stripped.as_bytes().last().copied();
        // Boundary must be a separator: `::`, `.`, or `/` (Go style).
        if matches!(prev, Some(b':') | Some(b'.') | Some(b'/')) {
            // For `::`, also reject single-colon false positives.
            if prev == Some(b':') {
                let len = stripped.len();
                if len < 2 || stripped.as_bytes().get(len - 2) != Some(&b':') {
                    return false;
                }
            }
            return true;
        }
    }
    false
}

#[cfg(test)]
mod imports_tests {
    use super::*;

    #[test]
    fn matches_rust_use_path() {
        assert!(matches_import("foo::bar", "bar"));
        assert!(matches_import("std::collections::HashMap", "HashMap"));
        assert!(!matches_import("foobar", "bar"));
        assert!(!matches_import("foo:bar", "bar")); // single colon ≠ `::`
    }

    #[test]
    fn matches_python_dot_path() {
        assert!(matches_import("foo.bar", "bar"));
        assert!(matches_import("a.b.c.bar", "bar"));
        assert!(!matches_import("foo_bar", "bar"));
    }

    #[test]
    fn rebinds_unambiguous_import() {
        let mut refs = vec![
            ResolvedRef {
                from_idx: 0,
                to_idx: None,
                to_name: "bar".into(),
                kind_str: "CALLS",
                line: 5,
                resolution: Resolution::Unresolved,
            },
            ResolvedRef {
                from_idx: 0,
                to_idx: None,
                to_name: "foo::bar".into(),
                kind_str: "IMPORTS",
                line: 1,
                resolution: Resolution::Unresolved,
            },
        ];
        rebind_via_imports(&mut refs);
        assert_eq!(refs[0].to_name, "foo::bar");
        assert_eq!(refs[0].resolution, Resolution::Exact);
    }

    #[test]
    fn ambiguous_import_falls_back_to_heuristic() {
        let mut refs = vec![
            ResolvedRef {
                from_idx: 0,
                to_idx: None,
                to_name: "bar".into(),
                kind_str: "CALLS",
                line: 5,
                resolution: Resolution::Unresolved,
            },
            ResolvedRef {
                from_idx: 0,
                to_idx: None,
                to_name: "foo::bar".into(),
                kind_str: "IMPORTS",
                line: 1,
                resolution: Resolution::Unresolved,
            },
            ResolvedRef {
                from_idx: 0,
                to_idx: None,
                to_name: "qux::bar".into(),
                kind_str: "IMPORTS",
                line: 2,
                resolution: Resolution::Unresolved,
            },
        ];
        rebind_via_imports(&mut refs);
        // First candidate wins (deterministic), but resolution
        // stays heuristic to flag the ambiguity.
        assert_eq!(refs[0].to_name, "foo::bar");
        assert_eq!(refs[0].resolution, Resolution::Heuristic);
    }
}

/// Phase-2 cross-file rebinder. For each symbol ref, if the local
/// in-file resolver came up empty, try to match by last-segment name
/// against every other file's symbol table. Returns the corpus-wide
/// symbol_id (not the in-file index).
///
/// Caller supplies a `corpus` map `name → Vec<symbol_id>`. Multiple
/// matches → first wins, `resolution = 'heuristic'`.
pub fn rebind_cross_file(
    resolved: &mut [ResolvedRef],
    in_file_symbol_ids: &[i64],
    corpus: &std::collections::HashMap<String, Vec<i64>>,
    out_xfile: &mut Vec<(usize, i64)>,
) {
    for (idx, r) in resolved.iter_mut().enumerate() {
        if r.to_idx.is_some() {
            continue;
        }
        let bare = last_segment(&r.to_name);
        if let Some(candidates) = corpus.get(bare) {
            if let Some(first) = candidates.first().copied() {
                // Don't let a ref bind to a symbol that's actually
                // in the same file under a different local index —
                // that case was already handled above.
                if !in_file_symbol_ids.contains(&first) {
                    out_xfile.push((idx, first));
                    r.resolution = if candidates.len() == 1 {
                        Resolution::Exact
                    } else {
                        Resolution::Heuristic
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::Language;
    use super::super::symbols::{Symbol, SymbolKind, SymbolRef, SymbolRefKind, Visibility};
    use super::*;

    fn mk_sym(name: &str) -> Symbol {
        Symbol {
            name: name.into(),
            qualified: name.into(),
            kind: SymbolKind::Function,
            signature: format!("fn {name}"),
            visibility: Visibility::Public,
            line_start: 1,
            line_end: 1,
            byte_start: 0,
            byte_end: 1,
            parent_idx: None,
        }
    }

    #[test]
    fn resolves_single_match_exact() {
        let syms = vec![mk_sym("foo"), mk_sym("bar")];
        let refs = vec![SymbolRef {
            from_idx: 0,
            to_name: "bar".into(),
            kind: SymbolRefKind::Calls,
            line: 2,
            byte_start: 10,
            byte_end: 13,
        }];
        let out = resolve_in_file(&syms, &refs);
        assert_eq!(out[0].to_idx, Some(1));
        assert_eq!(out[0].resolution, Resolution::Exact);
    }

    #[test]
    fn unresolved_when_missing() {
        let syms = vec![mk_sym("foo")];
        let refs = vec![SymbolRef {
            from_idx: 0,
            to_name: "doesnotexist".into(),
            kind: SymbolRefKind::Calls,
            line: 2,
            byte_start: 0,
            byte_end: 1,
        }];
        let out = resolve_in_file(&syms, &refs);
        assert_eq!(out[0].to_idx, None);
        assert_eq!(out[0].resolution, Resolution::Unresolved);
    }

    #[test]
    fn last_segment_strips_qualifier_and_parens() {
        assert_eq!(last_segment("Foo::bar"), "bar");
        assert_eq!(last_segment("mod::Foo::bar"), "bar");
        assert_eq!(last_segment("self.foo"), "foo");
        assert_eq!(last_segment("bar(x, y)"), "bar");
    }

    #[test]
    fn language_enum_usable_in_resolver_tests() {
        // Sanity compile — the Language re-export should be available
        // under the module path used by resolver::tests once the module
        // is referenced from here. (Ensures `use super::super::parse::Language`
        // keeps compiling if feature gates change.)
        let _ = Language::Rust;
    }
}
