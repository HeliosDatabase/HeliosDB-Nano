//! Four ingestion adapters: docs, email, issues, qa.

#![cfg(feature = "graph-rag")]

use heliosdb_nano::graph_rag::{
    ChunkStrategy, IngestDocsOptions, IngestEmailOptions, IngestIssuesOptions,
    IngestQaOptions,
};
use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn count(db: &EmbeddedDatabase, sql: &str) -> Result<i64> {
    let rows = db.query(sql, &[])?;
    Ok(match &rows[0].values[0] {
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        _ => -1,
    })
}

#[test]
fn ingest_docs_headings() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT)")?;
    db.execute_params_returning(
        "INSERT INTO docs (id, body) VALUES ($1, $2)",
        &[
            Value::String("readme".into()),
            Value::String(
                "# Intro\n\nHello.\n\n## Install\n\nRun cargo.\n".into(),
            ),
        ],
    )?;
    let stats = db.graph_rag_ingest_docs(&IngestDocsOptions {
        source_table: "docs".into(),
        id_col: "id".into(),
        text_col: "body".into(),
        title_col: None,
        chunk_by: ChunkStrategy::Headings,
    })?;
    assert!(stats.nodes_added >= 2, "{stats:?}");
    // DocSection + DocChunk node kinds present.
    let kinds = db.query(
        "SELECT node_kind, count(*) FROM _hdb_graph_nodes GROUP BY node_kind",
        &[],
    )?;
    let has_section = kinds.iter().any(|r| matches!(&r.values[0], Value::String(s) if s == "DocSection"));
    let has_chunk = kinds.iter().any(|r| matches!(&r.values[0], Value::String(s) if s == "DocChunk"));
    assert!(has_section && has_chunk);
    Ok(())
}

#[test]
fn ingest_docs_row_strategy() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT)")?;
    db.execute_params_returning(
        "INSERT INTO docs (id, body) VALUES ($1, $2)",
        &[Value::String("a".into()), Value::String("just body".into())],
    )?;
    let stats = db.graph_rag_ingest_docs(&IngestDocsOptions {
        source_table: "docs".into(),
        id_col: "id".into(),
        text_col: "body".into(),
        title_col: None,
        chunk_by: ChunkStrategy::Row,
    })?;
    assert_eq!(stats.nodes_added, 1);
    Ok(())
}

#[test]
fn ingest_email_emits_persons_and_replies() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE mail (mid TEXT PRIMARY KEY, sender TEXT, recipient TEXT, \
                            subj TEXT, body TEXT, reply_to TEXT)",
    )?;
    for (mid, from, to, subj, body, rep) in &[
        ("m1", "alice@x", "bob@x", "hi", "hello", ""),
        ("m2", "bob@x", "alice@x", "re: hi", "back", "m1"),
    ] {
        db.execute_params_returning(
            "INSERT INTO mail VALUES ($1,$2,$3,$4,$5,$6)",
            &[
                Value::String((*mid).into()),
                Value::String((*from).into()),
                Value::String((*to).into()),
                Value::String((*subj).into()),
                Value::String((*body).into()),
                Value::String((*rep).into()),
            ],
        )?;
    }
    let stats = db.graph_rag_ingest_email(&IngestEmailOptions {
        source_table: "mail".into(),
        message_id_col: "mid".into(),
        from_col: "sender".into(),
        to_col: Some("recipient".into()),
        subject_col: Some("subj".into()),
        body_col: "body".into(),
        in_reply_to_col: Some("reply_to".into()),
    })?;
    assert_eq!(stats.rows_seen, 2);
    assert!(stats.nodes_added >= 2);
    // REPLIES_TO edge exists from m2 → m1.
    let replies =
        count(&db, "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'REPLIES_TO'")?;
    assert_eq!(replies, 1);
    // AUTHORED_BY + SENT_TO edges exist.
    let auth =
        count(&db, "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'AUTHORED_BY'")?;
    assert!(auth >= 2);
    Ok(())
}

#[test]
fn ingest_issues_parses_comments_and_fixes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE issues (iid TEXT PRIMARY KEY, title TEXT, body TEXT, \
                              reporter TEXT, comments TEXT, fixed TEXT)",
    )?;
    db.execute_params_returning(
        "INSERT INTO issues VALUES ($1,$2,$3,$4,$5,$6)",
        &[
            Value::String("B1".into()),
            Value::String("bug!".into()),
            Value::String("it's bad".into()),
            Value::String("alice".into()),
            Value::String(
                r#"[{"author":"bob","body":"seen it"},{"author":"carol","body":"fixing"}]"#
                    .into(),
            ),
            Value::String(r#"["commit:abc123","Fix::apply"]"#.into()),
        ],
    )?;
    let stats = db.graph_rag_ingest_issues(&IngestIssuesOptions {
        source_table: "issues".into(),
        id_col: "iid".into(),
        title_col: "title".into(),
        body_col: "body".into(),
        reporter_col: Some("reporter".into()),
        comments_json_col: Some("comments".into()),
        fixed_by_json_col: Some("fixed".into()),
    })?;
    assert!(stats.nodes_added >= 4); // issue + 2 comments + reporter Person
    let comments =
        count(&db, "SELECT count(*) FROM _hdb_graph_nodes WHERE node_kind = 'Comment'")?;
    assert_eq!(comments, 2);
    let fixed =
        count(&db, "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'FIXED_BY'")?;
    assert_eq!(fixed, 2);
    Ok(())
}

#[test]
fn ingest_qa_emits_question_and_answer() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE qa (qid TEXT PRIMARY KEY, q TEXT, a TEXT, who_asked TEXT, who_answered TEXT)",
    )?;
    db.execute_params_returning(
        "INSERT INTO qa VALUES ($1,$2,$3,$4,$5)",
        &[
            Value::String("042".into()),
            Value::String("what's revenue?".into()),
            Value::String("positive.".into()),
            Value::String("investor-1".into()),
            Value::String("cfo".into()),
        ],
    )?;
    let stats = db.graph_rag_ingest_qa(&IngestQaOptions {
        source_table: "qa".into(),
        id_col: "qid".into(),
        question_col: "q".into(),
        answer_col: Some("a".into()),
        asker_col: Some("who_asked".into()),
        answerer_col: Some("who_answered".into()),
    })?;
    assert!(stats.nodes_added >= 4); // question, answer, asker, answerer
    let q = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_nodes WHERE node_kind = 'InvestorQuestion'",
    )?;
    let a = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_nodes WHERE node_kind = 'Answer'",
    )?;
    assert_eq!(q, 1);
    assert_eq!(a, 1);
    let ab = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'ANSWERED_BY'",
    )?;
    assert_eq!(ab, 1);
    Ok(())
}
