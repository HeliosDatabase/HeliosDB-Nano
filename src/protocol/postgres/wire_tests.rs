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
/// arm, which the text-family `plan_invalidates_sql_caches` gate in
/// `execute_in_transaction_inner` never covers — so the
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
    db.execute("INSERT INTO ov VALUES (9223372036854775807)")
        .expect("insert");

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

/// Round-3 Stage-0 partitioning over the wire (simple-query path): a
/// `PARTITION OF` child and `ATTACH`/`DETACH PARTITION` must succeed with the
/// correct PostgreSQL command tags — the wire path diverges from embedded on
/// the CommandComplete tag, so it is asserted here specifically.
#[tokio::test]
#[allow(clippy::expect_used)] // Test code: `expect` documents the failing step.
async fn partition_of_and_attach_detach_over_the_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));

    // Parent + PARTITION OF child; the child must report "CREATE TABLE".
    let (mut handler, mut client) = test_handler(Arc::clone(&db));
    handler
        .handle_single_query("CREATE TABLE w_parent (id INT, label TEXT) PARTITION BY RANGE (id)")
        .await
        .expect("parent create");
    handler
        .handle_single_query("CREATE TABLE w_child PARTITION OF w_parent FOR VALUES FROM (0) TO (100)")
        .await
        .expect("child create");
    let tags = command_tags(&drain(&mut client).await);
    assert!(
        tags.iter().any(|t| t == "CREATE TABLE"),
        "PARTITION OF child must complete as CREATE TABLE, got {tags:?}"
    );

    // Child cloned the parent's columns → INSERT/SELECT over the wire work.
    let (mut handler, mut client) = test_handler(Arc::clone(&db));
    handler
        .handle_single_query("INSERT INTO w_child (id, label) VALUES (5, 'hi')")
        .await
        .expect("insert");
    let _ = drain(&mut client).await;
    handler
        .handle_single_query("SELECT id, label FROM w_child")
        .await
        .expect("select");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1, "child SELECT must return the inserted row");

    // ATTACH / DETACH PARTITION accepted as no-ops with "ALTER TABLE" tags.
    let (mut handler, mut client) = test_handler(Arc::clone(&db));
    handler
        .handle_single_query("ALTER TABLE w_parent ATTACH PARTITION w_child FOR VALUES FROM (0) TO (100)")
        .await
        .expect("attach");
    handler
        .handle_single_query("ALTER TABLE w_parent DETACH PARTITION w_child")
        .await
        .expect("detach");
    let tags = command_tags(&drain(&mut client).await);
    assert_eq!(
        tags.iter().filter(|t| *t == "ALTER TABLE").count(),
        2,
        "ATTACH and DETACH PARTITION must each complete as ALTER TABLE, got {tags:?}"
    );
}

/// Schema namespacing over the wire: `SET search_path` scopes bare names, both
/// bare and qualified access resolve to the schema-scoped table, and switching
/// back to `public` un-resolves the bare name. FAILS on pre-change code, where
/// `SET search_path` is a silent no-op and every qualifier collapses to bare.
#[tokio::test]
async fn search_path_scopes_bare_names_over_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for stmt in [
        "CREATE SCHEMA wa",
        "SET search_path TO wa",
        "CREATE TABLE wt (v INT)",
        "INSERT INTO wt (v) VALUES (42)",
    ] {
        handler.handle_single_query(stmt).await.expect("setup stmt");
        let _ = drain(&mut client).await;
    }

    // SHOW reflects the current schema over the wire.
    handler.handle_single_query("SHOW search_path").await.expect("show");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("wa, public"),
        "SHOW search_path must reflect the SET"
    );

    // A bare reference resolves to wa.wt.
    handler
        .handle_single_query("SELECT v FROM wt")
        .await
        .expect("bare select");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("42"),
        "bare name resolves under search_path"
    );

    // The qualified reference resolves too.
    handler
        .handle_single_query("SELECT v FROM wa.wt")
        .await
        .expect("qualified select");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("42"),
        "qualified name resolves"
    );

    // Back to public: the bare name no longer resolves (no public `wt`).
    handler
        .handle_single_query("SET search_path TO public")
        .await
        .expect("reset to public");
    let _ = drain(&mut client).await;
    handler
        .handle_single_query("SELECT v FROM wt")
        .await
        .expect("bare select public");
    let out = drain(&mut client).await;
    assert!(
        parse_messages(&out).iter().any(|(t, _)| *t == b'E'),
        "bare `wt` must error under public search_path"
    );
}

/// `search_path` is per-CONNECTION, not process-wide: two connections sharing
/// one `Arc<EmbeddedDatabase>` each resolve bare names against THEIR OWN
/// `search_path`, even when interleaved. This is the cross-session-leak
/// regression guard — a shared `current_schema` field steers connection A's
/// bare `INSERT` to whatever schema connection B set last (silent cross-tenant
/// wrong-table write). Both connections use the SAME bare name `orders`.
#[test]
fn search_path_is_isolated_per_connection() {
    // Dedicated 16 MiB thread: this test's async state machine (~22 awaits of
    // 64 KB-class handler futures across TWO connections) exceeds the 2 MiB
    // default test-thread stack in debug builds. Box::pin cannot help — the
    // machine is constructed on the stack BEFORE the heap move (gdb: stack-
    // probe SIGSEGV at first poll, no recursion). A bigger stack is the
    // deterministic fix; the body is unchanged.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async move {
                    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
                    let (mut a, mut ca) = test_handler(db.clone());
                    let (mut b, mut cb) = test_handler(db);

                    // Two schemas (created once, globally); each connection will own one.
                    for stmt in ["CREATE SCHEMA sa", "CREATE SCHEMA sb"] {
                        a.handle_single_query(stmt).await.expect("schema");
                        let _ = drain(&mut ca).await;
                    }

                    // Pin each connection's search_path, then create a bare `orders` under each
                    // — they must land in different schemas (sa.orders vs sb.orders).
                    a.handle_single_query("SET search_path TO sa").await.expect("A set");
                    let _ = drain(&mut ca).await;
                    b.handle_single_query("SET search_path TO sb").await.expect("B set");
                    let _ = drain(&mut cb).await;
                    a.handle_single_query("CREATE TABLE orders (v INT)")
                        .await
                        .expect("A create");
                    let _ = drain(&mut ca).await;
                    b.handle_single_query("CREATE TABLE orders (v INT)")
                        .await
                        .expect("B create");
                    let _ = drain(&mut cb).await;

                    // Interleaved bare INSERTs. B set its search_path most recently, so a shared
                    // selector would send BOTH rows to sb.orders. Correct behavior: A -> sa, B -> sb.
                    a.handle_single_query("INSERT INTO orders VALUES (1)")
                        .await
                        .expect("A insert");
                    let _ = drain(&mut ca).await;
                    b.handle_single_query("INSERT INTO orders VALUES (2)")
                        .await
                        .expect("B insert");
                    let _ = drain(&mut cb).await;

                    // Qualified reads prove each row landed in its own schema.
                    a.handle_single_query("SELECT v FROM sa.orders").await.expect("read sa");
                    assert_eq!(
                        first_data_row_text(&drain(&mut ca).await).as_deref(),
                        Some("1"),
                        "A's row is in sa.orders"
                    );
                    b.handle_single_query("SELECT v FROM sb.orders").await.expect("read sb");
                    assert_eq!(
                        first_data_row_text(&drain(&mut cb).await).as_deref(),
                        Some("2"),
                        "B's row is in sb.orders"
                    );

                    // Neither schema's table absorbed the other connection's row.
                    a.handle_single_query("SELECT count(*) FROM sa.orders")
                        .await
                        .expect("count sa");
                    assert_eq!(
                        first_data_row_text(&drain(&mut ca).await).as_deref(),
                        Some("1"),
                        "sa.orders holds exactly one row"
                    );
                    b.handle_single_query("SELECT count(*) FROM sb.orders")
                        .await
                        .expect("count sb");
                    assert_eq!(
                        first_data_row_text(&drain(&mut cb).await).as_deref(),
                        Some("1"),
                        "sb.orders holds exactly one row"
                    );

                    // Each connection's BARE read resolves to its own schema.
                    a.handle_single_query("SELECT v FROM orders").await.expect("A bare");
                    assert_eq!(
                        first_data_row_text(&drain(&mut ca).await).as_deref(),
                        Some("1"),
                        "A bare orders -> sa"
                    );
                    b.handle_single_query("SELECT v FROM orders").await.expect("B bare");
                    assert_eq!(
                        first_data_row_text(&drain(&mut cb).await).as_deref(),
                        Some("2"),
                        "B bare orders -> sb"
                    );

                    // SHOW search_path is per-connection.
                    a.handle_single_query("SHOW search_path").await.expect("A show");
                    assert_eq!(
                        first_data_row_text(&drain(&mut ca).await).as_deref(),
                        Some("sa, public")
                    );
                    b.handle_single_query("SHOW search_path").await.expect("B show");
                    assert_eq!(
                        first_data_row_text(&drain(&mut cb).await).as_deref(),
                        Some("sb, public")
                    );
                });
        })
        .expect("spawn")
        .join()
        .expect("join");
}

/// FIX 1 over the wire: `SET CONSTRAINTS ALL DEFERRED` must arm transaction-
/// scoped FK deferral on the PG simple-query path too. The handler used to ack
/// `SET CONSTRAINTS` as a generic no-op SET, so a wire client's deferred FK
/// never armed and the valid "insert child, then parent, then COMMIT" sequence
/// was rejected IMMEDIATELY at the child insert. Mirrors the embedded
/// `set_constraints_defers_fk_on_partitioned_qualified_parent` (partitioned,
/// schema-qualified parent) but drives every statement through
/// `handle_single_query`. FAILS on pre-change code at the deferred child insert.
#[tokio::test]
async fn set_constraints_defers_fk_over_wire() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(Arc::clone(&db));

    for stmt in [
        "CREATE SCHEMA fkpart9w",
        "SET search_path TO fkpart9w",
        "CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY LIST (a)",
        "CREATE TABLE pk1 PARTITION OF pk FOR VALUES IN (1, 2) PARTITION BY LIST (a)",
        "CREATE TABLE pk11 PARTITION OF pk1 FOR VALUES IN (1)",
        "CREATE TABLE pk3 PARTITION OF pk FOR VALUES IN (3)",
        "CREATE TABLE fk (a int REFERENCES pk DEFERRABLE INITIALLY IMMEDIATE)",
    ] {
        handler.handle_single_query(stmt).await.expect("setup stmt");
        let _ = drain(&mut client).await;
    }

    // Immediate (no SET CONSTRAINTS): a reference to the empty parent is rejected
    // right away — the FK is real over the wire.
    handler
        .handle_single_query("INSERT INTO fk VALUES (1)")
        .await
        .expect_err("immediate FK must reject a reference to the empty parent");
    let _ = drain(&mut client).await;

    // Deferred: child before parent, both land, COMMIT finds the parent. On
    // pre-change code the child INSERT below panics here (rejected immediately
    // because `SET CONSTRAINTS` never armed deferral over the wire).
    for stmt in [
        "BEGIN",
        "SET CONSTRAINTS ALL DEFERRED",
        "INSERT INTO fk VALUES (1)", // child first — accepted only because deferral armed
        "INSERT INTO pk VALUES (1)", // parent gains a=1 (lands in the partition)
        "COMMIT",                    // deferred check finds fkpart9w.pk(a)=1
    ] {
        handler
            .handle_single_query(stmt)
            .await
            .unwrap_or_else(|e| panic!("deferred flow `{stmt}` must succeed over the wire: {e}"));
        let _ = drain(&mut client).await;
    }

    // Both rows are present after the deferred COMMIT.
    handler
        .handle_single_query("SELECT a FROM fk")
        .await
        .expect("select fk");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("1"),
        "the deferred child row must be committed"
    );
    handler
        .handle_single_query("SELECT a FROM pk")
        .await
        .expect("select pk");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("1"),
        "the parent row must be committed"
    );

    // Transaction-scoped: after COMMIT the deferral is cleared, so a plain
    // autocommit dangling reference is rejected again.
    handler
        .handle_single_query("INSERT INTO fk VALUES (2)")
        .await
        .expect_err("deferral must not leak past the transaction it was set in");
}

// ---------------------------------------------------------------------------
// Catalog pre-parse interceptor removal: `version()` / `current_database()` /
// `current_user` were served by a blind `String::contains()` substring router
// in `PgCatalog::handle_query` that ran BEFORE the real parser saw the
// statement. ANY statement merely MENTIONING one of these substrings had its
// real content silently discarded and replaced with a canned single-row reply
// — including compound expressions (`current_database() ~ 'x'`), function
// wrapping (`length(version())`), multi-column projections, table scans, and,
// worst of all, WHERE clauses on UPDATE/DELETE (the write never executed while
// the client got a fake SELECT-shaped row). The interceptor is now deleted; the
// real parser/planner/evaluator handles all three uniformly. These tests guard
// (a) the common client-probe forms the interceptor existed for still work, and
// (b) the compound / DML danger cases are now answered correctly.
// ---------------------------------------------------------------------------

/// (a) Regression safety: the bare `version()` probe (SQLAlchemy / psql /
/// pgAdmin / DBeaver) still returns exactly one row/column with the version
/// string via the real evaluator path — no interceptor required.
#[tokio::test]
async fn test_wire_bare_version_still_works() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler.handle_single_query("SELECT version()").await.unwrap();
    let out = drain(&mut client).await;
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1, "version() must return exactly one row");
    assert_eq!(rows[0].len(), 1, "version() must return exactly one column");
    let text = first_data_row_text(&out).expect("version text");
    assert!(
        text.starts_with("PostgreSQL 16.0"),
        "version() must return the PostgreSQL version banner, got {text:?}"
    );
}

/// (a) Regression safety: `current_database()` still returns one row/column
/// `"heliosdb"` via the real evaluator path.
#[tokio::test]
async fn test_wire_bare_current_database_still_works() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler.handle_single_query("SELECT current_database()").await.unwrap();
    let out = drain(&mut client).await;
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1, "current_database() must return exactly one row");
    assert_eq!(rows[0].len(), 1, "current_database() must return exactly one column");
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("heliosdb"),
        "current_database() must return the database name"
    );
}

/// (a) Regression safety — the critical one: bare `current_user` (the PG
/// KEYWORD form, no parentheses — exactly how real clients write it, and the
/// form the deleted interceptor's `contains("current_user") && starts_with
/// ("select")` substring check was the ONLY thing answering) still returns one
/// row/column `"heliosdb"`. This proves the real parser turns the bare keyword
/// into an `Expr::Function` that the evaluator's `"current_user"` arm handles
/// independently, with no interceptor.
#[tokio::test]
async fn test_wire_bare_current_user_still_works() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler.handle_single_query("SELECT current_user").await.unwrap();
    let out = drain(&mut client).await;
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1, "current_user must return exactly one row");
    assert_eq!(rows[0].len(), 1, "current_user must return exactly one column");
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("heliosdb"),
        "bare current_user keyword must return the user name via the real evaluator"
    );
}

/// (b) `current_database() ~ 'hel'` is a boolean predicate, NOT a request for
/// the database name. Pre-fix the substring router hijacked it and returned
/// `[('heliosdb',)]`. It must now evaluate to boolean `true` (`'heliosdb'`
/// matches `hel`). Boolean text wire format is `t` / `f`.
#[tokio::test]
async fn test_wire_current_database_with_operator_not_hijacked() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("SELECT current_database() ~ 'hel'")
        .await
        .unwrap();
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("t"),
        "current_database() ~ 'hel' must evaluate to boolean true, not return the db name"
    );
}

/// (b) `current_user ~ 'nomatchxyz'` must evaluate to boolean `false`
/// (`'heliosdb'` does not match `nomatchxyz`), not return the canned user row.
#[tokio::test]
async fn test_wire_current_user_with_operator_not_hijacked() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("SELECT current_user ~ 'nomatchxyz'")
        .await
        .unwrap();
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("f"),
        "current_user ~ 'nomatchxyz' must evaluate to boolean false, not return the user name"
    );
}

/// (b) `length(version()) > 0` proves the WRAPPING function call determines the
/// query's real semantics now: the bare `version()` marker no longer short-
/// circuits the statement. The version string is non-empty, so this is `true`.
#[tokio::test]
async fn test_wire_version_wrapped_in_function_not_hijacked() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("SELECT length(version()) > 0")
        .await
        .unwrap();
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("t"),
        "length(version()) > 0 must evaluate to boolean true, not return the raw version string"
    );
}

/// (c) The high-severity case: a WHERE clause that merely MENTIONS
/// `current_user` must still SCAN AND FILTER the real table, not short-circuit
/// to a fake `current_user` row. Non-matching pattern → zero rows; matching
/// pattern → the real table rows.
#[tokio::test]
async fn test_wire_where_clause_with_current_user_scans_real_table() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE wcu (id INT PRIMARY KEY, name TEXT)",
        "INSERT INTO wcu VALUES (1, 'alice'), (2, 'bob')",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    // `current_user` ('heliosdb') does NOT match 'nomatchxyz' → the predicate is
    // false for every row → the table is scanned and ZERO rows come back. A
    // hijack would instead return a fake single 'heliosdb' row.
    handler
        .handle_single_query("SELECT * FROM wcu WHERE current_user ~ 'nomatchxyz'")
        .await
        .expect("filtered select");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(
        rows.len(),
        0,
        "non-matching current_user predicate must scan the table and return zero rows, not a fake row"
    );

    // `current_user` DOES match 'helios' (substring of 'heliosdb') → predicate
    // true for every row → the REAL table rows come back (id/name), never the
    // canned 'heliosdb' text.
    handler
        .handle_single_query("SELECT id, name FROM wcu WHERE current_user ~ 'helios' ORDER BY id")
        .await
        .expect("matching select");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 2, "matching predicate must return the real table rows");
    assert_eq!(rows[0][0].as_deref(), Some(b"1".as_ref()));
    assert_eq!(rows[0][1].as_deref(), Some(b"alice".as_ref()));
    assert_eq!(rows[1][0].as_deref(), Some(b"2".as_ref()));
    assert_eq!(rows[1][1].as_deref(), Some(b"bob".as_ref()));
}

/// (c) The worst case: an `UPDATE ... WHERE current_database() = '...'` must
/// actually EXECUTE as an UPDATE (reporting a real `UPDATE <n>` command tag),
/// not be hijacked into a fake SELECT-shaped reply the client can't distinguish
/// from a real write. First an always-false condition → `UPDATE 0` and the row
/// is untouched; then an always-true condition → the row is genuinely mutated.
#[tokio::test]
async fn test_wire_update_with_current_database_in_where_actually_executes() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE wud (id INT PRIMARY KEY, name TEXT)",
        "INSERT INTO wud VALUES (1, 'before')",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    // Always-false: current_database() is 'heliosdb', never 'nonexistent_db_name'.
    // A real UPDATE runs and matches zero rows → the `UPDATE 0` command tag. A
    // hijack would emit a SELECT-shaped response with no UPDATE tag at all.
    handler
        .handle_single_query("UPDATE wud SET name = 'after' WHERE current_database() = 'nonexistent_db_name'")
        .await
        .expect("update with always-false predicate");
    let tags = command_tags(&drain(&mut client).await);
    assert!(
        tags.iter().any(|t| t == "UPDATE 0"),
        "an UPDATE whose WHERE mentions current_database() must execute and report `UPDATE 0`, got {tags:?}"
    );

    // The row must be untouched by the zero-match UPDATE.
    handler
        .handle_single_query("SELECT name FROM wud WHERE id = 1")
        .await
        .expect("verify unchanged");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("before"),
        "the always-false UPDATE must not have mutated the row"
    );

    // Always-true: current_database() = 'heliosdb' → the UPDATE genuinely
    // applies (`UPDATE 1`) and the follow-up read sees the new value.
    handler
        .handle_single_query("UPDATE wud SET name = 'after' WHERE current_database() = 'heliosdb'")
        .await
        .expect("update with always-true predicate");
    let tags = command_tags(&drain(&mut client).await);
    assert!(
        tags.iter().any(|t| t == "UPDATE 1"),
        "the always-true UPDATE must execute and report `UPDATE 1`, got {tags:?}"
    );
    handler
        .handle_single_query("SELECT name FROM wud WHERE id = 1")
        .await
        .expect("verify changed");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("after"),
        "the always-true UPDATE must have genuinely applied the new value"
    );
}

/// Multi-element `CREATE SCHEMA foo CREATE TABLE … CREATE TABLE …` over the
/// wire: it must complete as `CREATE SCHEMA` and create both tables under the
/// new schema (the second bare-referencing the first), reachable via a follow-up
/// schema-qualified SELECT. Proves the fix routes through
/// `execute_in_transaction_inner` and so is reachable from the wire path, not
/// just the embedded `db.execute()` entry point. FAILS on pre-change code with a
/// `SQL parse error: … Expected: end of statement, found: CREATE`.
#[tokio::test]
async fn test_wire_multi_element_create_schema() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query(
            "CREATE SCHEMA wms \
             CREATE TABLE tbl1(f1 int PRIMARY KEY) \
             CREATE TABLE tbl2(f1 int REFERENCES tbl1)",
        )
        .await
        .expect("multi-element create schema over wire");
    let tags = command_tags(&drain(&mut client).await);
    assert!(
        tags.iter().any(|t| t == "CREATE SCHEMA"),
        "multi-element CREATE SCHEMA must complete as `CREATE SCHEMA`, got {tags:?}"
    );

    // Populate the schema-qualified tables (the FK resolves to wms.tbl1).
    handler
        .handle_single_query("INSERT INTO wms.tbl1 (f1) VALUES (11)")
        .await
        .expect("insert parent");
    let _ = drain(&mut client).await;
    handler
        .handle_single_query("INSERT INTO wms.tbl2 (f1) VALUES (11)")
        .await
        .expect("insert child referencing parent");
    let _ = drain(&mut client).await;

    // A follow-up SELECT against the newly created schema-qualified table
    // returns the row.
    handler
        .handle_single_query("SELECT f1 FROM wms.tbl2")
        .await
        .expect("select from newly created schema table");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("11"),
        "the wire-created multi-element schema tables must be queryable and hold the inserted row"
    );

    // The cross-element FK genuinely enforces against wms.tbl1. A plain
    // (non-RETURNING) DML error propagates as a genuine `Err` from
    // `handle_single_query` itself — only the SELECT arm in handler.rs catches
    // and converts an error to a wire ErrorResponse inline; the plain-DML arm
    // propagates via `?` and relies on the higher-level simple-query run-loop
    // (not present when calling `handle_single_query` directly here) to convert
    // it to wire bytes. Assert on the propagated error directly instead.
    let err = handler
        .handle_single_query("INSERT INTO wms.tbl2 (f1) VALUES (999)")
        .await
        .expect_err("a dangling FK reference must be rejected");
    assert!(
        err.to_string().contains("wms.tbl1"),
        "the FK violation must reference wms.tbl1, proving tbl2's FK resolved into the new schema, got: {err}"
    );
}

/// Task #38 — the exact silent-write-loss reproducer from the audit. An
/// `UPDATE … SET note='see pg_tables' …` merely MENTIONS `pg_tables` inside a
/// string literal. Before the fix, `PgCatalog::handle_query`'s bare `pg_tables`
/// dispatch intercepted it on the raw statement text, returned the canned
/// SELECT-shaped pg_tables rows, and the UPDATE NEVER EXECUTED — a silent write
/// loss the client couldn't detect. After F1 (statement-kind gate) the UPDATE
/// runs for real: the command tag is `UPDATE 1` (not a SELECT-shaped reply) and
/// the mutated value is visible on read-back.
#[tokio::test]
async fn test_wire_update_with_pg_tables_in_literal_actually_executes() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE inventory (id INT PRIMARY KEY, note TEXT)",
        "INSERT INTO inventory VALUES (1, 'before')",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    // The literal mentions `pg_tables`. This must EXECUTE as an UPDATE and
    // report `UPDATE 1` — never a canned pg_tables SELECT response.
    handler
        .handle_single_query("UPDATE inventory SET note='see pg_tables' WHERE id=1")
        .await
        .expect("update with pg_tables in literal must execute");
    let tags = command_tags(&drain(&mut client).await);
    assert!(
        tags.iter().any(|t| t == "UPDATE 1"),
        "an UPDATE whose literal mentions pg_tables must execute and report `UPDATE 1`, got {tags:?}"
    );

    // The new value must have genuinely persisted.
    handler
        .handle_single_query("SELECT note FROM inventory WHERE id = 1")
        .await
        .expect("read back");
    assert_eq!(
        first_data_row_text(&drain(&mut client).await).as_deref(),
        Some("see pg_tables"),
        "the UPDATE must have genuinely written the new note value"
    );
}

// ---------------------------------------------------------------------------
// HC3 — catalog introspection over the WIRE.
//
// The repo's documented gotcha is that embedded tests never execute
// `src/protocol/postgres/catalog.rs`, so a catalog view could pass every
// embedded test and still be empty (or mis-shaped) for the only clients that
// matter here: psql, psycopg, JDBC, sqlx, drizzle-kit, Prisma. These tests
// drive the real PG wire — simple query AND the extended Parse/Describe/Bind/
// Execute path — and are the acceptance criterion for HC3; the embedded suite
// (tests/catalog_introspection_tests.rs) is the regression floor beneath them.
// ---------------------------------------------------------------------------

/// Decode the RowDescription column names from a backend byte stream.
/// Field layout after the NUL-terminated name: table_oid i32, col_attnum i16,
/// type_oid i32, type_len i16, type_mod i32, format i16 = 18 fixed bytes.
fn row_description_names(bytes: &[u8]) -> Vec<String> {
    for (ty, payload) in parse_messages(bytes) {
        if ty != b'T' {
            continue;
        }
        let n = i16::from_be_bytes([payload[0], payload[1]]) as usize;
        let mut pos = 2usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let end = payload[pos..]
                .iter()
                .position(|&b| b == 0)
                .map(|i| pos + i)
                .expect("RowDescription field name must be NUL-terminated");
            out.push(String::from_utf8_lossy(&payload[pos..end]).to_string());
            pos = end + 1 + 18;
        }
        return out;
    }
    Vec::new()
}

/// Column index by name in a RowDescription, panicking with the full list.
fn rd_index(names: &[String], want: &str) -> usize {
    names
        .iter()
        .position(|n| n == want)
        .unwrap_or_else(|| panic!("column `{want}` missing from RowDescription; got {names:?}"))
}

/// Text of a DataRow cell, `None` for SQL NULL.
fn cell(row: &[Option<Vec<u8>>], idx: usize) -> Option<String> {
    row.get(idx)
        .and_then(|v| v.as_ref())
        .map(|b| String::from_utf8_lossy(b).to_string())
}

/// THE regression this whole change exists for. The single most common ORM
/// introspection query in existence — Prisma / Drizzle / Rails / SQLAlchemy all
/// send some variant of it — used to come back from the PG wire with ZERO ROWS,
/// because the wire's fixed 7-column `information_schema.columns` shape had no
/// `table_schema` at all: `row_value` returned NULL for it and `lit_eq_value`
/// dropped every row. Written WITHOUT spaces around `=` it instead hit the
/// "unknown predicate, keep the row" branch and then `project_columns` silently
/// DROPPED `table_schema` from the projection, so the client got 5 columns where
/// it asked for 6 — a different shape than RowDescription implied.
///
/// Both spellings must now return rows AND exactly the six requested columns.
#[tokio::test]
async fn wire_orm_columns_introspection_returns_six_columns_and_rows() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("CREATE TABLE wire_orm (id INT PRIMARY KEY, name TEXT)")
        .await
        .expect("create table");
    let _ = drain(&mut client).await;

    for sql in [
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns WHERE table_schema = 'public'",
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns WHERE table_schema='public'",
    ] {
        handler.handle_single_query(sql).await.expect("introspection query");
        let out = drain(&mut client).await;
        let names = row_description_names(&out);
        assert_eq!(
            names,
            vec![
                "table_schema".to_string(),
                "table_name".to_string(),
                "column_name".to_string(),
                "data_type".to_string(),
                "is_nullable".to_string(),
                "column_default".to_string(),
            ],
            "RowDescription must be exactly the six requested columns for `{sql}`"
        );

        let rows = data_rows(&out);
        assert!(
            !rows.is_empty(),
            "the ORM introspection query must return rows over the wire for `{sql}` (this returned ZERO before HC3)"
        );
        for row in &rows {
            assert_eq!(
                row.len(),
                6,
                "every DataRow must carry the six columns RowDescription promised"
            );
            assert_eq!(
                cell(row, 0).as_deref(),
                Some("public"),
                "WHERE table_schema = 'public' must actually filter"
            );
        }
        assert!(
            rows.iter().any(|r| cell(r, 1).as_deref() == Some("wire_orm")),
            "the user table's columns must be present for `{sql}`"
        );
    }
}

/// Schema namespacing over the wire: the deleted wire copy HARDCODED
/// `table_schema = 'public'` and emitted the raw `app.t` storage key as
/// `table_name`, so a table in another schema was reported twice-wrong. The
/// registry splits the key correctly and now answers the wire too.
#[tokio::test]
async fn wire_information_schema_reports_the_real_schema() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(Arc::clone(&db));
    for stmt in ["CREATE SCHEMA wapp", "CREATE TABLE wapp.t (c INT)"] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    handler
        .handle_single_query("SELECT table_schema, table_name FROM information_schema.tables")
        .await
        .expect("tables");
    let out = drain(&mut client).await;
    let rows = data_rows(&out);
    assert!(
        rows.iter()
            .any(|r| cell(r, 0).as_deref() == Some("wapp") && cell(r, 1).as_deref() == Some("t")),
        "a table in schema `wapp` must report table_schema='wapp', table_name='t' over the wire; got {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| cell(r, 1).as_deref() == Some("wapp.t")),
        "table_name must never be the raw `schema.table` storage key"
    );

    // information_schema.schemata must enumerate the real schema list, not the
    // three hardcoded rows the wire copy returned.
    handler
        .handle_single_query("SELECT schema_name FROM information_schema.schemata")
        .await
        .expect("schemata");
    let schemata = data_rows(&drain(&mut client).await);
    assert!(
        schemata.iter().any(|r| cell(r, 0).as_deref() == Some("wapp")),
        "information_schema.schemata must list a CREATE SCHEMA schema; got {schemata:?}"
    );

    // The embedded route must agree exactly — the divergence is what HC3 removes.
    // NOTE: `Value`'s Display quotes strings, so unwrap the raw text explicitly
    // rather than comparing `'wapp'` against the wire's `wapp`.
    let raw = |v: &crate::Value| -> String {
        match v {
            crate::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    };
    let (embedded, cols) = db
        .query_with_columns("SELECT table_schema, table_name FROM information_schema.tables")
        .expect("embedded tables");
    assert_eq!(cols, vec!["table_schema".to_string(), "table_name".to_string()]);
    let embedded_pairs: Vec<(String, String)> = embedded
        .iter()
        .map(|r| (raw(&r.values[0]), raw(&r.values[1])))
        .collect();
    let wire_pairs: Vec<(String, String)> = rows
        .iter()
        .map(|r| (cell(r, 0).unwrap_or_default(), cell(r, 1).unwrap_or_default()))
        .collect();
    assert_eq!(
        wire_pairs, embedded_pairs,
        "the wire and embedded routes must return identical information_schema.tables rows"
    );
}

/// Extended-query family (psycopg / JDBC / sqlx / node-postgres all use it):
/// Describe derives RowDescription at PARSE time — before HC3 from the wire's
/// fixed shape, now from the planner. Describe and Execute MUST agree, or
/// SQLAlchemy's psycopg dialect raises. Guards handler_extended.rs:68 / :304.
#[tokio::test]
async fn wire_extended_describe_matches_execute_for_catalog_views() {
    use super::messages::DescribeTarget;

    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE wext (id INT PRIMARY KEY, name TEXT)",
        "CREATE VIEW wext_v AS SELECT id FROM wext",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    for (idx, sql) in [
        "SELECT table_name, column_name FROM information_schema.columns WHERE table_schema = 'public'",
        "SELECT schemaname, viewname, definition FROM pg_views",
    ]
    .iter()
    .enumerate()
    {
        let stmt_name = format!("hc3s{idx}");
        let portal = format!("hc3p{idx}");

        handler
            .handle_parse_extended(stmt_name.clone(), (*sql).to_string(), vec![])
            .await
            .expect("parse");
        handler
            .handle_describe_extended(DescribeTarget::Statement, stmt_name.clone())
            .await
            .expect("describe");
        let describe_names = row_description_names(&drain(&mut client).await);
        assert!(
            !describe_names.is_empty(),
            "Describe must produce a RowDescription for `{sql}`"
        );

        handler
            .handle_bind_extended(portal.clone(), stmt_name, vec![], vec![], vec![])
            .await
            .expect("bind");
        handler.handle_execute_extended(portal, 0).await.expect("execute");
        let out = drain(&mut client).await;
        let rows = data_rows(&out);
        assert!(!rows.is_empty(), "Execute must return rows for `{sql}`");
        for row in &rows {
            assert_eq!(
                row.len(),
                describe_names.len(),
                "Describe promised {} columns but Execute sent {} for `{sql}`",
                describe_names.len(),
                row.len()
            );
        }
    }
}

/// Views are visible over the wire on every surface at once.
#[tokio::test]
async fn wire_views_are_visible_in_pg_views_and_pg_class() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE wv_base (id INT PRIMARY KEY, n INT)",
        "CREATE VIEW wv AS SELECT id, n FROM wv_base",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    handler
        .handle_single_query("SELECT viewname, definition FROM pg_views")
        .await
        .expect("pg_views");
    let out = drain(&mut client).await;
    let names = row_description_names(&out);
    assert_eq!(names, vec!["viewname".to_string(), "definition".to_string()]);
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1, "one view exists; got {rows:?}");
    assert_eq!(cell(&rows[0], 0).as_deref(), Some("wv"));
    assert!(
        cell(&rows[0], 1).unwrap_or_default().contains("wv_base"),
        "pg_views.definition must be the stored body over the wire"
    );

    handler
        .handle_single_query("SELECT relname FROM pg_class WHERE relkind = 'v'")
        .await
        .expect("pg_class");
    let cls = data_rows(&drain(&mut client).await);
    assert!(
        cls.iter().any(|r| cell(r, 0).as_deref() == Some("wv")),
        "pg_class must expose the view with relkind='v' over the wire; got {cls:?}"
    );
}

/// `pg_indexes` moved from the wire router into the registry; it must still
/// answer over the wire (this is the "port, don't drop" check).
#[tokio::test]
async fn wire_pg_indexes_still_answers_after_the_migration() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE wpi (id INT PRIMARY KEY, email TEXT)",
        "CREATE INDEX wpi_email_idx ON wpi(email)",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    handler
        .handle_single_query("SELECT indexname, indexdef FROM pg_indexes")
        .await
        .expect("pg_indexes");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description_names(&out),
        vec!["indexname".to_string(), "indexdef".to_string()]
    );
    let rows = data_rows(&out);
    assert!(
        rows.iter().any(|r| cell(r, 0).as_deref() == Some("wpi_email_idx")),
        "the manual index must still be listed over the wire; got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| cell(r, 0).as_deref() == Some("wpi_pkey")),
        "the primary-key index must still be listed over the wire; got {rows:?}"
    );
}

/// Aggregates, GROUP BY and the drizzle-kit triple JOIN must keep working after
/// the deferral — they are the reason the deferral pattern existed at all.
#[tokio::test]
async fn wire_catalog_aggregates_and_joins_still_work() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("CREATE TABLE wagg (id INT PRIMARY KEY, name TEXT)")
        .await
        .expect("setup");
    let _ = drain(&mut client).await;

    handler
        .handle_single_query("SELECT count(*) FROM information_schema.tables")
        .await
        .expect("count");
    let count = first_data_row_text(&drain(&mut client).await).expect("count value");
    assert!(
        count.parse::<i64>().map(|n| n > 0).unwrap_or(false),
        "count(*) over information_schema.tables must be a positive integer, got {count:?}"
    );

    handler
        .handle_single_query("SELECT table_schema, count(*) FROM information_schema.columns GROUP BY table_schema")
        .await
        .expect("group by");
    let grouped = data_rows(&drain(&mut client).await);
    assert!(
        !grouped.is_empty(),
        "GROUP BY over information_schema.columns must return rows"
    );

    // drizzle-kit's introspection shape: three catalog views in one statement.
    // This shape ALREADY deferred to the planner before HC3 (the old
    // `needs_planner` check fired on " join "), so a failure here is a planner
    // gap, not an HC3 regression — but it must not start failing because of
    // this change either, which is exactly what the pin is for.
    handler
        .handle_single_query(
            "SELECT tc.constraint_name FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu ON kcu.constraint_name = tc.constraint_name \
             JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = tc.constraint_name",
        )
        .await
        .expect("drizzle triple join must plan and execute");
    let joined = data_rows(&drain(&mut client).await);
    assert!(
        !joined.is_empty(),
        "the drizzle-kit triple JOIN must return the table's PK constraint; got {joined:?}"
    );
}

/// NULL edge over the wire: a NOT NULL column with a DEFAULT and a nullable
/// column with none must render 'NO'/'YES' and a real SQL NULL (-1 length),
/// not an empty string.
#[tokio::test]
async fn wire_information_schema_columns_null_and_default_rendering() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("CREATE TABLE wnull (a INT NOT NULL DEFAULT 7, b TEXT)")
        .await
        .expect("setup");
    let _ = drain(&mut client).await;

    handler
        .handle_single_query(
            "SELECT column_name, is_nullable, column_default FROM information_schema.columns \
             WHERE table_name = 'wnull'",
        )
        .await
        .expect("columns");
    let out = drain(&mut client).await;
    let names = row_description_names(&out);
    let name_idx = rd_index(&names, "column_name");
    let null_idx = rd_index(&names, "is_nullable");
    let def_idx = rd_index(&names, "column_default");

    let rows = data_rows(&out);
    assert_eq!(rows.len(), 2, "two columns on wnull; got {rows:?}");

    let a = rows
        .iter()
        .find(|r| cell(r, name_idx).as_deref() == Some("a"))
        .expect("column a");
    assert_eq!(cell(a, null_idx).as_deref(), Some("NO"));
    assert!(
        cell(a, def_idx).unwrap_or_default().contains('7'),
        "a's default must read back as 7 over the wire"
    );

    let b = rows
        .iter()
        .find(|r| cell(r, name_idx).as_deref() == Some("b"))
        .expect("column b");
    assert_eq!(cell(b, null_idx).as_deref(), Some("YES"));
    assert_eq!(
        cell(b, def_idx),
        None,
        "a column with no default must be a real wire NULL, not an empty string"
    );
}

/// Task #38 boundary regression, re-pinned after the branch deletions: user
/// tables whose names merely CONTAIN a catalog marker must never be shadowed by
/// the catalog router. `pg_views_backup` and `app_pg_indexes` are the exact
/// shapes the word-boundary matcher exists for, and both markers just lost
/// their dispatch branches — the marker list keeps them, so the boundary check
/// is still the only thing standing between a user table and interception.
#[tokio::test]
async fn wire_user_tables_named_after_catalog_views_are_not_shadowed() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE pg_views_backup (id INT PRIMARY KEY, note TEXT)",
        "CREATE TABLE app_pg_indexes (id INT PRIMARY KEY, note TEXT)",
        "INSERT INTO pg_views_backup VALUES (1, 'mine')",
        "INSERT INTO app_pg_indexes VALUES (1, 'also mine')",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    for (table, expected) in [("pg_views_backup", "mine"), ("app_pg_indexes", "also mine")] {
        handler
            .handle_single_query(&format!("SELECT note FROM {table} WHERE id = 1"))
            .await
            .expect("user table read");
        assert_eq!(
            first_data_row_text(&drain(&mut client).await).as_deref(),
            Some(expected),
            "`{table}` is a USER table and must never be shadowed by the catalog router"
        );
    }
}

/// CHECK clauses reach the wire as SQL, not as the internal encoding.
#[tokio::test]
async fn wire_check_constraints_expose_the_clause() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("CREATE TABLE wck (qty INT, CONSTRAINT wck_qty_pos CHECK (qty > 0))")
        .await
        .expect("setup");
    let _ = drain(&mut client).await;

    handler
        .handle_single_query("SELECT constraint_name, check_clause FROM information_schema.check_constraints")
        .await
        .expect("check_constraints");
    let out = drain(&mut client).await;
    let rows = data_rows(&out);
    let row = rows
        .iter()
        .find(|r| cell(r, 0).as_deref() == Some("wck_qty_pos"))
        .unwrap_or_else(|| panic!("the CHECK must be listed over the wire; got {rows:?}"));
    let clause = cell(row, 1).unwrap_or_default();
    assert!(
        clause.contains("qty") && clause.contains('>'),
        "check_clause must be the SQL predicate over the wire, got {clause:?}"
    );
    assert!(
        !clause.contains("BinaryExpr"),
        "check_clause must not leak the internal expression encoding, got {clause:?}"
    );
}

// ---------------------------------------------------------------------------
// HC4 — roles / grants over the wire.
//
// These cover the paths the embedded suite CANNOT reach: the catalog
// interceptor's psql `\du` response, the command tags, and the SET intercept.
//
// None of this is enforcement. HeliosDB stores and reports roles and grants;
// it checks no privilege anywhere.
// ---------------------------------------------------------------------------

/// SQLSTATE codes carried by every ErrorResponse in a byte stream.
/// ErrorResponse payload is a sequence of (field-type byte, NUL-terminated
/// string); field `C` is the SQLSTATE.
fn sqlstates(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for (ty, payload) in parse_messages(bytes) {
        if ty != b'E' {
            continue;
        }
        let mut pos = 0;
        while pos < payload.len() && payload[pos] != 0 {
            let field = payload[pos];
            pos += 1;
            let start = pos;
            while pos < payload.len() && payload[pos] != 0 {
                pos += 1;
            }
            if field == b'C' {
                out.push(String::from_utf8_lossy(&payload[start..pos]).to_string());
            }
            pos += 1;
        }
    }
    out
}

/// psql's `\du` response is served by the catalog interceptor. It used to be
/// two hardcoded all-privilege superusers; it must now reflect the persisted
/// role catalog, with the created role's REAL bits.
#[tokio::test]
async fn wire_du_reflects_a_created_role() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("CREATE ROLE reporter NOLOGIN")
        .await
        .expect("CREATE ROLE over the wire");
    let _ = drain(&mut client).await;

    // The 11-column shape psql sends for \du.
    handler
        .handle_single_query(
            "SELECT r.rolname, r.rolsuper, r.rolinherit, r.rolcreaterole, r.rolcreatedb, \
             r.rolcanlogin, r.rolconnlimit, r.rolvaliduntil FROM pg_catalog.pg_roles r ORDER BY 1",
        )
        .await
        .expect("\\du query");
    let out = drain(&mut client).await;
    let rows = data_rows(&out);
    let row = rows
        .iter()
        .find(|r| cell(r, 0).as_deref() == Some("reporter"))
        .unwrap_or_else(|| panic!("\\du must list the created role; got {rows:?}"));
    // The wire renders booleans as PostgreSQL does: `t` / `f`.
    assert_eq!(
        cell(row, 1).as_deref(),
        Some("f"),
        "a created role is NOT a superuser — \\du used to claim it was"
    );
    assert_eq!(cell(row, 5).as_deref(), Some("f"), "NOLOGIN must be reported");
    // The built-ins are still listed for compatibility.
    assert!(
        rows.iter().any(|r| cell(r, 0).as_deref() == Some("postgres")),
        "the virtual built-in must remain listed"
    );
}

/// A GRANT issued over the wire is stored, and `table_privileges` reports it —
/// through the planner, since the substring router now defers these views.
#[tokio::test]
async fn wire_grant_is_stored_and_visible_in_table_privileges() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for setup in [
        "CREATE TABLE orders (id INT PRIMARY KEY)",
        "CREATE ROLE app",
        "GRANT SELECT ON orders TO app",
    ] {
        handler
            .handle_single_query(setup)
            .await
            .unwrap_or_else(|e| panic!("{setup}: {e}"));
        let out = drain(&mut client).await;
        assert!(
            sqlstates(&out).is_empty(),
            "{setup} must not error over the wire: {:?}",
            sqlstates(&out)
        );
    }

    handler
        .handle_single_query("SELECT grantee, table_name, privilege_type FROM information_schema.table_privileges")
        .await
        .expect("table_privileges");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(
        rows.len(),
        1,
        "the stored grant must be visible over the wire: {rows:?}"
    );
    assert_eq!(cell(&rows[0], 0).as_deref(), Some("app"));
    assert_eq!(cell(&rows[0], 1).as_deref(), Some("orders"));
    assert_eq!(cell(&rows[0], 2).as_deref(), Some("SELECT"));
}

/// Command tags: PostgreSQL parity. `GRANT` used to report `OK 0`, which made a
/// stored grant indistinguishable from the old discard-everything no-op.
#[tokio::test]
async fn wire_role_ddl_command_tags() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    handler
        .handle_single_query("CREATE TABLE orders (id INT PRIMARY KEY)")
        .await
        .unwrap();
    let _ = drain(&mut client).await;

    for (sql, tag) in [
        ("CREATE ROLE app", "CREATE ROLE"),
        ("ALTER ROLE app WITH LOGIN", "ALTER ROLE"),
        ("GRANT SELECT ON orders TO app", "GRANT"),
        ("REVOKE SELECT ON orders FROM app", "REVOKE"),
        ("DROP ROLE app", "DROP ROLE"),
        ("CREATE USER u LOGIN", "CREATE ROLE"),
        ("DROP USER u", "DROP ROLE"),
    ] {
        handler
            .handle_single_query(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let out = drain(&mut client).await;
        assert!(sqlstates(&out).is_empty(), "{sql} errored: {:?}", sqlstates(&out));
        assert!(
            command_tags(&out).iter().any(|t| t == tag),
            "{sql} must report the `{tag}` tag, got {:?}",
            command_tags(&out)
        );
    }
}

/// `SET ROLE <x>` used to be acked with `SET` and zero effect — telling a
/// client it had dropped to a restricted identity when it had not. It now
/// errors 0A000. `SET ROLE NONE` stays a genuine no-op ack.
#[tokio::test]
async fn wire_set_role_errors_but_set_role_none_still_acks() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for sql in ["SET ROLE readonly_user", "SET SESSION AUTHORIZATION readonly_user"] {
        handler
            .handle_single_query(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let out = drain(&mut client).await;
        assert_eq!(
            sqlstates(&out),
            vec!["0A000".to_string()],
            "{sql} must raise feature_not_supported, not a silent SET ack"
        );
        assert!(
            !command_tags(&out).iter().any(|t| t == "SET"),
            "{sql} must NOT be acknowledged as a successful SET"
        );
    }

    for sql in ["SET ROLE NONE", "SET SESSION AUTHORIZATION DEFAULT"] {
        handler
            .handle_single_query(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let out = drain(&mut client).await;
        assert!(
            sqlstates(&out).is_empty(),
            "{sql} must not error: {:?}",
            sqlstates(&out)
        );
        assert!(
            command_tags(&out).iter().any(|t| t == "SET"),
            "{sql} is a genuine no-op and must still ack"
        );
    }

    // A GUC whose name merely starts with `role` must keep the generic ack.
    handler.handle_single_query("SET role_cache = on").await.unwrap();
    let out = drain(&mut client).await;
    assert!(
        command_tags(&out).iter().any(|t| t == "SET"),
        "an unrelated GUC must not be mistaken for SET ROLE"
    );
}

/// SQLSTATE mapping for the role errors: 42710 duplicate_object,
/// 42704 undefined_object, 2BP01 dependent_objects_still_exist.
///
/// Driven through `dispatch_message` — the connection run loop's own body —
/// NOT through `handle_single_query` directly: a failing DDL statement
/// deliberately propagates out of the per-statement handler (so a
/// multi-statement simple query aborts on the first failure), and the
/// ErrorResponse a client actually receives is rendered one level up. Calling
/// the inner handler and unwrapping would assert a contract no driver sees.
#[tokio::test]
async fn wire_role_error_sqlstates() {
    use super::messages::FrontendMessage;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for setup in [
        "CREATE TABLE orders (id INT PRIMARY KEY)",
        "CREATE ROLE app",
        "GRANT SELECT ON orders TO app",
    ] {
        handler
            .dispatch_message(FrontendMessage::Query { query: setup.into() })
            .await
            .unwrap_or_else(|e| panic!("{setup}: {e}"));
        let out = drain(&mut client).await;
        assert!(
            sqlstates(&out).is_empty(),
            "{setup} must succeed over the wire, got {:?}",
            sqlstates(&out)
        );
    }

    for (sql, code) in [
        ("CREATE ROLE app", "42710"),
        ("DROP ROLE ghost", "42704"),
        ("ALTER ROLE ghost WITH LOGIN", "42704"),
        ("DROP ROLE app", "2BP01"),
    ] {
        handler
            .dispatch_message(FrontendMessage::Query { query: sql.into() })
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let out = drain(&mut client).await;
        assert_eq!(
            sqlstates(&out),
            vec![code.to_string()],
            "{sql} must map to SQLSTATE {code}"
        );
        // A rejected statement must NOT also be acknowledged as done: an
        // `ErrorResponse` beside a `CREATE ROLE` / `DROP ROLE` CommandComplete
        // is exactly the "silently succeeded" shape this slice exists to
        // remove.
        assert!(
            command_tags(&out).is_empty(),
            "{sql} was rejected and must not also be acked, got {:?}",
            command_tags(&out)
        );
        // ... and the connection must stay usable: the simple-query error path
        // owes the client a ReadyForQuery, or every subsequent statement in
        // this loop would hang a real driver.
        assert!(
            parse_messages(&out).iter().any(|(ty, _)| *ty == b'Z'),
            "{sql} must be followed by ReadyForQuery"
        );
    }
}

/// `DROP INDEX` over the PG WIRE: the command tag and the error SQLSTATEs.
///
/// Worth pinning separately from the engine tests because the wire is where
/// drivers read both, and because through 4.19.0 this statement DROPPED A TABLE
/// while reporting `OK 0`. The three shapes are:
///   * success                    → `DROP INDEX` tag, no SQLSTATE;
///   * missing index              → 42704 undefined_object (NOT 42P01 — the
///     message must never be classified as being about a relation);
///   * PK/UNIQUE backing index    → 2BP01 dependent_objects_still_exist.
///
/// Driven through `dispatch_message` for the same reason
/// `wire_role_error_sqlstates` is: the ErrorResponse a client receives is
/// rendered one level above `handle_single_query`.
#[tokio::test]
async fn wire_drop_index_tag_and_error_sqlstates() {
    use super::messages::FrontendMessage;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for setup in [
        "CREATE TABLE docs (id INT PRIMARY KEY, status TEXT)",
        "INSERT INTO docs (id, status) VALUES (1, 'open')",
        "CREATE INDEX docs_status_idx ON docs (status)",
    ] {
        handler
            .dispatch_message(FrontendMessage::Query { query: setup.into() })
            .await
            .unwrap_or_else(|e| panic!("{setup}: {e}"));
        let out = drain(&mut client).await;
        assert!(
            sqlstates(&out).is_empty(),
            "{setup} must succeed over the wire, got {:?}",
            sqlstates(&out)
        );
    }

    // Success: PostgreSQL's tag, not `OK 0`.
    handler
        .dispatch_message(FrontendMessage::Query {
            query: "DROP INDEX docs_status_idx".into(),
        })
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "DROP INDEX must succeed over the wire, got {:?}",
        sqlstates(&out)
    );
    assert!(
        command_tags(&out).iter().any(|t| t == "DROP INDEX"),
        "DROP INDEX must report the `DROP INDEX` tag, got {:?}",
        command_tags(&out)
    );

    // IF EXISTS on the now-missing index is a genuine no-op success.
    handler
        .dispatch_message(FrontendMessage::Query {
            query: "DROP INDEX IF EXISTS docs_status_idx".into(),
        })
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "DROP INDEX IF EXISTS on a missing index must not error, got {:?}",
        sqlstates(&out)
    );

    for (sql, code) in [
        // The index is gone, and `docs` is a TABLE — this must be 42704
        // undefined_object, never 42P01 undefined_table.
        ("DROP INDEX docs_status_idx", "42704"),
        ("DROP INDEX docs", "42704"),
        // The PRIMARY KEY's backing index is refused, not dropped.
        ("DROP INDEX docs_pkey", "2BP01"),
    ] {
        handler
            .dispatch_message(FrontendMessage::Query { query: sql.into() })
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let out = drain(&mut client).await;
        assert_eq!(
            sqlstates(&out),
            vec![code.to_string()],
            "{sql} must map to SQLSTATE {code}"
        );
        assert!(
            command_tags(&out).is_empty(),
            "{sql} was rejected and must not also be acked, got {:?}",
            command_tags(&out)
        );
    }

    // The TABLE that shares a name with a failed `DROP INDEX` is untouched.
    handler
        .dispatch_message(FrontendMessage::Query {
            query: "SELECT id FROM docs".into(),
        })
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "*** DATA LOSS *** `DROP INDEX docs` destroyed the TABLE docs: {:?}",
        sqlstates(&out)
    );
}

/// The SUBSTRING-HIJACK boundary for the DROP INDEX SQLSTATE arms — Task #38's
/// question asked of a different classifier.
///
/// The first draft of `sqlstate_for_query_execution_message`'s index arms tested
/// a bare `lower.contains("index")` and sat AHEAD of the table/relation rules,
/// so every error about a TABLE whose name merely contains "index" was
/// reclassified: `Table 'app_pg_indexes' does not exist` became 42704
/// undefined_object instead of 42P01 undefined_table, and `already exists`
/// became 42710 instead of 42P07. psycopg/Django raise `UndefinedTable` on
/// 42P01, and Rails/sqlx migrations use 42P07 for idempotency, so this is a
/// driver-facing contract, not cosmetics.
///
/// `app_pg_indexes` is the same table name Task #38 used for the catalog-router
/// boundary (`wire_user_tables_named_after_catalog_views_are_not_shadowed`) —
/// deliberately, because it is the shape that keeps catching this repo out.
#[tokio::test]
async fn wire_index_named_table_still_maps_to_undefined_table() {
    use super::messages::FrontendMessage;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    // Missing table whose name ends in "index"/"indexes" → 42P01, never 42704.
    for sql in ["SELECT * FROM app_pg_indexes", "SELECT * FROM search_index"] {
        handler
            .dispatch_message(FrontendMessage::Query { query: sql.into() })
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let out = drain(&mut client).await;
        assert_eq!(
            sqlstates(&out),
            vec!["42P01".to_string()],
            "{sql} names a missing TABLE — 42P01 undefined_table, not the index class"
        );
    }

    // ... and the duplicate-table half: 42P07, never 42710 duplicate_object.
    handler
        .dispatch_message(FrontendMessage::Query {
            query: "CREATE TABLE app_pg_indexes (id INT PRIMARY KEY)".into(),
        })
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "setup CREATE TABLE must succeed, got {:?}",
        sqlstates(&out)
    );

    handler
        .dispatch_message(FrontendMessage::Query {
            query: "CREATE TABLE app_pg_indexes (id INT PRIMARY KEY)".into(),
        })
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert_eq!(
        sqlstates(&out),
        vec!["42P07".to_string()],
        "a duplicate TABLE whose name contains \"index\" must be 42P07 duplicate_table"
    );

    // The genuine index error still classifies as an index error, so the fix
    // is an anchor and not a blanket disable.
    handler
        .dispatch_message(FrontendMessage::Query {
            query: "DROP INDEX app_pg_indexes".into(),
        })
        .await
        .unwrap();
    let out = drain(&mut client).await;
    assert_eq!(
        sqlstates(&out),
        vec!["42704".to_string()],
        "`DROP INDEX <name>` on a missing index is still 42704 undefined_object"
    );
}

/// HC3 deleted the PG wire's fixed-shape `information_schema` implementations,
/// so foreign-key reflection now travels the planner route on every interface.
/// `tests/markon_a4_reflection.rs` pins the engine half of that contract; this
/// pins the PG-WIRE half, which is where drizzle-kit / Prisma / Alembic
/// actually read it from. It also exercises the capability the deleted
/// substring router never had — a real WHERE clause — so "we deferred to the
/// planner" cannot quietly become "we returned everything" or "we returned
/// nothing".
#[tokio::test]
async fn wire_information_schema_exposes_foreign_key_reflection() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);
    for stmt in [
        "CREATE TABLE wfk_parent (id INT PRIMARY KEY)",
        "CREATE TABLE wfk_child (id INT PRIMARY KEY, parent_id INT, \
         CONSTRAINT wfk_child_parent_fk FOREIGN KEY(parent_id) REFERENCES wfk_parent(id))",
    ] {
        handler.handle_single_query(stmt).await.expect("setup");
        let _ = drain(&mut client).await;
    }

    handler
        .handle_single_query(
            "SELECT constraint_name, table_name, column_name FROM information_schema.key_column_usage \
             WHERE constraint_name = 'wfk_child_parent_fk'",
        )
        .await
        .expect("key_column_usage over the wire");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description_names(&out),
        vec![
            "constraint_name".to_string(),
            "table_name".to_string(),
            "column_name".to_string(),
        ]
    );
    let rows = data_rows(&out);
    // `wfk_child` also carries `wfk_child_pkey`, so an unfiltered or
    // wrongly-filtered answer cannot be exactly one row.
    assert_eq!(
        rows.len(),
        1,
        "the WHERE clause must be honoured over the wire; got {rows:?}"
    );
    let fk = rows.first().expect("the FK key_column_usage row");
    assert_eq!(cell(fk, 0).as_deref(), Some("wfk_child_parent_fk"));
    assert_eq!(cell(fk, 1).as_deref(), Some("wfk_child"));
    assert_eq!(cell(fk, 2).as_deref(), Some("parent_id"));

    handler
        .handle_single_query(
            "SELECT constraint_type FROM information_schema.table_constraints \
             WHERE constraint_name = 'wfk_child_parent_fk'",
        )
        .await
        .expect("table_constraints over the wire");
    let tc = data_rows(&drain(&mut client).await);
    assert_eq!(
        tc.len(),
        1,
        "table_constraints must expose exactly the FK constraint; got {tc:?}"
    );
    assert_eq!(tc.first().and_then(|r| cell(r, 0)).as_deref(), Some("FOREIGN KEY"));
}

// ---------------------------------------------------------------------------
// Task #89 / #86 — driver-facing error SQLSTATEs for NON-ROW-RETURNING
// statements, and the object-noun anchors that produce them.
//
// Everything below is driven through `dispatch_message` — the connection run
// loop's own body — for the reason spelled out on `wire_role_error_sqlstates`:
// a failing DDL/DML statement deliberately propagates out of
// `handle_single_query`, and the ErrorResponse a client actually receives is
// rendered one level up. Each case asserts the full three-part contract:
//
//   1. the SQLSTATE the driver branches on,
//   2. that a REJECTED statement is NOT also acknowledged with a CommandComplete
//      (the "silently succeeded" shape), and
//   3. that a ReadyForQuery follows, so the connection stays usable.
// ---------------------------------------------------------------------------

/// Transaction-status byte carried by each `ReadyForQuery`:
/// `I` idle, `T` inside a transaction block, `E` failed transaction.
fn ready_for_query_statuses(bytes: &[u8]) -> Vec<u8> {
    parse_messages(bytes)
        .into_iter()
        .filter(|(ty, _)| *ty == b'Z')
        .filter_map(|(_, payload)| payload.first().copied())
        .collect()
}

/// Send one simple query through the run-loop body and return the raw reply.
/// Boxed rather than a plain `async fn`, and that is load-bearing.
///
/// `dispatch_message`'s future is ~65 KB (clippy's `large_futures` lint flags it).
/// An `async fn`'s state is inlined into its CALLER's future, so a test issuing a
/// dozen statements accumulates a dozen copies and blows the 2 MB test-thread
/// stack — observed as `fatal runtime error: stack overflow` aborting the WHOLE
/// lib test binary with SIGABRT, which takes ~2,300 unrelated tests down with it
/// and reports no result at all. Boxing makes each await cost one pointer.
fn wire_query<'a>(
    handler: &'a mut PgConnectionHandler<DuplexStream>,
    client: &'a mut DuplexStream,
    sql: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + 'a>> {
    use super::messages::FrontendMessage;
    Box::pin(async move {
        handler
            .dispatch_message(FrontendMessage::Query { query: sql.into() })
            .await
            .unwrap_or_else(|e| panic!("dispatch `{sql}`: {e}"));
        drain(client).await
    })
}

/// Setup statement that must not produce an ErrorResponse.
fn wire_setup<'a>(
    handler: &'a mut PgConnectionHandler<DuplexStream>,
    client: &'a mut DuplexStream,
    sql: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let out = wire_query(handler, client, sql).await;
        assert!(
            sqlstates(&out).is_empty(),
            "setup `{sql}` must succeed over the wire, got {:?}",
            sqlstates(&out)
        );
    })
}

/// A statement that must be rejected with exactly `code`, with no command tag
/// and with a trailing ReadyForQuery.
fn assert_wire_sqlstate<'a>(
    handler: &'a mut PgConnectionHandler<DuplexStream>,
    client: &'a mut DuplexStream,
    sql: &'a str,
    code: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let out = wire_query(handler, client, sql).await;
        assert_eq!(
            sqlstates(&out),
            vec![code.to_string()],
            "`{sql}` must map to SQLSTATE {code}"
        );
        assert!(
            command_tags(&out).is_empty(),
            "`{sql}` was rejected and must not also be acked, got {:?}",
            command_tags(&out)
        );
        assert!(
            parse_messages(&out).iter().any(|(ty, _)| *ty == b'Z'),
            "`{sql}` must be followed by ReadyForQuery"
        );
    })
}

/// #89, the role arms. `roles` / `user_roles` are ordinary table names — Rails,
/// Django and Supabase schemas all ship them — and through v4.21.0 the bare
/// `lower.contains("role")` arms (shipped live in v4.20.0) reclassified every
/// error about them: `Table 'roles' does not exist` returned 42704
/// undefined_object instead of 42P01 undefined_table, and `already exists`
/// returned 42710 instead of 42P07.
///
/// The second half is the point: a GENUINE role error must still be classified
/// as one, so the fix is an anchor and not a blanket disable.
#[tokio::test]
async fn wire_role_named_table_still_maps_to_undefined_table() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for sql in ["SELECT * FROM roles", "SELECT * FROM user_roles"] {
        assert_wire_sqlstate(&mut handler, &mut client, sql, "42P01").await;
    }

    wire_setup(
        &mut handler,
        &mut client,
        "CREATE TABLE user_roles (id INT PRIMARY KEY, label TEXT)",
    )
    .await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "CREATE TABLE user_roles (id INT PRIMARY KEY, label TEXT)",
        "42P07",
    )
    .await;

    // The anchor half: every role emitter writes `role "<name>"`, so real role
    // errors keep their codes.
    assert_wire_sqlstate(&mut handler, &mut client, "DROP ROLE nosuchrole", "42704").await;
    assert_wire_sqlstate(&mut handler, &mut client, "ALTER ROLE nosuchrole WITH LOGIN", "42704").await;
    // `postgres` is reserved (`storage::RESERVED_ROLE_NAMES`) → 42501.
    assert_wire_sqlstate(&mut handler, &mut client, "CREATE ROLE postgres", "42501").await;
}

/// #89, the ORDERING half of the role bug. The role arms sit ahead of the
/// column arm, so a message that mentions BOTH — `Column 'user_role' not found`
/// — used to be classified as an undefined ROLE (42704) instead of an undefined
/// COLUMN (42703). psycopg/Django raise `UndefinedColumn` on 42703.
#[tokio::test]
async fn wire_column_whose_name_contains_role_maps_to_undefined_column() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    // A row is required: projection expressions are evaluated per row, so an
    // empty table returns zero rows instead of the column error.
    wire_setup(
        &mut handler,
        &mut client,
        "CREATE TABLE role_probe (id INT PRIMARY KEY, label TEXT)",
    )
    .await;
    wire_setup(&mut handler, &mut client, "INSERT INTO role_probe VALUES (1, 'x')").await;

    assert_wire_sqlstate(&mut handler, &mut client, "SELECT user_role FROM role_probe", "42703").await;
    assert_wire_sqlstate(&mut handler, &mut client, "SELECT nosuchcol FROM role_probe", "42703").await;
}

/// #89, the function and column arms. `functions` and `columns` are ordinary
/// table names; `Table 'functions' does not exist` used to return 42883
/// undefined_function and `Table 'columns' does not exist` 42703
/// undefined_column, because both arms were bare `contains` tests sitting ahead
/// of the table rules.
#[tokio::test]
async fn wire_function_and_column_named_tables_still_map_to_undefined_table() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for sql in ["SELECT * FROM functions", "SELECT * FROM columns"] {
        assert_wire_sqlstate(&mut handler, &mut client, sql, "42P01").await;
    }

    wire_setup(&mut handler, &mut client, "CREATE TABLE columns (id INT PRIMARY KEY)").await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "CREATE TABLE columns (id INT PRIMARY KEY)",
        "42P07",
    )
    .await;
}

/// #89, the anchor's hardest requirement: the UNQUOTED function emitters.
///
/// `Unknown scalar function: f` (src/sql/evaluator.rs) and
/// `Unknown window function: F` (src/sql/planner.rs) do not quote the name, so
/// an anchor built only from `function '` / `function "` would have demoted the
/// single most common function error a driver ever sees from 42883 to
/// `XX000 internal_error` — strictly WORSE than the wrong-but-specific code the
/// audit set out to fix. That degradation is what the `function: ` alternative
/// exists for, and this is its regression test.
#[tokio::test]
async fn wire_unquoted_unknown_function_shapes_map_to_undefined_function() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE fn_probe (id INT PRIMARY KEY)").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO fn_probe VALUES (1)").await;

    // `Unknown scalar function: no_such_scalar_fn`
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "SELECT no_such_scalar_fn(id) FROM fn_probe",
        "42883",
    )
    .await;
    // `Unknown window function: NO_SUCH_WINDOW_FN`
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "SELECT no_such_window_fn() OVER () FROM fn_probe",
        "42883",
    )
    .await;
}

/// #89 bonus arms, both same-class and both previously wrong:
///
/// * `Column 'c' already exists in table 't'` reached the
///   `(table||relation) && already exists` arm and reported 42P07
///   duplicate_TABLE for a duplicate COLUMN. PostgreSQL uses 42701.
/// * `Function 'f' already exists` matched nothing and reported
///   `XX000 internal_error`. PostgreSQL uses 42723.
#[tokio::test]
async fn wire_duplicate_column_and_duplicate_function_sqlstates() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE dup_col (id INT PRIMARY KEY)").await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "ALTER TABLE dup_col ADD COLUMN id INT",
        "42701",
    )
    .await;

    wire_setup(
        &mut handler,
        &mut client,
        "CREATE FUNCTION dup_fn() RETURNS INTEGER LANGUAGE sql AS $$SELECT 1$$",
    )
    .await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "CREATE FUNCTION dup_fn() RETURNS INTEGER LANGUAGE sql AS $$SELECT 1$$",
        "42723",
    )
    .await;
}

/// #86 P0 — the missing cell. 23503 foreign_key_violation had NO coverage of
/// any kind (no wire test, no unit test), and it is the code an ORM branches on
/// to distinguish "your parent row is missing" from "retry this upsert".
///
/// Also the substring-hijack boundary: the FK message interpolates the OFFENDING
/// PARENT VALUES, so with the classifier's `contains("unique")` arm sitting
/// ahead of the `foreign key` arm, inserting the literal text `unique` against a
/// missing parent reported 23505 unique_violation — and an ON CONFLICT retry
/// loop keyed on 23505 would retry forever. The reverse direction is asserted
/// too, so the fix is an anchor and not a reordering that merely moves the bug.
#[tokio::test]
async fn wire_fk_violation_is_23503_and_is_not_hijacked_by_unique() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for stmt in [
        "CREATE TABLE fk_parent (id INT PRIMARY KEY)",
        "CREATE TABLE fk_child (id INT PRIMARY KEY, pid INT REFERENCES fk_parent(id) ON DELETE RESTRICT)",
        "INSERT INTO fk_parent VALUES (1)",
        "INSERT INTO fk_child VALUES (10, 1)",
    ] {
        wire_setup(&mut handler, &mut client, stmt).await;
    }

    // INSERT, UPDATE and the ON DELETE RESTRICT parent delete are three
    // different emitters; all three are 23503.
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "INSERT INTO fk_child VALUES (11, 999)",
        "23503",
    )
    .await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "UPDATE fk_child SET pid = 998 WHERE id = 10",
        "23503",
    )
    .await;
    assert_wire_sqlstate(&mut handler, &mut client, "DELETE FROM fk_parent WHERE id = 1", "23503").await;

    // The hijack: the offending value is the literal text `unique`.
    for stmt in [
        "CREATE TABLE unique_codes (code TEXT PRIMARY KEY)",
        "CREATE TABLE code_child (id INT PRIMARY KEY, code TEXT REFERENCES unique_codes(code))",
    ] {
        wire_setup(&mut handler, &mut client, stmt).await;
    }
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "INSERT INTO code_child VALUES (1, 'unique')",
        "23503",
    )
    .await;

    // ... and the other direction: a GENUINE unique violation on a table whose
    // name contains "foreign" must still be 23505.
    wire_setup(
        &mut handler,
        &mut client,
        "CREATE TABLE foreign_key_registry (id INT PRIMARY KEY)",
    )
    .await;
    wire_setup(&mut handler, &mut client, "INSERT INTO foreign_key_registry VALUES (1)").await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "INSERT INTO foreign_key_registry VALUES (1)",
        "23505",
    )
    .await;
}

/// #86 P1 — 42P01 and 42703 were covered only through SELECT. These are the
/// non-row-returning routes: different planner paths, same codes.
#[tokio::test]
async fn wire_dml_against_missing_table_and_column_sqlstates() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    for sql in [
        "INSERT INTO ghost_table VALUES (1)",
        "UPDATE ghost_table SET x = 1",
        "DELETE FROM ghost_table",
    ] {
        assert_wire_sqlstate(&mut handler, &mut client, sql, "42P01").await;
    }

    wire_setup(&mut handler, &mut client, "CREATE TABLE dml_probe (id INT PRIMARY KEY)").await;
    assert_wire_sqlstate(
        &mut handler,
        &mut client,
        "INSERT INTO dml_probe (no_such_col) VALUES (1)",
        "42703",
    )
    .await;
}

/// #86 P0 — the EXTENDED protocol, which had ZERO error-SQLSTATE coverage even
/// though it is the family every real driver uses (psycopg3, JDBC, sqlx,
/// node-postgres, Drizzle) and the one the REST/BaaS layer writes through.
///
/// Asserts the three things the simple-query path cannot tell you:
///   * the code matches the simple-query path for the same violation,
///   * NO ReadyForQuery is emitted before the client's Sync (sending it early
///     wedges drivers), and
///   * after Sync the connection is usable again.
#[tokio::test]
async fn wire_extended_protocol_dml_error_sqlstates() {
    use super::messages::FrontendMessage;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE ext_uniq (id INT PRIMARY KEY)").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO ext_uniq VALUES (1)").await;

    handler
        .dispatch_message(FrontendMessage::Parse {
            statement_name: "s_dup".into(),
            query: "INSERT INTO ext_uniq VALUES ($1)".into(),
            param_types: vec![23],
        })
        .await
        .expect("parse");
    handler
        .dispatch_message(FrontendMessage::Bind {
            portal_name: "p_dup".into(),
            statement_name: "s_dup".into(),
            param_formats: vec![0],
            params: vec![Some(b"1".to_vec())],
            result_formats: vec![],
        })
        .await
        .expect("bind");
    let _ = drain(&mut client).await;

    handler
        .dispatch_message(FrontendMessage::Execute {
            portal_name: "p_dup".into(),
            max_rows: 0,
        })
        .await
        .expect("execute");
    let out = drain(&mut client).await;
    assert_eq!(
        sqlstates(&out),
        vec!["23505".to_string()],
        "the extended path must report the same 23505 the simple path does"
    );
    assert!(
        command_tags(&out).is_empty(),
        "a rejected Execute must not also be acked, got {:?}",
        command_tags(&out)
    );
    assert!(
        ready_for_query_statuses(&out).is_empty(),
        "extended-protocol errors must defer ReadyForQuery until Sync"
    );

    handler.dispatch_message(FrontendMessage::Sync).await.expect("sync");
    let out = drain(&mut client).await;
    assert!(
        !ready_for_query_statuses(&out).is_empty(),
        "Sync must release the deferred ReadyForQuery"
    );

    // The connection is still usable.
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM ext_uniq").await;
    assert!(
        sqlstates(&out).is_empty(),
        "the connection must survive the extended-protocol error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(data_rows(&out).len(), 1);
}

// ---------------------------------------------------------------------------
// Task #104 — ONE transaction-control classifier.
//
// All four copies prefix-matched BEGIN/START TRANSACTION but used an EXACT
// `eq_ignore_ascii_case` for COMMIT and ROLLBACK, so `END`, `ABORT`,
// `COMMIT WORK`, `COMMIT TRANSACTION`, `ROLLBACK WORK` and
// `ROLLBACK TRANSACTION` all fell through to the SQL executor — which has no
// `Commit` operator. **`END;` did not commit.**
// ---------------------------------------------------------------------------

/// `END` commits, `ROLLBACK WORK` / `ABORT` roll back, and every spelling
/// reports the tag PostgreSQL reports (`COMMIT` for END, `ROLLBACK` for ABORT)
/// and leaves the connection idle.
#[tokio::test]
async fn wire_standard_transaction_control_spellings_reach_the_boundary() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE txnctl (id INT PRIMARY KEY)").await;

    // END must COMMIT.
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO txnctl VALUES (1)").await;
    let out = wire_query(&mut handler, &mut client, "END").await;
    assert!(
        sqlstates(&out).is_empty(),
        "`END` must not error (it used to reach `Operator not yet implemented: Commit`), got {:?}",
        sqlstates(&out)
    );
    assert_eq!(
        command_tags(&out),
        vec!["COMMIT".to_string()],
        "PostgreSQL reports END with the COMMIT tag"
    );
    assert_eq!(
        ready_for_query_statuses(&out),
        vec![b'I'],
        "`END` must leave the connection idle, not inside a transaction"
    );
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM txnctl").await;
    assert_eq!(data_rows(&out).len(), 1, "*** `END;` did not commit ***");

    // ROLLBACK WORK must roll back.
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO txnctl VALUES (2)").await;
    let out = wire_query(&mut handler, &mut client, "ROLLBACK WORK").await;
    assert!(sqlstates(&out).is_empty(), "`ROLLBACK WORK` must not error");
    assert_eq!(command_tags(&out), vec!["ROLLBACK".to_string()]);
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM txnctl").await;
    assert_eq!(data_rows(&out).len(), 1, "*** `ROLLBACK WORK;` did not roll back ***");

    // ABORT is PostgreSQL's synonym for ROLLBACK.
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO txnctl VALUES (3)").await;
    let out = wire_query(&mut handler, &mut client, "ABORT").await;
    assert!(sqlstates(&out).is_empty(), "`ABORT` must not error");
    assert_eq!(command_tags(&out), vec!["ROLLBACK".to_string()]);
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM txnctl").await;
    assert_eq!(data_rows(&out).len(), 1, "`ABORT` must roll back");

    // COMMIT TRANSACTION.
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO txnctl VALUES (4)").await;
    let out = wire_query(&mut handler, &mut client, "COMMIT TRANSACTION").await;
    assert!(sqlstates(&out).is_empty(), "`COMMIT TRANSACTION` must not error");
    assert_eq!(command_tags(&out), vec!["COMMIT".to_string()]);
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM txnctl").await;
    assert_eq!(data_rows(&out).len(), 2, "`COMMIT TRANSACTION` must commit");

    // `AND NO CHAIN` is the default behaviour, spelled out.
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO txnctl VALUES (5)").await;
    let out = wire_query(&mut handler, &mut client, "COMMIT WORK AND NO CHAIN").await;
    assert!(sqlstates(&out).is_empty(), "`COMMIT WORK AND NO CHAIN` must not error");
    assert_eq!(ready_for_query_statuses(&out), vec![b'I']);
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM txnctl").await;
    assert_eq!(data_rows(&out).len(), 3);
}

/// `COMMIT AND CHAIN` must open the NEXT transaction immediately. Accepting the
/// spelling and then not chaining would leave every following statement
/// autocommitting — an atomicity hole dressed as success.
#[tokio::test]
async fn wire_commit_and_chain_opens_the_next_transaction() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE chained (id INT PRIMARY KEY)").await;
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO chained VALUES (1)").await;

    let out = wire_query(&mut handler, &mut client, "COMMIT AND CHAIN").await;
    assert!(sqlstates(&out).is_empty(), "`COMMIT AND CHAIN` must not error");
    assert_eq!(
        ready_for_query_statuses(&out),
        vec![b'T'],
        "`AND CHAIN` must leave the connection INSIDE the next transaction"
    );

    // Proof it is a real transaction: this write is discarded by ROLLBACK.
    wire_setup(&mut handler, &mut client, "INSERT INTO chained VALUES (2)").await;
    wire_setup(&mut handler, &mut client, "ROLLBACK").await;
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM chained").await;
    assert_eq!(
        data_rows(&out).len(),
        1,
        "the chained transaction's write must be rolled back, so only the committed row survives"
    );
}

/// `ROLLBACK TO [SAVEPOINT] n` is NOT a transaction boundary and must never be
/// intercepted by the transaction-control classifier — doing so would silently
/// turn a partial rollback into a full one. This pins the CLASSIFICATION, not
/// the savepoint engine (that contract lives in
/// `tests/savepoint_rollback_regression_tests.rs`): after `ROLLBACK TO
/// SAVEPOINT` the connection must STILL be inside the transaction, and the
/// pre-savepoint work must survive the COMMIT.
#[tokio::test]
async fn wire_rollback_to_savepoint_does_not_end_the_transaction() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE sp_probe (id INT PRIMARY KEY)").await;
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO sp_probe VALUES (1)").await;
    wire_setup(&mut handler, &mut client, "SAVEPOINT sp1").await;

    let out = wire_query(&mut handler, &mut client, "ROLLBACK TO SAVEPOINT sp1").await;
    assert!(
        sqlstates(&out).is_empty(),
        "`ROLLBACK TO SAVEPOINT` must not error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(
        ready_for_query_statuses(&out),
        vec![b'T'],
        "`ROLLBACK TO SAVEPOINT` must leave the transaction OPEN"
    );

    wire_setup(&mut handler, &mut client, "COMMIT").await;
    let out = wire_query(&mut handler, &mut client, "SELECT id FROM sp_probe").await;
    assert_eq!(
        data_rows(&out).len(),
        1,
        "work done before the savepoint must survive the COMMIT"
    );
}

/// The extended protocol reaches the SAME classifier: a driver that prepares
/// `END` (psycopg3, JDBC and sqlx all send Parse/Bind/Execute for statements
/// they cache) must commit, not receive an executor error.
#[tokio::test]
async fn wire_extended_execute_of_end_commits() {
    use super::messages::FrontendMessage;
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let (mut handler, mut client) = test_handler(db);

    wire_setup(&mut handler, &mut client, "CREATE TABLE ext_txn (id INT PRIMARY KEY)").await;
    wire_setup(&mut handler, &mut client, "BEGIN").await;
    wire_setup(&mut handler, &mut client, "INSERT INTO ext_txn VALUES (1)").await;

    handler
        .dispatch_message(FrontendMessage::Parse {
            statement_name: "s_end".into(),
            query: "END".into(),
            param_types: vec![],
        })
        .await
        .expect("parse END");
    handler
        .dispatch_message(FrontendMessage::Bind {
            portal_name: "p_end".into(),
            statement_name: "s_end".into(),
            param_formats: vec![],
            params: vec![],
            result_formats: vec![],
        })
        .await
        .expect("bind END");
    let _ = drain(&mut client).await;
    handler
        .dispatch_message(FrontendMessage::Execute {
            portal_name: "p_end".into(),
            max_rows: 0,
        })
        .await
        .expect("execute END");
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "extended `END` must not error, got {:?}",
        sqlstates(&out)
    );

    handler.dispatch_message(FrontendMessage::Sync).await.expect("sync");
    let _ = drain(&mut client).await;

    let out = wire_query(&mut handler, &mut client, "SELECT id FROM ext_txn").await;
    assert_eq!(
        data_rows(&out).len(),
        1,
        "*** extended-protocol `END` did not commit ***"
    );
}

// ---------------------------------------------------------------------------
// Prisma P0 spec 02 — RowDescription names for DML … RETURNING.
//
// Prisma 7.10 fully qualifies every RETURNING item and then maps the result
// row BY COLUMN NAME. Through v4.30.0 the planner named an unaliased RETURNING
// item with sqlparser's `Display` (`Planner::convert_returning` →
// `format!("{expr}")`), which re-emits the quote characters, so the wire
// advertised a field literally called `"public"."Account"."id"` where
// PostgreSQL calls it `id` — and the portal needed a client-side rename shim.
// A qualified column reference is now lowered to `ReturningItem::Column` on
// its bare part — same name as the SELECT list gives it, plus the catalog
// column's TYPE (int4, not the `Expression` fallback's hard-coded text) and its
// VALUE (resolved by bare name, not by a byte-exact qualifier match that a
// case-folded or aliased qualifier could never satisfy). Everything that is not
// a column reference keeps its `Expression` lowering but takes its name from
// `Planner::extract_expr_alias`, the same function the SELECT projection list
// uses.
//
// Both wire routes derive the RowDescription from the plan's ReturningItems
// via `EmbeddedDatabase::returning_schema`, but through different call sites:
// Describe on the extended protocol (`handler_extended::derive_result_schema`)
// and `derive_returning_schema` on the simple-query path.
// ---------------------------------------------------------------------------

/// Decode a RowDescription into `(name, data_type_oid, format_code)` per field.
/// [`row_description`] above drops the format code; the binary-result contract
/// below needs all three, because the defect was a field whose DECLARED type
/// and ACTUAL encoding disagreed.
fn row_description_fields(bytes: &[u8]) -> Vec<(String, i32, i16)> {
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
            // skip table_oid(i32) + column_attr_num(i16)
            let oid_pos = pos + 4 + 2;
            let oid = i32::from_be_bytes([
                payload[oid_pos],
                payload[oid_pos + 1],
                payload[oid_pos + 2],
                payload[oid_pos + 3],
            ]);
            // …then skip data_type_size(i16) + type_modifier(i32)
            let fmt_pos = oid_pos + 4 + 2 + 4;
            let format = i16::from_be_bytes([payload[fmt_pos], payload[fmt_pos + 1]]);
            out.push((name, oid, format));
            pos += 18;
        }
    }
    out
}

/// Raw bytes of a DataRow cell, `None` for SQL NULL. The text-mode sibling
/// [`cell`] is lossy, and a binary-format cell is not UTF-8 at all.
fn cell_bytes(row: &[Option<Vec<u8>>], idx: usize) -> Option<&[u8]> {
    row.get(idx).and_then(|v| v.as_deref())
}

/// The Prisma `Account` table, quoted and mixed-case exactly as Prisma emits it.
fn prisma_account_db() -> Arc<EmbeddedDatabase> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute(r#"CREATE TABLE "Account" (id INT PRIMARY KEY, email TEXT, "createdAt" TEXT, "updatedAt" TEXT)"#)
        .expect("create");
    db
}

/// Extended protocol (psycopg3 / Prisma / JDBC): the exact statement a
/// `prisma.account.create()` sends. Describe must advertise `id, email,
/// createdAt, updatedAt`.
///
/// FAILS on the unfixed tree twice over: the four field names come back as
/// `"public"."Account"."id"`, `"public"."Account"."email"`,
/// `"public"."Account"."createdAt"`, `"public"."Account"."updatedAt"` — and
/// `id` is advertised with OID 25 (text) instead of 23 (int4), because a
/// qualified reference was lowered to `ReturningItem::Expression`, which
/// `EmbeddedDatabase::returning_schema` types as `DataType::Text` regardless of
/// the catalog.
#[tokio::test]
async fn prisma_insert_returning_describe_names_bare_columns() {
    let db = prisma_account_db();
    let (mut handler, mut client) = test_handler(db);

    let sql = r#"INSERT INTO "public"."Account" ("id","email","createdAt","updatedAt") VALUES ($1,$2,$3,$4) RETURNING "public"."Account"."id", "public"."Account"."email", "public"."Account"."createdAt", "public"."Account"."updatedAt""#;
    handler
        .handle_parse_extended("prisma_ins".into(), sql.into(), vec![23, 25, 25, 25])
        .await
        .expect("parse");
    handler
        .handle_describe_extended(super::messages::DescribeTarget::Statement, "prisma_ins".into())
        .await
        .expect("describe");
    let described = drain(&mut client).await;
    assert_eq!(
        row_description_names(&described),
        vec![
            "id".to_string(),
            "email".to_string(),
            "createdAt".to_string(),
            "updatedAt".to_string()
        ],
        "RETURNING field names must be the bare column names PostgreSQL uses — \
         quotes stripped, quoted case preserved"
    );
    // …and the TYPES must come from the catalog column, exactly as the bare
    // `RETURNING id` spelling already did. A qualified reference lowered to
    // `ReturningItem::Expression` was hard-coded to `DataType::Text` by
    // `EmbeddedDatabase::returning_schema`, so the same INT column was
    // advertised as int4 (23) when written bare and as text (25) when written
    // the way Prisma writes it.
    assert_eq!(
        row_description(&described),
        vec![
            ("id".to_string(), 23),
            ("email".to_string(), 25),
            ("createdAt".to_string(), 25),
            ("updatedAt".to_string(), 25)
        ],
        "a qualified RETURNING reference must carry the CATALOG column's type \
         (id → int4/23), not the `Expression` fallback's hard-coded text/25"
    );

    // The rename must not disturb the rows: Bind/Execute still returns the
    // inserted row, in RETURNING order.
    let params: Vec<Option<Vec<u8>>> = vec![
        Some(b"1".to_vec()),
        Some(b"a@example.com".to_vec()),
        Some(b"2026-09-06".to_vec()),
        Some(b"2026-09-07".to_vec()),
    ];
    handler
        .handle_bind_extended("prisma_p".into(), "prisma_ins".into(), vec![0; 4], params, vec![])
        .await
        .expect("bind");
    handler
        .handle_execute_extended("prisma_p".into(), 0)
        .await
        .expect("execute");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1, "INSERT … RETURNING must emit exactly one DataRow");
    assert_eq!(cell(&rows[0], 0).as_deref(), Some("1"));
    assert_eq!(cell(&rows[0], 1).as_deref(), Some("a@example.com"));
    assert_eq!(cell(&rows[0], 2).as_deref(), Some("2026-09-06"));
    assert_eq!(cell(&rows[0], 3).as_deref(), Some("2026-09-07"));
}

/// Simple query protocol (psycopg2 / psql): the `prisma.account.update()`
/// shape. The simple path builds its RowDescription in
/// `PgConnectionHandler::derive_returning_schema`, a different call site from
/// the extended path's Describe — so it gets its own assertion.
///
/// FAILS on the unfixed tree with `"public"."Account"."id"` /
/// `"public"."Account"."email"` — and, once the names are right, with OID 25
/// on `id`: psycopg2 casts by OID, so a text-typed primary key comes back as
/// the str `"1"` where PostgreSQL hands back `1`.
#[tokio::test]
async fn prisma_update_returning_simple_query_names_bare_columns() {
    let db = prisma_account_db();
    db.execute(
        r#"INSERT INTO "public"."Account" ("id","email","createdAt","updatedAt")
           VALUES (1,'a@example.com','2026-09-06','2026-09-06')"#,
    )
    .expect("seed");
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query(
            r#"UPDATE "public"."Account" SET "email"='b@example.com' WHERE "public"."Account"."id"=1 RETURNING "public"."Account"."id", "public"."Account"."email""#,
        )
        .await
        .expect("update … returning");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description(&out),
        vec![("id".to_string(), 23), ("email".to_string(), 25)],
        "simple-query UPDATE … RETURNING must name AND type its fields like \
         PostgreSQL (psycopg2 casts by OID, so id must be int4/23 here too)"
    );
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1, "UPDATE … RETURNING must emit exactly one DataRow");
    assert_eq!(cell(&rows[0], 0).as_deref(), Some("1"));
    assert_eq!(
        cell(&rows[0], 1).as_deref(),
        Some("b@example.com"),
        "RETURNING must carry the POST-update value"
    );
}

/// `RETURNING *` must keep naming every column of the table — the shape
/// `tests/delete_returning_tests.rs` and drizzle depend on — and an explicit
/// `AS` alias must still win over the derived name.
#[tokio::test]
async fn returning_wildcard_and_alias_names_are_unchanged() {
    let db = prisma_account_db();
    db.execute(
        r#"INSERT INTO "public"."Account" ("id","email","createdAt","updatedAt")
           VALUES (1,'a@example.com','2026-09-06','2026-09-06')"#,
    )
    .expect("seed");
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query(r#"DELETE FROM "public"."Account" WHERE "public"."Account"."id"=1 RETURNING *"#)
        .await
        .expect("delete … returning *");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description_names(&out),
        vec![
            "id".to_string(),
            "email".to_string(),
            "createdAt".to_string(),
            "updatedAt".to_string()
        ],
        "RETURNING * must still expand to the table's own column names"
    );
    assert_eq!(data_rows(&out).len(), 1);

    handler
        .handle_single_query(
            r#"INSERT INTO "public"."Account" ("id","email") VALUES (2,'c@example.com') RETURNING "public"."Account"."id" AS "accountId""#,
        )
        .await
        .expect("insert … returning alias");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description_names(&out),
        vec!["accountId".to_string()],
        "an explicit alias must win over the derived name, case intact"
    );
}

/// The half of the defect a NAME-only fix leaves in place, and the reason the
/// planner lowers a qualified reference to `ReturningItem::Column` rather than
/// renaming an `Expression`.
///
/// `EmbeddedDatabase::returning_schema` types every `ReturningItem::Expression`
/// as `DataType::Text` (OID 25). `DataType::Text` IS in
/// `datatype_has_binary_result`, so `effective_result_format` grants
/// `format_code = 1` to a portal that asked for binary — while
/// `tuple_to_pg_values_with_formats` encodes the row's ACTUAL `Value::Int4`
/// with `value_to_pg_binary`, i.e. four big-endian bytes. The client is told
/// "text, binary format" and handed `00 00 00 01`, which it decodes as a UTF-8
/// string. tokio-postgres — the driver under Prisma's Rust query engine —
/// always requests binary results, so this is the shape that actually ships.
///
/// FAILS on the unfixed tree: `id` is advertised with OID 25 (text) instead of
/// 23 (int4) while its DataRow payload is int4-binary. Both new name tests
/// above bind with `vec![]` result formats (all text), so neither can catch it.
#[tokio::test]
async fn prisma_returning_types_are_coherent_with_binary_result_formats() {
    let db = prisma_account_db();
    let (mut handler, mut client) = test_handler(db);

    let sql = r#"INSERT INTO "public"."Account" ("id","email","createdAt","updatedAt") VALUES ($1,$2,$3,$4) RETURNING "public"."Account"."id", "public"."Account"."email""#;
    handler
        .handle_parse_extended("bin_ins".into(), sql.into(), vec![23, 25, 25, 25])
        .await
        .expect("parse");

    let params: Vec<Option<Vec<u8>>> = vec![
        Some(b"1".to_vec()),
        Some(b"a@example.com".to_vec()),
        Some(b"2026-09-06".to_vec()),
        Some(b"2026-09-07".to_vec()),
    ];
    // Result formats = binary for BOTH output columns.
    handler
        .handle_bind_extended("bin_p".into(), "bin_ins".into(), vec![0; 4], params, vec![1, 1])
        .await
        .expect("bind");
    let _ = drain(&mut client).await;

    handler
        .handle_describe_extended(super::messages::DescribeTarget::Portal, "bin_p".into())
        .await
        .expect("describe portal");
    let fields = row_description_fields(&drain(&mut client).await);
    assert_eq!(
        fields.iter().map(|f| (f.1, f.2)).collect::<Vec<_>>(),
        vec![(23, 1), (25, 1)],
        "the portal must advertise (int4, binary) for `id` and (text, binary) for \
         `email`; OID 25 on `id` is the `ReturningItem::Expression` text fallback \
         describing an int4 column as text while sending int4-binary bytes"
    );
    assert_eq!(
        fields.iter().map(|f| f.0.clone()).collect::<Vec<_>>(),
        vec!["id".to_string(), "email".to_string()]
    );

    handler
        .handle_execute_extended("bin_p".into(), 0)
        .await
        .expect("execute");
    let rows = data_rows(&drain(&mut client).await);
    assert_eq!(rows.len(), 1, "INSERT … RETURNING must emit exactly one DataRow");
    let int4_one = 1i32.to_be_bytes();
    assert_eq!(
        cell_bytes(&rows[0], 0),
        Some(&int4_one[..]),
        "int4 binary output is 4 big-endian bytes — and the field it lands in \
         must be the one advertised as int4"
    );
    assert_eq!(cell_bytes(&rows[0], 1), Some(&b"a@example.com"[..]));
}

/// A CASE-FOLDED qualifier (`Account.id` against table `"Account"`) and an
/// ALIAS qualifier (`AS x … RETURNING x."id"`) are the fail-open half of the
/// same defect: `Evaluator::evaluate` compares the qualifier to the catalog's
/// stamped `source_table_name` byte-exactly, misses, and
/// `project_returning_columns` maps the `Err` to `Value::Null`. Getting the
/// NAME right without the lowering would have made that worse — the field
/// would arrive over the wire under the exact name an ORM binds, carrying NULL
/// for the primary key, with no error anywhere.
///
/// FAILS on the unfixed tree on the NAMES (`Account.id`, `x."id"`); it also
/// fails on a name-only fix, on the NULL values.
#[tokio::test]
async fn returning_folded_and_alias_qualifiers_resolve_over_the_wire() {
    let db = prisma_account_db();
    db.execute(
        r#"INSERT INTO "public"."Account" ("id","email","createdAt","updatedAt")
           VALUES (1,'a@example.com','2026-09-06','2026-09-06'),
                  (2,'b@example.com','2026-09-06','2026-09-06')"#,
    )
    .expect("seed");
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query(
            r#"UPDATE "public"."Account" SET "email"='c@example.com' WHERE id=1 RETURNING Account.id, Account.email"#,
        )
        .await
        .expect("update … returning folded qualifier");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description_names(&out),
        vec!["id".to_string(), "email".to_string()],
        "an unquoted qualifier must not leak into the field name"
    );
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        cell(&rows[0], 0).as_deref(),
        Some("1"),
        "*** fail-open: a case-folded qualifier projected NULL under the right name ***"
    );
    assert_eq!(cell(&rows[0], 1).as_deref(), Some("c@example.com"));

    handler
        .handle_single_query(r#"DELETE FROM "Account" AS x WHERE id=2 RETURNING x."id", x."email""#)
        .await
        .expect("delete … returning alias qualifier");
    let out = drain(&mut client).await;
    assert_eq!(
        row_description_names(&out),
        vec!["id".to_string(), "email".to_string()],
        "an alias qualifier must not leak into the field name"
    );
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        cell(&rows[0], 0).as_deref(),
        Some("2"),
        "*** fail-open: an alias-qualified RETURNING projected NULL ***"
    );
    assert_eq!(cell(&rows[0], 1).as_deref(), Some("b@example.com"));
}

// Prisma P0 spec 03 — pg_advisory_lock over the wire.
//
// Prisma Migrate serialises every migration run with
// `SELECT pg_advisory_lock(72707369)` and releases it with
// `SELECT pg_advisory_unlock(72707369)`. Both raised
// `Unknown scalar function` (42883) before this change, so no migration could
// run against Nano at all. Two handlers over the same `Arc<EmbeddedDatabase>`
// are two connections: each mints its own wire session, and the handler's
// `Drop` is the disconnect.
// ---------------------------------------------------------------------------

/// The `void` then `true` result shapes psycopg / Prisma expect from the
/// canonical lock/unlock pair, on the simple-query path.
#[tokio::test]
async fn advisory_lock_then_unlock_returns_void_then_true() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut handler, mut client) = test_handler(db);

    handler
        .handle_single_query("SELECT pg_advisory_lock(72707369)")
        .await
        .expect("pg_advisory_lock must not fail");
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "pg_advisory_lock() must not error, got {:?}",
        sqlstates(&out)
    );
    let rows = data_rows(&out);
    assert_eq!(rows.len(), 1, "pg_advisory_lock() returns exactly one row");
    let row = rows.first().expect("one row");
    assert_eq!(row.len(), 1, "pg_advisory_lock() returns exactly one column");
    let column = row.first().expect("one column");
    assert!(
        column.is_none(),
        "pg_advisory_lock() is a void function: the column is NULL-typed, got {column:?}"
    );

    handler
        .handle_single_query("SELECT pg_advisory_unlock(72707369)")
        .await
        .expect("pg_advisory_unlock must not fail");
    let out = drain(&mut client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "pg_advisory_unlock() must not error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("t"),
        "the holder's pg_advisory_unlock() returns true"
    );
}

/// A holds the migration lock, B cannot take it, and B CAN take it as soon as
/// A's connection ends — the handler's `Drop` (Terminate, dropped socket and
/// error path alike) releases through `destroy_session`.
///
/// Also proves the result cache never serves an advisory answer: B runs the
/// byte-identical statement twice and gets `f` then `t`.
#[tokio::test]
async fn advisory_lock_is_released_when_the_holding_connection_drops() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut holder, mut holder_client) = test_handler(Arc::clone(&db));
    let (mut other, mut other_client) = test_handler(Arc::clone(&db));

    holder
        .handle_single_query("SELECT pg_advisory_lock(72707374)")
        .await
        .expect("holder acquires");
    let _ = drain(&mut holder_client).await;

    other
        .handle_single_query("SELECT pg_try_advisory_lock(72707374)")
        .await
        .expect("try must not error");
    assert_eq!(
        first_data_row_text(&drain(&mut other_client).await).as_deref(),
        Some("f"),
        "a second connection must not get the migration lock"
    );

    // The holding connection goes away.
    drop(holder);
    drop(holder_client);

    other
        .handle_single_query("SELECT pg_try_advisory_lock(72707374)")
        .await
        .expect("try must not error");
    assert_eq!(
        first_data_row_text(&drain(&mut other_client).await).as_deref(),
        Some("t"),
        "*** the lock outlived the connection that held it ***"
    );
}

/// A BLOCKING `pg_advisory_lock` on a busy key is granted once the holder's
/// connection drops, and it does not wedge the runtime while it waits — the
/// wait is handed to `block_in_place`, so the other task still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocking_advisory_lock_is_granted_after_the_holder_disconnects() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut holder, mut holder_client) = test_handler(Arc::clone(&db));
    holder
        .handle_single_query("SELECT pg_advisory_lock(72707370)")
        .await
        .expect("holder acquires");
    let _ = drain(&mut holder_client).await;

    let (mut waiter, mut waiter_client) = test_handler(Arc::clone(&db));
    let waiting = tokio::spawn(async move {
        waiter
            .handle_single_query("SELECT pg_advisory_lock(72707370)")
            .await
            .expect("waiter must be granted the lock");
        waiter
    });

    // Still blocked while the holder keeps the key.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!waiting.is_finished(), "the waiter must still be blocked");

    drop(holder);
    drop(holder_client);

    let waiter = tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("*** a blocked pg_advisory_lock was never granted after the holder disconnected ***")
        .expect("waiter task must not panic");
    let out = drain(&mut waiter_client).await;
    assert!(
        sqlstates(&out).is_empty(),
        "the granted pg_advisory_lock must not error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(data_rows(&out).len(), 1, "one void row");
    drop(waiter);
}

/// A transaction-level advisory lock taken over the wire is released by the
/// wire COMMIT, and by ROLLBACK.
#[tokio::test]
async fn advisory_xact_lock_is_released_by_wire_commit_and_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut a, mut ca) = test_handler(Arc::clone(&db));
    let (mut b, mut cb) = test_handler(Arc::clone(&db));

    for (finish, key) in [("COMMIT", 72707371_i64), ("ROLLBACK", 72707372_i64)] {
        a.handle_single_query("BEGIN").await.expect("begin");
        let _ = drain(&mut ca).await;
        a.handle_single_query(&format!("SELECT pg_advisory_xact_lock({key})"))
            .await
            .expect("xact lock");
        let out = drain(&mut ca).await;
        assert!(sqlstates(&out).is_empty(), "{:?}", sqlstates(&out));

        b.handle_single_query(&format!("SELECT pg_try_advisory_lock({key})"))
            .await
            .expect("try");
        assert_eq!(
            first_data_row_text(&drain(&mut cb).await).as_deref(),
            Some("f"),
            "held for the duration of A's transaction"
        );

        a.handle_single_query(finish).await.expect("finish txn");
        let _ = drain(&mut ca).await;

        b.handle_single_query(&format!("SELECT pg_try_advisory_lock({key})"))
            .await
            .expect("try");
        assert_eq!(
            first_data_row_text(&drain(&mut cb).await).as_deref(),
            Some("t"),
            "*** {finish} did not release the transaction-level advisory lock ***"
        );
    }
}

/// The extended query protocol (Parse/Bind/Execute — the params family) serves
/// the advisory functions too, and attributes the lock to the same connection.
#[tokio::test]
async fn advisory_lock_works_on_the_extended_protocol() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut a, mut ca) = test_handler(Arc::clone(&db));
    let (mut b, mut cb) = test_handler(Arc::clone(&db));

    a.handle_parse_extended("adv".into(), "SELECT pg_advisory_lock(72707373)".into(), vec![])
        .await
        .expect("parse");
    a.handle_bind_extended("padv".into(), "adv".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    a.handle_execute_extended("padv".into(), 0).await.expect("execute");
    let out = drain(&mut ca).await;
    assert!(
        sqlstates(&out).is_empty(),
        "extended pg_advisory_lock() must not error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(data_rows(&out).len(), 1, "one void row");

    b.handle_single_query("SELECT pg_try_advisory_lock(72707373)")
        .await
        .expect("try");
    assert_eq!(
        first_data_row_text(&drain(&mut cb).await).as_deref(),
        Some("f"),
        "*** the extended-protocol lock was never taken ***"
    );
}

/// Spec 03 / ownership: an advisory lock taken by an EXTENDED-protocol
/// `INSERT … RETURNING` belongs to the connection that took it, and dies with
/// it.
///
/// That arm (`handler_extended::handle_execute_extended`) routed through the
/// SESSION-LESS `execute_params_returning`, so the lock was attributed to the
/// process-wide embedded owner instead of this wire session: `destroy_session`
/// never matched it and `Drop` only fires for the last handle, so
/// `INSERT … RETURNING pg_try_advisory_lock(k)` over psycopg3 / JDBC / sqlx
/// stranded key `k` for the life of the server. The simple-query twin was
/// always session-aware — the "fixed in one family only" defect class.
///
/// FAILS on the unfixed tree at the last assertion (`f`, not `t`).
#[tokio::test]
async fn extended_dml_returning_advisory_lock_dies_with_the_connection() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    db.execute("CREATE TABLE adv_ret (id INT PRIMARY KEY, got BOOLEAN)")
        .expect("create table");
    let (mut a, mut ca) = test_handler(Arc::clone(&db));
    let (mut b, mut cb) = test_handler(Arc::clone(&db));

    a.handle_parse_extended(
        "ins".into(),
        "INSERT INTO adv_ret VALUES (1, pg_try_advisory_lock(72707375)) RETURNING got".into(),
        vec![],
    )
    .await
    .expect("parse");
    a.handle_bind_extended("pins".into(), "ins".into(), vec![], vec![], vec![])
        .await
        .expect("bind");
    a.handle_execute_extended("pins".into(), 0).await.expect("execute");
    let out = drain(&mut ca).await;
    assert!(
        sqlstates(&out).is_empty(),
        "extended INSERT … RETURNING pg_try_advisory_lock must not error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("t"),
        "the RETURNING expression must actually take the lock"
    );

    b.handle_single_query("SELECT pg_try_advisory_lock(72707375)")
        .await
        .expect("try");
    assert_eq!(
        first_data_row_text(&drain(&mut cb).await).as_deref(),
        Some("f"),
        "A holds it while its connection is alive"
    );

    // A's connection ends — Terminate, dropped socket and error path all funnel
    // through the handler's `Drop` → `destroy_session`.
    drop(a);
    drop(ca);

    b.handle_single_query("SELECT pg_try_advisory_lock(72707375)")
        .await
        .expect("try");
    assert_eq!(
        first_data_row_text(&drain(&mut cb).await).as_deref(),
        Some("t"),
        "*** an extended-protocol RETURNING advisory lock outlived the connection that took it ***"
    );
}

/// Spec 03 / pooling: `DISCARD ALL` releases this session's advisory locks,
/// exactly as PostgreSQL documents (`DISCARD ALL` is defined to include
/// `SELECT pg_advisory_unlock_all();`).
///
/// Without it a pool that recycles a physical connection after a failed
/// migration leaves key 72707369 held: the client believes the lock is gone,
/// every other connection blocks forever, and the recycled session itself
/// re-enters the lock (same owner) and appears to succeed.
///
/// FAILS on the unfixed tree at the first assertion (`t`, not `f`): the lock
/// survives the reset.
#[tokio::test]
async fn discard_all_releases_session_advisory_locks() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut pooled, mut cp) = test_handler(Arc::clone(&db));
    let (mut other, mut co) = test_handler(Arc::clone(&db));

    pooled
        .handle_single_query("SELECT pg_advisory_lock(72707376)")
        .await
        .expect("holder acquires");
    let _ = drain(&mut cp).await;

    pooled.handle_single_query("DISCARD ALL").await.expect("discard all");
    let out = drain(&mut cp).await;
    assert!(
        sqlstates(&out).is_empty(),
        "DISCARD ALL must not error, got {:?}",
        sqlstates(&out)
    );

    // The recycled connection no longer owns the key...
    pooled
        .handle_single_query("SELECT pg_advisory_unlock(72707376)")
        .await
        .expect("unlock");
    assert_eq!(
        first_data_row_text(&drain(&mut cp).await).as_deref(),
        Some("f"),
        "*** DISCARD ALL left the migration lock held on the recycled connection ***"
    );

    // ... and the next client can take it.
    other
        .handle_single_query("SELECT pg_try_advisory_lock(72707376)")
        .await
        .expect("try");
    assert_eq!(
        first_data_row_text(&drain(&mut co).await).as_deref(),
        Some("t"),
        "*** the migration lock was never released by DISCARD ALL ***"
    );
}

// ---------------------------------------------------------------------------
// Prisma P0 spec 04 — a parameterized DML … RETURNING must join the session's
// explicit transaction.
//
// Prisma binds every value and appends RETURNING to every create/update/delete,
// so every write inside `prisma.$transaction(...)` arrives as Parse/Bind/Execute
// of a DML … RETURNING with bound parameters. That arm of
// `handle_execute_extended` routed through the SESSION-LESS
// `execute_params_returning`, which resolves its transaction from the GLOBAL
// `current_transaction` slot a wire session never uses — so the write went
// straight to storage and AUTOCOMMITTED. Every neighbouring spelling honoured
// the transaction (the simple protocol, and the non-RETURNING parameterized
// form), which is exactly why it went unnoticed.
//
// Tests marked *** UNFIXED *** fail on the unfixed tree at the marked assertion.
// ---------------------------------------------------------------------------

/// Parse / Bind / Execute one statement over the EXTENDED protocol (no
/// Describe — the assertions below read DataRow / CommandComplete /
/// ErrorResponse only) and return the drained reply bytes.
///
/// Boxed for the same reason `wire_query` is: these futures are large, and an
/// `async fn`'s state is inlined into its caller's, so a test issuing half a
/// dozen statements otherwise overflows the 2 MB test-thread stack and aborts
/// the whole binary.
fn wire_extended<'a>(
    handler: &'a mut PgConnectionHandler<DuplexStream>,
    client: &'a mut DuplexStream,
    name: &'a str,
    sql: &'a str,
    param_types: Vec<i32>,
    params: Vec<Option<Vec<u8>>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + 'a>> {
    Box::pin(async move {
        let portal = format!("portal_{name}");
        let formats = vec![0i16; params.len()];
        handler
            .handle_parse_extended(name.to_string(), sql.to_string(), param_types)
            .await
            .unwrap_or_else(|e| panic!("parse `{sql}`: {e}"));
        handler
            .handle_bind_extended(portal.clone(), name.to_string(), formats, params, vec![])
            .await
            .unwrap_or_else(|e| panic!("bind `{sql}`: {e}"));
        handler
            .handle_execute_extended(portal, 0)
            .await
            .unwrap_or_else(|e| panic!("execute `{sql}`: {e}"));
        drain(client).await
    })
}

/// An extended-protocol statement that must complete without an ErrorResponse.
fn assert_extended_ok(out: &[u8], sql: &str) {
    assert!(
        sqlstates(out).is_empty(),
        "`{sql}` must not error over the extended protocol, got {:?}",
        sqlstates(out)
    );
}

/// Every `id` in `t` visible to this connection, as wire text, sorted here
/// rather than by the engine (a bare scan keeps the read on the plain
/// `data:`-key path, which is the one an uncommitted row must be absent from).
///
/// Deliberately a row-returning scan rather than `SELECT count(*)`: COUNT(\*)
/// can be answered from the primary-key ART index, and that index is maintained
/// EAGERLY for in-transaction inserts (with a rollback undo log), so it is not a
/// witness for what a row read — or another connection — can actually see.
fn wire_ids<'a>(
    handler: &'a mut PgConnectionHandler<DuplexStream>,
    client: &'a mut DuplexStream,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + 'a>> {
    Box::pin(async move {
        let out = wire_query(handler, client, "SELECT id FROM t").await;
        assert!(
            sqlstates(&out).is_empty(),
            "`SELECT id FROM t` must not error, got {:?}",
            sqlstates(&out)
        );
        let mut seen: Vec<String> = data_rows(&out)
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .next()
                    .flatten()
                    .map(|b| String::from_utf8_lossy(&b).to_string())
                    .unwrap_or_else(|| "NULL".to_string())
            })
            .collect();
        seen.sort();
        seen
    })
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// The spec's literal reproducer:
/// `BEGIN; INSERT INTO t (id, v) VALUES ($1,$2) RETURNING id; ROLLBACK;`
///
/// *** UNFIXED ***: the final read still returns `["1"]` — the row autocommitted
/// straight past the open transaction, so `ROLLBACK` had nothing to undo.
#[tokio::test]
async fn extended_parameterized_insert_returning_honours_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id";
    let out = wire_extended(
        &mut h,
        &mut c,
        "ins",
        sql,
        vec![23, 25],
        vec![Some(b"1".to_vec()), Some(b"alpha".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("1"),
        "RETURNING id must come back"
    );
    assert_eq!(command_tags(&out), vec!["INSERT 0 1".to_string()]);

    // Read-your-writes, and the connection is STILL inside the transaction
    // block: the statement joined the transaction rather than bypassing it.
    let inside = wire_query(&mut h, &mut c, "SELECT id FROM t").await;
    assert_eq!(
        data_rows(&inside).len(),
        1,
        "the inserting connection must see its own uncommitted RETURNING row"
    );
    assert_eq!(
        ready_for_query_statuses(&inside),
        vec![b'T'],
        "the RETURNING insert must not have ended the transaction block"
    );

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        Vec::<String>::new(),
        "*** a parameterized INSERT … RETURNING escaped the session transaction: \
         ROLLBACK did not undo it ***"
    );
}

/// The COMMIT half — the write must still persist. Passes on the unfixed tree
/// (it autocommitted); pinned so the fix cannot turn the write into a no-op.
#[tokio::test]
async fn extended_parameterized_insert_returning_honours_commit() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id";
    let out = wire_extended(
        &mut h,
        &mut c,
        "ins",
        sql,
        vec![23, 25],
        vec![Some(b"1".to_vec()), Some(b"alpha".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);

    wire_setup(&mut h, &mut c, "COMMIT").await;
    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        ids(&["1"]),
        "a committed parameterized RETURNING insert must persist"
    );
}

/// *** UNFIXED ***: `v` is still `new` after ROLLBACK.
#[tokio::test]
async fn extended_parameterized_update_returning_honours_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "INSERT INTO t VALUES (1, 'old')").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "UPDATE t SET v = $1 WHERE id = $2 RETURNING v";
    let out = wire_extended(
        &mut h,
        &mut c,
        "upd",
        sql,
        vec![25, 23],
        vec![Some(b"new".to_vec()), Some(b"1".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("new"),
        "RETURNING must show the post-update value"
    );
    assert_eq!(command_tags(&out), vec!["UPDATE 1".to_string()]);

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    let out = wire_query(&mut h, &mut c, "SELECT v FROM t WHERE id = 1").await;
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("old"),
        "*** a parameterized UPDATE … RETURNING escaped the session transaction ***"
    );
}

/// *** UNFIXED ***: the table is empty after ROLLBACK — the delete was already
/// durable when ROLLBACK ran.
#[tokio::test]
async fn extended_parameterized_delete_returning_honours_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "INSERT INTO t VALUES (1, 'old')").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "DELETE FROM t WHERE id = $1 RETURNING v";
    let out = wire_extended(&mut h, &mut c, "del", sql, vec![23], vec![Some(b"1".to_vec())]).await;
    assert_extended_ok(&out, sql);
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("old"),
        "RETURNING must show the deleted row"
    );
    assert_eq!(command_tags(&out), vec!["DELETE 1".to_string()]);

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        ids(&["1"]),
        "*** a parameterized DELETE … RETURNING escaped the session transaction ***"
    );
}

/// A second connection must not see the uncommitted RETURNING row.
///
/// *** UNFIXED ***: the observer's first read returns `["1"]` — an autocommitted
/// write is visible to everyone immediately, which is precisely the dirty read
/// an interactive transaction exists to prevent.
#[tokio::test]
async fn extended_parameterized_returning_insert_is_invisible_to_other_connections() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut writer, mut cw) = test_handler(Arc::clone(&db));
    let (mut observer, mut co) = test_handler(Arc::clone(&db));

    wire_setup(&mut writer, &mut cw, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut writer, &mut cw, "BEGIN").await;

    let sql = "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id";
    let out = wire_extended(
        &mut writer,
        &mut cw,
        "ins",
        sql,
        vec![23, 25],
        vec![Some(b"1".to_vec()), Some(b"alpha".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);

    assert_eq!(
        wire_ids(&mut observer, &mut co).await,
        Vec::<String>::new(),
        "*** a second connection saw an UNCOMMITTED parameterized RETURNING row ***"
    );

    wire_setup(&mut writer, &mut cw, "COMMIT").await;
    assert_eq!(
        wire_ids(&mut observer, &mut co).await,
        ids(&["1"]),
        "and must see it once the writer commits"
    );
}

/// `SAVEPOINT` / `ROLLBACK TO SAVEPOINT` must undo a parameterized RETURNING
/// insert, exactly as they undo every other write in the transaction.
///
/// *** UNFIXED ***: the surviving ids are `["1", "2"]` — the savepoint stack can
/// only restore the transaction's write set, and this row never entered it.
#[tokio::test]
async fn extended_parameterized_returning_insert_is_undone_by_rollback_to_savepoint() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;
    wire_setup(&mut h, &mut c, "INSERT INTO t VALUES (1, 'keep')").await;
    wire_setup(&mut h, &mut c, "SAVEPOINT sp1").await;

    let sql = "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id";
    let out = wire_extended(
        &mut h,
        &mut c,
        "ins",
        sql,
        vec![23, 25],
        vec![Some(b"2".to_vec()), Some(b"discard".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);

    wire_setup(&mut h, &mut c, "ROLLBACK TO SAVEPOINT sp1").await;
    wire_setup(&mut h, &mut c, "COMMIT").await;

    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        ids(&["1"]),
        "*** ROLLBACK TO SAVEPOINT did not undo the parameterized RETURNING insert ***"
    );
}

/// The statement must READ the session's uncommitted state, which is what makes
/// the RETURNING projection correct: the first statement stages `v = 'mid'` in
/// the transaction, and the parameterized statement then selects on `v = 'mid'`.
///
/// *** UNFIXED ***: the tag is `UPDATE 0` and no DataRow arrives — running
/// outside the transaction, the statement only ever saw the committed `'old'`
/// row, so it matched nothing.
#[tokio::test]
async fn extended_parameterized_returning_reads_the_transactions_own_uncommitted_writes() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "INSERT INTO t VALUES (1, 'old')").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;
    wire_setup(&mut h, &mut c, "UPDATE t SET v = 'mid' WHERE id = 1").await;

    let sql = "UPDATE t SET v = $1 WHERE v = $2 RETURNING v";
    let out = wire_extended(
        &mut h,
        &mut c,
        "upd",
        sql,
        vec![25, 25],
        vec![Some(b"new".to_vec()), Some(b"mid".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);
    assert_eq!(
        command_tags(&out),
        vec!["UPDATE 1".to_string()],
        "*** the parameterized RETURNING statement did not see its own transaction's \
         uncommitted write ***"
    );
    assert_eq!(first_data_row_text(&out).as_deref(), Some("new"));

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    let out = wire_query(&mut h, &mut c, "SELECT v FROM t WHERE id = 1").await;
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("old"),
        "both writes must roll back together"
    );
}

/// The same defect with ZERO bound parameters. Every driver that prepares
/// statements (JDBC, sqlx, node-postgres) sends Parse/Bind/Execute even when
/// there are no placeholders, so this is not a parameters bug — it is a
/// `RETURNING`-arm bug.
///
/// (psycopg3 falls back to the simple protocol when a statement has no
/// parameters, which is why the spike recorded "the same statement WITHOUT
/// bound parameters honours ROLLBACK": it was never on this path.)
///
/// *** UNFIXED ***: the final read still returns `["1"]`.
#[tokio::test]
async fn extended_zero_parameter_insert_returning_honours_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "INSERT INTO t (id, v) VALUES (1, 'alpha') RETURNING id";
    let out = wire_extended(&mut h, &mut c, "ins0", sql, vec![], vec![]).await;
    assert_extended_ok(&out, sql);
    assert_eq!(first_data_row_text(&out).as_deref(), Some("1"));

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        Vec::<String>::new(),
        "*** an unparameterized EXTENDED-protocol INSERT … RETURNING escaped the \
         session transaction ***"
    );
}

/// Pin: the NON-RETURNING parameterized form already honoured the transaction
/// (`execute_params_for_session`). It is the control for the failing tests
/// above — same family, same parameters, only `RETURNING` differs — and it must
/// keep working.
#[tokio::test]
async fn extended_parameterized_insert_without_returning_still_honours_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "INSERT INTO t (id, v) VALUES ($1, $2)";
    let out = wire_extended(
        &mut h,
        &mut c,
        "insp",
        sql,
        vec![23, 25],
        vec![Some(b"1".to_vec()), Some(b"alpha".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        Vec::<String>::new(),
        "the non-RETURNING parameterized form must still roll back"
    );
}

/// Pin: the SIMPLE-protocol `RETURNING` form already honoured the transaction
/// (`execute_returning_for_session`) and must keep doing so.
#[tokio::test]
async fn simple_protocol_insert_returning_still_honours_rollback() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let out = wire_query(&mut h, &mut c, "INSERT INTO t VALUES (1, 'alpha') RETURNING id").await;
    assert!(
        sqlstates(&out).is_empty(),
        "simple-protocol RETURNING must not error, got {:?}",
        sqlstates(&out)
    );
    assert_eq!(first_data_row_text(&out).as_deref(), Some("1"));

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    assert_eq!(
        wire_ids(&mut h, &mut c).await,
        Vec::<String>::new(),
        "the simple-protocol RETURNING form must still roll back"
    );
}

/// Pin: outside a transaction the extended RETURNING form still autocommits, so
/// another connection sees the row at once. Guards the new
/// `session_transactions` branch against swallowing the autocommit path.
#[tokio::test]
async fn extended_parameterized_returning_outside_a_transaction_still_autocommits() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut writer, mut cw) = test_handler(Arc::clone(&db));
    let (mut observer, mut co) = test_handler(Arc::clone(&db));

    wire_setup(&mut writer, &mut cw, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;

    let sql = "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id";
    let out = wire_extended(
        &mut writer,
        &mut cw,
        "ins",
        sql,
        vec![23, 25],
        vec![Some(b"1".to_vec()), Some(b"alpha".to_vec())],
    )
    .await;
    assert_extended_ok(&out, sql);
    assert_eq!(first_data_row_text(&out).as_deref(), Some("1"));

    assert_eq!(
        wire_ids(&mut observer, &mut co).await,
        ids(&["1"]),
        "an autocommit RETURNING insert must be visible to every connection at once"
    );
}

/// Two parameterized `UPDATE … RETURNING`s against the SAME ROW in one
/// transaction — a Prisma `$transaction` that updates a record twice, and the
/// commonest shape of "write a row you already wrote".
///
/// Joining the session transaction (the change above) exposed an older defect
/// underneath it: row locks are held for the whole transaction, so the second
/// statement re-requested a lock the transaction was already holding, the
/// wait-for graph gained the self-edge `txn -> txn`, and the DFS cycle check
/// reported it as a deadlock — a transaction deadlocked against itself, with no
/// second connection anywhere.
///
/// *** UNFIXED ***: the second Execute fails with SQLSTATE 40P01
/// (`Deadlock detected for transaction N`).
#[tokio::test]
async fn extended_parameterized_returning_may_update_the_same_row_twice_in_one_transaction() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let (mut h, mut c) = test_handler(db);
    wire_setup(&mut h, &mut c, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").await;
    wire_setup(&mut h, &mut c, "INSERT INTO t VALUES (1, 'old')").await;
    wire_setup(&mut h, &mut c, "BEGIN").await;

    let sql = "UPDATE t SET v = $1 WHERE id = $2 RETURNING v";
    let first = wire_extended(
        &mut h,
        &mut c,
        "upd1",
        sql,
        vec![25, 23],
        vec![Some(b"first".to_vec()), Some(b"1".to_vec())],
    )
    .await;
    assert_extended_ok(&first, sql);
    assert_eq!(first_data_row_text(&first).as_deref(), Some("first"));

    let second = wire_extended(
        &mut h,
        &mut c,
        "upd2",
        sql,
        vec![25, 23],
        vec![Some(b"second".to_vec()), Some(b"1".to_vec())],
    )
    .await;
    assert!(
        sqlstates(&second).is_empty(),
        "*** the transaction deadlocked against its own row lock: {:?} ***",
        sqlstates(&second)
    );
    assert_eq!(
        command_tags(&second),
        vec!["UPDATE 1".to_string()],
        "the second update must match the row the first one wrote"
    );
    assert_eq!(first_data_row_text(&second).as_deref(), Some("second"));

    wire_setup(&mut h, &mut c, "ROLLBACK").await;
    let out = wire_query(&mut h, &mut c, "SELECT v FROM t WHERE id = 1").await;
    assert_eq!(
        first_data_row_text(&out).as_deref(),
        Some("old"),
        "both updates roll back together"
    );
}
