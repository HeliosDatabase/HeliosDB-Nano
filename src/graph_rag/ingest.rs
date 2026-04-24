//! Ingestion adapters (FR 4 §4.2).
//!
//! Each adapter takes a **structured** user table — one that the
//! caller has already loaded (via INSERT, COPY, an external parser,
//! etc.) — and projects it into the universal `_hdb_graph_*` schema.
//! The Nano project does not ship raw mbox or JSON-tracker parsers;
//! users either call the adapter against pre-parsed rows or use the
//! optional helper constructors on each adapter's options struct.
//!
//! The four adapters are:
//!
//! * [`ingest_docs`]    — text column → DocSection + DocChunk nodes.
//!                        Splits on Markdown-style heading boundaries
//!                        (`# heading`, `## heading`) when `chunk_by =
//!                        Headings`; otherwise one DocChunk per row.
//!                        `PART_OF` edges connect chunks to their
//!                        section.
//! * [`ingest_email`]   — Email + Person nodes.  AUTHORED_BY /
//!                        REPLIES_TO / SENT_TO edges.
//! * [`ingest_issues`]  — Issue + Comment + Person nodes.
//!                        REPORTED_BY / REPLIES_TO / FIXED_BY edges.
//! * [`ingest_qa`]      — InvestorQuestion + Answer + Person nodes.
//!                        ASKS_ABOUT / ANSWERED_BY edges.
//!
//! Idempotent via `source_ref` keys (`doc:<id>`, `email:<mid>`, …).

use std::collections::{HashMap, HashSet};

use crate::{EmbeddedDatabase, Error, Result, Value};

use super::schema::ensure_tables;

// -- ingest_docs -------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IngestDocsOptions {
    pub source_table: String,
    /// Column that holds the unique stable id per row (path, guid, …).
    pub id_col: String,
    /// Column that holds the full text body.
    pub text_col: String,
    /// Optional column for the title; falls back to the first line of
    /// the body.
    pub title_col: Option<String>,
    /// How to split the body.
    pub chunk_by: ChunkStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkStrategy {
    /// One DocChunk per row, no splitting.
    Row,
    /// Split on Markdown atx headings (`#`, `##`, …).  Each heading
    /// becomes a DocSection; the text beneath becomes a DocChunk
    /// with a `PART_OF` edge into the section.
    Headings,
}

#[derive(Debug, Clone, Default)]
pub struct IngestStats {
    pub nodes_added: u64,
    pub edges_added: u64,
    pub rows_seen: u64,
    pub rows_skipped: u64,
}

pub fn ingest_docs(
    db: &EmbeddedDatabase,
    opts: &IngestDocsOptions,
) -> Result<IngestStats> {
    ensure_tables(db)?;
    let mut stats = IngestStats::default();
    let cols = match &opts.title_col {
        Some(tc) => format!("{id}, {text}, {title}", id = opts.id_col, text = opts.text_col, title = tc),
        None => format!("{id}, {text}", id = opts.id_col, text = opts.text_col),
    };
    let rows = db.query(
        &format!("SELECT {cols} FROM {tbl}", tbl = opts.source_table),
        &[],
    )?;
    // Dedup against what's already projected.
    let existing = source_refs_with_prefix(db, "doc:")?;
    for row in rows {
        stats.rows_seen += 1;
        let id = as_string(row.values.first()).unwrap_or_default();
        if id.is_empty() {
            stats.rows_skipped += 1;
            continue;
        }
        let body = as_string(row.values.get(1)).unwrap_or_default();
        let title = opts
            .title_col
            .as_deref()
            .and_then(|_| as_string(row.values.get(2)))
            .or_else(|| body.lines().next().map(|l| l.trim_start_matches('#').trim().to_string()));

        match opts.chunk_by {
            ChunkStrategy::Row => {
                let src_ref = format!("doc:{id}");
                if existing.contains(&src_ref) {
                    continue;
                }
                let node_id = insert_node(
                    db,
                    "DocChunk",
                    &src_ref,
                    title.as_deref(),
                    Some(&body),
                )?;
                let _ = node_id;
                stats.nodes_added += 1;
            }
            ChunkStrategy::Headings => {
                let chunks = split_markdown_headings(&body);
                let mut section_parent: Option<i64> = None;
                for (i, (level, heading, content)) in chunks.iter().enumerate() {
                    if *level > 0 {
                        // DocSection
                        let section_ref = format!("doc:{id}:section:{i}");
                        if !existing.contains(&section_ref) {
                            let sid = insert_node(
                                db,
                                "DocSection",
                                &section_ref,
                                Some(heading),
                                None,
                            )?;
                            section_parent = Some(sid);
                            stats.nodes_added += 1;
                        }
                    }
                    if !content.trim().is_empty() {
                        let chunk_ref = format!("doc:{id}:chunk:{i}");
                        if existing.contains(&chunk_ref) {
                            continue;
                        }
                        let chunk_title = if !heading.is_empty() {
                            Some(heading.as_str())
                        } else {
                            title.as_deref()
                        };
                        let cid = insert_node(
                            db,
                            "DocChunk",
                            &chunk_ref,
                            chunk_title,
                            Some(content),
                        )?;
                        stats.nodes_added += 1;
                        if let Some(pid) = section_parent {
                            insert_edge(db, cid, pid, "PART_OF", 1.0)?;
                            stats.edges_added += 1;
                        }
                    }
                }
            }
        }
    }
    Ok(stats)
}

fn split_markdown_headings(body: &str) -> Vec<(u32, String, String)> {
    // Returns tuples of (heading_level, heading_text, content_text).
    // level=0 means "leading content with no preceding heading".
    let mut chunks: Vec<(u32, String, String)> = Vec::new();
    let mut cur_level: u32 = 0;
    let mut cur_heading = String::new();
    let mut cur_body = String::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= 6 && trimmed[hashes..].starts_with(' ') {
            // Start of a new heading section — flush the current chunk.
            if !cur_heading.is_empty() || !cur_body.is_empty() {
                chunks.push((cur_level, cur_heading.clone(), cur_body.clone()));
            }
            cur_level = hashes as u32;
            cur_heading = trimmed[hashes..].trim().to_string();
            cur_body.clear();
        } else {
            cur_body.push_str(line);
            cur_body.push('\n');
        }
    }
    if !cur_heading.is_empty() || !cur_body.is_empty() {
        chunks.push((cur_level, cur_heading, cur_body));
    }
    chunks
}

// -- ingest_email ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IngestEmailOptions {
    pub source_table: String,
    pub message_id_col: String,
    pub from_col: String,
    pub to_col: Option<String>,
    pub subject_col: Option<String>,
    pub body_col: String,
    pub in_reply_to_col: Option<String>,
}

pub fn ingest_email(
    db: &EmbeddedDatabase,
    opts: &IngestEmailOptions,
) -> Result<IngestStats> {
    ensure_tables(db)?;
    let mut stats = IngestStats::default();

    let mut sel = format!(
        "SELECT {mid}, {from}, {body}",
        mid = opts.message_id_col,
        from = opts.from_col,
        body = opts.body_col,
    );
    if let Some(t) = &opts.to_col { sel.push_str(&format!(", {t}")); } else { sel.push_str(", NULL"); }
    if let Some(s) = &opts.subject_col { sel.push_str(&format!(", {s}")); } else { sel.push_str(", NULL"); }
    if let Some(r) = &opts.in_reply_to_col { sel.push_str(&format!(", {r}")); } else { sel.push_str(", NULL"); }
    sel.push_str(&format!(" FROM {}", opts.source_table));
    let rows = db.query(&sel, &[])?;

    let mut existing_emails = source_refs_with_prefix(db, "email:")?;
    let mut known_persons: HashMap<String, i64> =
        source_refs_map_with_prefix(db, "person:")?;

    // Second pass so we can resolve in_reply_to to its message id.
    let mut mid_to_node: HashMap<String, i64> = HashMap::new();

    for row in &rows {
        stats.rows_seen += 1;
        let mid = as_string(row.values.first()).unwrap_or_default();
        if mid.is_empty() {
            stats.rows_skipped += 1;
            continue;
        }
        let from = as_string(row.values.get(1)).unwrap_or_default();
        let body = as_string(row.values.get(2)).unwrap_or_default();
        let to = as_string(row.values.get(3)).unwrap_or_default();
        let subject = as_string(row.values.get(4)).unwrap_or_default();
        let _ = as_string(row.values.get(5));

        let src_ref = format!("email:{mid}");
        if existing_emails.contains(&src_ref) {
            // Still need to know the node id for in_reply_to resolution.
            if let Some(gid) = lookup_graph_node(db, &src_ref)? {
                mid_to_node.insert(mid.clone(), gid);
            }
            continue;
        }
        let eid = insert_node(
            db,
            "Email",
            &src_ref,
            if subject.is_empty() { None } else { Some(&subject) },
            Some(&body),
        )?;
        mid_to_node.insert(mid.clone(), eid);
        existing_emails.insert(src_ref);
        stats.nodes_added += 1;

        if !from.is_empty() {
            let pid = upsert_person(db, &mut known_persons, &from, &mut stats)?;
            insert_edge(db, eid, pid, "AUTHORED_BY", 1.0)?;
            stats.edges_added += 1;
        }
        if !to.is_empty() {
            for addr in to.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let pid = upsert_person(db, &mut known_persons, addr, &mut stats)?;
                insert_edge(db, eid, pid, "SENT_TO", 1.0)?;
                stats.edges_added += 1;
            }
        }
    }

    // Second pass: REPLIES_TO.
    for row in &rows {
        let mid = as_string(row.values.first()).unwrap_or_default();
        let reply_to = as_string(row.values.get(5)).unwrap_or_default();
        if reply_to.is_empty() {
            continue;
        }
        let (Some(this), Some(parent)) = (
            mid_to_node.get(&mid).copied(),
            mid_to_node.get(&reply_to).copied(),
        ) else {
            continue;
        };
        insert_edge(db, this, parent, "REPLIES_TO", 1.0)?;
        stats.edges_added += 1;
    }

    Ok(stats)
}

// -- ingest_issues -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IngestIssuesOptions {
    pub source_table: String,
    pub id_col: String,
    pub title_col: String,
    pub body_col: String,
    pub reporter_col: Option<String>,
    /// Optional column holding JSON-encoded comments, shape
    /// `[{"author":"..","body":".."}, ...]`. Each comment becomes a
    /// Comment node with a REPLIES_TO edge to the issue.
    pub comments_json_col: Option<String>,
    /// Optional column holding JSON-encoded fix refs (commit shas or
    /// symbol qualified names) to connect via FIXED_BY edges.
    pub fixed_by_json_col: Option<String>,
}

pub fn ingest_issues(
    db: &EmbeddedDatabase,
    opts: &IngestIssuesOptions,
) -> Result<IngestStats> {
    ensure_tables(db)?;
    let mut stats = IngestStats::default();
    let mut sel = format!(
        "SELECT {id}, {title}, {body}",
        id = opts.id_col,
        title = opts.title_col,
        body = opts.body_col,
    );
    if let Some(c) = &opts.reporter_col { sel.push_str(&format!(", {c}")); } else { sel.push_str(", NULL"); }
    if let Some(c) = &opts.comments_json_col { sel.push_str(&format!(", {c}")); } else { sel.push_str(", NULL"); }
    if let Some(c) = &opts.fixed_by_json_col { sel.push_str(&format!(", {c}")); } else { sel.push_str(", NULL"); }
    sel.push_str(&format!(" FROM {}", opts.source_table));
    let rows = db.query(&sel, &[])?;

    let mut existing = source_refs_with_prefix(db, "issue:")?;
    let mut known_persons: HashMap<String, i64> =
        source_refs_map_with_prefix(db, "person:")?;

    for row in rows {
        stats.rows_seen += 1;
        let id = as_string(row.values.first()).unwrap_or_default();
        if id.is_empty() {
            stats.rows_skipped += 1;
            continue;
        }
        let title = as_string(row.values.get(1)).unwrap_or_default();
        let body = as_string(row.values.get(2)).unwrap_or_default();
        let reporter = as_string(row.values.get(3)).unwrap_or_default();
        let comments = as_string(row.values.get(4)).unwrap_or_default();
        let fixed_by = as_string(row.values.get(5)).unwrap_or_default();

        let src_ref = format!("issue:{id}");
        if existing.contains(&src_ref) {
            continue;
        }
        let iid = insert_node(db, "Issue", &src_ref, Some(&title), Some(&body))?;
        existing.insert(src_ref);
        stats.nodes_added += 1;

        if !reporter.is_empty() {
            let pid = upsert_person(db, &mut known_persons, &reporter, &mut stats)?;
            insert_edge(db, iid, pid, "REPORTED_BY", 1.0)?;
            stats.edges_added += 1;
        }

        if !comments.is_empty() {
            if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&comments) {
                if let Some(list) = arr.as_array() {
                    for (ci, c) in list.iter().enumerate() {
                        let author = c.get("author").and_then(|v| v.as_str()).unwrap_or("");
                        let cbody = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
                        if cbody.is_empty() {
                            continue;
                        }
                        let c_ref = format!("issue:{id}:comment:{ci}");
                        let cid = insert_node(
                            db,
                            "Comment",
                            &c_ref,
                            None,
                            Some(cbody),
                        )?;
                        stats.nodes_added += 1;
                        insert_edge(db, cid, iid, "REPLIES_TO", 1.0)?;
                        stats.edges_added += 1;
                        if !author.is_empty() {
                            let pid = upsert_person(
                                db,
                                &mut known_persons,
                                author,
                                &mut stats,
                            )?;
                            insert_edge(db, cid, pid, "AUTHORED_BY", 1.0)?;
                            stats.edges_added += 1;
                        }
                    }
                }
            }
        }

        if !fixed_by.is_empty() {
            if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&fixed_by) {
                if let Some(list) = arr.as_array() {
                    for item in list {
                        let Some(tgt) = item.as_str() else { continue };
                        let sid = upsert_external_ref(db, tgt, "ExternalRef")?;
                        insert_edge(db, iid, sid, "FIXED_BY", 1.0)?;
                        stats.edges_added += 1;
                    }
                }
            }
        }
    }
    Ok(stats)
}

// -- ingest_qa ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IngestQaOptions {
    pub source_table: String,
    pub id_col: String,
    pub question_col: String,
    pub answer_col: Option<String>,
    pub asker_col: Option<String>,
    pub answerer_col: Option<String>,
}

pub fn ingest_qa(
    db: &EmbeddedDatabase,
    opts: &IngestQaOptions,
) -> Result<IngestStats> {
    ensure_tables(db)?;
    let mut stats = IngestStats::default();
    let mut sel = format!(
        "SELECT {id}, {q}",
        id = opts.id_col,
        q = opts.question_col
    );
    if let Some(c) = &opts.answer_col { sel.push_str(&format!(", {c}")); } else { sel.push_str(", NULL"); }
    if let Some(c) = &opts.asker_col { sel.push_str(&format!(", {c}")); } else { sel.push_str(", NULL"); }
    if let Some(c) = &opts.answerer_col { sel.push_str(&format!(", {c}")); } else { sel.push_str(", NULL"); }
    sel.push_str(&format!(" FROM {}", opts.source_table));
    let rows = db.query(&sel, &[])?;

    let mut existing = source_refs_with_prefix(db, "qa:")?;
    let mut known_persons: HashMap<String, i64> =
        source_refs_map_with_prefix(db, "person:")?;

    for row in rows {
        stats.rows_seen += 1;
        let id = as_string(row.values.first()).unwrap_or_default();
        if id.is_empty() {
            stats.rows_skipped += 1;
            continue;
        }
        let question = as_string(row.values.get(1)).unwrap_or_default();
        let answer = as_string(row.values.get(2)).unwrap_or_default();
        let asker = as_string(row.values.get(3)).unwrap_or_default();
        let answerer = as_string(row.values.get(4)).unwrap_or_default();

        let q_ref = format!("qa:{id}:q");
        if existing.contains(&q_ref) {
            continue;
        }
        let qid = insert_node(
            db,
            "InvestorQuestion",
            &q_ref,
            Some(&format!("Q-{id}")),
            Some(&question),
        )?;
        existing.insert(q_ref);
        stats.nodes_added += 1;

        if !asker.is_empty() {
            let pid = upsert_person(db, &mut known_persons, &asker, &mut stats)?;
            insert_edge(db, qid, pid, "AUTHORED_BY", 1.0)?;
            stats.edges_added += 1;
        }

        if !answer.is_empty() {
            let a_ref = format!("qa:{id}:a");
            let aid = insert_node(
                db,
                "Answer",
                &a_ref,
                Some(&format!("A-{id}")),
                Some(&answer),
            )?;
            stats.nodes_added += 1;
            insert_edge(db, aid, qid, "ANSWERED_BY", 1.0)?;
            stats.edges_added += 1;
            if !answerer.is_empty() {
                let pid =
                    upsert_person(db, &mut known_persons, &answerer, &mut stats)?;
                insert_edge(db, aid, pid, "AUTHORED_BY", 1.0)?;
                stats.edges_added += 1;
            }
        }
    }

    Ok(stats)
}

// -- helpers -----------------------------------------------------------------

fn as_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn insert_node(
    db: &EmbeddedDatabase,
    kind: &str,
    source_ref: &str,
    title: Option<&str>,
    text: Option<&str>,
) -> Result<i64> {
    let (_, rows) = db.execute_params_returning(
        "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) \
         VALUES ($1, $2, $3, $4) RETURNING node_id",
        &[
            Value::String(kind.into()),
            Value::String(source_ref.into()),
            title.map(|s| Value::String(s.into())).unwrap_or(Value::Null),
            text.map(|s| Value::String(s.into())).unwrap_or(Value::Null),
        ],
    )?;
    rows.first()
        .and_then(|r| r.values.first())
        .and_then(|v| match v {
            Value::Int4(n) => Some(*n as i64),
            Value::Int8(n) => Some(*n),
            _ => None,
        })
        .ok_or_else(|| Error::query_execution("insert_node: no RETURNING node_id"))
}

fn insert_edge(
    db: &EmbeddedDatabase,
    from: i64,
    to: i64,
    kind: &str,
    weight: f64,
) -> Result<()> {
    db.execute_params_returning(
        "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) \
         VALUES ($1, $2, $3, $4)",
        &[
            Value::Int8(from),
            Value::Int8(to),
            Value::String(kind.into()),
            Value::Float8(weight),
        ],
    )?;
    Ok(())
}

fn source_refs_with_prefix(db: &EmbeddedDatabase, prefix: &str) -> Result<HashSet<String>> {
    let rows = db.query_params(
        "SELECT source_ref FROM _hdb_graph_nodes WHERE source_ref LIKE $1",
        &[Value::String(format!("{prefix}%"))],
    )?;
    let mut out = HashSet::with_capacity(rows.len());
    for row in rows {
        if let Some(Value::String(s)) = row.values.first() {
            out.insert(s.clone());
        }
    }
    Ok(out)
}

fn source_refs_map_with_prefix(
    db: &EmbeddedDatabase,
    prefix: &str,
) -> Result<HashMap<String, i64>> {
    let rows = db.query_params(
        "SELECT source_ref, node_id FROM _hdb_graph_nodes WHERE source_ref LIKE $1",
        &[Value::String(format!("{prefix}%"))],
    )?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let s = match row.values.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let id = match row.values.get(1) {
            Some(Value::Int4(n)) => *n as i64,
            Some(Value::Int8(n)) => *n,
            _ => continue,
        };
        out.insert(s, id);
    }
    Ok(out)
}

fn lookup_graph_node(db: &EmbeddedDatabase, source_ref: &str) -> Result<Option<i64>> {
    let rows = db.query_params(
        "SELECT node_id FROM _hdb_graph_nodes WHERE source_ref = $1",
        &[Value::String(source_ref.into())],
    )?;
    Ok(rows.first().and_then(|r| r.values.first()).and_then(|v| match v {
        Value::Int4(n) => Some(*n as i64),
        Value::Int8(n) => Some(*n),
        _ => None,
    }))
}

fn upsert_person(
    db: &EmbeddedDatabase,
    cache: &mut HashMap<String, i64>,
    ident: &str,
    stats: &mut IngestStats,
) -> Result<i64> {
    let src_ref = format!("person:{ident}");
    if let Some(id) = cache.get(&src_ref) {
        return Ok(*id);
    }
    let pid = insert_node(db, "Person", &src_ref, Some(ident), None)?;
    cache.insert(src_ref, pid);
    stats.nodes_added += 1;
    Ok(pid)
}

fn upsert_external_ref(db: &EmbeddedDatabase, target: &str, kind: &str) -> Result<i64> {
    let src_ref = format!("ext:{target}");
    if let Some(id) = lookup_graph_node(db, &src_ref)? {
        return Ok(id);
    }
    insert_node(db, kind, &src_ref, Some(target), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_splitter_simple() {
        let chunks = split_markdown_headings("# A\n\nbody\n\n## B\n\nmore\n");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, 1);
        assert_eq!(chunks[0].1, "A");
        assert_eq!(chunks[1].0, 2);
        assert_eq!(chunks[1].1, "B");
    }
}
