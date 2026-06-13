//! Wire-level equivalence tests for the extended query protocol (R5.W1/W2).
//!
//! These drive Parse / Bind / Execute directly on the handler over a duplex
//! stream and decode the resulting wire bytes, asserting that the
//! direct-encoder path produces exactly the same DataRow payloads as the
//! simple-query path (which has used the direct encoder all along), and
//! that binary result-format requests still work via the legacy path.

use super::handler::PgConnectionHandler;
use crate::EmbeddedDatabase;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, DuplexStream};

/// Build a pre-authenticated handler over a duplex stream with a buffer
/// large enough that sequential (single-task) write/read tests don't
/// deadlock on duplex backpressure.
fn test_handler(db: Arc<EmbeddedDatabase>) -> (PgConnectionHandler<DuplexStream>, DuplexStream) {
    let (server, client) = tokio::io::duplex(8 << 20);
    (PgConnectionHandler::new_for_tests(db, server), client)
}

/// Read whatever bytes are currently buffered on the client end.
async fn drain(client: &mut DuplexStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(50), client.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    out
}

/// Split a PG backend byte stream into (message_type, payload) frames.
fn parse_messages(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 5 <= bytes.len() {
        let ty = bytes[pos];
        let len = i32::from_be_bytes([bytes[pos + 1], bytes[pos + 2], bytes[pos + 3], bytes[pos + 4]]) as usize;
        let end = pos + 1 + len;
        assert!(end <= bytes.len(), "truncated message {ty:#x}");
        out.push((ty, bytes[pos + 5..end].to_vec()));
        pos = end;
    }
    out
}

/// Decode a DataRow payload into per-column optional byte vectors.
fn decode_data_row(payload: &[u8]) -> Vec<Option<Vec<u8>>> {
    let ncols = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut pos = 2;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let len = i32::from_be_bytes([payload[pos], payload[pos + 1], payload[pos + 2], payload[pos + 3]]);
        pos += 4;
        if len < 0 {
            cols.push(None);
        } else {
            let len = len as usize;
            cols.push(Some(payload[pos..pos + len].to_vec()));
            pos += len;
        }
    }
    cols
}

fn data_rows(bytes: &[u8]) -> Vec<Vec<Option<Vec<u8>>>> {
    parse_messages(bytes)
        .into_iter()
        .filter(|(ty, _)| *ty == b'D')
        .map(|(_, payload)| decode_data_row(&payload))
        .collect()
}

fn wide_test_db(rows: usize) -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute(
        "CREATE TABLE wide (id INT PRIMARY KEY, a TEXT, b TEXT, c BIGINT, d DOUBLE PRECISION, \
         e TEXT, f INT, g TEXT, h BIGINT, i TEXT, j DOUBLE PRECISION, k TEXT)",
    )
    .expect("create");
    for n in 0..rows {
        db.execute(&format!(
            "INSERT INTO wide VALUES ({n}, 'alpha-{n}', 'beta-{n}', {}, {}.5, 'gamma-{n}', {}, \
             'delta-{n}', {}, 'epsilon-{n}', {}.25, 'zeta-{n}')",
            n * 1000,
            n,
            n % 97,
            (n as i64) * 7,
            n
        ))
        .expect("insert");
    }
    db
}

/// Extended-protocol Execute (text formats) must emit byte-identical
/// DataRows to the simple-query path for the same SELECT.
#[tokio::test]
async fn extended_select_matches_simple_query_data_rows() {
    let db = wide_test_db(50);

    // Simple query reference bytes
    let (mut handler, mut client) = test_handler(Arc::clone(&db));
    handler
        .handle_single_query("SELECT * FROM wide ORDER BY id")
        .await
        .expect("simple query");
    let simple_rows = data_rows(&drain(&mut client).await);
    assert_eq!(simple_rows.len(), 50);

    // Extended protocol on a fresh handler
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_parse_extended("s1".into(), "SELECT * FROM wide ORDER BY id".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended("p1".into(), "s1".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    handler.handle_execute_extended("p1".into(), 0).await.expect("execute");
    let extended_rows = data_rows(&drain(&mut client).await);

    assert_eq!(extended_rows, simple_rows, "extended DataRows must be byte-identical");
}

/// Many text-format parameters must round-trip through Bind / Execute and
/// produce direct-encoded rows.
#[tokio::test]
async fn extended_select_with_many_params() {
    let db = wide_test_db(30);
    let (mut handler, mut client) = test_handler(db);

    let sql = "SELECT id, a, c FROM wide WHERE id = $1 OR id = $2 OR id = $3 OR id = $4 \
               OR id = $5 OR id = $6 OR id = $7 OR id = $8 ORDER BY id";
    handler
        .handle_parse_extended("s2".into(), sql.into(), vec![23; 8])
        .await
        .expect("parse");
    let params: Vec<Option<Vec<u8>>> = [1, 3, 5, 7, 11, 13, 17, 19]
        .iter()
        .map(|n: &i32| Some(n.to_string().into_bytes()))
        .collect();
    handler
        .handle_bind_extended("p2".into(), "s2".into(), vec![0; 8], params, vec![])
        .await
        .expect("bind");
    handler.handle_execute_extended("p2".into(), 0).await.expect("execute");

    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 8);
    assert_eq!(rows[0][0].as_deref(), Some(b"1".as_ref()));
    assert_eq!(rows[0][1].as_deref(), Some(b"alpha-1".as_ref()));
    assert_eq!(rows[0][2].as_deref(), Some(b"1000".as_ref()));
    assert_eq!(rows[7][0].as_deref(), Some(b"19".as_ref()));
    assert_eq!(rows[7][2].as_deref(), Some(b"19000".as_ref()));
}

/// NULL values must arrive as the -1 length sentinel through the direct
/// encoder on the extended path.
#[tokio::test]
async fn extended_select_null_handling() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE n (id INT PRIMARY KEY, v TEXT)")
        .expect("create");
    db.execute("INSERT INTO n VALUES (1, NULL), (2, 'x')").expect("insert");

    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_parse_extended("s3".into(), "SELECT v FROM n ORDER BY id".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended("p3".into(), "s3".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    handler.handle_execute_extended("p3".into(), 0).await.expect("execute");

    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], None, "NULL must be the -1 sentinel");
    assert_eq!(rows[1][0].as_deref(), Some(b"x".as_ref()));
}

/// A binary result-format request must keep working through the legacy
/// conversion path (W1 only reroutes all-text requests).
#[tokio::test]
async fn extended_select_binary_format_fallback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE bi (id INT PRIMARY KEY)").expect("create");
    db.execute("INSERT INTO bi VALUES (305419896)").expect("insert"); // 0x12345678

    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_parse_extended("s4".into(), "SELECT id FROM bi".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended("p4".into(), "s4".into(), vec![], vec![], vec![1])
        .await
        .expect("bind");
    handler.handle_execute_extended("p4".into(), 0).await.expect("execute");

    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0].as_deref(),
        Some([0x12u8, 0x34, 0x56, 0x78].as_ref()),
        "int4 must arrive as 4-byte big-endian binary"
    );
}

/// R5.W2: repeated Executes of the same prepared statement must keep
/// returning correct (identical) rows — the pinned plan serves every
/// Execute after the first.
#[tokio::test]
async fn repeated_execute_serves_identical_rows() {
    let db = wide_test_db(20);
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_parse_extended(
            "rep".into(),
            "SELECT id, a, c FROM wide WHERE id = $1 OR id = $2 ORDER BY id".into(),
            vec![23, 23],
        )
        .await
        .expect("parse");

    let mut first_rows = None;
    for i in 0..5 {
        let portal = format!("rp{i}");
        handler
            .handle_bind_extended(
                portal.clone(),
                "rep".into(),
                vec![0, 0],
                vec![Some(b"3".to_vec()), Some(b"7".to_vec())],
                vec![],
            )
            .await
            .expect("bind");
        handler.handle_execute_extended(portal, 0).await.expect("execute");
        let rows = data_rows(&drain(&mut client).await);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].as_deref(), Some(b"3".as_ref()));
        assert_eq!(rows[1][0].as_deref(), Some(b"7".as_ref()));
        match &first_rows {
            None => first_rows = Some(rows),
            Some(expected) => assert_eq!(&rows, expected, "execute #{i} diverged"),
        }
    }
}

/// R5.W2: DDL between Executes clears the engine plan cache (epoch bump);
/// the pinned plan must be re-fetched, not served stale.
#[tokio::test]
async fn ddl_between_executes_invalidates_pinned_plan() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE evolve (id INT PRIMARY KEY)").expect("create");
    db.execute("INSERT INTO evolve VALUES (1)").expect("insert");

    let (mut handler, mut client) = test_handler(Arc::clone(&db));
    handler
        .handle_parse_extended("ev".into(), "SELECT * FROM evolve".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended("evp1".into(), "ev".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    handler
        .handle_execute_extended("evp1".into(), 0)
        .await
        .expect("execute");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1, "one column before DDL");

    // Schema change through the embedded API (bumps the plan-cache epoch)
    db.execute("ALTER TABLE evolve ADD COLUMN extra TEXT").expect("alter");

    handler
        .handle_bind_extended("evp2".into(), "ev".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    handler
        .handle_execute_extended("evp2".into(), 0)
        .await
        .expect("execute");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].len(),
        2,
        "SELECT * must see the new column — stale pinned plan detected"
    );
}

/// R5.W2: catalog-emulated queries (decided at Parse) must still be served
/// by the catalog dispatcher on the extended path.
#[tokio::test]
async fn catalog_query_still_served_after_parse_decision() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_parse_extended("cat".into(), "SELECT version()".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended("catp".into(), "cat".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    handler
        .handle_execute_extended("catp".into(), 0)
        .await
        .expect("execute");

    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1);
    let version = String::from_utf8(rows[0][0].clone().expect("version text")).expect("utf8");
    assert!(version.contains("PostgreSQL"), "catalog version() reply: {version}");
}

/// Foreground timing probe for R5.W2 — run with:
/// `cargo test --release --lib probe_w2 -- --ignored --nocapture`
/// Measures repeated Bind+Execute of a prepared two-parameter SELECT.
#[tokio::test]
#[ignore]
async fn probe_w2_repeated_prepared_execute() {
    let db = wide_test_db(1_000);
    let (server, mut client) = tokio::io::duplex(1 << 20);
    let mut handler = PgConnectionHandler::new_for_tests(db, server);

    let drain_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        while let Ok(n) = client.read(&mut buf).await {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    });

    handler
        .handle_parse_extended(
            "probe2".into(),
            "SELECT id, a, c FROM wide WHERE id = $1".into(),
            vec![23],
        )
        .await
        .expect("parse");

    const ITERS: usize = 20_000;
    let start = std::time::Instant::now();
    for i in 0..ITERS {
        let a = (i % 1000).to_string().into_bytes();
        handler
            .handle_bind_extended("".into(), "probe2".into(), vec![0], vec![Some(a)], vec![])
            .await
            .expect("bind");
        handler.handle_execute_extended("".into(), 0).await.expect("execute");
    }
    let elapsed = start.elapsed();
    drop(handler);
    let bytes = drain_task.await.expect("drain");

    println!(
        "W2 probe: {ITERS} Bind+Execute of prepared point-SELECT in {:?} ({:.1} us/exec, {:.1} KB total)",
        elapsed,
        elapsed.as_secs_f64() * 1e6 / ITERS as f64,
        bytes as f64 / 1024.0
    );
}

/// Foreground timing probe for R5.W1 — run with:
/// `cargo test --release -p heliosdb-nano --lib probe_w1 -- --ignored --nocapture`
/// Measures repeated extended-protocol Executes of a 10k-row × 12-column
/// SELECT, with a concurrent drain task standing in for the client.
#[tokio::test]
#[ignore]
async fn probe_w1_extended_select_10k_wide_rows() {
    let db = wide_test_db(10_000);
    let (server, mut client) = tokio::io::duplex(1 << 20);
    let mut handler = PgConnectionHandler::new_for_tests(db, server);

    // Concurrent drain so duplex backpressure doesn't serialize the writes.
    let drain_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        while let Ok(n) = client.read(&mut buf).await {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    });

    handler
        .handle_parse_extended("probe".into(), "SELECT * FROM wide".into(), vec![])
        .await
        .expect("parse");

    const ITERS: usize = 30;
    let start = std::time::Instant::now();
    for i in 0..ITERS {
        let portal = format!("portal{i}");
        handler
            .handle_bind_extended(portal.clone(), "probe".into(), vec![], vec![], vec![])
            .await
            .expect("bind");
        handler.handle_execute_extended(portal, 0).await.expect("execute");
    }
    let elapsed = start.elapsed();
    drop(handler);
    let bytes = drain_task.await.expect("drain");

    println!(
        "W1 probe: {ITERS} extended Executes of 10k×12 rows in {:?} ({:.2} ms/exec, {:.1} MB total)",
        elapsed,
        elapsed.as_secs_f64() * 1000.0 / ITERS as f64,
        bytes as f64 / (1024.0 * 1024.0)
    );
}
