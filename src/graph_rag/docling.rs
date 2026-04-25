//! Content-conversion ingestion via [docling-serve], the official
//! HTTP API for the [docling] document toolkit.
//!
//! [docling]:       https://github.com/docling-project/docling
//! [docling-serve]: https://github.com/docling-project/docling-serve
//!
//! Nano runs embedded; docling-serve runs out-of-process (typically
//! a Docker / Podman container). For each binary content blob (PDF /
//! DOCX / PPTX / image / audio) the adapter:
//!
//!  1. POSTs `/v1/convert/source` with the bytes (base64) or a URL.
//!  2. Receives a JSON `DoclingDocument` describing the structure.
//!  3. Walks the structure, emitting one `Document` root node, one
//!     `DocSection` per heading, and one `DocChunk` per paragraph
//!     into `_hdb_graph_nodes`.  `CONTAINS` edges preserve hierarchy.
//!
//! The full DoclingDocument schema is large; we only parse the
//! fields we project (texts + body navigation).  Unknown fields are
//! skipped via serde defaults so a docling-serve upgrade doesn't
//! break us.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{EmbeddedDatabase, Error, Result, Value};

use super::ingest::IngestStats;
use super::schema::ensure_tables;

#[derive(Debug, Clone, Default)]
pub struct DoclingIngestOptions {
    /// Either supply `source_url`, `source_path`, or `source_bytes`.
    pub source_url: Option<String>,
    pub source_path: Option<std::path::PathBuf>,
    pub source_bytes: Option<Vec<u8>>,
    /// Optional override of the filename docling-serve sees;
    /// inferred from `source_path` if absent.
    pub filename: Option<String>,
    /// docling-serve `/v1/convert/source` endpoint URL.
    /// Default: `http://localhost:5001/v1/convert/source`.
    pub docling_endpoint: String,
    /// Optional bearer token if the docling-serve instance is
    /// auth-gated.
    pub auth_bearer: Option<String>,
    /// `node_kind` for the root document node.  Different per
    /// modality so callers can filter (e.g. "Pdf" vs "Email").
    pub corpus_kind: String,
    /// HTTP timeout in milliseconds. Default 60_000 — docling
    /// conversions can be slow on first model load.
    pub timeout_ms: u64,
}

impl DoclingIngestOptions {
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self {
            source_path: Some(path.as_ref().to_path_buf()),
            docling_endpoint: default_endpoint(),
            corpus_kind: "Document".into(),
            timeout_ms: 60_000,
            ..Self::default()
        }
    }
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            source_url: Some(url.into()),
            docling_endpoint: default_endpoint(),
            corpus_kind: "Document".into(),
            timeout_ms: 60_000,
            ..Self::default()
        }
    }
    pub fn with_endpoint(mut self, ep: impl Into<String>) -> Self {
        self.docling_endpoint = ep.into();
        self
    }
    pub fn with_corpus_kind(mut self, k: impl Into<String>) -> Self {
        self.corpus_kind = k.into();
        self
    }
}

fn default_endpoint() -> String {
    "http://localhost:5001/v1/convert/source".to_string()
}

// ── docling-serve wire types (subset) ──────────────────────────────────

#[derive(Serialize)]
struct ConvertRequest {
    sources: Vec<Source>,
    to_formats: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Source {
    Http {
        url: String,
    },
    File {
        filename: String,
        #[serde(rename = "data_base64")]
        data_base64: String,
    },
}

#[derive(Deserialize, Debug)]
struct ConvertResponse {
    #[serde(default)]
    documents: Vec<ConvertedDocument>,
}

#[derive(Deserialize, Debug)]
struct ConvertedDocument {
    #[serde(default)]
    name: Option<String>,
    /// `json_content` is a `DoclingDocument` blob.
    json_content: DoclingDocument,
}

/// DoclingDocument subset: each text element plus a flat reference
/// list. The full schema includes pictures, tables, figures, etc.;
/// we project the ones we recognise and skip the rest.
#[derive(Deserialize, Debug)]
struct DoclingDocument {
    #[serde(default)]
    texts: Vec<TextItem>,
    #[serde(default)]
    tables: Vec<TableItem>,
}

#[derive(Deserialize, Debug)]
struct TextItem {
    #[serde(default)]
    self_ref: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    prov: Vec<Provenance>,
    #[serde(default)]
    level: Option<i32>,
}

#[derive(Deserialize, Debug)]
struct TableItem {
    #[serde(default)]
    self_ref: Option<String>,
    #[serde(default)]
    prov: Vec<Provenance>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct Provenance {
    #[serde(default)]
    page_no: Option<i64>,
}

// ── Public entry points (one per modality) ────────────────────────────

pub fn ingest_pdf(db: &EmbeddedDatabase, opts: &DoclingIngestOptions) -> Result<IngestStats> {
    ingest_with_kind(db, opts, "Pdf")
}

pub fn ingest_office(db: &EmbeddedDatabase, opts: &DoclingIngestOptions) -> Result<IngestStats> {
    // DOCX / PPTX / XLSX — same path as PDF, different default kind.
    ingest_with_kind(db, opts, "Office")
}

pub fn ingest_audio(db: &EmbeddedDatabase, opts: &DoclingIngestOptions) -> Result<IngestStats> {
    // docling's ASR pipeline returns text-only DoclingDocuments;
    // the projection still produces DocChunk rows under the
    // Audio root.
    ingest_with_kind(db, opts, "Audio")
}

pub fn ingest_image(db: &EmbeddedDatabase, opts: &DoclingIngestOptions) -> Result<IngestStats> {
    ingest_with_kind(db, opts, "Image")
}

fn ingest_with_kind(
    db: &EmbeddedDatabase,
    opts: &DoclingIngestOptions,
    default_kind: &str,
) -> Result<IngestStats> {
    ensure_tables(db)?;
    let kind = if opts.corpus_kind == "Document" {
        default_kind.to_string()
    } else {
        opts.corpus_kind.clone()
    };
    let response = call_docling_serve(opts)?;
    let mut stats = IngestStats::default();
    let Some(converted) = response.documents.into_iter().next() else {
        return Ok(stats);
    };
    project_document(db, &kind, &converted, &mut stats)?;
    Ok(stats)
}

fn call_docling_serve(opts: &DoclingIngestOptions) -> Result<ConvertResponse> {
    let source = build_source(opts)?;
    let body = ConvertRequest {
        sources: vec![source],
        to_formats: vec!["json".to_string()],
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(opts.timeout_ms))
        .build()
        .map_err(|e| Error::query_execution(format!("docling client: {e}")))?;
    let mut req = client.post(&opts.docling_endpoint).json(&body);
    if let Some(tok) = &opts.auth_bearer {
        req = req.bearer_auth(tok);
    }
    let resp = req
        .send()
        .map_err(|e| Error::query_execution(format!("docling request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::query_execution(format!(
            "docling-serve returned HTTP {}",
            resp.status()
        )));
    }
    let parsed: ConvertResponse = resp
        .json()
        .map_err(|e| Error::query_execution(format!("docling response: {e}")))?;
    Ok(parsed)
}

fn build_source(opts: &DoclingIngestOptions) -> Result<Source> {
    if let Some(url) = &opts.source_url {
        return Ok(Source::Http { url: url.clone() });
    }
    if let Some(bytes) = &opts.source_bytes {
        let filename = opts
            .filename
            .clone()
            .unwrap_or_else(|| "document.bin".into());
        use base64::{engine::general_purpose::STANDARD as B64, Engine};
        return Ok(Source::File {
            filename,
            data_base64: B64.encode(bytes),
        });
    }
    if let Some(path) = &opts.source_path {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::query_execution(format!("read {}: {e}", path.display())))?;
        let filename = opts.filename.clone().unwrap_or_else(|| {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("document.bin")
                .to_string()
        });
        use base64::{engine::general_purpose::STANDARD as B64, Engine};
        return Ok(Source::File {
            filename,
            data_base64: B64.encode(bytes),
        });
    }
    Err(Error::query_execution(
        "DoclingIngestOptions requires source_url, source_path, or source_bytes",
    ))
}

fn project_document(
    db: &EmbeddedDatabase,
    kind: &str,
    converted: &ConvertedDocument,
    stats: &mut IngestStats,
) -> Result<()> {
    let name = converted
        .name
        .as_deref()
        .unwrap_or("converted-document");
    let root_ref = format!("docling:document:{name}");
    let root_id = upsert_node(
        db,
        kind,
        &root_ref,
        Some(name),
        None,
        None,
    )?;
    stats.nodes_added = stats.nodes_added.saturating_add(1);
    stats.rows_seen = stats.rows_seen.saturating_add(1);

    let mut current_section: Option<i64> = None;
    for (i, t) in converted.json_content.texts.iter().enumerate() {
        let label = t.label.as_deref().unwrap_or("text").to_ascii_lowercase();
        let text_body = t.text.clone().unwrap_or_default();
        if text_body.trim().is_empty() {
            continue;
        }
        let self_ref = t
            .self_ref
            .clone()
            .unwrap_or_else(|| format!("docling:text:{i}"));
        let extra = serde_json::json!({
            "page_no": t.prov.first().and_then(|p| p.page_no),
            "level": t.level,
        })
        .to_string();
        if label.contains("section_header") || label.contains("title") {
            let id = upsert_node(
                db,
                "DocSection",
                &format!("docling:section:{self_ref}"),
                Some(&text_body),
                None,
                Some(&extra),
            )?;
            stats.nodes_added = stats.nodes_added.saturating_add(1);
            insert_edge(db, root_id, id, "CONTAINS", 1.0)?;
            stats.edges_added = stats.edges_added.saturating_add(1);
            current_section = Some(id);
        } else {
            let id = upsert_node(
                db,
                "DocChunk",
                &format!("docling:chunk:{self_ref}"),
                None,
                Some(&text_body),
                Some(&extra),
            )?;
            stats.nodes_added = stats.nodes_added.saturating_add(1);
            let parent = current_section.unwrap_or(root_id);
            insert_edge(db, parent, id, "CONTAINS", 1.0)?;
            stats.edges_added = stats.edges_added.saturating_add(1);
        }
    }

    // Tables: opaque structured payloads; project as DocTable with
    // page_no metadata.  Caller can post-process via SQL.
    for (i, table) in converted.json_content.tables.iter().enumerate() {
        let self_ref = table
            .self_ref
            .clone()
            .unwrap_or_else(|| format!("docling:table:{i}"));
        let extra = serde_json::json!({
            "page_no": table.prov.first().and_then(|p| p.page_no),
        })
        .to_string();
        let id = upsert_node(
            db,
            "DocTable",
            &format!("docling:table:{self_ref}"),
            None,
            None,
            Some(&extra),
        )?;
        stats.nodes_added = stats.nodes_added.saturating_add(1);
        insert_edge(db, root_id, id, "CONTAINS", 1.0)?;
        stats.edges_added = stats.edges_added.saturating_add(1);
    }
    Ok(())
}

fn upsert_node(
    db: &EmbeddedDatabase,
    kind: &str,
    source_ref: &str,
    title: Option<&str>,
    text: Option<&str>,
    extra: Option<&str>,
) -> Result<i64> {
    // Idempotent: if a node with the same source_ref already exists,
    // return its id; otherwise insert.
    let existing = db.query_params(
        "SELECT node_id FROM _hdb_graph_nodes WHERE source_ref = $1",
        &[Value::String(source_ref.to_string())],
    )?;
    if let Some(row) = existing.first() {
        if let Some(v) = row.values.first() {
            return Ok(match v {
                Value::Int4(n) => i64::from(*n),
                Value::Int8(n) => *n,
                _ => 0,
            });
        }
    }
    let title_v = title
        .map(|s| Value::String(s.to_string()))
        .unwrap_or(Value::Null);
    let text_v = text
        .map(|s| Value::String(s.to_string()))
        .unwrap_or(Value::Null);
    let extra_v = extra
        .map(|s| Value::String(s.to_string()))
        .unwrap_or(Value::Null);
    let (_, rows) = db.execute_params_returning(
        "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text, extra) \
         VALUES ($1, $2, $3, $4, $5) RETURNING node_id",
        &[
            Value::String(kind.to_string()),
            Value::String(source_ref.to_string()),
            title_v,
            text_v,
            extra_v,
        ],
    )?;
    Ok(rows
        .first()
        .and_then(|r| r.values.first())
        .map(|v| match v {
            Value::Int4(n) => i64::from(*n),
            Value::Int8(n) => *n,
            _ => 0,
        })
        .unwrap_or(0))
}

fn insert_edge(
    db: &EmbeddedDatabase,
    from: i64,
    to: i64,
    kind: &str,
    weight: f32,
) -> Result<()> {
    db.execute(&format!(
        "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
         VALUES ({from}, {to}, '{kind}', {weight})"
    ))?;
    Ok(())
}
