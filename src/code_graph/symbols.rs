//! Per-language symbol extraction. Walk a tree-sitter parse tree and
//! emit:
//!
//! - `Symbol` — a named definition (function, class, method, struct, …).
//! - `SymbolRef` — a relationship between two symbols (CALLS, REFERENCES, …).
//!
//! Phase 1 implements Rust and Python. Extractors are intentionally
//! simple and heuristic; `resolution = 'heuristic'` is a normal and
//! stable outcome — downstream consumers (`lsp_*`) take the best match.

use super::parse::Language;
use tree_sitter::{Node, Tree, TreeCursor};

/// A named definition within a source file.
///
/// Line numbers are 1-indexed (PG convention). Byte offsets are 0-indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Local (unqualified) name — `foo` in `fn foo()`.
    pub name: String,
    /// Dotted qualified name — `module::Struct::method` on best effort.
    pub qualified: String,
    pub kind: SymbolKind,
    /// Signature text verbatim (first line of the definition).
    pub signature: String,
    pub visibility: Visibility,
    pub line_start: u32,
    pub line_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    /// Index of the enclosing symbol in the emitted list (for nested
    /// items). `None` = top-level.
    pub parent_idx: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Trait,
    Impl,
    Enum,
    Type,
    Module,
    Const,
    Var,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Enum => "enum",
            SymbolKind::Type => "type",
            SymbolKind::Module => "module",
            SymbolKind::Const => "const",
            SymbolKind::Var => "var",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Crate,
    Module,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Crate => "crate",
            Visibility::Module => "module",
        }
    }
}

/// A directed relationship between two symbols.
///
/// `from` / `to` are **local names or qualified paths** — the
/// cross-file resolver (phase 1 in-file only; phase 2 expands) turns
/// these into `node_id`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRef {
    pub from_idx: usize,
    pub to_name: String,
    pub kind: SymbolRefKind,
    pub line: u32,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolRefKind {
    Calls,
    References,
    Contains,
    Defines,
    Imports,
}

impl SymbolRefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolRefKind::Calls => "CALLS",
            SymbolRefKind::References => "REFERENCES",
            SymbolRefKind::Contains => "CONTAINS",
            SymbolRefKind::Defines => "DEFINES",
            SymbolRefKind::Imports => "IMPORTS",
        }
    }
}

/// Trait that turns a parsed tree-sitter tree into `(symbols, refs)`.
/// Implemented by each statically-supported language plus by any
/// downstream extractor registered against a runtime grammar via
/// [`register_extractor`].
pub trait SymbolExtractor: Send + Sync {
    fn extract(&self, source: &str, tree: &Tree) -> (Vec<Symbol>, Vec<SymbolRef>);
}

/// Default extractor implementation that delegates to the static
/// language dispatcher in [`extract`].  Useful for callers that
/// want to register a static-language wrapper under a custom name.
pub struct StaticLanguageExtractor {
    pub language: Language,
}

impl SymbolExtractor for StaticLanguageExtractor {
    fn extract(&self, source: &str, tree: &Tree) -> (Vec<Symbol>, Vec<SymbolRef>) {
        extract(self.language, source, tree)
    }
}

/// Process-static registry: language tag → extractor.
fn extractor_registry()
    -> &'static parking_lot::RwLock<std::collections::HashMap<String, std::sync::Arc<dyn SymbolExtractor>>>
{
    static R: std::sync::OnceLock<
        parking_lot::RwLock<std::collections::HashMap<String, std::sync::Arc<dyn SymbolExtractor>>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(|| parking_lot::RwLock::new(std::collections::HashMap::new()))
}

/// Register a custom extractor under a language tag.  Returns the
/// previous registration if any.  Pair with
/// [`crate::code_graph::parse::register_grammar`] under the same tag
/// so the parser and the extractor share resolution.
pub fn register_extractor(
    name: impl Into<String>,
    extractor: std::sync::Arc<dyn SymbolExtractor>,
) -> Option<std::sync::Arc<dyn SymbolExtractor>> {
    extractor_registry().write().insert(name.into(), extractor)
}

/// Drop a registered extractor.
pub fn unregister_extractor(name: &str) -> Option<std::sync::Arc<dyn SymbolExtractor>> {
    extractor_registry().write().remove(name)
}

/// Snapshot of every registered extractor's tag.
pub fn registered_extractors() -> Vec<String> {
    let mut v: Vec<String> = extractor_registry().read().keys().cloned().collect();
    v.sort();
    v
}

/// Look up an extractor by language tag.  Used by the indexer when
/// `Language::from_lang_str` doesn't recognise the row's `lang`
/// column.
pub fn registered_extractor(name: &str) -> Option<std::sync::Arc<dyn SymbolExtractor>> {
    extractor_registry().read().get(name).cloned()
}

/// Extract symbols and in-file references from a parsed tree.
pub fn extract(lang: Language, source: &str, tree: &Tree) -> (Vec<Symbol>, Vec<SymbolRef>) {
    let mut symbols: Vec<Symbol> = Vec::new();
    let mut refs: Vec<SymbolRef> = Vec::new();
    let mut cursor = tree.walk();
    let scope: Vec<String> = Vec::new();
    walk(lang, source, &mut cursor, &scope, None, &mut symbols, &mut refs);

    // Second pass: file-level IMPORTS. Attached to the first
    // top-level symbol so downstream consumers can still resolve
    // `from_symbol`.  If a file has no top-level symbol at all
    // (unusual), imports are dropped.
    if !symbols.is_empty() {
        collect_imports(lang, source, tree, &mut refs);
    }
    (symbols, refs)
}

fn walk(
    lang: Language,
    source: &str,
    cursor: &mut TreeCursor,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
) {
    loop {
        let node = cursor.node();
        let (sym_emitted, new_scope_component, descend_parent_idx) =
            match lang {
                Language::Rust => visit_rust(node, source, scope, parent_idx, symbols, refs),
                Language::Python => visit_python(node, source, scope, parent_idx, symbols, refs),
                Language::TypeScript | Language::Tsx | Language::JavaScript => {
                    visit_typescript(node, source, scope, parent_idx, symbols, refs)
                }
                Language::Go => visit_go(node, source, scope, parent_idx, symbols, refs),
                Language::Markdown => {
                    visit_markdown(node, source, scope, parent_idx, symbols)
                }
                Language::Sql => visit_sql(node, source, scope, parent_idx, symbols),
            };

        // Descend into children with the possibly-updated scope / parent.
        if cursor.goto_first_child() {
            let child_scope: Vec<String> = match &new_scope_component {
                Some(c) => {
                    let mut s = scope.to_vec();
                    s.push(c.clone());
                    s
                }
                None => scope.to_vec(),
            };
            walk(
                lang,
                source,
                cursor,
                &child_scope,
                descend_parent_idx.or(parent_idx),
                symbols,
                refs,
            );
            cursor.goto_parent();
        }
        let _ = sym_emitted;

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Returns `(emitted_symbol?, new_scope_component?, descend_parent_idx?)`.
///
/// * `emitted_symbol` is informational only — caller ignores it today.
/// * `new_scope_component` is pushed onto the scope for descendants.
/// * `descend_parent_idx` overrides the parent index passed down.
fn visit_rust(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
) -> (bool, Option<String>, Option<usize>) {
    let kind = node.kind();
    match kind {
        "function_item" => {
            let (name, sig) = rust_name_and_sig(node, source);
            let vis = rust_visibility(node, source);
            let qualified = make_qualified(scope, &name);
            let idx = push(symbols, Symbol {
                name,
                qualified: qualified.clone(),
                kind: SymbolKind::Function,
                signature: sig,
                visibility: vis,
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
                byte_start: node.start_byte() as u32,
                byte_end: node.end_byte() as u32,
                parent_idx,
            });
            collect_call_refs(source, node, idx, refs);
            (true, Some(qualified), Some(idx))
        }
        "struct_item" => {
            emit_named_block(
                node,
                source,
                SymbolKind::Struct,
                scope,
                parent_idx,
                symbols,
            )
        }
        "enum_item" => emit_named_block(
            node,
            source,
            SymbolKind::Enum,
            scope,
            parent_idx,
            symbols,
        ),
        "trait_item" => emit_named_block(
            node,
            source,
            SymbolKind::Trait,
            scope,
            parent_idx,
            symbols,
        ),
        "impl_item" => {
            // impl <type> or impl <trait> for <type>
            let (name, sig) = rust_impl_header(node, source);
            let qualified = make_qualified(scope, &name);
            let idx = push(symbols, Symbol {
                name: name.clone(),
                qualified: qualified.clone(),
                kind: SymbolKind::Impl,
                signature: sig,
                visibility: Visibility::Module,
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
                byte_start: node.start_byte() as u32,
                byte_end: node.end_byte() as u32,
                parent_idx,
            });
            (true, Some(qualified), Some(idx))
        }
        "mod_item" => emit_named_block(
            node,
            source,
            SymbolKind::Module,
            scope,
            parent_idx,
            symbols,
        ),
        "type_item" => emit_named_block(
            node,
            source,
            SymbolKind::Type,
            scope,
            parent_idx,
            symbols,
        ),
        "const_item" | "static_item" => emit_named_block(
            node,
            source,
            SymbolKind::Const,
            scope,
            parent_idx,
            symbols,
        ),
        _ => (false, None, None),
    }
}

fn emit_named_block(
    node: Node<'_>,
    source: &str,
    kind: SymbolKind,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
) -> (bool, Option<String>, Option<usize>) {
    let name = node
        .child_by_field_name("name")
        .and_then(|n| node_text(n, source).map(str::to_string))
        .unwrap_or_default();
    if name.is_empty() {
        return (false, None, None);
    }
    let qualified = make_qualified(scope, &name);
    let sig = first_line(node_text(node, source).unwrap_or("")).to_string();
    let idx = push(symbols, Symbol {
        name,
        qualified: qualified.clone(),
        kind,
        signature: sig,
        visibility: Visibility::Module,
        line_start: node.start_position().row as u32 + 1,
        line_end: node.end_position().row as u32 + 1,
        byte_start: node.start_byte() as u32,
        byte_end: node.end_byte() as u32,
        parent_idx,
    });
    (true, Some(qualified), Some(idx))
}

fn visit_typescript(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
) -> (bool, Option<String>, Option<usize>) {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            let (emitted, scope_c, idx) = emit_named_block(
                node,
                source,
                SymbolKind::Function,
                scope,
                parent_idx,
                symbols,
            );
            if let Some(i) = idx {
                collect_call_refs(source, node, i, refs);
            }
            (emitted, scope_c, idx)
        }
        "method_definition" => {
            let (emitted, scope_c, idx) = emit_named_block(
                node,
                source,
                SymbolKind::Method,
                scope,
                parent_idx,
                symbols,
            );
            if let Some(i) = idx {
                collect_call_refs(source, node, i, refs);
            }
            (emitted, scope_c, idx)
        }
        "class_declaration" | "abstract_class_declaration" => emit_named_block(
            node,
            source,
            SymbolKind::Class,
            scope,
            parent_idx,
            symbols,
        ),
        "interface_declaration" => emit_named_block(
            node,
            source,
            SymbolKind::Trait,
            scope,
            parent_idx,
            symbols,
        ),
        "type_alias_declaration" => emit_named_block(
            node,
            source,
            SymbolKind::Type,
            scope,
            parent_idx,
            symbols,
        ),
        "enum_declaration" => emit_named_block(
            node,
            source,
            SymbolKind::Enum,
            scope,
            parent_idx,
            symbols,
        ),
        _ => (false, None, None),
    }
}

fn visit_python(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
) -> (bool, Option<String>, Option<usize>) {
    match node.kind() {
        "function_definition" => {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| node_text(n, source).map(str::to_string))
                .unwrap_or_default();
            if name.is_empty() {
                return (false, None, None);
            }
            let qualified = make_qualified(scope, &name);
            let sig = first_line(node_text(node, source).unwrap_or("")).to_string();
            let kind = if scope.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            let idx = push(symbols, Symbol {
                name,
                qualified: qualified.clone(),
                kind,
                signature: sig,
                visibility: Visibility::Module,
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
                byte_start: node.start_byte() as u32,
                byte_end: node.end_byte() as u32,
                parent_idx,
            });
            collect_call_refs(source, node, idx, refs);
            (true, Some(qualified), Some(idx))
        }
        "class_definition" => emit_named_block(
            node,
            source,
            SymbolKind::Class,
            scope,
            parent_idx,
            symbols,
        ),
        _ => (false, None, None),
    }
}

fn visit_go(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
) -> (bool, Option<String>, Option<usize>) {
    match node.kind() {
        "function_declaration" => {
            let (emitted, scope_c, idx) = emit_named_block(
                node,
                source,
                SymbolKind::Function,
                scope,
                parent_idx,
                symbols,
            );
            if let Some(i) = idx {
                collect_call_refs(source, node, i, refs);
            }
            (emitted, scope_c, idx)
        }
        "method_declaration" => {
            let (emitted, scope_c, idx) = emit_named_block(
                node,
                source,
                SymbolKind::Method,
                scope,
                parent_idx,
                symbols,
            );
            if let Some(i) = idx {
                collect_call_refs(source, node, i, refs);
            }
            (emitted, scope_c, idx)
        }
        "type_spec" => {
            // Go: `type Foo struct { ... }` — treat as a Struct if
            // the body is a struct_type, Trait if interface_type,
            // otherwise Type.
            let kind = node
                .child_by_field_name("type")
                .map(|n| match n.kind() {
                    "struct_type" => SymbolKind::Struct,
                    "interface_type" => SymbolKind::Trait,
                    _ => SymbolKind::Type,
                })
                .unwrap_or(SymbolKind::Type);
            emit_named_block(node, source, kind, scope, parent_idx, symbols)
        }
        "const_spec" | "var_spec" => {
            // Take the first identifier as the declaration name.
            let name_node = node.child_by_field_name("name").or_else(|| {
                let n = node.named_child_count();
                (0..n)
                    .filter_map(|i| node.named_child(i))
                    .find(|ch| ch.kind() == "identifier")
            });
            let Some(n) = name_node else {
                return (false, None, None);
            };
            let name = node_text(n, source).unwrap_or_default().to_string();
            if name.is_empty() {
                return (false, None, None);
            }
            let qualified = make_qualified(scope, &name);
            let sig = first_line(node_text(node, source).unwrap_or("")).to_string();
            let kind = if node.kind() == "const_spec" {
                SymbolKind::Const
            } else {
                SymbolKind::Var
            };
            let idx = push(
                symbols,
                Symbol {
                    name: name.clone(),
                    qualified: qualified.clone(),
                    kind,
                    signature: sig,
                    visibility: if name
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_uppercase())
                        .unwrap_or(false)
                    {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                    byte_start: node.start_byte() as u32,
                    byte_end: node.end_byte() as u32,
                    parent_idx,
                },
            );
            (true, Some(qualified), Some(idx))
        }
        _ => (false, None, None),
    }
}

fn visit_markdown(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
) -> (bool, Option<String>, Option<usize>) {
    // Every `atx_heading` / `setext_heading` is a navigable symbol.
    // Heading level is recorded in `kind` so consumers can filter.
    match node.kind() {
        "atx_heading" | "setext_heading" => {
            let text = node_text(node, source).unwrap_or("").trim();
            // Pick the first line after stripping leading `#`s.
            let first = text.lines().next().unwrap_or(text);
            let name = first
                .trim_start_matches('#')
                .trim()
                .trim_end_matches('=')
                .trim_end_matches('-')
                .trim()
                .to_string();
            if name.is_empty() {
                return (false, None, None);
            }
            let qualified = make_qualified(scope, &name);
            let idx = push(
                symbols,
                Symbol {
                    name: name.clone(),
                    qualified: qualified.clone(),
                    kind: SymbolKind::Module,
                    signature: first.to_string(),
                    visibility: Visibility::Public,
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                    byte_start: node.start_byte() as u32,
                    byte_end: node.end_byte() as u32,
                    parent_idx,
                },
            );
            (true, Some(qualified), Some(idx))
        }
        _ => (false, None, None),
    }
}

fn visit_sql(
    node: Node<'_>,
    source: &str,
    scope: &[String],
    parent_idx: Option<usize>,
    symbols: &mut Vec<Symbol>,
) -> (bool, Option<String>, Option<usize>) {
    // The `tree-sitter-sequel` grammar tags statement kinds in the
    // node kind. We surface CREATE TABLE / CREATE VIEW / CREATE
    // FUNCTION / CREATE PROCEDURE as symbols — the common things
    // users navigate to in SQL codebases.
    let (kind, kind_str) = match node.kind() {
        "create_table" => (SymbolKind::Struct, "table"),
        "create_view" | "create_materialized_view" => (SymbolKind::Type, "view"),
        "create_function" => (SymbolKind::Function, "function"),
        "create_procedure" => (SymbolKind::Function, "procedure"),
        _ => return (false, None, None),
    };
    let _ = kind_str;

    // The name is under field "name" on every CREATE variant.
    let name_node = node.child_by_field_name("name").or_else(|| {
        let n = node.named_child_count();
        (0..n).filter_map(|i| node.named_child(i)).find(|ch| {
            let k = ch.kind();
            k == "object_reference" || k == "identifier"
        })
    });
    let Some(n) = name_node else {
        return (false, None, None);
    };
    let name = node_text(n, source).unwrap_or_default().to_string();
    if name.is_empty() {
        return (false, None, None);
    }
    let qualified = make_qualified(scope, &name);
    let sig = first_line(node_text(node, source).unwrap_or("")).to_string();
    let idx = push(
        symbols,
        Symbol {
            name,
            qualified: qualified.clone(),
            kind,
            signature: sig,
            visibility: Visibility::Public,
            line_start: node.start_position().row as u32 + 1,
            line_end: node.end_position().row as u32 + 1,
            byte_start: node.start_byte() as u32,
            byte_end: node.end_byte() as u32,
            parent_idx,
        },
    );
    (true, Some(qualified), Some(idx))
}

fn collect_call_refs(
    source: &str,
    owner: Node<'_>,
    owner_idx: usize,
    refs: &mut Vec<SymbolRef>,
) {
    // Walk the owner's subtree looking for `call_expression` /
    // `call`. Record the called name. Cross-file resolution happens
    // later; here we only capture the textual target.
    let mut c = owner.walk();
    let mut stack: Vec<Node<'_>> = vec![owner];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "call_expression" | "call" => {
                let target = n
                    .child_by_field_name("function")
                    .or_else(|| n.child_by_field_name("callee"))
                    .or_else(|| n.named_child(0))
                    .and_then(|f| node_text(f, source).map(str::to_string))
                    .unwrap_or_default();
                if !target.is_empty() {
                    refs.push(SymbolRef {
                        from_idx: owner_idx,
                        to_name: target,
                        kind: SymbolRefKind::Calls,
                        line: n.start_position().row as u32 + 1,
                        byte_start: n.start_byte() as u32,
                        byte_end: n.end_byte() as u32,
                    });
                }
            }
            _ => {}
        }
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
    }
}

fn rust_name_and_sig(node: Node<'_>, source: &str) -> (String, String) {
    let name = node
        .child_by_field_name("name")
        .and_then(|n| node_text(n, source).map(str::to_string))
        .unwrap_or_default();
    let sig = first_line(node_text(node, source).unwrap_or("")).to_string();
    (name, sig)
}

fn rust_impl_header(node: Node<'_>, source: &str) -> (String, String) {
    let sig = first_line(node_text(node, source).unwrap_or("")).to_string();
    // Strip the trailing `{` if any; keep `impl A for B` as the display name.
    let name = sig
        .trim_end_matches('{')
        .trim()
        .to_string();
    (name, sig)
}

fn rust_visibility(node: Node<'_>, source: &str) -> Visibility {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() == "visibility_modifier" {
            let t = node_text(child, source).unwrap_or("").trim();
            return match t {
                "pub" => Visibility::Public,
                "pub(crate)" => Visibility::Crate,
                s if s.starts_with("pub(") => Visibility::Module,
                _ => Visibility::Private,
            };
        }
    }
    Visibility::Private
}

fn node_text<'a>(n: Node<'_>, src: &'a str) -> Option<&'a str> {
    src.get(n.start_byte()..n.end_byte())
}

fn first_line(s: &str) -> &str {
    s.split(|c| c == '\n' || c == '\r').next().unwrap_or(s).trim_end_matches('{').trim_end()
}

fn make_qualified(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        let mut s = scope.join("::");
        s.push_str("::");
        s.push_str(name);
        s
    }
}

fn push(symbols: &mut Vec<Symbol>, sym: Symbol) -> usize {
    let idx = symbols.len();
    symbols.push(sym);
    idx
}

/// Walk the tree once more looking for import / use / require nodes.
/// Each hit emits an IMPORTS `SymbolRef` anchored at the first
/// top-level symbol (index 0).  Caller has already guaranteed that
/// `symbols` is non-empty.
fn collect_imports(lang: Language, source: &str, tree: &Tree, refs: &mut Vec<SymbolRef>) {
    let root = tree.root_node();
    let mut stack: Vec<Node<'_>> = Vec::with_capacity(64);
    stack.push(root);
    while let Some(n) = stack.pop() {
        let before = refs.len();
        match lang {
            Language::Rust => rust_import(n, source, refs),
            Language::Python => python_import(n, source, refs),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                ts_import(n, source, refs)
            }
            Language::Go => go_import(n, source, refs),
            Language::Markdown | Language::Sql => {} // no imports modelled
        }
        // Only descend into children if the node wasn't handled as an
        // import itself — imports are always top-level or near-top-
        // level, and descending into call expressions costs a lot.
        if refs.len() == before {
            let nc = n.named_child_count();
            for i in 0..nc {
                if let Some(ch) = n.named_child(i) {
                    stack.push(ch);
                }
            }
        }
    }
}

fn rust_import(n: Node<'_>, source: &str, refs: &mut Vec<SymbolRef>) {
    if n.kind() != "use_declaration" {
        return;
    }
    // Take the full path text (after `use `, before `;`) as the
    // target — callers can split as they see fit.
    let full = node_text(n, source).unwrap_or("").trim();
    let target = full
        .trim_start_matches("pub ")
        .trim_start_matches("use ")
        .trim_end_matches(';')
        .trim()
        .to_string();
    if !target.is_empty() {
        refs.push(SymbolRef {
            from_idx: 0,
            to_name: target,
            kind: SymbolRefKind::Imports,
            line: n.start_position().row as u32 + 1,
            byte_start: n.start_byte() as u32,
            byte_end: n.end_byte() as u32,
        });
    }
}

fn python_import(n: Node<'_>, source: &str, refs: &mut Vec<SymbolRef>) {
    match n.kind() {
        "import_statement" | "import_from_statement" => {
            let full = node_text(n, source).unwrap_or("").trim();
            if !full.is_empty() {
                refs.push(SymbolRef {
                    from_idx: 0,
                    to_name: full.to_string(),
                    kind: SymbolRefKind::Imports,
                    line: n.start_position().row as u32 + 1,
                    byte_start: n.start_byte() as u32,
                    byte_end: n.end_byte() as u32,
                });
            }
        }
        _ => {}
    }
}

fn ts_import(n: Node<'_>, source: &str, refs: &mut Vec<SymbolRef>) {
    match n.kind() {
        "import_statement" => {
            let full = node_text(n, source).unwrap_or("").trim();
            if !full.is_empty() {
                refs.push(SymbolRef {
                    from_idx: 0,
                    to_name: full.to_string(),
                    kind: SymbolRefKind::Imports,
                    line: n.start_position().row as u32 + 1,
                    byte_start: n.start_byte() as u32,
                    byte_end: n.end_byte() as u32,
                });
            }
        }
        _ => {}
    }
}

fn go_import(n: Node<'_>, source: &str, refs: &mut Vec<SymbolRef>) {
    if n.kind() != "import_declaration" {
        return;
    }
    let full = node_text(n, source).unwrap_or("").trim();
    if !full.is_empty() {
        refs.push(SymbolRef {
            from_idx: 0,
            to_name: full.to_string(),
            kind: SymbolRefKind::Imports,
            line: n.start_position().row as u32 + 1,
            byte_start: n.start_byte() as u32,
            byte_end: n.end_byte() as u32,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::*;

    #[test]
    fn rust_extracts_top_level_function() {
        let src = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let tree = parse::parse(Language::Rust, src).unwrap();
        let (syms, _refs) = extract(Language::Rust, src, &tree);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "add");
        assert_eq!(syms[0].kind, SymbolKind::Function);
        assert_eq!(syms[0].visibility, Visibility::Public);
        assert_eq!(syms[0].line_start, 1);
    }

    #[test]
    fn rust_extracts_struct_and_impl_method() {
        let src = r#"
pub struct Point { x: i32, y: i32 }
impl Point {
    pub fn zero() -> Point { Point { x: 0, y: 0 } }
}
"#;
        let tree = parse::parse(Language::Rust, src).unwrap();
        let (syms, _refs) = extract(Language::Rust, src, &tree);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"));
        assert!(names.contains(&"zero"));
    }

    #[test]
    fn python_extracts_class_and_method() {
        let src = "class Foo:\n    def bar(self):\n        return 1\n";
        let tree = parse::parse(Language::Python, src).unwrap();
        let (syms, _refs) = extract(Language::Python, src, &tree);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn python_collects_call_refs() {
        let src = "def a():\n    return b()\n\ndef b():\n    return 1\n";
        let tree = parse::parse(Language::Python, src).unwrap();
        let (_syms, refs) = extract(Language::Python, src, &tree);
        assert!(refs.iter().any(|r| r.to_name == "b" && r.kind == SymbolRefKind::Calls));
    }
}
