//! The three interfaces a script can be run through: the embedded API, the PostgreSQL
//! wire (a listener on an ephemeral loopback port, a real client library) and the MySQL
//! wire (the connection handler over an in-memory stream, a minimal text-protocol client).

use super::script::Cell;
use heliosdb_nano::protocol::mysql::MySqlHandler;
use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// Which interface a run goes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interface {
    Embedded,
    PgWire,
    MySqlWire,
}

impl Interface {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "embedded" => Some(Self::Embedded),
            "pgwire" => Some(Self::PgWire),
            "mysqlwire" => Some(Self::MySqlWire),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::PgWire => "pgwire",
            Self::MySqlWire => "mysqlwire",
        }
    }

    /// The engine name the corpus's `skipif` / `onlyif` lines are matched against.
    pub fn engine_label(self) -> &'static str {
        match self {
            Self::Embedded | Self::PgWire => "postgresql",
            Self::MySqlWire => "mysql",
        }
    }

    pub fn open(self) -> Result<Box<dyn Engine>, String> {
        Ok(match self {
            Self::Embedded => Box::new(Embedded::open()?),
            Self::PgWire => Box::new(PgWire::open()?),
            Self::MySqlWire => Box::new(MySqlWire::open()?),
        })
    }
}

/// One fresh, empty database behind one interface.
pub trait Engine {
    fn statement(&mut self, sql: &str) -> Result<(), String>;
    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String>;
}

fn fresh_db() -> Result<EmbeddedDatabase, String> {
    EmbeddedDatabase::new_in_memory().map_err(|e| format!("open: {e}"))
}

pub struct Embedded {
    db: EmbeddedDatabase,
}

impl Embedded {
    pub fn open() -> Result<Self, String> {
        Ok(Self { db: fresh_db()? })
    }
}

fn cell_of(value: &Value) -> Cell {
    match value {
        Value::Null => Cell::Null,
        Value::Boolean(b) => Cell::Text(if *b { "t" } else { "f" }.to_string()),
        Value::Int2(i) => Cell::Int(i64::from(*i)),
        Value::Int4(i) => Cell::Int(i64::from(*i)),
        Value::Int8(i) => Cell::Int(*i),
        Value::Float4(f) => Cell::Float(f64::from(*f)),
        Value::Float8(f) => Cell::Float(*f),
        Value::Numeric(n) => Cell::Text(n.clone()),
        Value::String(s) => Cell::Text(s.clone()),
        other => {
            let shown = other.to_string();
            let inner = shown
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .unwrap_or(&shown);
            Cell::Text(inner.to_string())
        }
    }
}

impl Engine for Embedded {
    fn statement(&mut self, sql: &str) -> Result<(), String> {
        self.db.execute(sql).map(|_| ()).map_err(|e| e.to_string())
    }

    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
        let rows = self.db.query(sql, &[]).map_err(|e| e.to_string())?;
        Ok(rows.iter().map(|t| t.values.iter().map(cell_of).collect()).collect())
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))
}

pub struct PgWire {
    rt: tokio::runtime::Runtime,
    client: tokio_postgres::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl PgWire {
    pub fn open() -> Result<Self, String> {
        let rt = runtime()?;
        let db = Arc::new(fresh_db()?);
        let (client, tasks) = rt.block_on(async {
            // An ephemeral port: bind to 0, read the port back, hand it to the server.
            let probe = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            let addr = probe.local_addr().map_err(|e| e.to_string())?;
            drop(probe);
            let server = PgServer::new(PgServerConfig::with_address(addr).with_max_connections(4), db)
                .map_err(|e| e.to_string())?;
            let listener = tokio::spawn(async move {
                let _ = server.serve().await;
            });
            let conn = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", addr.port());
            let mut last = String::new();
            for _ in 0..100 {
                match tokio_postgres::connect(&conn, tokio_postgres::NoTls).await {
                    Ok((client, connection)) => {
                        let pump = tokio::spawn(async move {
                            let _ = connection.await;
                        });
                        return Ok((client, vec![listener, pump]));
                    }
                    Err(e) => last = e.to_string(),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            listener.abort();
            Err::<_, String>(format!("no connection to the listener on {addr}: {last}"))
        })?;
        Ok(Self { rt, client, tasks })
    }

    fn run(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
        let messages = self
            .rt
            .block_on(self.client.simple_query(sql))
            .map_err(|e| match e.as_db_error() {
                Some(db) => format!("{}: {}", db.code().code(), db.message()),
                None => e.to_string(),
            })?;
        let mut rows = Vec::new();
        for message in messages {
            if let tokio_postgres::SimpleQueryMessage::Row(row) = message {
                rows.push(
                    (0..row.len())
                        .map(|i| match row.get(i) {
                            Some(text) => Cell::Text(text.to_string()),
                            None => Cell::Null,
                        })
                        .collect(),
                );
            }
        }
        Ok(rows)
    }
}

impl Drop for PgWire {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Engine for PgWire {
    fn statement(&mut self, sql: &str) -> Result<(), String> {
        self.run(sql).map(|_| ())
    }

    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
        self.run(sql)
    }
}

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const COM_QUERY: u8 = 0x03;

pub struct MySqlWire {
    rt: tokio::runtime::Runtime,
    stream: DuplexStream,
    server: tokio::task::JoinHandle<()>,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = self.buf.get(self.pos).copied().ok_or("short packet")?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let s = self.buf.get(self.pos..self.pos + n).ok_or("short packet")?;
        self.pos += n;
        Ok(s)
    }

    fn lenenc(&mut self) -> Result<u64, String> {
        let first = self.byte()?;
        let width = match first {
            0xFC => 2,
            0xFD => 3,
            0xFE => 8,
            v => return Ok(u64::from(v)),
        };
        let mut out = [0u8; 8];
        for (slot, b) in out.iter_mut().zip(self.take(width)?) {
            *slot = *b;
        }
        Ok(u64::from_le_bytes(out))
    }
}

impl MySqlWire {
    pub fn open() -> Result<Self, String> {
        let rt = runtime()?;
        let db = Arc::new(fresh_db()?);
        let (client, server) = tokio::io::duplex(1 << 20);
        let server = rt.spawn(async move {
            let _ = MySqlHandler::handle_connection(db, server, 1).await;
        });
        let mut this = Self {
            rt,
            stream: client,
            server,
        };
        let _greeting = this.read_packet()?;
        let mut p = Vec::new();
        p.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        p.extend_from_slice(&(1u32 << 24).to_le_bytes());
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"root\0");
        p.push(0);
        this.write_packet(1, &p)?;
        let ok = this.read_packet()?;
        if ok.first().copied() != Some(0x00) {
            return Err(format!("login refused: {}", String::from_utf8_lossy(&ok)));
        }
        Ok(this)
    }

    fn write_packet(&mut self, seq: u8, payload: &[u8]) -> Result<(), String> {
        let len = payload.len();
        if len >= 0x00FF_FFFF {
            return Err("statement too large for one packet".to_string());
        }
        let hdr = [
            (len & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            ((len >> 16) & 0xFF) as u8,
            seq,
        ];
        let stream = &mut self.stream;
        self.rt
            .block_on(async {
                stream.write_all(&hdr).await?;
                stream.write_all(payload).await?;
                stream.flush().await
            })
            .map_err(|e| format!("write: {e}"))
    }

    fn read_packet(&mut self) -> Result<Vec<u8>, String> {
        let stream = &mut self.stream;
        self.rt.block_on(async {
            let mut hdr = [0u8; 4];
            stream.read_exact(&mut hdr).await.map_err(|e| format!("read: {e}"))?;
            let len = usize::from(hdr[0]) | (usize::from(hdr[1]) << 8) | (usize::from(hdr[2]) << 16);
            let mut payload = vec![0u8; len];
            stream
                .read_exact(&mut payload)
                .await
                .map_err(|e| format!("read: {e}"))?;
            Ok(payload)
        })
    }

    fn run(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p)?;
        let first = self.read_packet()?;
        match first.first().copied() {
            Some(0x00) => return Ok(Vec::new()),
            Some(0xFF) => {
                let code = u16::from_le_bytes([first.get(1).copied().unwrap_or(0), first.get(2).copied().unwrap_or(0)]);
                let rest = first.get(3..).unwrap_or(&[]);
                let (state, message) = if rest.first().copied() == Some(b'#') && rest.len() >= 6 {
                    (
                        String::from_utf8_lossy(&rest[1..6]).into_owned(),
                        String::from_utf8_lossy(&rest[6..]).into_owned(),
                    )
                } else {
                    (String::new(), String::from_utf8_lossy(rest).into_owned())
                };
                return Err(format!("{code} ({state}): {message}"));
            }
            _ => {}
        }
        let ncols = Reader { buf: &first, pos: 0 }.lenenc()? as usize;
        for _ in 0..ncols {
            self.read_packet()?;
        }
        let eof = self.read_packet()?;
        if eof.first().copied() != Some(0xFE) {
            return Err("no end marker after the column definitions".to_string());
        }
        let mut rows = Vec::new();
        loop {
            let packet = self.read_packet()?;
            if packet.first().copied() == Some(0xFE) && packet.len() < 9 {
                break;
            }
            if packet.first().copied() == Some(0xFF) {
                return Err(format!(
                    "error in the middle of a result: {}",
                    String::from_utf8_lossy(packet.get(3..).unwrap_or(&[]))
                ));
            }
            let mut reader = Reader { buf: &packet, pos: 0 };
            let mut row = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                if reader.buf.get(reader.pos).copied() == Some(0xFB) {
                    reader.pos += 1;
                    row.push(Cell::Null);
                } else {
                    let len = reader.lenenc()? as usize;
                    row.push(Cell::Text(String::from_utf8_lossy(reader.take(len)?).into_owned()));
                }
            }
            rows.push(row);
        }
        Ok(rows)
    }
}

impl Drop for MySqlWire {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Engine for MySqlWire {
    fn statement(&mut self, sql: &str) -> Result<(), String> {
        self.run(sql).map(|_| ())
    }

    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Cell>>, String> {
        self.run(sql)
    }
}
