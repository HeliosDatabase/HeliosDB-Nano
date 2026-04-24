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
