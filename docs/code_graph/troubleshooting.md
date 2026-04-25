# Code-graph troubleshooting

Common pilot-deployment gotchas + fixes.

## `MCP connection refused`

The agent (Claude Code / Cursor / etc.) can't reach the MCP
endpoint.

* Check the binary path in your `claude.json`. The installer
  emits an absolute path; copy it verbatim.
* Re-build with the right features:
  ```sh
  cargo build --release --features code-graph,graph-rag,mcp-endpoint
  ```
  A binary built without `mcp-endpoint` has no `mcp-server`
  subcommand.
* If you bound to a non-loopback address, you need
  `McpAuth::Jwt(...)` configured — `bind_safety_check` refuses
  the bind otherwise.

## `lsp_definition` returns nothing

* Was `code_index` actually run? Pilot uses
  `heliosdb-nano code-graph index --table src`.
* Is `lang` set on the source rows? The indexer's per-row
  `Language::from_lang_str` only recognises canonical names
  (`rust`, `python`, `typescript`, `tsx`, `javascript`, `go`,
  `markdown`, `sql`). Custom languages need
  `register_grammar` + `register_extractor` (see
  [#179](../followups/179-grammar-loader.md) /
  [#183](../followups/183-extractor-registry.md)).
* Run `SELECT name FROM hdb_code_languages` to confirm the
  language tag is recognised.

## `body_vec NULL`

The embedder isn't configured. Either:

* Pass `embed_endpoint = "http://..."` to the indexer for the
  external HTTP embedder.
* Build with `--features code-embed` and pass a
  `FastEmbedder` for in-process inference (see
  [#187](../followups/187-code-embed.md)).

The default no-op embedder writes `body_vec = NULL` and BM25 +
hybrid retrieval still work — just no vector-aware features.

## Auto-reparse fires too often

`auto_reparse = true` is on by default in the pilot config.
Every `INSERT/UPDATE/DELETE` on the source table triggers a
reparse pass; the content-hash gate skips unchanged files but
the dispatch still costs a few ms per write. Disable when
bulk-importing:

```sql
SELECT hdb_code.pause('src_ast_index');
-- bulk imports here...
SELECT hdb_code.resume('src_ast_index');
```

## Logs

```sh
HELIOS_LOG=debug ./bin/heliosdb-nano mcp-server --db .helios-nano/data 2>./helios.log
```

Useful filters:

```
HELIOS_LOG=heliosdb_nano::code_graph=trace,heliosdb_nano::mcp=debug
```

## Re-indexing from scratch

Drop and rebuild:

```sql
DROP TABLE _hdb_code_symbol_refs;
DROP TABLE _hdb_code_symbols;
DROP TABLE _hdb_code_files;
DROP TABLE IF EXISTS _hdb_code_merkle;
```

Then run `code_index` again. The catalog re-creates them
idempotently.

## Catalog / version mismatch

If you upgraded the binary but kept the data directory and see
"column not found" errors, the schema may have drifted. Easiest
fix: drop the `_hdb_code_*` and `_hdb_graph_*` tables and re-run
the indexer.
