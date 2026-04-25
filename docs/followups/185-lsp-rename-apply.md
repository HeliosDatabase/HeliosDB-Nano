# Task 185 — `helios_lsp_rename_apply` write-back tool

## Goal

`helios_lsp_rename_preview` returns an edit list. This tool takes
that list (or the same `(symbol_id, new_name)` pair) and writes the
changes back to the source rows so the rename is durable.

The FR's "preview-only by design" stance was conservative — with the
content-hash gate + auto-reparse trigger, applying a rename safely
is straightforward. We ship the apply path explicitly opt-in
(separate tool from preview) so callers that only want preview
behaviour still get it.

## Acceptance

```json
{
  "name": "helios_lsp_rename_apply",
  "arguments": {
    "symbol_id": 42,
    "new_name": "improved_name",
    "dry_run": false
  }
}
```

Returns `{ "files_modified": N, "occurrences_replaced": M, "applied": true }`.

After the call:
* The source table has `old_name → new_name` replacements at the
  identified `(path, line)` sites.
* The auto-reparse trigger has fired for each touched file.
* `lsp_definition('improved_name')` returns the renamed symbol.

## Design

* Reuses the same `lsp_references` collection that
  `lsp_rename_preview` does. For each `(path, line)` site, reads
  the current source row, applies a *line-anchored, word-bounded*
  replacement of `old_name → new_name`, writes the row back via
  `UPDATE`.
* The replacement is identifier-boundary aware (same `is_ident_char`
  check the linker uses) so `foo` doesn't match `foobar`.
* All updates run inside a single transaction. If any update fails,
  the transaction rolls back and the tool returns
  `applied: false` with the partial-progress count.
* `dry_run: true` skips the write-back, just counts what *would*
  have been touched. Useful for paired previewing.
* Conflict detection: before writing, hash each row's content; if
  the hash changed since the preview snapshot, fail loudly so the
  caller doesn't overwrite concurrent edits.

## Files to touch

* `src/code_graph/refactor.rs` — new module with
  `rename_apply(db, symbol_id, new_name, opts)`.
* `src/lib.rs` — forwarding method `lsp_rename_apply`.
* `src/mcp/lsp_tools.rs` — auto-registered MCP tool.
* `src/code_graph/mod.rs` — re-exports.
* New test: `tests/code_graph_rename_apply.rs`.

## Tests

1. Round-trip rename: index source, run apply, query
   `lsp_definition(new_name)` → succeeds.
2. `dry_run: true` returns counts but leaves source unchanged.
3. Concurrent edit detection: simulate a row update between
   preview and apply, confirm apply rejects.
4. Word-boundary correctness: `foo` rename doesn't touch `foobar`,
   `foo_x`, etc.

## Out of scope

- Multi-symbol rename (renaming `Foo` and `FooImpl` together).
- Cross-language rename. Bound to a single symbol_id at a time.
