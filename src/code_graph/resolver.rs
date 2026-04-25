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

/// Constraint-propagation pass (FR §189-extended): looks at the
/// surrounding function body of each unresolved CALLS / REFERENCES
/// ref and tries to bind the *receiver* name to a type, then
/// rewrites the ref's `to_name` to `Type::method`.
///
/// Strictly cheaper than full HM:
/// * Only handles syntactically explicit annotations
///   (`let x: Foo = …`, `: Foo` parameter types) and `Type::new`-
///   shaped initialisers (`let x = Foo::new(...)`).
/// * Per-language regex-driven; bound to the function's textual
///   range from the symbol metadata caller passes in.
/// * Generics, traits, impl-blocks, and lifetime params are left
///   alone — those pin to `Heuristic` and downstream rebinding
///   takes its chances.
///
/// Works hand-in-hand with [`rebind_via_imports`]: the type name
/// resulting from this pass is fed back through imports and the
/// cross-file rebinder.
pub fn rebind_via_local_types(
    refs: &mut [ResolvedRef],
    function_bodies: &[FunctionBody<'_>],
) {
    if function_bodies.is_empty() {
        return;
    }
    for r in refs.iter_mut() {
        if r.kind_str == "IMPORTS" {
            continue;
        }
        if matches!(r.resolution, Resolution::Exact) {
            continue;
        }
        // Look at the receiver: `x.method` or `x.method(...)`.
        let to_name = r.to_name.clone();
        let bare = last_segment(&to_name);
        let parent = receiver_segment(&to_name);
        let Some(parent) = parent else { continue };
        // Find a function body that owns this ref (by line).
        let Some(body) = function_bodies
            .iter()
            .find(|f| r.line >= f.line_start && r.line <= f.line_end)
        else {
            continue;
        };
        // Try to type-bind the receiver name.
        if let Some(ty) = body.type_of(parent) {
            r.to_name = format!("{ty}::{bare}");
            // Don't pin Exact yet — the cross-file pass will lift
            // this once it matches a qualified symbol.  Promote
            // from Unresolved to Heuristic so callers see we got
            // somewhere.
            if matches!(r.resolution, Resolution::Unresolved) {
                r.resolution = Resolution::Heuristic;
            }
        }
    }
}

/// Function body the local-type resolver inspects. Caller emits
/// one per indexed function symbol; the `body_text` is the raw
/// source slice between `byte_start` / `byte_end`.
#[derive(Debug, Clone)]
pub struct FunctionBody<'a> {
    pub line_start: u32,
    pub line_end: u32,
    pub body_text: &'a str,
}

impl<'a> FunctionBody<'a> {
    /// Look up `name`'s declared type via syntactic `let name:
    /// Type = …` / `let name = Type::new(...)` / `let name =
    /// Type { … }` patterns.  None when no declarative shape
    /// matches.
    pub fn type_of(&self, name: &str) -> Option<&str> {
        let body = self.body_text;
        // Pattern 1: `let <name>: <type> = …`
        if let Some(t) = scan_let_typed(body, name) {
            return Some(t);
        }
        // Pattern 2: `let <name> = <Type>::new(...)`
        if let Some(t) = scan_let_assoc_call(body, name) {
            return Some(t);
        }
        // Pattern 3: `let <name> = <Type> { … }`  (struct literal)
        if let Some(t) = scan_let_struct_literal(body, name) {
            return Some(t);
        }
        None
    }
}

fn receiver_segment(qualified: &str) -> Option<&str> {
    // `x.method` → Some("x");   `Foo::bar` → None (already typed);
    // `bar` → None.
    let bare = qualified.trim_end_matches(')');
    let bare = bare.split('(').next().unwrap_or(bare);
    if bare.contains("::") {
        return None;
    }
    let dot = bare.rfind('.')?;
    let head = &bare[..dot];
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

fn scan_let_typed<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    // Match `let <name>: <Type>` or `let mut <name>: <Type>`.
    // <Type> stops at the first `=`, `;`, `,`, or whitespace
    // that's not part of a generic.  We strip generic args entirely.
    let mut search_start = 0usize;
    while let Some(pos) = body[search_start..].find("let ") {
        let abs = search_start + pos;
        let after = &body[abs + 4..];
        let after_mut = after.strip_prefix("mut ").unwrap_or(after);
        let after_trim = after_mut.trim_start();
        if let Some(rest) = after_trim.strip_prefix(name) {
            let rest_t = rest.trim_start();
            if let Some(after_colon) = rest_t.strip_prefix(':') {
                let after_colon = after_colon.trim_start();
                let mut end = 0usize;
                let bytes = after_colon.as_bytes();
                while end < bytes.len() {
                    let b = bytes[end];
                    if b == b'=' || b == b';' || b == b',' || b == b'\n' {
                        break;
                    }
                    end += 1;
                }
                let ty = after_colon[..end].trim();
                let ty = ty.split('<').next().unwrap_or(ty).trim();
                let ty = ty.trim_end_matches('&').trim();
                if !ty.is_empty() {
                    return Some(unsafe {
                        std::mem::transmute::<&str, &'a str>(ty)
                    });
                }
            }
        }
        search_start = abs + 4;
    }
    None
}

fn scan_let_assoc_call<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    // `let <name> = <Type>::<method>(...)` — receiver Type comes
    // from the LHS of `::`.
    let needle = format!("let {name} = ");
    let pos = body.find(&needle)?;
    let after = &body[pos + needle.len()..];
    let after_amp = after.trim_start_matches('&').trim();
    let cc = after_amp.find("::")?;
    let ty = after_amp[..cc].trim();
    let ty = ty.split_whitespace().next_back()?;
    if ty.is_empty() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<&str, &'a str>(ty) })
    }
}

fn scan_let_struct_literal<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("let {name} = ");
    let pos = body.find(&needle)?;
    let after = &body[pos + needle.len()..];
    let after_amp = after.trim_start_matches('&').trim();
    // Look for `Type {`  — first whitespace separator before `{`.
    let brace = after_amp.find('{')?;
    let head = after_amp[..brace].trim();
    // Reject `if` / `match` / `unsafe` etc.
    if head.is_empty() || head.contains('(') || head.contains('=') {
        return None;
    }
    let ty = head.split_whitespace().next_back()?;
    let ty = ty.trim_end_matches(',').trim();
    if ty.is_empty() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<&str, &'a str>(ty) })
    }
}

#[cfg(test)]
mod local_types_tests {
    use super::*;

    fn body(text: &'static str) -> FunctionBody<'static> {
        FunctionBody {
            line_start: 1,
            line_end: 100,
            body_text: text,
        }
    }

    #[test]
    fn binds_let_with_explicit_type() {
        let b = body("let x: Foo = bar();\nx.method();\n");
        assert_eq!(b.type_of("x"), Some("Foo"));
    }

    #[test]
    fn binds_let_with_assoc_call() {
        let b = body("let x = Foo::new();\nx.method();\n");
        assert_eq!(b.type_of("x"), Some("Foo"));
    }

    #[test]
    fn binds_let_struct_literal() {
        let b = body("let x = Foo { a: 1, b: 2 };\nx.do();\n");
        assert_eq!(b.type_of("x"), Some("Foo"));
    }

    #[test]
    fn ignores_anonymous_locals() {
        let b = body("let _ = bar();\n");
        assert!(b.type_of("y").is_none());
    }

    #[test]
    fn rebinds_method_call_through_local_type() {
        let mut refs = vec![ResolvedRef {
            from_idx: 0,
            to_idx: None,
            to_name: "x.method".into(),
            kind_str: "CALLS",
            line: 5,
            resolution: Resolution::Unresolved,
        }];
        let bodies = vec![FunctionBody {
            line_start: 1,
            line_end: 10,
            body_text: "let x: Foo = bar();\nx.method();",
        }];
        rebind_via_local_types(&mut refs, &bodies);
        assert_eq!(refs[0].to_name, "Foo::method");
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
