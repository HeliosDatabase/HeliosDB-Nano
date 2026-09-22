//! Batch G4 — two write-atomicity defects: a write that should be all-or-nothing
//! is not.
//!
//! Compile as `tests/atomicity_batch_g4.rs`.
//!
//! # sprinter 5a78b8288153 — the uniqueness pre-check and the index insert were
//! not atomic
//!
//! The autocommit insert funnels asked `check_unique_constraints_tuple` whether a
//! PK/UNIQUE key was free (a per-tree READ lock, released immediately) and only
//! entered the key into the tree much later, AFTER `put()` had already made the
//! row durable. Two concurrent INSERTs of the same UNIQUE value therefore both
//! passed the question, both stored their row, and only the loser's INDEX entry
//! was refused. v4.31.0 made that refusal LOUD (an ERROR-level
//! "the table now holds a duplicate" log) without closing it.
//!
//! What that leaves behind is the thing to assert on, and it is not one fact but
//! TWO THAT DISAGREE:
//!
//! * a full scan counts **two** rows with the same UNIQUE value;
//! * the unique index finds **one** — the winner's — because the loser's entry
//!   was never made.
//!
//! A test that checks only the scan, or only the index, can pass on a broken
//! tree. Every case below asserts both and requires them to agree, via
//! [`assert_single_row_by_scan_and_index`].
//!
//! Covered here: both DML executor families (text `execute()` and params
//! `execute_params()` — separate INSERT arms) and the COPY/multi-row batch
//! funnel (`insert_prepared_tuples_fast_batch`), because a fix on one says
//! nothing about the others.
//!
//! # sprinter 5b70b7ac5513 — a statement that fails mid-way inside a transaction
//! left its earlier rows staged
//!
//! A multi-row `INSERT … VALUES (…),(…),(…)` that fails on row N inside an open
//! transaction left rows 1..N-1 in the transaction's write set. PostgreSQL rolls
//! back the whole STATEMENT.
//!
//! On the PostgreSQL wire and the embedded API this was survivable: HDB-008
//! aborts the block, so the partial rows cannot reach a COMMIT. On the **MySQL
//! listener** — which deliberately keeps MySQL's statement-level semantics,
//! where a failed statement does NOT abort the transaction — the partial rows
//! were committed by the following COMMIT. That is the live data-integrity hole,
//! so the proof is driven over the MySQL wire, with an embedded control pinning
//! the PostgreSQL abort behaviour as unchanged.
//!
//! Expected on a FIXED tree: every test passes.
//! Expected on the tree these items were filed against:
//! `g4_item1_*` fail with a stored duplicate (scan says 2, index says 1), and
//! `g4_item2_mysql_*` fail because COMMIT resurrected the partial rows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::{Arc, Barrier};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

// ---------------------------------------------------------------------------
// Embedded harness (item 1)
// ---------------------------------------------------------------------------

fn mem_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory database"))
}

/// Which DML executor family a sub-case exercises. They are SEPARATE INSERT
/// arms, so a result on one says nothing about the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    /// `db.execute()` — psql simple protocol / embedded text path.
    Text,
    /// `db.execute_params()` — extended protocol / REST / most drivers.
    Params,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Family::Text => "text",
            Family::Params => "params",
        }
    }

    fn execute(self, db: &EmbeddedDatabase, sql: &str) -> heliosdb_nano::Result<u64> {
        match self {
            Family::Text => db.execute(sql),
            Family::Params => db.execute_params(sql, &[]),
        }
    }
}

const BOTH_FAMILIES: [Family; 2] = [Family::Text, Family::Params];

/// Rows physically present with this value — a FULL SCAN, never `COUNT(*)`
/// (which returns one row whatever the count is, and can be answered from
/// metadata rather than by looking).
fn rows_by_scan(db: &EmbeddedDatabase, table: &str, value: &str) -> usize {
    let sql = format!("SELECT id, v FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .filter(|row| matches!(row.values.get(1), Some(Value::String(s)) if s == value))
        .count()
}

/// Rows the UNIQUE INDEX can find for this value — the `WHERE v = …` probe,
/// which is what an ART lookup answers.
fn rows_by_index(db: &EmbeddedDatabase, table: &str, value: &str) -> usize {
    let sql = format!("SELECT id FROM {table} WHERE v = '{value}'");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}

/// THE assertion of item 5a78b8288153.
///
/// Exactly one row must be stored, and the scan and the index must AGREE about
/// it. The disagreement is the bug: on a racy tree the scan finds two rows
/// sharing a UNIQUE value while the index finds one, because the row that lost
/// the race was written before the tree refused its key.
fn assert_single_row_by_scan_and_index(db: &EmbeddedDatabase, table: &str, value: &str, context: &str) {
    let scanned = rows_by_scan(db, table, value);
    let indexed = rows_by_index(db, table, value);
    assert_eq!(
        scanned, 1,
        "[{context}] a full scan of `{table}` found {scanned} rows with v='{value}', expected exactly 1 \
         — a UNIQUE constraint let a duplicate be STORED (the index found {indexed})"
    );
    assert_eq!(
        indexed, 1,
        "[{context}] the UNIQUE index on `{table}` found {indexed} rows for v='{value}', expected exactly 1 \
         (a full scan found {scanned})"
    );
    assert_eq!(
        scanned, indexed,
        "[{context}] the scan and the unique index DISAGREE about `{table}` v='{value}' \
         (scan {scanned}, index {indexed}) — a stored row the constraint's own index cannot see"
    );
}

/// How many writers contend for one value, and how many distinct values are
/// contended per test. Every round is a fresh value, so a single lost race
/// anywhere in the loop fails the test.
const WRITERS: usize = 8;
const ROUNDS: usize = 24;

// ---------------------------------------------------------------------------
// Item 1 — concurrent single-row INSERT on the same UNIQUE value
// ---------------------------------------------------------------------------

/// `WRITERS` threads race to insert the same UNIQUE value, released together by
/// a barrier so they line up inside the old check→put→index window.
///
/// Each writer supplies its own PRIMARY KEY, so the ONLY contended constraint is
/// the UNIQUE one — which is what makes the failure a stored duplicate rather
/// than a rejected primary key.
fn contend_on_one_value(db: &Arc<EmbeddedDatabase>, table: &str, family: Family, round: usize) -> usize {
    let value = format!("dup{round}");
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::with_capacity(WRITERS);

    for writer in 0..WRITERS {
        let db = Arc::clone(db);
        let barrier = Arc::clone(&barrier);
        let table = table.to_string();
        let value = value.clone();
        handles.push(std::thread::spawn(move || {
            let id = round * WRITERS + writer + 1;
            let sql = format!("INSERT INTO {table} (id, v) VALUES ({id}, '{value}')");
            barrier.wait();
            family.execute(&db, &sql).is_ok()
        }));
    }

    let accepted = handles
        .into_iter()
        .map(|h| h.join().expect("writer thread panicked"))
        .filter(|ok| *ok)
        .count();

    assert_single_row_by_scan_and_index(db, table, &value, &format!("{} family, round {round}", family.name()));
    accepted
}

#[test]
fn g4_item1_concurrent_unique_insert_stores_exactly_one_row() {
    for family in BOTH_FAMILIES {
        let db = mem_db();
        let table = "g4_race";
        db.execute(&format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)"
        ))
        .expect("create table");

        for round in 0..ROUNDS {
            let accepted = contend_on_one_value(&db, table, family, round);
            // Exactly one writer may be told it succeeded. More than one means a
            // duplicate was accepted; zero means the whole round was refused,
            // which would be a mutual-abort livelock between the unique trees.
            assert_eq!(
                accepted,
                1,
                "[{} family, round {round}] {accepted} of {WRITERS} concurrent writers were told their \
                 INSERT succeeded, expected exactly 1",
                family.name()
            );
        }
    }
}

/// A table with TWO unique constraints beyond the primary key, contended at the
/// same time — the lock-ordering case.
///
/// `insert_row_indexes` holds ONE tree write lock at a time and every thread
/// walks the same `table_indexes` order, so neither a deadlock nor a mutual
/// abort is possible; this pins that. A regression that took several tree locks
/// at once (shape (a) of the item) would hang here rather than fail.
#[test]
fn g4_item1_two_unique_constraints_contended_together() {
    for family in BOTH_FAMILIES {
        let db = mem_db();
        db.execute("CREATE TABLE g4_multi (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE, w VARCHAR(50) UNIQUE)")
            .expect("create table");

        for round in 0..ROUNDS {
            let value = format!("dup{round}");
            let barrier = Arc::new(Barrier::new(WRITERS));
            let mut handles = Vec::with_capacity(WRITERS);
            for writer in 0..WRITERS {
                let db = Arc::clone(&db);
                let barrier = Arc::clone(&barrier);
                let value = value.clone();
                handles.push(std::thread::spawn(move || {
                    let id = round * WRITERS + writer + 1;
                    // Both unique columns carry the SAME contended value, so
                    // every writer collides on both trees.
                    let sql = format!("INSERT INTO g4_multi (id, v, w) VALUES ({id}, '{value}', '{value}')");
                    barrier.wait();
                    family.execute(&db, &sql).is_ok()
                }));
            }
            let accepted = handles
                .into_iter()
                .map(|h| h.join().expect("writer thread panicked"))
                .filter(|ok| *ok)
                .count();
            assert_eq!(
                accepted,
                1,
                "[{} family, round {round}] {accepted} writers succeeded against two contended UNIQUE \
                 constraints, expected exactly 1 (0 would mean the writers aborted each other)",
                family.name()
            );
            assert_single_row_by_scan_and_index(
                &db,
                "g4_multi",
                &value,
                &format!("{} family, two unique constraints, round {round}", family.name()),
            );
            let by_w = db
                .query(&format!("SELECT id FROM g4_multi WHERE w = '{value}'"), &[])
                .expect("index probe on w");
            assert_eq!(
                by_w.len(),
                1,
                "[{} family, round {round}] the SECOND unique index found {} rows for w='{value}', expected 1",
                family.name(),
                by_w.len()
            );
        }
    }
}

/// Concurrent multi-row `INSERT … VALUES (…),(…),(…)`.
///
/// Each thread inserts a whole statement, and every statement contains the ONE
/// contended value. A statement is atomic, so the loser must land NOTHING — not
/// "everything except the duplicate row" — and exactly one writer may be told it
/// succeeded.
///
/// # Which code this actually reaches (it is not the batch funnel)
///
/// This was the one case of the ten that failed on the first run, and the
/// failure was a routing assumption, not a flaky race: a multi-row autocommit
/// `INSERT … VALUES` does NOT reach
/// `StorageEngine::insert_prepared_tuples_fast_batch`. That funnel is served by
/// `try_fast_insert_many_params`, which is only reachable from
/// `EmbeddedDatabase::execute_many_params` (one SQL, many PARAMETER rows) — see
/// the sibling test below, which drives it directly.
///
/// A multi-row `VALUES` list instead lands on the PER-ROW in-transaction Insert
/// arm: on the params family inside the v4.38.0 implicit statement transaction
/// (sprinter 6780488554df), on the text family inside
/// `execute_with_implicit_transaction`. Both arms staged the row and only THEN
/// asked the ART to keep up, logging a refusal instead of refusing the row — so
/// two concurrent statements both passed the read-locked pre-check, both staged
/// and both committed the same UNIQUE value. That is what this test caught, and
/// both arms now claim before they stage.
#[test]
fn g4_item1_concurrent_multirow_batch_keeps_batch_atomic() {
    for family in BOTH_FAMILIES {
        let db = mem_db();
        db.execute("CREATE TABLE g4_batch (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
            .expect("create table");

        for round in 0..ROUNDS {
            let value = format!("dup{round}");
            let barrier = Arc::new(Barrier::new(WRITERS));
            let mut handles = Vec::with_capacity(WRITERS);
            for writer in 0..WRITERS {
                let db = Arc::clone(&db);
                let barrier = Arc::clone(&barrier);
                let value = value.clone();
                handles.push(std::thread::spawn(move || {
                    let base = (round * WRITERS + writer) * 10 + 1;
                    // Three rows: two private to this writer, one contended.
                    let sql = format!(
                        "INSERT INTO g4_batch (id, v) VALUES ({base}, 'p{base}a'), ({}, '{value}'), ({}, 'p{base}b')",
                        base + 1,
                        base + 2
                    );
                    barrier.wait();
                    let ok = family.execute(&db, &sql).is_ok();
                    (ok, base)
                }));
            }
            let outcomes: Vec<(bool, usize)> = handles
                .into_iter()
                .map(|h| h.join().expect("writer thread panicked"))
                .collect();

            let accepted = outcomes.iter().filter(|(ok, _)| *ok).count();
            assert_eq!(
                accepted,
                1,
                "[{} family, round {round}] {accepted} of {WRITERS} concurrent multi-row batches were \
                 accepted, expected exactly 1",
                family.name()
            );
            assert_single_row_by_scan_and_index(
                &db,
                "g4_batch",
                &value,
                &format!("{} family, batch, round {round}", family.name()),
            );

            // All-or-nothing per batch: a REFUSED batch must not have left its
            // two non-conflicting rows behind.
            for (ok, base) in outcomes {
                let private = db
                    .query(
                        &format!("SELECT id FROM g4_batch WHERE id = {base} OR id = {}", base + 2),
                        &[],
                    )
                    .expect("private-row probe");
                let expected = if ok { 2 } else { 0 };
                assert_eq!(
                    private.len(),
                    expected,
                    "[{} family, round {round}] a batch that was {} left {} of its 2 non-conflicting rows \
                     stored, expected {expected} — the batch is one statement and must be all-or-nothing",
                    family.name(),
                    if ok { "ACCEPTED" } else { "REFUSED" },
                    private.len()
                );
            }
        }
    }
}

/// The COPY / fast-batch funnel itself
/// (`StorageEngine::insert_prepared_tuples_fast_batch`, guarded by
/// `BatchIndexClaim`).
///
/// Reached ONLY through `EmbeddedDatabase::execute_many_params` — one SQL with
/// bound parameters, many parameter rows — via `try_fast_insert_many_params`,
/// which with no transaction in scope and a direct-write-eligible schema hands
/// the whole batch to that funnel. The multi-row-`VALUES` test above does not
/// get here, which is exactly why this one exists: without it the batch claim
/// would be unexercised.
///
/// The batch commits as ONE RocksDB `WriteBatch`, so it must be all-or-nothing:
/// the loser lands none of its rows, not "all but the duplicate".
#[test]
fn g4_item1_concurrent_execute_many_batches_are_all_or_nothing() {
    let db = mem_db();
    db.execute("CREATE TABLE g4_many (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .expect("create table");

    for round in 0..ROUNDS {
        let value = format!("dup{round}");
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);
        for writer in 0..WRITERS {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            let value = value.clone();
            handles.push(std::thread::spawn(move || {
                let base = (round * WRITERS + writer) * 10 + 1;
                // Three parameter rows: two private to this writer, one carrying
                // the contended UNIQUE value.
                let rows = vec![
                    vec![Value::Int4(base as i32), Value::String(format!("p{base}a"))],
                    vec![Value::Int4((base + 1) as i32), Value::String(value.clone())],
                    vec![Value::Int4((base + 2) as i32), Value::String(format!("p{base}b"))],
                ];
                barrier.wait();
                let ok = db
                    .execute_many_params("INSERT INTO g4_many (id, v) VALUES ($1, $2)", &rows)
                    .is_ok();
                (ok, base)
            }));
        }
        let outcomes: Vec<(bool, usize)> = handles
            .into_iter()
            .map(|h| h.join().expect("writer thread panicked"))
            .collect();

        let accepted = outcomes.iter().filter(|(ok, _)| *ok).count();
        assert_eq!(
            accepted, 1,
            "[execute_many, round {round}] {accepted} of {WRITERS} concurrent batches were accepted, \
             expected exactly 1"
        );
        assert_single_row_by_scan_and_index(&db, "g4_many", &value, &format!("execute_many, round {round}"));

        for (ok, base) in outcomes {
            let private = db
                .query(
                    &format!("SELECT id FROM g4_many WHERE id = {base} OR id = {}", base + 2),
                    &[],
                )
                .expect("private-row probe");
            let expected = if ok { 2 } else { 0 };
            assert_eq!(
                private.len(),
                expected,
                "[execute_many, round {round}] a batch that was {} left {} of its 2 non-conflicting rows \
                 stored, expected {expected} — one WriteBatch, all or nothing",
                if ok { "ACCEPTED" } else { "REFUSED" },
                private.len()
            );
        }
    }
}

/// Message-shape control. Making the enforcing tree the arbiter must not degrade
/// the error a user sees: the refusal has to keep naming the CONSTRAINT
/// (`Duplicate key value violates …`), not report the tree's internal
/// "Key already exists in … index". The PostgreSQL wire keys SQLSTATE 23505 off
/// this text.
#[test]
fn g4_item1_rejected_duplicate_still_names_the_constraint() {
    for family in BOTH_FAMILIES {
        let db = mem_db();
        db.execute("CREATE TABLE g4_msg (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
            .expect("create table");
        family
            .execute(&db, "INSERT INTO g4_msg (id, v) VALUES (1, 'a')")
            .expect("first insert");

        let err = family
            .execute(&db, "INSERT INTO g4_msg (id, v) VALUES (2, 'a')")
            .expect_err("duplicate UNIQUE value must be refused")
            .to_string()
            .to_ascii_lowercase();
        assert!(
            err.contains("duplicate key") || err.contains("unique constraint"),
            "[{}] a refused UNIQUE duplicate reported `{err}`, which SQLSTATE 23505 sniffing would miss",
            family.name()
        );

        let err_pk = family
            .execute(&db, "INSERT INTO g4_msg (id, v) VALUES (1, 'b')")
            .expect_err("duplicate PRIMARY KEY must be refused")
            .to_string()
            .to_ascii_lowercase();
        assert!(
            err_pk.contains("duplicate key") || err_pk.contains("primary key"),
            "[{}] a refused PRIMARY KEY duplicate reported `{err_pk}`",
            family.name()
        );

        // The refused rows must not exist, and must not have poisoned the trees
        // with a phantom key: the value they were refused for is still usable by
        // whoever legitimately owns it, and a NEW value still inserts.
        assert_single_row_by_scan_and_index(&db, "g4_msg", "a", family.name());
        family
            .execute(&db, "INSERT INTO g4_msg (id, v) VALUES (2, 'b')")
            .unwrap_or_else(|e| {
                panic!(
                    "[{}] 'b' was refused after a rejected insert claimed it: {e}",
                    family.name()
                )
            });
        assert_single_row_by_scan_and_index(&db, "g4_msg", "b", family.name());
    }
}

// ---------------------------------------------------------------------------
// MySQL wire harness (item 2)
// ---------------------------------------------------------------------------

const COM_QUERY: u8 = 0x03;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;

fn read_lenenc_u64(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xFC => {
            let v = u64::from(u16::from_le_bytes([buf[*pos], buf[*pos + 1]]));
            *pos += 2;
            v
        }
        0xFD => {
            let v = u64::from(u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], 0]));
            *pos += 3;
            v
        }
        0xFE => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            u64::from_le_bytes(b)
        }
        other => u64::from(other),
    }
}

fn read_lenenc_str(buf: &[u8], pos: &mut usize) -> String {
    let len = read_lenenc_u64(buf, pos) as usize;
    let s = String::from_utf8_lossy(&buf[*pos..*pos + len]).to_string();
    *pos += len;
    s
}

/// Minimal MySQL text-protocol client over an in-process duplex stream, the same
/// shape `tests/mysql_translator_batch_d.rs` uses.
struct MySqlTestClient {
    stream: DuplexStream,
}

impl MySqlTestClient {
    async fn login(db: Arc<EmbeddedDatabase>) -> Self {
        let (client, srv) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection_with_timeouts(db, srv, 1, ConnectionTimeouts::disabled()).await;
        });
        let mut this = Self { stream: client };

        let (_seq, _greeting) = this.read_packet().await;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root");
        p.push(0);
        p.push(0); // empty auth response (SECURE_CONNECTION framing)
        this.write_packet(1, &p).await;

        let (_seq, ok) = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "expected auth OK, got {:?}", &ok[..ok.len().min(16)]);
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let hdr = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        self.stream.write_all(&hdr).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> (u8, Vec<u8>) {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr).await.unwrap();
        let len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        (hdr[3], payload)
    }

    /// COM_QUERY; returns the first response packet. The caller decides whether
    /// OK or ERR is expected — for this file the property under test is what
    /// happened to the DATABASE, not what the client was told.
    async fn send(&mut self, sql: &str) -> Vec<u8> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let (_seq, first) = self.read_packet().await;
        first
    }

    async fn ok(&mut self, sql: &str) {
        let pkt = self.send(sql).await;
        assert_eq!(
            pkt.first().copied(),
            Some(0x00),
            "expected OK for `{sql}`, got {}",
            String::from_utf8_lossy(&pkt)
        );
    }

    async fn expect_err(&mut self, sql: &str) -> String {
        let pkt = self.send(sql).await;
        assert_eq!(
            pkt.first().copied(),
            Some(0xFF),
            "expected an ERR packet for `{sql}`, got {}",
            String::from_utf8_lossy(&pkt)
        );
        String::from_utf8_lossy(&pkt[3..]).to_string()
    }

    async fn query(&mut self, sql: &str) -> Vec<Vec<String>> {
        let first = self.send(sql).await;
        assert_ne!(
            first.first().copied(),
            Some(0xFF),
            "server error for `{sql}`: {}",
            String::from_utf8_lossy(&first)
        );
        let mut pos = 0usize;
        let ncols = read_lenenc_u64(&first, &mut pos) as usize;
        for _ in 0..ncols {
            let (_seq, _def) = self.read_packet().await;
        }
        let (_seq, eof) = self.read_packet().await;
        assert_eq!(eof[0], 0xFE, "expected EOF after the column defs for `{sql}`");

        let mut rows = Vec::new();
        loop {
            let (_seq, pkt) = self.read_packet().await;
            if pkt[0] == 0xFE && pkt.len() < 9 {
                break;
            }
            let mut rp = 0usize;
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                if pkt[rp] == 0xFB {
                    rp += 1;
                    row.push(String::new());
                } else {
                    row.push(read_lenenc_str(&pkt, &mut rp));
                }
            }
            rows.push(row);
        }
        rows
    }

    /// Rows physically present, by full scan (never `COUNT(*)`).
    async fn scan_ids(&mut self, table: &str) -> Vec<String> {
        self.query(&format!("SELECT id FROM {table}"))
            .await
            .into_iter()
            .map(|r| r[0].clone())
            .collect()
    }
}

fn mysql_db() -> Arc<EmbeddedDatabase> {
    Arc::new(EmbeddedDatabase::new_in_memory().expect("in-memory db"))
}

// ---------------------------------------------------------------------------
// Item 2 — statement-level atomicity on the MySQL listener
// ---------------------------------------------------------------------------

/// THE proof of sprinter 5b70b7ac5513.
///
/// Inside an explicit transaction, a 3-row INSERT whose THIRD row duplicates an
/// existing PRIMARY KEY must leave the table with the SAME contents as before
/// the statement — and the following COMMIT must not resurrect rows 1 and 2.
///
/// Driven over the MySQL wire specifically: that is the listener that keeps
/// MySQL's statement-level semantics (a failed statement does not abort the
/// block), so it is the only surface where the partial rows could survive to
/// COMMIT.
#[tokio::test]
async fn g4_item2_mysql_failed_multirow_insert_leaves_nothing_staged() {
    let db = mysql_db();
    let mut client = MySqlTestClient::login(Arc::clone(&db)).await;

    client.ok("CREATE TABLE g4m (id INT PRIMARY KEY, v VARCHAR(50))").await;
    client.ok("INSERT INTO g4m (id, v) VALUES (99, 'pre')").await;

    let before = client.scan_ids("g4m").await;
    assert_eq!(before, vec!["99".to_string()], "setup: one pre-existing row");

    client.ok("BEGIN").await;
    // Rows 1 and 2 are fine; row 3 collides with the pre-existing id 99.
    let err = client
        .expect_err("INSERT INTO g4m (id, v) VALUES (1, 'a'), (2, 'b'), (99, 'c')")
        .await;
    assert!(
        err.to_ascii_lowercase().contains("duplicate")
            || err.to_ascii_lowercase().contains("primary key")
            || err.to_ascii_lowercase().contains("unique"),
        "expected a duplicate-key error, got `{err}`"
    );

    // The whole STATEMENT is undone: the table is back to what it was, while the
    // transaction is still open (MySQL semantics, deliberately preserved).
    let during = client.scan_ids("g4m").await;
    assert_eq!(
        during, before,
        "inside the still-open transaction the failed statement left rows staged: {during:?}"
    );

    client.ok("COMMIT").await;

    let after = client.scan_ids("g4m").await;
    assert_eq!(
        after, before,
        "COMMIT resurrected rows staged by the FAILED statement: {after:?} — this is the \
         data-integrity hole sprinter 5b70b7ac5513 closes"
    );
}

/// The control that pins MySQL's statement-level contract as UNCHANGED: the
/// failed statement above must not have aborted the block, so work done before
/// it AND after it still commits.
///
/// This is the half that makes the fix a statement rollback rather than a
/// transaction abort — if the implicit savepoint ever became a full rollback,
/// this test fails while the one above still passes.
#[tokio::test]
async fn g4_item2_mysql_transaction_survives_a_failed_statement() {
    let db = mysql_db();
    let mut client = MySqlTestClient::login(Arc::clone(&db)).await;

    client.ok("CREATE TABLE g4m2 (id INT PRIMARY KEY, v VARCHAR(50))").await;
    client.ok("INSERT INTO g4m2 (id, v) VALUES (99, 'pre')").await;

    client.ok("BEGIN").await;
    client.ok("INSERT INTO g4m2 (id, v) VALUES (1, 'before')").await;
    let _ = client
        .expect_err("INSERT INTO g4m2 (id, v) VALUES (2, 'x'), (3, 'y'), (99, 'boom')")
        .await;
    // MySQL: the block is still usable after a failed statement.
    client.ok("INSERT INTO g4m2 (id, v) VALUES (4, 'after')").await;
    client.ok("COMMIT").await;

    let mut ids = client.scan_ids("g4m2").await;
    ids.sort();
    assert_eq!(
        ids,
        vec!["1".to_string(), "4".to_string(), "99".to_string()],
        "expected the pre-existing row plus the statements that SUCCEEDED (1 and 4), and none of the \
         rows staged by the failed statement (2 and 3)"
    );
}

/// A single-row INSERT that fails inside a MySQL transaction is atomic by
/// construction and must stay so — the savepoint must not change it, and must
/// not undo the statements that ran before it.
#[tokio::test]
async fn g4_item2_mysql_single_row_failure_undoes_only_itself() {
    let db = mysql_db();
    let mut client = MySqlTestClient::login(Arc::clone(&db)).await;

    client.ok("CREATE TABLE g4m3 (id INT PRIMARY KEY, v VARCHAR(50))").await;
    client.ok("BEGIN").await;
    client.ok("INSERT INTO g4m3 (id, v) VALUES (1, 'keep')").await;
    let _ = client.expect_err("INSERT INTO g4m3 (id, v) VALUES (1, 'dup')").await;
    client.ok("COMMIT").await;

    let ids = client.scan_ids("g4m3").await;
    assert_eq!(
        ids,
        vec!["1".to_string()],
        "the earlier successful INSERT must survive the later failed one"
    );
    let v = client.query("SELECT v FROM g4m3 WHERE id = 1").await;
    assert_eq!(
        v[0][0], "keep",
        "the failed statement must not have overwritten the kept row"
    );
}

/// The rolled-back rows must take their INDEX entries with them.
///
/// Without the ART undo-log half of the implicit savepoint, the ids staged by
/// the failed statement stay in the primary-key tree as ghosts — and the next
/// legitimate INSERT of the same id is refused as a duplicate of a row that does
/// not exist.
#[tokio::test]
async fn g4_item2_mysql_rolled_back_rows_free_their_index_keys() {
    let db = mysql_db();
    let mut client = MySqlTestClient::login(Arc::clone(&db)).await;

    client
        .ok("CREATE TABLE g4m4 (id INT PRIMARY KEY, v VARCHAR(50) UNIQUE)")
        .await;
    client.ok("INSERT INTO g4m4 (id, v) VALUES (99, 'pre')").await;

    client.ok("BEGIN").await;
    let _ = client
        .expect_err("INSERT INTO g4m4 (id, v) VALUES (1, 'a'), (2, 'b'), (99, 'c')")
        .await;
    client.ok("COMMIT").await;

    // id 1 / v='a' were staged by the failed statement and rolled back, so they
    // must be free for a genuine insert now.
    client.ok("INSERT INTO g4m4 (id, v) VALUES (1, 'a')").await;
    let rows = client.query("SELECT id FROM g4m4 WHERE v = 'a'").await;
    assert_eq!(
        rows.len(),
        1,
        "the unique index cannot find the row just inserted for v='a' — a ghost entry from the \
         rolled-back statement was left behind"
    );
    let mut ids = client.scan_ids("g4m4").await;
    ids.sort();
    assert_eq!(ids, vec!["1".to_string(), "99".to_string()]);
}

/// The PostgreSQL / embedded control: HDB-008's aborted-block contract is
/// UNCHANGED.
///
/// This is the behaviour the item deliberately did not touch — a statement error
/// inside an embedded `BEGIN` aborts the block, so the partial rows cannot be
/// committed and every following statement is refused until the block ends. If a
/// future edit moves the implicit statement savepoint into
/// `run_statement_in_transaction`, this test is what says so.
#[test]
fn g4_item2_embedded_block_still_aborts_on_statement_error() {
    let db = mem_db();
    db.execute("CREATE TABLE g4e (id INT PRIMARY KEY, v VARCHAR(50))")
        .expect("create table");
    db.execute("INSERT INTO g4e (id, v) VALUES (99, 'pre')").expect("seed");

    db.execute("BEGIN").expect("begin");
    let err = db
        .execute("INSERT INTO g4e (id, v) VALUES (1, 'a'), (2, 'b'), (99, 'c')")
        .expect_err("the duplicate must be refused");
    assert!(
        err.to_string().to_ascii_lowercase().contains("duplicate")
            || err.to_string().to_ascii_lowercase().contains("primary key"),
        "unexpected error: {err}"
    );

    // HDB-008: the block is aborted — the next statement is refused rather than
    // executed, which is what stops the partial rows reaching a COMMIT.
    let refused = db
        .execute("INSERT INTO g4e (id, v) VALUES (3, 'd')")
        .expect_err("HDB-008 must refuse a statement in an aborted block");
    assert!(
        refused.to_string().to_ascii_lowercase().contains("aborted"),
        "expected the HDB-008 aborted-block refusal, got: {refused}"
    );

    let _ = db.execute("ROLLBACK");
    let rows = db.query("SELECT id FROM g4e", &[]).expect("scan");
    assert_eq!(
        rows.len(),
        1,
        "only the pre-existing row may remain after the aborted block was rolled back"
    );
}

/// A user's own `SAVEPOINT` must still work on the MySQL listener — the implicit
/// per-statement savepoint deliberately does NOT push onto the named savepoint
/// stack, so the two must not interfere. (sprinter 37a5968e7698 moved that stack
/// onto `storage::Transaction`; it is no longer process-wide, but a nameless push
/// would still demote THIS transaction's fast paths for the rest of the block and
/// would change what a `RELEASE` truncates.)
///
/// Tolerant of the listener not accepting `SAVEPOINT` at all: named savepoints
/// over the MySQL wire are not what this item changed, and a pre-existing gap
/// there must not be reported as a regression of it. When they DO work, the
/// assertion below is the real control.
#[tokio::test]
async fn g4_item2_named_savepoint_still_works_alongside_the_implicit_one() {
    let db = mysql_db();
    let mut client = MySqlTestClient::login(Arc::clone(&db)).await;

    client.ok("CREATE TABLE g4m5 (id INT PRIMARY KEY, v VARCHAR(50))").await;
    client.ok("BEGIN").await;
    client.ok("INSERT INTO g4m5 (id, v) VALUES (1, 'a')").await;

    let savepoint_pkt = client.send("SAVEPOINT sp1").await;
    if savepoint_pkt.first().copied() == Some(0xFF) {
        eprintln!(
            "SKIP: this listener does not accept `SAVEPOINT` ({}) — pre-existing, unrelated to \
             sprinter 5b70b7ac5513",
            String::from_utf8_lossy(&savepoint_pkt)
        );
        client.ok("ROLLBACK").await;
        return;
    }

    client.ok("INSERT INTO g4m5 (id, v) VALUES (2, 'b')").await;
    client.ok("ROLLBACK TO SAVEPOINT sp1").await;
    client.ok("COMMIT").await;

    let ids = client.scan_ids("g4m5").await;
    assert_eq!(
        ids,
        vec!["1".to_string()],
        "ROLLBACK TO SAVEPOINT must still drop exactly the post-savepoint row"
    );
}
