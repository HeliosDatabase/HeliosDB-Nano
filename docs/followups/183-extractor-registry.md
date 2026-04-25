# Task 183 — Symbol-extractor pluggability for runtime grammars

## Goal

Today `register_grammar(name, lang)` lets callers parse arbitrary
languages, but the indexer extracts zero symbols for them — the
extract pipeline keys off the static `Language` enum. This task
plugs in a parallel registry: callers register a `dyn SymbolExtractor`
alongside the grammar, the indexer consults it for unknown
languages.

## Acceptance

```rust
db.register_grammar("cobol", cobol_lang);
db.register_extractor("cobol", Box::new(MyCobolExtractor));
db.code_index(opts.for_table("cobol_src"))?;
// _hdb_code_symbols now contains cobol symbols
```

## Design

* New trait in `src/code_graph/symbols.rs`:
  ```rust
  pub trait SymbolExtractor: Send + Sync {
      fn extract(&self, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<SymbolRef>);
  }
  ```
  with two thin implementations: one wrapping the existing
  `extract(lang, source, tree)` for static languages, and the
  trait-object path for runtime ones.
* Process-static `RwLock<HashMap<String, Arc<dyn SymbolExtractor>>>`
  keyed by language tag, mirroring the grammar registry layout.
* Three forwarding methods on `EmbeddedDatabase`:
  `register_extractor`, `unregister_extractor`,
  `registered_extractors`.
* Indexer flow: when `Language::from_lang_str` returns `None` but
  a runtime grammar exists, look up the matching extractor; if
  absent, emit zero symbols (current behaviour) and a warn-level
  trace.

## Files to touch

* `src/code_graph/symbols.rs` — trait definition.
* `src/code_graph/parse.rs` — extractor registry alongside grammar
  registry.
* `src/code_graph/storage.rs` — indexer dispatch.
* `src/lib.rs` — forwarding methods.
* New test: `tests/code_graph_extractor_registry.rs`.

## Tests

1. Indexer for an unregistered runtime language emits zero symbols
   (current behaviour; regression guard).
2. After registering a passthrough extractor that wraps a Rust
   extractor under tag `"cobol"`, indexing rust source with
   `lang = 'cobol'` emits the expected symbols.
3. `registered_extractors()` reflects insert/remove.

## Out of scope

- Actual COBOL / Swift / etc. extractors. The shipping deliverable
  is the registration mechanism. Concrete extractors live in
  external crates per the plugin model.
