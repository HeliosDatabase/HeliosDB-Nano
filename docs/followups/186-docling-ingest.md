# Task 186 — Docling content-conversion ingestion

## Goal

Land first-class ingestion of binary content formats (PDF, DOCX,
PPTX, audio, images) into the graph-rag corpus by integrating with
[docling-serve](https://github.com/docling-project/docling-serve) —
the official HTTP API for the
[docling](https://github.com/docling-project/docling) document
conversion toolkit.

Nano stays embedded; docling-serve runs out-of-process in a
container. Nano POSTs raw bytes (or a URL), gets a structured
DoclingDocument JSON back, walks its sections, and feeds them into
the existing `ingest_docs` pipeline.

## Acceptance

```rust
db.graph_rag_ingest_pdf(&IngestPdfOptions {
    source_path: Path::new("./paper.pdf"),
    docling_endpoint: "http://localhost:5001/v1/convert/source".into(),
    auth_bearer: None,
})?;
```

After the call:
* `_hdb_graph_nodes` carries one `DocSection` per section in the
  PDF, plus one `DocChunk` per paragraph-level child.
* Section hierarchy preserved via `CONTAINS` edges.
* Page numbers + bounding boxes stashed in the `extra` JSON column.
* `helios_graphrag_search` finds content from the PDF.

## Architecture

```
              ┌──────────────────┐
PDF/MP3/...   │   Nano process    │     HTTP             ┌────────────────┐
─────────────►│ ingest_pdf(...)   ├────POST /v1/convert──►│ docling-serve  │
              │                   │                       │ (sidecar       │
              │ DoclingDocument   │◄───── JSON ───────────│  container)    │
              │   parser          │                       └────────────────┘
              │ ─►ingest_docs     │
              └──────────────────┘
```

## Design

### New module `src/graph_rag/docling.rs`

```rust
pub struct IngestPdfOptions {
    pub source_path: Option<PathBuf>,   // local file
    pub source_url:  Option<String>,    // OR remote URL
    pub source_bytes: Option<Vec<u8>>,  // OR in-memory bytes
    pub docling_endpoint: String,        // e.g. http://localhost:5001/v1/convert/source
    pub auth_bearer: Option<String>,
    pub timeout: Option<Duration>,
    pub corpus_kind: String,             // node_kind for the root document, default "Document"
}

pub fn ingest_pdf(db: &EmbeddedDatabase, opts: &IngestPdfOptions) -> Result<IngestStats>;
pub fn ingest_office(db: &EmbeddedDatabase, opts: &IngestPdfOptions) -> Result<IngestStats>;
pub fn ingest_audio(db: &EmbeddedDatabase, opts: &IngestPdfOptions) -> Result<IngestStats>;
pub fn ingest_image(db: &EmbeddedDatabase, opts: &IngestPdfOptions) -> Result<IngestStats>;
```

The four helpers share a single `convert_via_docling(opts)` →
`DoclingDocument` core and differ only in default `corpus_kind` /
`mime_type` hints.

### docling-serve request shape

```http
POST /v1/convert/source
Content-Type: application/json

{
  "sources": [
    { "kind": "http", "url": "https://..." }
    | { "kind": "file", "filename": "...", "data_base64": "..." }
  ],
  "to_formats": ["json"]
}
```

Response (abbreviated):

```json
{
  "documents": [{
    "name": "paper.pdf",
    "json_content": {
      "schema_name": "DoclingDocument",
      "version": "1.0.0",
      "body": { "children": [{ "$ref": "#/texts/0" }, ...] },
      "texts": [
        { "self_ref": "#/texts/0", "label": "section_header", "text": "Abstract", "prov": [{ "page_no": 1, "bbox": {...} }] },
        ...
      ]
    }
  }]
}
```

### DoclingDocument → graph-rag mapping

| DoclingDocument node | `_hdb_graph_nodes.node_kind` | `extra` JSON |
|--|--|--|
| `body` (root) | `Document` | `{ "name": "paper.pdf" }` |
| `section_header` | `DocSection` | `{ "page_no", "bbox", "level" }` |
| `text` (paragraph) | `DocChunk` | `{ "page_no", "bbox" }` |
| `table` | `DocTable` | `{ "page_no", "html": "..." }` |
| `picture` | `DocFigure` | `{ "page_no", "caption" }` |

Edges:
* `CONTAINS`  Document → DocSection → DocChunk
* `CITES`     emitted by the existing entity linker (downstream)

## Cargo

* New optional dep: `reqwest` already present.
* `base64` already present (used elsewhere).
* No tree-sitter / grammar deps changed.
* Feature flag: keep under existing `graph-rag`. No new flag —
  docling integration is just a pluggable HTTP client; if the
  user doesn't have docling-serve running, the call fails clearly
  with a connection error.

## Files to touch

* `src/graph_rag/docling.rs` — new
* `src/graph_rag/mod.rs` — re-exports
* `src/lib.rs` — forwarders: `graph_rag_ingest_pdf`,
  `graph_rag_ingest_office`, `graph_rag_ingest_audio`,
  `graph_rag_ingest_image`
* `tests/graph_rag_docling.rs` — uses a mock HTTP server (existing
  pattern) + a recorded DoclingDocument response.

## Tests

1. Mock docling-serve returns a recorded DoclingDocument with two
   sections and three paragraphs → ingest produces 1 + 2 + 3 graph
   nodes and the matching CONTAINS edges.
2. Connection refused → `IngestStats::default()` not silently
   returned; error surfaces through Result.
3. Audio path: mock returns a transcript-only DoclingDocument →
   ingest produces a single `DocChunk` with the transcript.
4. Bytes input mode: ingest_pdf with `source_bytes` posts as
   base64-encoded `data` field.

## Out of scope

- Embedding the docling pipeline in-process. The Rust port would
  re-implement layout / OCR / VLM models and is multi-quarter.
- Auto-spawn of docling-serve container. Caller manages the
  service lifecycle.
- Streaming conversion (large multi-document batch). Single-doc
  per call.
