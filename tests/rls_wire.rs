//! Row-level security over the wires: a PostgreSQL client (simple and
//! extended protocol, COPY) and a MySQL client get the filtered rows and the
//! right error codes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use futures::StreamExt;
use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::EmbeddedDatabase;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

/// docs: 1, 2 alice; 3 bob. Row security is on; each role sees its own rows.
fn seeded() -> Arc<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT)").unwrap();
    db.execute("INSERT INTO docs VALUES (1, 'alice'), (2, 'alice'), (3, 'bob')").unwrap();
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    db.execute("CREATE MATERIALIZED VIEW docs_ids AS SELECT id FROM docs").unwrap();
    Arc::new(db)
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

async fn serve(db: Arc<EmbeddedDatabase>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let server = PgServer::new(PgServerConfig::with_address(addr), db).expect("server");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (addr.port(), handle)
}

async fn connect(port: u16, user: &str) -> Client {
    let conn = format!("host=127.0.0.1 port={port} user={user} dbname=postgres");
    let (client, connection) = tokio_postgres::connect(&conn, NoTls).await.expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn simple_ids(client: &Client, sql: &str) -> Vec<String> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or("").to_string()),
            _ => None,
        })
        .collect()
}

async fn sqlstate(client: &Client, sql: &str) -> String {
    let e = client.simple_query(sql).await.expect_err(sql);
    e.code()
        .unwrap_or_else(|| panic!("{sql}: no SQLSTATE in {e}"))
        .code()
        .to_string()
}

async fn copy_lines(client: &Client, sql: &str) -> Vec<String> {
    let stream = client.copy_out(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    let chunks: Vec<_> = stream.collect().await;
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(&chunk.unwrap_or_else(|e| panic!("{sql}: {e}")));
    }
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_postgres_client_sees_its_own_rows_on_both_protocols() {
    let (port, server) = serve(seeded()).await;
    let alice = connect(port, "alice").await;
    let bob = connect(port, "bob").await;
    let postgres = connect(port, "postgres").await;

    for _ in 0..2 {
        assert_eq!(simple_ids(&alice, "SELECT id FROM docs ORDER BY id").await, ["1", "2"]);
        assert_eq!(simple_ids(&bob, "SELECT id FROM docs ORDER BY id").await, ["3"]);
        assert_eq!(simple_ids(&postgres, "SELECT id FROM docs ORDER BY id").await, ["1", "2", "3"]);
        assert_eq!(simple_ids(&alice, "SELECT count(*) FROM docs").await, ["2"]);
        assert_eq!(simple_ids(&bob, "SELECT id FROM docs WHERE id = 1").await, Vec::<String>::new());
    }

    // Extended protocol: a prepared statement, reused, with a bound parameter.
    let by_id = "SELECT id FROM docs WHERE id = $1";
    for (client, id, expected) in [(&alice, 1, 1), (&alice, 3, 0), (&bob, 3, 1), (&bob, 1, 0), (&postgres, 1, 1)] {
        let statement = client.prepare(by_id).await.unwrap();
        for _ in 0..2 {
            let rows = client.query(&statement, &[&id]).await.unwrap();
            assert_eq!(rows.len(), expected, "id {id}");
        }
    }
    let rows = alice.query("SELECT id FROM docs ORDER BY id", &[]).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.get::<_, i32>(0)).collect::<Vec<_>>(), [1, 2]);

    // Writes.
    assert_eq!(alice.execute("UPDATE docs SET owner = owner", &[]).await.unwrap(), 2);
    assert_eq!(bob.execute("DELETE FROM docs WHERE id = $1", &[&1i32]).await.unwrap(), 0);
    let e = bob
        .execute("INSERT INTO docs VALUES ($1, $2)", &[&9i32, &"alice"])
        .await
        .unwrap_err();
    assert_eq!(e.code().unwrap().code(), "42501", "{e}");
    assert!(e.to_string().contains("new row violates row-level security policy for table \"docs\""));
    assert_eq!(simple_ids(&postgres, "SELECT count(*) FROM docs").await, ["3"]);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_out_is_filtered() {
    let (port, server) = serve(seeded()).await;
    let alice = connect(port, "alice").await;
    let bob = connect(port, "bob").await;
    let postgres = connect(port, "postgres").await;
    assert_eq!(copy_lines(&alice, "COPY docs TO STDOUT").await.len(), 2);
    assert_eq!(copy_lines(&bob, "COPY docs TO STDOUT").await, ["3\tbob"]);
    assert_eq!(copy_lines(&postgres, "COPY docs TO STDOUT").await.len(), 3);
    assert_eq!(copy_lines(&alice, "COPY (SELECT id FROM docs ORDER BY id) TO STDOUT").await, ["1", "2"]);
    assert_eq!(copy_lines(&bob, "COPY (SELECT id FROM docs ORDER BY id) TO STDOUT").await, ["3"]);
    assert_eq!(copy_lines(&alice, "COPY docs (id) TO STDOUT").await.len(), 2);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_errors_carry_postgres_sqlstates() {
    let (port, server) = serve(seeded()).await;
    let alice = connect(port, "alice").await;
    let postgres = connect(port, "postgres").await;

    assert_eq!(sqlstate(&alice, "INSERT INTO docs VALUES (9, 'bob')").await, "42501");
    assert_eq!(sqlstate(&alice, "CREATE POLICY p ON docs USING (true)").await, "42501");
    assert_eq!(sqlstate(&alice, "ALTER TABLE docs DISABLE ROW LEVEL SECURITY").await, "42501");
    // A materialized view whose stored rows the policy cannot be applied to.
    assert_eq!(sqlstate(&alice, "SELECT * FROM docs_ids").await, "42501");
    assert_eq!(simple_ids(&postgres, "SELECT count(*) FROM docs_ids").await, ["3"]);
    alice.simple_query("SET row_security = off").await.unwrap();
    assert_eq!(sqlstate(&alice, "SELECT id FROM docs").await, "42501");
    alice.simple_query("SET row_security = on").await.unwrap();

    assert_eq!(sqlstate(&postgres, "CREATE POLICY own ON docs USING (true)").await, "42710");
    assert_eq!(sqlstate(&postgres, "DROP POLICY nope ON docs").await, "42704");
    assert_eq!(sqlstate(&postgres, "ALTER POLICY nope ON docs USING (true)").await, "42704");
    assert_eq!(sqlstate(&postgres, "CREATE POLICY p ON docs TO nobody USING (true)").await, "42704");
    assert_eq!(sqlstate(&postgres, "CREATE POLICY p ON missing USING (true)").await, "42P01");
    assert_eq!(sqlstate(&postgres, "CREATE POLICY p ON docs FOR SELECT WITH CHECK (true)").await, "42601");
    assert_eq!(
        sqlstate(&postgres, "CREATE VIEW v WITH (security_invoker = false) AS SELECT id FROM docs").await,
        "0A000"
    );
    assert_eq!(
        sqlstate(&postgres, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY, ADD COLUMN x INT").await,
        "0A000"
    );
    postgres
        .simple_query("CREATE POLICY loops ON docs USING (id IN (SELECT id FROM docs))")
        .await
        .unwrap();
    assert_eq!(sqlstate(&alice, "SELECT id FROM docs").await, "42P17");
    postgres.simple_query("DROP POLICY loops ON docs").await.unwrap();
    assert_eq!(simple_ids(&alice, "SELECT id FROM docs ORDER BY id").await, ["1", "2"]);

    // The statements themselves, and the catalog, over the wire.
    postgres
        .simple_query("ALTER TABLE docs FORCE ROW LEVEL SECURITY")
        .await
        .unwrap();
    assert_eq!(
        simple_ids(&postgres, "SELECT relforcerowsecurity FROM pg_class WHERE relname = 'docs'").await,
        ["t"]
    );
    assert_eq!(
        simple_ids(&postgres, "SELECT policyname FROM pg_policies WHERE tablename = 'docs'").await,
        ["own"]
    );
    server.abort();
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

struct MySqlClient {
    stream: DuplexStream,
}

enum MySqlReply {
    Ok(u64),
    Err { code: u16, message: String },
    Rows(Vec<Vec<Option<String>>>),
}

fn lenenc(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xFC => {
            let v = u64::from(u16::from_le_bytes([buf[*pos], buf[*pos + 1]]));
            *pos += 2;
            v
        }
        0xFD => {
            let v = u64::from(buf[*pos]) | (u64::from(buf[*pos + 1]) << 8) | (u64::from(buf[*pos + 2]) << 16);
            *pos += 3;
            v
        }
        v => u64::from(v),
    }
}

impl MySqlClient {
    async fn connect(db: Arc<EmbeddedDatabase>, user: &str) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = MySqlHandler::handle_connection(db, server, 1).await;
        });
        let mut this = Self { stream: client };
        let _greeting = this.read_packet().await;
        // HandshakeResponse41: PROTOCOL_41 | SECURE_CONNECTION, max packet,
        // charset, 23 zero bytes, user, empty auth response.
        let mut p = Vec::new();
        p.extend_from_slice(&(0x0000_0200u32 | 0x0000_8000u32).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(user.as_bytes());
        p.push(0);
        p.push(0);
        this.write_packet(1, &p).await;
        let ok = this.read_packet().await;
        assert_eq!(ok[0], 0x00, "expected auth OK, got {:?}", String::from_utf8_lossy(&ok));
        this
    }

    async fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let len = payload.len();
        let header = [(len & 0xFF) as u8, ((len >> 8) & 0xFF) as u8, ((len >> 16) & 0xFF) as u8, seq];
        self.stream.write_all(&header).await.unwrap();
        self.stream.write_all(payload).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> Vec<u8> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).await.unwrap();
        let len = (header[0] as usize) | ((header[1] as usize) << 8) | ((header[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await.unwrap();
        payload
    }

    async fn query(&mut self, sql: &str) -> MySqlReply {
        let mut p = vec![0x03u8];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p).await;
        let first = self.read_packet().await;
        match first[0] {
            0x00 => {
                let mut pos = 1;
                MySqlReply::Ok(lenenc(&first, &mut pos))
            }
            0xFF => MySqlReply::Err {
                code: u16::from_le_bytes([first[1], first[2]]),
                message: String::from_utf8_lossy(&first[3..]).into_owned(),
            },
            _ => {
                let mut pos = 0;
                let columns = lenenc(&first, &mut pos) as usize;
                for _ in 0..columns {
                    self.read_packet().await;
                }
                let eof = self.read_packet().await;
                assert_eq!(eof[0], 0xFE, "expected EOF after the column definitions");
                let mut rows = Vec::new();
                loop {
                    let packet = self.read_packet().await;
                    if packet[0] == 0xFE && packet.len() < 9 {
                        break;
                    }
                    let mut at = 0;
                    let mut row = Vec::with_capacity(columns);
                    for _ in 0..columns {
                        if packet[at] == 0xFB {
                            at += 1;
                            row.push(None);
                        } else {
                            let len = lenenc(&packet, &mut at) as usize;
                            row.push(Some(String::from_utf8_lossy(&packet[at..at + len]).into_owned()));
                            at += len;
                        }
                    }
                    rows.push(row);
                }
                MySqlReply::Rows(rows)
            }
        }
    }

    async fn first_column(&mut self, sql: &str) -> Vec<String> {
        match self.query(sql).await {
            MySqlReply::Rows(rows) => rows.into_iter().map(|r| r[0].clone().unwrap_or_default()).collect(),
            MySqlReply::Ok(n) => panic!("{sql}: OK({n}) instead of rows"),
            MySqlReply::Err { code, message } => panic!("{sql}: error {code}: {message}"),
        }
    }
}

#[tokio::test]
async fn a_mysql_client_sees_its_own_rows_and_is_refused_a_forbidden_write() {
    let db = seeded();
    let mut alice = MySqlClient::connect(Arc::clone(&db), "alice").await;
    let mut bob = MySqlClient::connect(Arc::clone(&db), "bob").await;
    for _ in 0..2 {
        assert_eq!(alice.first_column("SELECT id FROM docs ORDER BY id").await, ["1", "2"]);
        assert_eq!(bob.first_column("SELECT id FROM docs ORDER BY id").await, ["3"]);
        assert_eq!(alice.first_column("SELECT COUNT(*) FROM docs").await, ["2"]);
        assert_eq!(bob.first_column("SELECT id FROM docs WHERE id = 1").await, Vec::<String>::new());
    }
    match bob.query("UPDATE docs SET owner = 'bob'").await {
        MySqlReply::Ok(n) => assert_eq!(n, 1),
        _ => panic!("the UPDATE should report one row"),
    }
    match alice.query("INSERT INTO docs VALUES (9, 'bob')").await {
        MySqlReply::Err { message, .. } => {
            assert!(message.contains("new row violates row-level security policy"), "{message}");
        }
        _ => panic!("the INSERT should be refused"),
    }
    match alice.query("DELETE FROM docs WHERE id = 3").await {
        MySqlReply::Ok(n) => assert_eq!(n, 0),
        _ => panic!("the DELETE should report no row"),
    }
    match alice.query("CREATE POLICY p ON docs USING (true)").await {
        MySqlReply::Err { message, .. } => assert!(message.contains("must be owner of table"), "{message}"),
        _ => panic!("CREATE POLICY by a role that does not own the table should be refused"),
    }
    assert_eq!(db.query("SELECT id FROM docs", &[]).unwrap().len(), 3);
}
