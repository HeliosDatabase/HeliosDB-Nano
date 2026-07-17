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

// ---------------------------------------------------------------------------
// Item 6 — pipelining contract: one ReadyForQuery per Sync, never per Execute.
// ---------------------------------------------------------------------------

/// A pipelined `Parse, Bind, Execute, Bind, Execute, Sync` must emit results
/// per Execute but EXACTLY ONE ReadyForQuery, as the LAST message. A
/// per-Execute RFQ would terminate the HeliosProxy batch relay early. This
/// drives the same `handle_message` dispatch the main loop uses.
#[tokio::test]
async fn pipelined_executes_emit_exactly_one_ready_for_query() {
    use super::messages::FrontendMessage;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (1),(2),(3)").unwrap();

    let (mut handler, mut client) = test_handler(db);

    let pipeline = vec![
        FrontendMessage::Parse {
            statement_name: "s1".into(),
            query: "SELECT id FROM t ORDER BY id".into(),
            param_types: vec![],
        },
        FrontendMessage::Bind {
            portal_name: "p1".into(),
            statement_name: "s1".into(),
            param_formats: vec![],
            params: vec![],
            result_formats: vec![],
        },
        FrontendMessage::Execute {
            portal_name: "p1".into(),
            max_rows: 0,
        },
        FrontendMessage::Bind {
            portal_name: "p2".into(),
            statement_name: "s1".into(),
            param_formats: vec![],
            params: vec![],
            result_formats: vec![],
        },
        FrontendMessage::Execute {
            portal_name: "p2".into(),
            max_rows: 0,
        },
        FrontendMessage::Sync,
    ];
    for msg in pipeline {
        handler.handle_message(msg).await.expect("dispatch");
    }

    let out = drain(&mut client).await;
    let types: Vec<u8> = parse_messages(&out).iter().map(|(t, _)| *t).collect();
    let render: String = types.iter().map(|&t| t as char).collect();

    assert_eq!(
        types.iter().filter(|&&t| t == b'Z').count(),
        1,
        "exactly one ReadyForQuery for the whole pipeline: {render}"
    );
    assert_eq!(*types.last().unwrap(), b'Z', "ReadyForQuery must be last: {render}");
    assert_eq!(
        types.iter().filter(|&&t| t == b'1').count(),
        1,
        "one ParseComplete: {render}"
    );
    assert_eq!(
        types.iter().filter(|&&t| t == b'2').count(),
        2,
        "two BindComplete: {render}"
    );
    assert_eq!(
        types.iter().filter(|&&t| t == b'C').count(),
        2,
        "two CommandComplete: {render}"
    );
    assert_eq!(
        types.iter().filter(|&&t| t == b'D').count(),
        6,
        "six DataRows (3 per Execute): {render}"
    );
}

// ---------------------------------------------------------------------------
// Items 5 / 9 / 10 — session GUCs and DISCARD over the wire.
// ---------------------------------------------------------------------------

fn command_tags(bytes: &[u8]) -> Vec<String> {
    parse_messages(bytes)
        .into_iter()
        .filter(|(t, _)| *t == b'C')
        .map(|(_, p)| String::from_utf8_lossy(p.split(|&b| b == 0).next().unwrap_or(&[])).to_string())
        .collect()
}

fn param_status(bytes: &[u8]) -> Vec<(String, String)> {
    parse_messages(bytes)
        .into_iter()
        .filter(|(t, _)| *t == b'S')
        .map(|(_, p)| {
            let mut it = p.split(|&b| b == 0);
            let name = String::from_utf8_lossy(it.next().unwrap_or(&[])).to_string();
            let val = String::from_utf8_lossy(it.next().unwrap_or(&[])).to_string();
            (name, val)
        })
        .collect()
}

fn first_data_row_text(bytes: &[u8]) -> Option<String> {
    data_rows(bytes)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
        .map(|b| String::from_utf8_lossy(&b).to_string())
}

/// Item 9 + item 10: `SET helios.fast_autocommit = on` echoes a GUC_REPORT
/// `ParameterStatus` (so a capability-probing pool sees the change) and a
/// `SET` CommandComplete; `SHOW` reflects it; an invalid value errors cleanly.
#[tokio::test]
async fn helios_fast_autocommit_set_show_roundtrip() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query("SET helios.fast_autocommit = on")
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert!(
        param_status(&out)
            .iter()
            .any(|(n, v)| n == "helios.fast_autocommit" && v == "on"),
        "expected GUC_REPORT helios.fast_autocommit=on, got {:?}",
        param_status(&out)
    );
    assert!(command_tags(&out).iter().any(|t| t == "SET"), "expected SET tag");

    handler
        .handle_single_query("SHOW helios.fast_autocommit")
        .await
        .unwrap();
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("on"),
        "SHOW must reflect the SET"
    );

    // Invalid value must error cleanly (handler sends ErrorResponse, returns Ok).
    handler
        .handle_single_query("SET helios.fast_autocommit = banana")
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert!(
        parse_messages(&out).iter().any(|(t, _)| *t == b'E'),
        "invalid value must produce an ErrorResponse"
    );
}

/// Item 5: `DISCARD ALL` acks with the `DISCARD ALL` tag and resets the
/// session's `helios.fast_autocommit` GUC back to its default.
#[tokio::test]
async fn discard_all_resets_session_guc() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query("SET helios.fast_autocommit = on")
        .await
        .unwrap();
    let _ = drain(&mut client).await;

    handler.handle_single_query("DISCARD ALL").await.unwrap();
    let out = drain(&mut client).await;
    assert!(
        command_tags(&out).iter().any(|t| t == "DISCARD ALL"),
        "expected DISCARD ALL tag, got {:?}",
        command_tags(&out)
    );

    handler
        .handle_single_query("SHOW helios.fast_autocommit")
        .await
        .unwrap();
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("off"),
        "DISCARD ALL must reset helios.fast_autocommit to off"
    );
}

/// BUG E: bytea must be sent to the client as `\x<hex>` (PostgreSQL
/// bytea_output=hex), NOT as raw bytes. Raw bytes make libpq/psycopg2 apply
/// escape-format un-escaping to the field, silently dropping any 0x5C
/// (backslash) byte — corrupting Any2HeliosDB BLOB/RAW round-trips. The literal
/// below contains 0x5c (`5a5b5c5d5e`), the byte that was being lost.
#[tokio::test]
async fn bytea_text_output_is_hex_not_raw_bytes() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler.handle_single_query("CREATE TABLE wbt (b bytea)").await.unwrap();
    let _ = drain(&mut client).await;
    handler
        .handle_single_query("INSERT INTO wbt VALUES ('\\x5a5b5c5d5e')")
        .await
        .unwrap();
    let _ = drain(&mut client).await;
    handler.handle_single_query("SELECT b FROM wbt").await.unwrap();
    let out = drain(&mut client).await;
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("\\x5a5b5c5d5e"),
        "bytea text output must be `\\x`-hex encoded, not raw bytes"
    );
}

// ---------------------------------------------------------------------------
// W2.3 — Extended-protocol Parse reuse from the shared parameterized plan
// cache (OID-parity contract).
// ---------------------------------------------------------------------------

/// Decode a RowDescription ('T') payload stream into `(name, data_type_oid)`
/// per field. Field layout after the null-terminated name: table_oid(i32),
/// column_attr_num(i16), data_type_oid(i32), data_type_size(i16),
/// type_modifier(i32), format_code(i16) — 18 fixed bytes.
fn row_description(bytes: &[u8]) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    for (ty, payload) in parse_messages(bytes) {
        if ty != b'T' {
            continue;
        }
        let nfields = i16::from_be_bytes([payload[0], payload[1]]) as usize;
        let mut pos = 2;
        for _ in 0..nfields {
            let name_end = pos + payload[pos..].iter().position(|&b| b == 0).expect("field name cstring");
            let name = String::from_utf8_lossy(&payload[pos..name_end]).to_string();
            pos = name_end + 1;
            let oid_pos = pos + 4 + 2; // skip table_oid(i32) + column_attr_num(i16)
            let oid = i32::from_be_bytes([
                payload[oid_pos],
                payload[oid_pos + 1],
                payload[oid_pos + 2],
                payload[oid_pos + 3],
            ]);
            out.push((name, oid));
            pos += 18; // table_oid+col_attr+type_oid+size+modifier+format
        }
    }
    out
}

/// W2.3 flip: parsing a plain SELECT must seed the prepared statement's
/// `cached_plan` from the SHARED parameterized plan cache AT PARSE TIME.
/// Pre-W2.3 the Describe schema came from a throwaway private plan and
/// `cached_plan` stayed `None` until the first Execute — this assertion flips
/// `None` → `Some`.
#[tokio::test]
async fn parse_seeds_shared_plan_for_select() {
    let db = wide_test_db(3);
    let (mut handler, _client) = test_handler(db);
    handler
        .handle_parse_extended("sp".into(), "SELECT id, a FROM wide WHERE id = $1".into(), vec![23])
        .await
        .expect("parse");
    let stmt = handler
        .prepared_statements
        .get_statement("sp")
        .expect("get")
        .expect("stmt present");
    assert!(
        stmt.cached_plan.is_some(),
        "SELECT Parse must seed cached_plan from the shared parameterized plan cache"
    );
}

/// W2.3 invariant: INSERT … RETURNING must NOT take the shared-plan path —
/// `LogicalPlan::schema()` is EMPTY for DML even with a RETURNING clause, so
/// routing it through the shared path would regress Describe to `NoData`. It
/// stays on the private `derive_result_schema` path: its Describe schema keeps
/// the RETURNING column and its `cached_plan` is left unseeded at Parse.
#[tokio::test]
async fn dml_returning_keeps_private_schema_path() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE ins (id INT PRIMARY KEY, v TEXT)")
        .expect("create");
    let (mut handler, _client) = test_handler(db);
    handler
        .handle_parse_extended(
            "dr".into(),
            "INSERT INTO ins (id, v) VALUES ($1, $2) RETURNING id".into(),
            vec![23, 25],
        )
        .await
        .expect("parse");
    let stmt = handler
        .prepared_statements
        .get_statement("dr")
        .expect("get")
        .expect("stmt present");
    assert!(
        stmt.cached_plan.is_none(),
        "DML-RETURNING must not be seeded via the shared plan path (empty LogicalPlan::schema)"
    );
    let schema = stmt
        .result_schema
        .expect("RETURNING must still yield a result schema (RowDescription), not NoData");
    assert_eq!(
        schema.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["id"],
        "RETURNING column must survive the private fallback path"
    );
}

/// W2.3 OID-parity contract: the Describe RowDescription derived from the
/// SHARED plan cache must advertise the exact pg_type OIDs — crucially
/// numeric → 1700 (the 3.58.3 regression class), plus text→25, int4→23,
/// int8→20, varchar→1043 — and the correct column names.
#[tokio::test]
async fn describe_reports_pg_type_oids() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE acct (id INT PRIMARY KEY, name TEXT, bal NUMERIC, big BIGINT, code VARCHAR(8))")
        .expect("create");
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_parse_extended(
            "d".into(),
            "SELECT id, name, bal, big, code FROM acct WHERE id = $1".into(),
            vec![23],
        )
        .await
        .expect("parse");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "d".into())
        .await
        .expect("describe");
    let fields = row_description(&drain(&mut client).await);
    assert_eq!(
        fields,
        vec![
            ("id".to_string(), 23),
            ("name".to_string(), 25),
            ("bal".to_string(), 1700),
            ("big".to_string(), 20),
            ("code".to_string(), 1043),
        ],
        "Describe RowDescription names + pg_type OIDs (numeric MUST be 1700)"
    );
}

/// W2.3 aggregate alias: `count(*) AS n` must Describe as one column named
/// `n` with the int8 OID (20) — proving the shared path preserves projection
/// aliases and aggregate result typing on the Describe metadata.
#[tokio::test]
async fn describe_aggregate_alias_names_and_types() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE ev (id INT PRIMARY KEY, k TEXT)")
        .expect("create");
    db.execute("INSERT INTO ev VALUES (1,'a'),(2,'b'),(3,'a')")
        .expect("insert");
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_parse_extended("ag".into(), "SELECT count(*) AS n FROM ev".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "ag".into())
        .await
        .expect("describe");
    let fields = row_description(&drain(&mut client).await);
    assert_eq!(
        fields,
        vec![("n".to_string(), 20)],
        "count(*) AS n → int8 (OID 20) named n"
    );
}

/// W2.3 regression (review finding): the Describe schema is now sourced from
/// the SHARED parameterized plan cache, which is keyed by SQL TEXT. Regular
/// (non-materialized) views are INLINED into that cached plan, so redefining a
/// view (CREATE OR REPLACE VIEW) must invalidate the plan cache — otherwise a
/// second Parse of the SAME `SELECT * FROM v` text Describes the STALE column
/// set. Pre-fix `plan_invalidates_sql_caches` omitted CreateView/DropView, so
/// the shared path served the pre-redefine one-column plan and the `name`
/// column assertion below flips false → true with the fix. (Pre-W2.3 the
/// private `derive_result_schema` re-planned against the live view catalog on
/// every Parse, so this metadata was correct — W2.3 widened the hole to
/// Describe.)
#[tokio::test]
async fn view_redefine_invalidates_describe_schema() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE vt (id INT PRIMARY KEY, name TEXT)")
        .expect("create table");
    db.execute("CREATE VIEW vv AS SELECT id FROM vt").expect("create view");

    let (mut handler, mut client) = test_handler(Arc::clone(&db));

    // First Parse+Describe caches the parameterized plan for the view text.
    handler
        .handle_parse_extended("v1".into(), "SELECT * FROM vv".into(), vec![])
        .await
        .expect("parse v1");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "v1".into())
        .await
        .expect("describe v1");
    let before = row_description(&drain(&mut client).await);
    assert_eq!(
        before,
        vec![("id".to_string(), 23)],
        "view Describe before redefine: single int4 column id"
    );

    // Redefine the SAME view name with an extra TEXT column. This must clear the
    // shared plan cache (CreateView invalidation) so the identical SQL text
    // re-plans against the new view shape instead of the stale cached entry.
    db.execute("CREATE OR REPLACE VIEW vv AS SELECT id, name FROM vt")
        .expect("replace view");

    handler
        .handle_parse_extended("v2".into(), "SELECT * FROM vv".into(), vec![])
        .await
        .expect("parse v2");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "v2".into())
        .await
        .expect("describe v2");
    let after = row_description(&drain(&mut client).await);
    assert_eq!(
        after,
        vec![("id".to_string(), 23), ("name".to_string(), 25)],
        "view Describe after CREATE OR REPLACE must reflect the added TEXT column \
         (a stale shared-plan-cache entry would still report only id)"
    );
}

/// W2.3 regression (review finding): the same stale-Describe hole, but with the
/// redefining DDL run over the EXTENDED protocol (Parse/Bind/Execute) — the
/// route psycopg3 / JDBC / Npgsql use by default (e.g. Alembic migrations).
/// That route lands in `execute_plan_with_params_inner`'s catch-all executor
/// arm, which the first two `plan_invalidates_sql_caches` gates
/// (`execute_internal`, `execute_in_transaction_inner`) never cover — so the
/// `CREATE OR REPLACE VIEW` executed here left the shared `"\0params\0<sql>"`
/// plan cache un-cleared and its epoch un-bumped, and the second Parse of the
/// SAME `SELECT * FROM vv` text Described the STALE single-column plan. Unlike
/// `view_redefine_invalidates_describe_schema` (which redefines via
/// `db.execute`, i.e. the already-gated route), THIS test drives the DDL
/// through the wire funnel, so the `name` column assertion below flips
/// false → true only with the params-funnel gate.
#[tokio::test]
async fn view_redefine_via_extended_protocol_invalidates_describe_schema() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE vt (id INT PRIMARY KEY, name TEXT)")
        .expect("create table");
    db.execute("CREATE VIEW vv AS SELECT id FROM vt").expect("create view");

    let (mut handler, mut client) = test_handler(Arc::clone(&db));

    // First Parse+Describe caches the parameterized plan for the view text.
    handler
        .handle_parse_extended("v1".into(), "SELECT * FROM vv".into(), vec![])
        .await
        .expect("parse v1");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "v1".into())
        .await
        .expect("describe v1");
    let before = row_description(&drain(&mut client).await);
    assert_eq!(
        before,
        vec![("id".to_string(), 23)],
        "view Describe before redefine: single int4 column id"
    );

    // Redefine the SAME view via Parse/Bind/Execute — the extended-protocol
    // DDL funnel. Without the params-path gate this executes the view change
    // but never clears the shared plan cache.
    handler
        .handle_parse_extended(
            "ddl".into(),
            "CREATE OR REPLACE VIEW vv AS SELECT id, name FROM vt".into(),
            vec![],
        )
        .await
        .expect("parse ddl");
    handler
        .handle_bind_extended("ddlp".into(), "ddl".into(), vec![], vec![], vec![])
        .await
        .expect("bind ddl");
    handler
        .handle_execute_extended("ddlp".into(), 0)
        .await
        .expect("execute ddl");
    let _ = drain(&mut client).await; // discard CommandComplete for the DDL

    handler
        .handle_parse_extended("v2".into(), "SELECT * FROM vv".into(), vec![])
        .await
        .expect("parse v2");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "v2".into())
        .await
        .expect("describe v2");
    let after = row_description(&drain(&mut client).await);
    assert_eq!(
        after,
        vec![("id".to_string(), 23), ("name".to_string(), 25)],
        "view Describe after CREATE OR REPLACE via the extended protocol must reflect \
         the added TEXT column (a stale shared-plan-cache entry would still report only id)"
    );
}

/// Per-statement panic isolation must cover the extended/prepared path — and
/// specifically its third execution entry, the `is_dml_returning` branch that
/// calls `execute_params_returning`. A statement that fails at execute time on
/// that path must surface as a recoverable error (which the connection loop
/// renders as an ErrorResponse) and leave the connection fully usable for the
/// next statement, rather than unwinding the task and dropping the client.
///
/// The `run_guarded` unit test in `handler.rs` proves the panic→XX000
/// conversion deterministically; this drives the guarded DML-RETURNING path
/// end-to-end over the wire with a deterministic execute-time failure
/// (BIGINT overflow) and asserts the handler stays healthy afterwards.
#[tokio::test]
async fn extended_dml_returning_failure_keeps_connection_usable() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE ov (id BIGINT PRIMARY KEY)").expect("create");
    // i64::MAX — `id + 1` overflows when the UPDATE is evaluated.
    db.execute("INSERT INTO ov VALUES (9223372036854775807)").expect("insert");

    let (mut handler, mut client) = test_handler(Arc::clone(&db));

    // `UPDATE ... RETURNING` is not a row-returning (SELECT/CTE) query, so it
    // routes through the `is_dml_returning` branch -> the guarded
    // `execute_params_returning` call. The `id + 1` overflow raises a checked-
    // arithmetic error at execute time.
    handler
        .handle_parse_extended("bad".into(), "UPDATE ov SET id = id + 1 RETURNING id".into(), vec![])
        .await
        .expect("parse");
    handler
        .handle_bind_extended("bp".into(), "bad".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    let err = handler
        .handle_execute_extended("bp".into(), 0)
        .await
        .expect_err("overflowing UPDATE ... RETURNING must surface an error, not panic/drop");
    assert_eq!(
        super::handler::sqlstate_for_error(&err),
        "XX000",
        "a failure on the guarded DML-RETURNING path must map to a recoverable SQLSTATE; got {err}"
    );
    let _ = drain(&mut client).await;

    // The connection/handler must still be fully usable — the failed statement
    // did not corrupt state or drop the client.
    handler
        .handle_parse_extended("ok".into(), "SELECT id FROM ov ORDER BY id".into(), vec![])
        .await
        .expect("parse after error");
    handler
        .handle_bind_extended("op".into(), "ok".into(), vec![], vec![], vec![])
        .await
        .expect("bind after error");
    handler
        .handle_execute_extended("op".into(), 0)
        .await
        .expect("a fresh statement after the error must execute normally");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1, "the row must be unchanged and the connection healthy");
    assert_eq!(
        rows[0][0].as_deref(),
        Some(b"9223372036854775807".as_ref()),
        "the overflowing UPDATE must not have mutated the row"
    );
}
