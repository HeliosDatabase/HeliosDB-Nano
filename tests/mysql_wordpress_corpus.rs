//! The statements WordPress core sends, replayed over the MySQL wire against a
//! listener that asks for a password.
//!
//! `tests/fixtures/wordpress_core_queries.sql` holds one statement per line
//! with what is expected of it: `ok` (MySQL runs it and so must this server),
//! `err` (MySQL refuses it too) or `gap` (MySQL runs it, this server does not
//! yet). A `gap` that starts working fails the test, so the list only shrinks
//! on purpose; an `ok` that stops working fails it too.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

mod mysql_wire_client;

use mysql_wire_client::*;
use std::sync::Arc;

const FIXTURE: &str = include_str!("fixtures/wordpress_core_queries.sql");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Ok,
    Err,
    Gap,
}

struct Entry {
    line: usize,
    expect: Expect,
    count: Option<u64>,
    sql: &'static str,
}

fn entries() -> Vec<Entry> {
    FIXTURE
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|(index, line)| {
            let (head, sql) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("line {}: <expectation><TAB><statement>", index + 1));
            let (kind, count) = match head.split_once(':') {
                Some((kind, count)) => (kind, Some(count.parse().expect("a row count"))),
                None => (head, None),
            };
            let expect = match kind {
                "ok" => Expect::Ok,
                "err" => Expect::Err,
                "gap" => Expect::Gap,
                other => panic!("line {}: unknown expectation `{other}`", index + 1),
            };
            Entry {
                line: index + 1,
                expect,
                count,
                sql,
            }
        })
        .collect()
}

/// What the server did with a statement: `Err(text)` or the rows it returned /
/// affected.
fn outcome(answer: &Answer) -> Result<u64, String> {
    match answer {
        Answer::Ok { affected, .. } => Ok(*affected),
        Answer::Rows { rows, .. } => Ok(rows.len() as u64),
        Answer::Err { code, message, .. } => Err(format!("{code} {message}")),
    }
}

#[tokio::test]
async fn wordpress_core_corpus_replays() {
    let db = test_db();
    // WordPress's usual layout: its own account, a password, plain TCP inside
    // a container network (the RSA public-key exchange).
    let auth = scram_auth(&[("admin", "admin-secret")]);
    let mut admin = open(&db, Some(Arc::clone(&auth)), false).await;
    assert!(admin.login_sha2(CAPS, "admin", "admin-secret", FullAuth::PublicKey).await.accepted());
    admin.ok("CREATE USER 'wordpress'@'%' IDENTIFIED BY 'wp-db-password'").await;

    let mut wp = open(&db, Some(auth), false).await;
    wp.send_handshake_response(
        CAPS | CLIENT_CONNECT_WITH_DB,
        "wordpress",
        &sha2_scramble("wp-db-password", &wp.nonce.clone()),
        Some("wordpress"),
        Some(CACHING_SHA2),
    )
    .await;
    assert!(wp.finish_sha2("wp-db-password", FullAuth::PublicKey).await.accepted());

    let entries = entries();
    let mut problems = Vec::new();
    let (mut ok, mut refused, mut gaps) = (0usize, 0usize, 0usize);
    for entry in &entries {
        let got = outcome(&wp.query(entry.sql).await);
        let as_expected = match (&got, entry.count) {
            (Err(_), _) => false,
            (Ok(_), None) => true,
            (Ok(n), Some(expected)) => *n == expected,
        };
        let shown = match &got {
            Ok(n) => format!("{n} rows"),
            Err(text) => format!("error {text}"),
        };
        match entry.expect {
            Expect::Ok if as_expected => ok += 1,
            Expect::Ok => problems.push(format!(
                "line {}: expected ok{}, got {shown}\n    {}",
                entry.line,
                entry.count.map(|n| format!(":{n}")).unwrap_or_default(),
                entry.sql
            )),
            Expect::Err if got.is_err() => refused += 1,
            Expect::Err => problems.push(format!(
                "line {}: MySQL refuses this, got {shown}\n    {}",
                entry.line, entry.sql
            )),
            Expect::Gap if !as_expected => gaps += 1,
            Expect::Gap => problems.push(format!(
                "line {}: listed as a gap and it works now ({shown}): mark it ok\n    {}",
                entry.line, entry.sql
            )),
        }
    }
    assert!(
        problems.is_empty(),
        "{} of {} statements differ from the fixture:\n{}",
        problems.len(),
        entries.len(),
        problems.join("\n")
    );
    // The counts the README sentence about WordPress states.
    println!(
        "wordpress corpus: {} statements, {ok} run as in MySQL, {refused} refused as in MySQL, {gaps} known gaps",
        entries.len()
    );
    assert_eq!(ok + refused + gaps, entries.len());
    assert!(entries.len() >= 100, "the corpus was cut down to {}", entries.len());
}
