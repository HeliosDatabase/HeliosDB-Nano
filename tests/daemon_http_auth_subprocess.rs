//! Real-binary acceptance matrix for the daemon HTTP authentication / bind policy.
//!
//! Every case here launches the ACTUAL `heliosdb-nano` binary and drives it over a
//! real loopback socket, so what is asserted is the assembled product — `start`'s
//! ordering, `run_http_listener`'s router, the wire [`AuthManager`] and the HTTP
//! layer — not a hand-built `tower::oneshot` router. That distinction matters:
//! router-only tests passed while `POST /v1/branches/main/query` was unauthenticated
//! on the daemon.
//!
//! Coverage:
//! * the exact bypass route, `POST /v1/branches/main/query`, with absent / wrong /
//!   unknown-user / valid Basic credentials, asserting the EXACT `SELECT` payload;
//! * a real write-then-read side effect through `POST /v1/branches/main/execute`
//!   and `POST /v1/branches/main/query`;
//! * REST (`/rest/v1/*`), auth (`/auth/v1/signup`), docs (`/docs`,
//!   `/openapi.json`), the realtime WebSocket upgrade, and MCP (only when the
//!   `mcp-endpoint` feature is compiled in), each with a concrete expected status
//!   rather than a bare `!= 401`;
//! * all three password methods (`password`, `md5`, `scram-sha-256`) plus the
//!   `trust --password` HTTP-only override;
//! * `/health` is reachable unauthenticated and is status-ONLY;
//! * the default HTTP bind is proven loopback by reading the child's listening
//!   socket local address out of `/proc/<pid>/net/tcp*` (connecting to 127.0.0.1
//!   alone would also succeed against an `0.0.0.0` bind, so it proves nothing),
//!   with a non-loopback connection-refusal check when a routable address exists;
//! * fail-closed startup cases for remote binds and for MySQL (TCP and UDS) with a
//!   configured credential.
//!
//! The harness owns the child through `Daemon`, whose `Drop` kills and reaps it on
//! every path including a panic, forwards no inherited `HELIOSDB_PASSWORD`, and
//! captures stdout/stderr plus a binary/source identity string into every assertion
//! message.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_heliosdb-nano");

/// Generous: a debug build has to create a RocksDB-style data directory.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// Daemon harness (RAII: kill + reap on every path, including panic)
// ===========================================================================

struct Daemon {
    child: Child,
    http_port: u16,
    pg_port: u16,
    sock_dir: Option<TempDir>,
    _config_dir: TempDir,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    args: Vec<String>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Kill and reap unconditionally, even while unwinding from a failed
        // assertion, so no orphaned daemon survives a test run.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn http_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.http_port)
    }

    fn log_stdout(&self) -> String {
        std::fs::read_to_string(&self.stdout_path).unwrap_or_default()
    }

    fn log_stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn diagnostics(&self) -> String {
        format!(
            "\n--- daemon diagnostics [{}] ---\nargs: {:?}\nhttp: {}\nstdout:\n{}\nstderr:\n{}\n--- end ---",
            identity(),
            self.args,
            self.http_addr(),
            self.log_stdout(),
            self.log_stderr(),
        )
    }

    /// Poll `/health` until it answers the fixed status body, then require proof
    /// that this child owns the HTTP listening socket inode before returning.
    ///
    /// The health probe alone only proves *something* answers on the loopback
    /// address; the `/proc/<pid>/net/tcp*` ownership check proves the socket the
    /// port maps to is a descriptor held by this child, so every real HTTP test
    /// starts from owned-listener evidence.
    fn wait_http_ready(&mut self) {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("daemon exited during startup with {status}{}", self.diagnostics());
            }
            let health_ok = matches!(
                http_request(self.http_addr(), "GET", "/health", None, None, &[]),
                Ok(resp) if resp.status == 200
            );
            if health_ok && !listening_addrs(self.pid(), self.http_port).is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "HTTP listener never became ready with an owned socket inode{}",
                    self.diagnostics()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn pg_socket_path(&self) -> Option<PathBuf> {
        self.sock_dir
            .as_ref()
            .map(|dir| dir.path().join(format!(".s.PGSQL.{}", self.pg_port)))
    }
}

/// Spawn the daemon. `extra` is appended after the fixed port/HTTP args;
/// `with_socket_dir` adds a managed `--pg-socket-dir`.
fn spawn(extra: &[&str], http_port: u16, pg_port: u16, with_socket_dir: bool) -> Daemon {
    spawn_with_failure_limit(extra, http_port, pg_port, with_socket_dir, 100)
}

fn spawn_with_failure_limit(
    extra: &[&str],
    http_port: u16,
    pg_port: u16,
    with_socket_dir: bool,
    max_failures: u32,
) -> Daemon {
    let sock_dir = if with_socket_dir {
        Some(TempDir::new().expect("socket tempdir"))
    } else {
        None
    };
    let config_dir = TempDir::new().expect("config tempdir");
    let config_path = config_dir.path().join("nano-http-test.toml");
    std::fs::write(
        &config_path,
        &format!(
            "[api]\nhttp_auth_max_failures = {max_failures}\nhttp_auth_failure_window_secs = 60\nhttp_auth_max_tracked_peers = 4096\nhttp_auth_max_in_flight = 64\n"
        ),
    )
    .expect("HTTP test config");

    let mut args: Vec<String> = vec![
        "start".to_string(),
        "--memory".to_string(),
        "--port".to_string(),
        pg_port.to_string(),
        "--http-port".to_string(),
        http_port.to_string(),
        "--config".to_string(),
        config_path.display().to_string(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_string()));
    if let Some(ref dir) = sock_dir {
        args.push("--pg-socket-dir".to_string());
        args.push(dir.path().display().to_string());
    }

    let stdout_path = std::env::temp_dir().join(format!("heliosdb-http-{http_port}-{}.out", std::process::id()));
    let stderr_path = std::env::temp_dir().join(format!("heliosdb-http-{http_port}-{}.err", std::process::id()));
    let stdout = std::fs::File::create(&stdout_path).expect("stdout log");
    let stderr = std::fs::File::create(&stderr_path).expect("stderr log");

    let child = Command::new(BIN)
        .args(&args)
        // Clear inherited credentials so a developer's environment cannot change
        // what these tests exercise (or leak a secret into the child).
        .env_remove("HELIOSDB_PASSWORD")
        .env_remove("HELIOSDB_PASSWORD_FILE")
        .env_remove("HELIOSDB_MCP_TOKEN")
        .env_remove("MCP_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("the heliosdb-nano binary must be runnable");

    let mut daemon = Daemon {
        child,
        http_port,
        pg_port,
        sock_dir,
        _config_dir: config_dir,
        stdout_path,
        stderr_path,
        args,
    };
    daemon.wait_http_ready();
    daemon
}

/// Reserve a free loopback TCP port.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Binary + source identity, appended to every failure so a result can be tied to
/// an exact artifact.
fn identity() -> String {
    let mut id = format!("version={} binary={}", env!("CARGO_PKG_VERSION"), BIN);
    if let Ok(meta) = std::fs::metadata(BIN) {
        let modified = meta
            .modified()
            .map(|t| format!("{t:?}"))
            .unwrap_or_else(|_| "unknown".to_string());
        id.push_str(&format!(" len={} mtime={modified}", meta.len()));
    }
    if let Ok(out) = Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
        .output()
    {
        if out.status.success() {
            id.push_str(&format!(" commit={}", String::from_utf8_lossy(&out.stdout).trim()));
        }
    }
    id
}

// ===========================================================================
// Minimal blocking HTTP/1.1 client (no extra dependency)
// ===========================================================================

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| {
            panic!(
                "response was not JSON ({e}): status={} body={:?}",
                self.status, self.body
            )
        })
    }
}

fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> std::io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    // Default to `Connection: close` unless the caller supplied a Connection
    // header (e.g. `Upgrade` for the WebSocket handshake), which must win.
    if !extra_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("connection"))
    {
        request.push_str("Connection: close\r\n");
    }
    if let Some(auth) = auth {
        request.push_str(&format!("Authorization: {auth}\r\n"));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut deadline_reader = DeadlineReader {
        stream: &mut stream,
        deadline: Instant::now() + MAX_RESPONSE_TIME,
    };
    let response = read_response(&mut deadline_reader, method)?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(response)
}

// ---------------------------------------------------------------------------
// Framing-aware HTTP/1.1 response reader
//
// The daemon answers many routes with `Transfer-Encoding: chunked` and no
// `Content-Length`. A reader that treats a missing length as an empty body
// therefore stops at the end of the headers and hands the raw chunk framing
// (`6C\r\n{json}\r\n0\r\n\r\n`) to `serde_json`, which is the exact SCRAM
// failure this harness logged. The reader below de-frames each supported body
// kind and rejects malformed or truncated framing instead of silently
// succeeding on a partial read.
// ---------------------------------------------------------------------------

/// Bounds on what a single response may consume so a broken or hostile peer
/// cannot make the harness allocate without limit while reading the body.
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHUNK_LINE_BYTES: usize = 8 * 1024;
const MAX_RESPONSE_TIME: Duration = Duration::from_secs(30);

/// Cumulative cap on EVERY byte read for one response — headers, chunk-size
/// lines, chunk data, the CRLFs between them, and trailers. Per-piece limits
/// alone are not enough: an endless run of one-byte trailers (or of tiny
/// chunks) never crosses the head, line, or body bound, so a peer could keep
/// the reader looping forever. Charging every read against one response-wide
/// budget makes total framing consumption finite no matter how the bytes split.
const MAX_RESPONSE_BYTES: usize = MAX_HEAD_BYTES + MAX_BODY_BYTES + MAX_CHUNK_LINE_BYTES;

/// Applies one wall-clock deadline to the real socket. `TcpStream`'s ordinary
/// read timeout is refreshed for every read; this adapter recomputes the
/// remaining time before each read so fragmented framing cannot extend the
/// request forever.
struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "HTTP response deadline exceeded"))?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

/// A [`Read`] adapter that charges every byte it yields against one budget, so
/// no combination of headers, framing lines, data, CRLFs, or trailers can
/// outrun the cumulative response bound.
struct BudgetReader<R> {
    inner: R,
    remaining: usize,
    deadline: Instant,
}

impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP response exceeded the cumulative time limit",
            ));
        }
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP response exceeds the cumulative bounded limit",
            ));
        }
        let cap = buf.len().min(self.remaining);
        let read = self.inner.read(&mut buf[..cap])?;
        self.remaining -= read;
        Ok(read)
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// A `Transfer-Encoding` value is a comma-separated list whose tokens may carry
/// `;parameters`. This reader de-frames exactly one codec: a single `chunked`
/// coding, matched case-insensitively with its parameters ignored. Any other
/// value — a different coding, or a coding list that merely CONTAINS `chunked`
/// (`gzip, chunked`, `chunked, gzip`) — is not something this reader can decode,
/// so it must be rejected rather than de-chunked and mistaken for the body.
fn transfer_encoding_is_chunked(value: &str) -> bool {
    let mut codings = value.split(',');
    let only = codings.next().unwrap_or("");
    if codings.next().is_some() {
        return false;
    }
    only.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("chunked")
}

/// Read exactly the response head, terminating at `\r\n\r\n`. The scan is
/// byte-by-byte, so a fragmented stream is handled; a peer that closes or
/// overruns before the head completes is a hard error, never a partial success.
fn read_head<R: Read>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP response headers exceed the bounded limit",
            ));
        }
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before the HTTP response headers completed",
                ))
            }
            Ok(_) => head.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(head)
}

fn parse_head(head: &[u8]) -> (u16, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    (status, headers)
}

/// Read a CRLF-terminated line, rejecting EOF before the terminator (truncation)
/// and absurdly long framing lines.
fn read_crlf_line<R: Read>(reader: &mut R) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed within chunked framing",
                ))
            }
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() > MAX_CHUNK_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "chunked framing line exceeds the bounded limit",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    if !line.ends_with(b"\r\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chunked framing line is not CRLF-terminated",
        ));
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunked framing line is not valid UTF-8"))
}

/// Decode a `Transfer-Encoding: chunked` body: hex sizes (optionally carrying
/// `;extensions`), a CRLF after each chunk's data, and a zero-size chunk followed
/// by optional trailer fields and a terminal blank line. Every truncation point
/// is a typed error so malformed framing cannot be mistaken for a complete body.
fn read_chunked_body<R: Read>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_crlf_line(reader)?;
        let size_token = line.split(';').next().unwrap_or("").trim();
        if size_token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk size is missing before its extension",
            ));
        }
        let size = usize::from_str_radix(size_token, 16).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid hexadecimal chunk size {size_token:?}"),
            )
        })?;
        if size == 0 {
            // Consume any trailer fields up to the terminating blank line.
            loop {
                let trailer = read_crlf_line(reader)?;
                if trailer.is_empty() {
                    return Ok(body);
                }
            }
        }
        if body
            .len()
            .checked_add(size)
            .map_or(true, |total| total > MAX_BODY_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked response body exceeds the bounded limit",
            ));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        if &crlf != b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk data is not followed by CRLF",
            ));
        }
    }
}

fn read_body_exact<R: Read>(reader: &mut R, len: usize) -> std::io::Result<Vec<u8>> {
    if len > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "declared Content-Length exceeds the bounded limit",
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(body)
}

fn read_body_to_end<R: Read>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(body),
            Ok(n) => {
                if body.len() + n > MAX_BODY_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "response body exceeds the bounded limit",
                    ));
                }
                body.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Read one HTTP/1.1 response, de-framing `Content-Length`,
/// `Transfer-Encoding: chunked`, and close-delimited bodies. Statuses that carry
/// no body by definition (1xx, 204, 304) and HEAD responses read no body, so the
/// WebSocket `101` upgrade does not block waiting for bytes that never arrive.
fn read_response<R: Read>(reader: &mut R, method: &str) -> std::io::Result<HttpResponse> {
    // Every byte of the head, framing, data, and trailers is charged against one
    // cumulative budget before it reaches the piece-specific readers.
    let mut reader = BudgetReader {
        inner: reader,
        remaining: MAX_RESPONSE_BYTES,
        deadline: Instant::now() + MAX_RESPONSE_TIME,
    };
    let head = read_head(&mut reader)?;
    let (status, headers) = parse_head(&head);

    let transfer_values: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .map(|(_, value)| value.as_str())
        .collect();
    let content_lengths: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim())
        .collect();
    if transfer_values.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate Transfer-Encoding headers are rejected",
        ));
    }
    if transfer_values.len() == 1 && !content_lengths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Transfer-Encoding and Content-Length cannot both frame a response",
        ));
    }
    if content_lengths.len() > 1 && content_lengths.windows(2).any(|pair| pair[0] != pair[1]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "conflicting Content-Length headers are rejected",
        ));
    }

    let body = if method.eq_ignore_ascii_case("HEAD") || (100..200).contains(&status) || status == 204 || status == 304
    {
        Vec::new()
    } else if let Some(encoding) = header_value(&headers, "transfer-encoding") {
        if transfer_encoding_is_chunked(encoding) {
            read_chunked_body(&mut reader)?
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported Transfer-Encoding {encoding:?}; only a single chunked coding is decoded"),
            ));
        }
    } else if let Some(len) = header_value(&headers, "content-length") {
        let len: usize = len.trim().parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Content-Length value {len:?}"),
            )
        })?;
        read_body_exact(&mut reader, len)?
    } else {
        read_body_to_end(&mut reader)?
    };

    Ok(HttpResponse {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

// ---------------------------------------------------------------------------
// Pure framing regressions (no daemon, no sockets)
// ---------------------------------------------------------------------------

/// The exact chunked-body shape from the logged SCRAM failure: a `200` response
/// with no `Content-Length`, framed `6C\r\n{json}\r\n0\r\n\r\n`. The de-framer
/// must strip the framing, not hand `6C\r\n...` to `serde_json`.
const LOGGED_JSON_BODY: &str =
    r#"{"columns":["column_0"],"column_types":["int4"],"rows":[{"column_0":1}],"row_count":1,"execution_time_ms":1}"#;

/// Frame `body` as a single chunked response with no `Content-Length`.
fn chunked_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n0\r\n\r\n",
        body.len(),
        body
    )
    .into_bytes()
}

/// Yields its bytes in fixed-size fragments so a single logical frame arrives
/// split across many `read` calls.
struct FragmentedReader {
    data: Vec<u8>,
    cursor: usize,
    fragment: usize,
}

impl Read for FragmentedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cursor >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        let n = self.fragment.min(buf.len()).min(self.data.len() - self.cursor);
        buf[..n].copy_from_slice(&self.data[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

#[test]
fn chunked_without_content_length_is_decoded_before_json() {
    assert_eq!(
        LOGGED_JSON_BODY.len(),
        0x6C,
        "fixture must reproduce the logged 0x6C chunk size"
    );
    let raw = chunked_response(LOGGED_JSON_BODY);
    // Sanity: the wire really has no Content-Length and does carry 6C framing.
    assert!(!raw.windows(7).any(|w| w.eq_ignore_ascii_case(b"content")));
    assert!(raw.windows(4).any(|w| w == b"6C\r\n"));

    let resp = read_response(&mut &raw[..], "POST").expect("well-formed chunked response must parse");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body, LOGGED_JSON_BODY,
        "chunk framing must be stripped and the JSON body preserved"
    );
    let json = resp.json();
    assert_eq!(json["row_count"], 1);
    assert_eq!(json["rows"][0]["column_0"], 1);
}

#[test]
fn chunked_body_decodes_across_fragmented_reads() {
    for fragment in [1usize, 3, 7, 64] {
        let mut reader = FragmentedReader {
            data: chunked_response(LOGGED_JSON_BODY),
            cursor: 0,
            fragment,
        };
        let resp = read_response(&mut reader, "POST")
            .unwrap_or_else(|e| panic!("{fragment}-byte fragments must de-frame: {e}"));
        assert_eq!(resp.body, LOGGED_JSON_BODY, "fragment size {fragment}");
    }
}

#[test]
fn transfer_encoding_match_is_case_insensitive() {
    let raw = format!(
        "HTTP/1.1 200 OK\r\ntRaNsFeR-EnCoDiNg: CHUNKED\r\n\r\n{:X}\r\n{}\r\n0\r\n\r\n",
        LOGGED_JSON_BODY.len(),
        LOGGED_JSON_BODY
    );
    let resp = read_response(&mut raw.as_bytes(), "POST").expect("case-folded header must match");
    assert_eq!(resp.body, LOGGED_JSON_BODY);
}

#[test]
fn chunk_size_extensions_are_ignored() {
    let raw = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:X};name=value;flag\r\n{}\r\n0\r\n\r\n",
        LOGGED_JSON_BODY.len(),
        LOGGED_JSON_BODY
    );
    let resp = read_response(&mut raw.as_bytes(), "POST").expect("chunk extensions must be ignored");
    assert_eq!(resp.body, LOGGED_JSON_BODY);
}

#[test]
fn chunked_trailers_are_consumed() {
    let raw = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n0\r\nX-Checksum: abc123\r\nX-Trailer: done\r\n\r\n",
        LOGGED_JSON_BODY.len(),
        LOGGED_JSON_BODY
    );
    let resp = read_response(&mut raw.as_bytes(), "POST").expect("trailers must be consumed");
    assert_eq!(
        resp.body, LOGGED_JSON_BODY,
        "trailer fields must not leak into the decoded body"
    );
}

#[test]
fn transfer_encoding_must_be_a_single_chunked_coding() {
    let framing = format!("{:X}\r\n{}\r\n0\r\n\r\n", LOGGED_JSON_BODY.len(), LOGGED_JSON_BODY);
    for (label, te) in [
        ("a different coding", "gzip"),
        ("chunked stacked after another coding", "gzip, chunked"),
        ("chunked stacked before another coding", "chunked, gzip"),
        ("chunked listed twice", "chunked, chunked"),
        ("an empty trailing list element", "chunked,"),
    ] {
        let raw = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: {te}\r\n\r\n{framing}");
        assert!(
            read_response(&mut raw.as_bytes(), "POST").is_err(),
            "an unsupported Transfer-Encoding must be rejected, not decoded: {label} ({te:?})"
        );
    }

    // Positive control: a lone chunked coding, with optional parameters, decodes.
    let raw = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: CHUNKED;q=1\r\n\r\n{framing}");
    let resp = read_response(&mut raw.as_bytes(), "POST").expect("a lone chunked coding must decode");
    assert_eq!(resp.body, LOGGED_JSON_BODY);
}

#[test]
fn duplicate_or_conflicting_response_framing_is_rejected() {
    let framing = format!("{:X}\r\n{}\r\n0\r\n\r\n", LOGGED_JSON_BODY.len(), LOGGED_JSON_BODY);
    for raw in [
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n{framing}"),
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: {}\r\n\r\n{framing}",
            LOGGED_JSON_BODY.len()
        ),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n{}",
            LOGGED_JSON_BODY
        ),
    ] {
        assert!(
            read_response(&mut raw.as_bytes(), "POST").is_err(),
            "ambiguous response framing must be rejected"
        );
    }
}

/// A peer that streams endless one-byte trailer fields must never keep the
/// reader looping; the cumulative response budget has to terminate it.
#[test]
fn endless_short_trailers_are_bounded_by_the_cumulative_limit() {
    struct EndlessTrailers {
        head: Vec<u8>,
        cursor: usize,
        pattern: usize,
    }
    impl Read for EndlessTrailers {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let mut written = 0;
            while written < buf.len() && self.cursor < self.head.len() {
                buf[written] = self.head[self.cursor];
                self.cursor += 1;
                written += 1;
            }
            let pattern = b"x\r\n";
            while written < buf.len() {
                buf[written] = pattern[self.pattern];
                self.pattern = (self.pattern + 1) % pattern.len();
                written += 1;
            }
            Ok(written)
        }
    }

    let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_vec();
    let mut reader = EndlessTrailers {
        head,
        cursor: 0,
        pattern: 0,
    };
    let outcome = read_response(&mut reader, "POST");
    assert!(
        matches!(outcome, Err(ref e) if e.kind() == io::ErrorKind::InvalidData),
        "endless one-byte trailers must be stopped by the cumulative budget"
    );
}

#[test]
fn truncated_and_malformed_chunk_framing_is_rejected() {
    let len = LOGGED_JSON_BODY.len();
    let cases: Vec<(&str, String)> = vec![
        (
            "chunk-size line never terminated",
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{len:X}"),
        ),
        (
            "chunk data shorter than declared",
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{len:X}\r\n{}",
                &LOGGED_JSON_BODY[..len - 4]
            ),
        ),
        (
            "missing CRLF after chunk data",
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{len:X}\r\n{LOGGED_JSON_BODY}"),
        ),
        (
            "zero chunk without terminal blank line",
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{len:X}\r\n{LOGGED_JSON_BODY}\r\n0\r\n"),
        ),
        (
            "non-hexadecimal chunk size",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZ\r\nbogus\r\n0\r\n\r\n".to_string(),
        ),
    ];
    for (label, raw) in cases {
        assert!(
            read_response(&mut raw.as_bytes(), "POST").is_err(),
            "truncated/malformed framing must be rejected: {label}"
        );
    }
}

#[test]
fn content_length_body_is_read_exactly() {
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        LOGGED_JSON_BODY.len(),
        LOGGED_JSON_BODY
    );
    let resp = read_response(&mut raw.as_bytes(), "POST").expect("content-length body must parse");
    assert_eq!(resp.body, LOGGED_JSON_BODY);
}

#[test]
fn bodiless_status_does_not_wait_for_a_body() {
    let raw = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
    let resp = read_response(&mut &raw[..], "GET").expect("101 must parse with no body");
    assert_eq!(resp.status, 101);
    assert!(resp.body.is_empty(), "101 must not consume a body");
}

fn basic(username: &str, password: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

fn status_of(daemon: &Daemon, method: &str, path: &str, auth: Option<&str>, body: Option<&str>) -> u16 {
    http_request(daemon.http_addr(), method, path, auth, body, &[])
        .unwrap_or_else(|e| panic!("{method} {path} failed: {e}{}", daemon.diagnostics()))
        .status
}

fn body_of(daemon: &Daemon, method: &str, path: &str, auth: Option<&str>, body: Option<&str>) -> HttpResponse {
    http_request(daemon.http_addr(), method, path, auth, body, &[])
        .unwrap_or_else(|e| panic!("{method} {path} failed: {e}{}", daemon.diagnostics()))
}

// ===========================================================================
// /proc proof that the HTTP listener is bound to loopback
// ===========================================================================

/// Parse one `/proc/<pid>/net/tcp*` body, returning the local addresses of
/// LISTEN entries for `port` whose socket inode is in `owned_inodes`.
///
/// The kernel writes these columns as whitespace-separated fields, header
/// `sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid
/// timeout inode`; after `split_whitespace` (0-based) `fields[7]` is `uid`,
/// `fields[8]` is `timeout`, and `fields[9]` is the kernel's `inode` column —
/// the field matched against the child-owned descriptor set. `fields[3]` is the
/// socket state, `0A` == LISTEN.
///
/// ```text
/// sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
/// 0:  0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 ...
/// ```
fn parse_listening_addrs(content: &str, port: u16, owned_inodes: &std::collections::HashSet<String>) -> Vec<String> {
    let mut found = Vec::new();
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Need at least through the inode column (fields[9]); st "0A" == LISTEN.
        if fields.len() < 10 || fields[3] != "0A" || !owned_inodes.contains(fields[9]) {
            continue;
        }
        let local = fields[1];
        let Some((addr_hex, port_hex)) = local.split_once(':') else {
            continue;
        };
        if u16::from_str_radix(port_hex, 16) != Ok(port) {
            continue;
        }
        found.push(addr_hex.to_string());
    }
    found
}

/// Listening local addresses for `port`, read from the child's own network
/// namespace via `/proc/<pid>/net/tcp` and `tcp6`. The inode check ties the
/// entry to a descriptor owned by this child; `/proc/<pid>/net/*` is namespace
/// wide and can otherwise report an unrelated listener.
fn listening_addrs(pid: u32, port: u16) -> Vec<String> {
    let mut owned_inodes = std::collections::HashSet::new();
    if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for entry in entries.flatten() {
            if let Ok(link) = std::fs::read_link(entry.path()) {
                let text = link.to_string_lossy();
                if let Some(inode) = text.strip_prefix("socket:[").and_then(|v| v.strip_suffix(']')) {
                    owned_inodes.insert(inode.to_string());
                }
            }
        }
    }
    let mut found = Vec::new();
    for file in ["tcp", "tcp6"] {
        let path = format!("/proc/{pid}/net/{file}");
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        found.extend(parse_listening_addrs(&content, port, &owned_inodes));
    }
    found
}

/// Fixture: prove the socket inode is read from the expected `/proc/net/tcp`
/// column, not some other numeric field.
#[test]
fn proc_tcp_parser_reads_inode_from_field_nine() {
    // A real Linux row shape: fields[7] = uid, fields[8] = timeout (0),
    // fields[9] = inode. Local port 0x1F90 == 8080.
    let row = "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 \
               00:00000000 00000000  1000 0 4242 1 0000000000000000 100 0 0 10 0";
    let content = format!(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  \
         timeout inode\n{row}\n"
    );

    let mut owned = std::collections::HashSet::new();
    owned.insert("4242".to_string());
    assert_eq!(
        parse_listening_addrs(&content, 8080, &owned),
        vec!["0100007F".to_string()],
        "the LISTEN socket inode and loopback address are parsed together"
    );

    let mut other = std::collections::HashSet::new();
    other.insert("9999".to_string());
    assert!(
        parse_listening_addrs(&content, 8080, &other).is_empty(),
        "ownership must key on the index-9 inode field, not uid/timeout/other columns"
    );
}

/// Decode a little-endian `/proc/net/tcp` IPv4 hex address.
fn decode_proc_ipv4(hex: &str) -> Option<Ipv4Addr> {
    if hex.len() != 8 {
        return None;
    }
    let bytes: Vec<u8> = (0..4)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(Ipv4Addr::new(bytes[3], bytes[2], bytes[1], bytes[0]))
}

/// The `::1` encoding used by `/proc/net/tcp6`.
const PROC_IPV6_LOOPBACK: &str = "00000000000000000000000001000000";

fn is_loopback_proc_addr(hex: &str) -> bool {
    match hex.len() {
        8 => decode_proc_ipv4(hex).map(|ip| ip.is_loopback()).unwrap_or(false),
        32 => hex == PROC_IPV6_LOOPBACK,
        _ => false,
    }
}

/// A routable local IPv4, used only for the optional refusal probe.
fn primary_non_loopback_ip() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    // No packet is sent by `connect` on a UDP socket; it only selects a route.
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

// ===========================================================================
// 1. The exact P0 bypass matrix over all password methods
// ===========================================================================

const PASSWORD: &str = "subprocess-secret";

/// Drive the full Basic matrix against the exact branch-query route and assert the
/// exact SELECT payload plus a write/read side effect.
fn assert_branch_query_matrix(daemon: &Daemon) {
    let query_path = "/v1/branches/main/query";
    let select_body = r#"{"sql":"SELECT 1 AS answer"}"#;

    // Absent credentials: 401 with a challenge.
    {
        let resp = body_of(daemon, "POST", query_path, None, Some(select_body));
        assert_eq!(
            resp.status,
            401,
            "absent credentials must be 401{}",
            daemon.diagnostics()
        );
        assert!(
            resp.header("www-authenticate").is_some(),
            "401 must carry WWW-Authenticate{}",
            daemon.diagnostics()
        );
    }

    // Wrong password: 401.
    assert_eq!(
        status_of(
            daemon,
            "POST",
            query_path,
            Some(&basic("postgres", "definitely-wrong")),
            Some(select_body)
        ),
        401,
        "wrong password must be 401{}",
        daemon.diagnostics()
    );

    // Unknown user with the right password: 401 (fail closed, no "any username").
    assert_eq!(
        status_of(
            daemon,
            "POST",
            query_path,
            Some(&basic("nobody", PASSWORD)),
            Some(select_body)
        ),
        401,
        "unknown user must be 401{}",
        daemon.diagnostics()
    );

    // Valid credentials: 200 with the EXACT row.
    let authed = body_of(
        daemon,
        "POST",
        query_path,
        Some(&basic("postgres", PASSWORD)),
        Some(select_body),
    );
    assert_eq!(
        authed.status,
        200,
        "valid credentials must reach the handler{}",
        daemon.diagnostics()
    );
    let json = authed.json();
    assert_eq!(json["row_count"], 1, "SELECT 1 AS answer must return one row: {json}");
    assert_eq!(
        json["rows"][0]["column_0"], 1,
        "the exact SELECT result must be column_0 = 1: {json}"
    );

    // Write then read, through the real endpoints, proving a side effect.
    let auth = basic("postgres", PASSWORD);
    for (path, sql) in [
        (
            "/v1/branches/main/execute",
            "CREATE TABLE http_auth_probe (id INT, name TEXT)",
        ),
        (
            "/v1/branches/main/execute",
            "INSERT INTO http_auth_probe (id, name) VALUES (7, 'alpha')",
        ),
    ] {
        let resp = body_of(
            daemon,
            "POST",
            path,
            Some(&auth),
            Some(&format!(r#"{{"sql":"{sql}"}}"#)),
        );
        assert_eq!(
            resp.status,
            200,
            "authenticated {} must succeed: {}{}",
            sql,
            resp.body,
            daemon.diagnostics()
        );
    }

    let read = body_of(
        daemon,
        "POST",
        query_path,
        Some(&auth),
        Some(r#"{"sql":"SELECT id, name FROM http_auth_probe"}"#),
    );
    assert_eq!(
        read.status,
        200,
        "reading the probe row must succeed{}",
        daemon.diagnostics()
    );
    let json = read.json();
    assert_eq!(json["row_count"], 1, "the inserted row must be visible: {json}");
    assert_eq!(json["rows"][0]["column_0"], 7, "id side effect must round-trip: {json}");
    assert_eq!(
        json["rows"][0]["column_1"], "alpha",
        "name side effect must round-trip: {json}"
    );
}

/// Every non-health surface must be closed without credentials and reach its
/// concrete success state (or a concrete service-level status) with them.
fn assert_surface_matrix(daemon: &Daemon) {
    let auth = basic("postgres", PASSWORD);

    // Absent credentials: every listed surface is 401.
    for (method, path, body) in [
        ("GET", "/version", None),
        ("GET", "/docs", None),
        ("GET", "/openapi.json", None),
        ("GET", "/rest/v1/http_auth_probe", None),
        (
            "POST",
            "/auth/v1/signup",
            Some(r#"{"email":"x@example.com","password":"pw"}"#),
        ),
    ] {
        assert_eq!(
            status_of(daemon, method, path, None, body),
            401,
            "{method} {path} must require credentials{}",
            daemon.diagnostics()
        );
    }

    // Health is reachable unauthenticated and is status-ONLY.
    let health = body_of(daemon, "GET", "/health", None, None);
    assert_eq!(
        health.status,
        200,
        "health must be a public 200{}",
        daemon.diagnostics()
    );
    let health_json = health.json();
    assert_eq!(health_json["status"], "ok");
    assert_eq!(
        health_json.as_object().map(|o| o.len()),
        Some(1),
        "health must be status-only, never version/branch data: {health_json}"
    );
    assert!(
        health_json.get("version").is_none(),
        "health must not leak a version: {health_json}"
    );

    // Concrete successes with the wire credential.
    for path in ["/version", "/docs", "/openapi.json"] {
        assert_eq!(
            status_of(daemon, "GET", path, Some(&auth), None),
            200,
            "GET {path} must return 200 with the wire credential{}",
            daemon.diagnostics()
        );
    }

    // REST: the table created by the branch-query matrix is listable (200).
    assert_eq!(
        status_of(daemon, "GET", "/rest/v1/http_auth_probe", Some(&auth), None),
        200,
        "authenticated REST select must return 200{}",
        daemon.diagnostics()
    );

    // Auth: a fresh signup authenticates and returns a session (200).
    let email = format!("http-auth-{}@example.com", std::process::id());
    let signup = body_of(
        daemon,
        "POST",
        "/auth/v1/signup",
        Some(&auth),
        Some(&format!(
            r#"{{"email":"{email}","password":"correct-horse-battery-staple"}}"#
        )),
    );
    assert_eq!(
        signup.status,
        200,
        "authenticated signup must return 200: {}{}",
        signup.body,
        daemon.diagnostics()
    );
    let signup_json = signup.json();
    assert_eq!(
        signup_json["user"]["email"], email,
        "signup must return the created email"
    );
    let signup_id = signup_json["user"]["id"]
        .as_str()
        .expect("signup must return a user id")
        .to_string();
    let app_token = signup_json["access_token"]
        .as_str()
        .expect("signup must return an access_token")
        .to_string();
    let user = http_request(
        daemon.http_addr(),
        "GET",
        "/auth/v1/user",
        Some(&format!("Bearer {app_token}")),
        None,
        &[("X-HeliosDB-Authorization", &auth)],
    )
    .unwrap_or_else(|e| panic!("dual-header user lookup failed: {e}{}", daemon.diagnostics()));
    assert_eq!(
        user.status,
        200,
        "dual-header app JWT flow must return 200{}",
        daemon.diagnostics()
    );
    let user_json = user.json();
    assert_eq!(user_json["email"], email, "user lookup must return signup email");
    assert_eq!(user_json["id"], signup_id, "user lookup must return signup id");
    let app_only = status_of(
        daemon,
        "GET",
        "/auth/v1/user",
        Some(&format!("Bearer {app_token}")),
        None,
    );
    assert_eq!(
        app_only,
        401,
        "app JWT alone must not bypass outer auth{}",
        daemon.diagnostics()
    );
    let wrong_app = http_request(
        daemon.http_addr(),
        "GET",
        "/auth/v1/user",
        Some("Bearer definitely-not-a-valid-app-jwt"),
        None,
        &[("X-HeliosDB-Authorization", &auth)],
    )
    .unwrap_or_else(|e| panic!("wrong app JWT request failed: {e}{}", daemon.diagnostics()));
    assert_eq!(
        wrong_app.status,
        401,
        "valid outer auth must not accept a wrong app JWT{}",
        daemon.diagnostics()
    );
    let invalid_outer = http_request(
        daemon.http_addr(),
        "GET",
        "/auth/v1/user",
        Some(&format!("Bearer {app_token}")),
        None,
        &[("X-HeliosDB-Authorization", "Basic malformed")],
    )
    .unwrap_or_else(|e| panic!("invalid dedicated header request failed: {e}{}", daemon.diagnostics()));
    assert_eq!(
        invalid_outer.status,
        401,
        "invalid dedicated header must not fall back{}",
        daemon.diagnostics()
    );

    // Realtime WebSocket: absent credentials 401; valid credentials complete the
    // real HTTP upgrade (101), proving the route is behind the policy but usable.
    let ws_path = "/realtime/v1/websocket";
    assert_eq!(
        status_of(daemon, "GET", ws_path, None, None),
        401,
        "the websocket route must require credentials{}",
        daemon.diagnostics()
    );
    let upgrade_headers: &[(&str, &str)] = &[
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];
    let upgrade = http_request(daemon.http_addr(), "GET", ws_path, Some(&auth), None, upgrade_headers)
        .unwrap_or_else(|e| panic!("websocket upgrade failed: {e}{}", daemon.diagnostics()));
    assert_eq!(
        upgrade.status,
        101,
        "authenticated websocket upgrade must be 101{}",
        daemon.diagnostics()
    );
}

#[test]
fn scram_http_matrix_is_authenticated_and_functional() {
    let http_port = free_port();
    let pg_port = free_port();
    let daemon = spawn(
        &["--auth", "scram-sha-256", "--password", PASSWORD],
        http_port,
        pg_port,
        false,
    );
    assert_branch_query_matrix(&daemon);
    assert_surface_matrix(&daemon);
}

#[test]
fn cleartext_password_http_matrix_is_authenticated() {
    let daemon = spawn(
        &["--auth", "password", "--password", PASSWORD],
        free_port(),
        free_port(),
        false,
    );
    assert_eq!(
        status_of(
            &daemon,
            "POST",
            "/v1/branches/main/query",
            None,
            Some(r#"{"sql":"SELECT 1"}"#)
        ),
        401,
        "cleartext method must still require the wire credential{}",
        daemon.diagnostics()
    );
    assert_eq!(
        status_of(
            &daemon,
            "POST",
            "/v1/branches/main/query",
            Some(&basic("postgres", PASSWORD)),
            Some(r#"{"sql":"SELECT 1 AS answer"}"#)
        ),
        200,
        "cleartext method must accept the wire credential{}",
        daemon.diagnostics()
    );
}

#[test]
fn md5_http_matrix_is_authenticated() {
    let daemon = spawn(
        &["--auth", "md5", "--password", PASSWORD],
        free_port(),
        free_port(),
        false,
    );
    assert_eq!(
        status_of(
            &daemon,
            "POST",
            "/v1/branches/main/query",
            Some(&basic("postgres", "wrong")),
            Some(r#"{"sql":"SELECT 1"}"#)
        ),
        401,
        "md5 method must reject a wrong password{}",
        daemon.diagnostics()
    );
    assert_eq!(
        status_of(
            &daemon,
            "POST",
            "/v1/branches/main/query",
            Some(&basic("postgres", PASSWORD)),
            Some(r#"{"sql":"SELECT 1 AS answer"}"#)
        ),
        200,
        "md5 method must accept the wire credential{}",
        daemon.diagnostics()
    );
}

#[test]
fn trust_wire_with_password_still_protects_http() {
    // Wire is trust (for local/dev), but an explicit password protects HTTP.
    let daemon = spawn(
        &["--auth", "trust", "--password", PASSWORD],
        free_port(),
        free_port(),
        false,
    );
    assert_eq!(
        status_of(&daemon, "GET", "/version", None, None),
        401,
        "trust+password must still protect HTTP{}",
        daemon.diagnostics()
    );
    assert_eq!(
        status_of(&daemon, "GET", "/version", Some(&basic("postgres", PASSWORD)), None),
        200,
        "trust+password must accept the explicit HTTP password{}",
        daemon.diagnostics()
    );
}

#[cfg(feature = "mcp-endpoint")]
#[test]
fn mcp_http_is_gated_by_the_wire_policy_and_its_own_bearer() {
    const TOKEN: &str = "td8-mcp-secret";
    let daemon = spawn(
        &["--auth", "scram-sha-256", "--password", PASSWORD, "--mcp-token", TOKEN],
        free_port(),
        free_port(),
        false,
    );
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;

    // No credential at all: refused by the wire layer.
    assert_eq!(
        status_of(&daemon, "POST", "/mcp", None, Some(body)),
        401,
        "MCP without credentials must be 401{}",
        daemon.diagnostics()
    );

    // Basic passes the wire layer but not MCP's own bearer check.
    assert_eq!(
        status_of(&daemon, "POST", "/mcp", Some(&basic("postgres", PASSWORD)), Some(body)),
        401,
        "Basic alone must not satisfy MCP's bearer check{}",
        daemon.diagnostics()
    );

    // The shared bearer token satisfies both layers and returns tools.
    let resp = body_of(&daemon, "POST", "/mcp", Some(&format!("Bearer {TOKEN}")), Some(body));
    assert_eq!(
        resp.status,
        200,
        "the bearer token must reach MCP{}",
        daemon.diagnostics()
    );
    let json = resp.json();
    assert!(
        json["result"]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "MCP tools/list must return tools: {json}"
    );
}

// ===========================================================================
// 2. Default bind is loopback, proven from the child's own /proc
// ===========================================================================

#[test]
fn default_http_bind_is_loopback_read_from_proc() {
    // The PostgreSQL listener is deliberately on all interfaces while
    // `--http-listen` is left at its default; the HTTP default must NOT inherit
    // `--listen`.
    let daemon = spawn(
        &["--listen", "0.0.0.0", "--auth", "scram-sha-256", "--password", PASSWORD],
        free_port(),
        free_port(),
        false,
    );

    let listeners = listening_addrs(daemon.pid(), daemon.http_port);
    assert!(
        !listeners.is_empty(),
        "no LISTEN socket for HTTP port {} in /proc/{}/net/tcp*: {}",
        daemon.http_port,
        daemon.pid(),
        daemon.diagnostics()
    );
    for addr in &listeners {
        assert!(
            is_loopback_proc_addr(addr),
            "HTTP port {} is bound to non-loopback /proc address {addr}; the default \
             --http-listen is loopback and must not follow --listen{}",
            daemon.http_port,
            daemon.diagnostics()
        );
    }

    // Positive control: a loopback connection actually serves health.
    assert_eq!(
        status_of(&daemon, "GET", "/health", None, None),
        200,
        "the loopback HTTP listener must serve health{}",
        daemon.diagnostics()
    );

    // Optional refusal probe: if this host has a routable address, that address on
    // the same port must be refused (an `0.0.0.0` bind would accept here).
    if let Some(ip) = primary_non_loopback_ip() {
        let target = SocketAddr::new(IpAddr::V4(ip), daemon.http_port);
        let reached = TcpStream::connect_timeout(&target, Duration::from_secs(1)).is_ok();
        assert!(
            !reached,
            "HTTP listener accepted a connection on non-loopback {target}{}",
            daemon.diagnostics()
        );
    }
}

#[test]
fn default_live_failure_budget_returns_retry_after_without_blocking_health() {
    let daemon = spawn_with_failure_limit(
        &["--auth", "scram-sha-256", "--password", PASSWORD],
        free_port(),
        free_port(),
        false,
        10,
    );
    for _ in 0..10 {
        assert_eq!(
            status_of(
                &daemon,
                "GET",
                "/version",
                Some(&basic("postgres", "wrong-password")),
                None,
            ),
            401,
            "the first ten failed attempts should be unauthorized{}",
            daemon.diagnostics()
        );
    }
    let locked = body_of(
        &daemon,
        "GET",
        "/version",
        Some(&basic("postgres", "wrong-password")),
        None,
    );
    assert_eq!(
        locked.status,
        429,
        "the next attempt must be rate limited{}",
        daemon.diagnostics()
    );
    assert_eq!(locked.header("retry-after"), Some("60"));
    assert_eq!(status_of(&daemon, "GET", "/health", None, None), 200);
}

// ===========================================================================
// 3. Fail-closed startup cases
// ===========================================================================

/// Run the daemon expecting a non-zero exit with `needle` in its output.
fn assert_startup_refused(args: &[&str], needle: &str) {
    let stdout_path = std::env::temp_dir().join(format!("heliosdb-refuse-{}-{}.out", std::process::id(), free_port()));
    let stderr_path = stdout_path.with_extension("err");
    let stdout = std::fs::File::create(&stdout_path).expect("stdout log");
    let stderr = std::fs::File::create(&stderr_path).expect("stderr log");

    let mut child = Command::new(BIN)
        .args(args)
        .env_remove("HELIOSDB_PASSWORD")
        .env_remove("HELIOSDB_PASSWORD_FILE")
        .env_remove("HELIOSDB_MCP_TOKEN")
        .env_remove("MCP_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn heliosdb-nano");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("daemon did not exit within 30s for args {args:?}");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };

    let mut output = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    output.push_str(&std::fs::read_to_string(&stderr_path).unwrap_or_default());
    assert!(
        !status.success(),
        "daemon must exit non-zero for {args:?}, output:\n{output}"
    );
    assert!(
        output.contains(needle),
        "output must contain {needle:?} for {args:?}, output:\n{output}"
    );
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
}

fn assert_refused(args: &[&str], needle: &str) {
    assert_startup_refused(args, needle);
}

#[test]
fn trust_mode_remote_http_bind_is_refused_at_startup() {
    assert_refused(
        &[
            "start",
            "--memory",
            "--auth",
            "trust",
            "--port",
            "0",
            "--http-port",
            &free_port().to_string(),
            "--http-listen",
            "0.0.0.0",
        ],
        "trust mode",
    );
}

#[test]
fn authenticated_remote_http_bind_is_refused_even_with_allow_insecure() {
    assert_refused(
        &[
            "start",
            "--memory",
            "--auth",
            "scram-sha-256",
            "--password",
            PASSWORD,
            "--port",
            "0",
            "--http-port",
            &free_port().to_string(),
            "--http-listen",
            "0.0.0.0",
            "--allow-insecure-http",
        ],
        "refusing to expose authenticated HTTP over plaintext",
    );
}

#[test]
fn mysql_tcp_with_authenticated_postgres_fails_closed() {
    assert_refused(
        &[
            "start",
            "--memory",
            "--auth",
            "scram-sha-256",
            "--password",
            PASSWORD,
            "--port",
            "0",
            "--http-port",
            "0",
            "--mysql",
        ],
        "MySQL authentication is not implemented",
    );
}

#[test]
fn mysql_uds_with_authenticated_postgres_fails_closed() {
    let sock = std::env::temp_dir().join(format!("heliosdb-mysql-{}.sock", std::process::id()));
    assert_refused(
        &[
            "start",
            "--memory",
            "--auth",
            "scram-sha-256",
            "--password",
            PASSWORD,
            "--port",
            "0",
            "--http-port",
            "0",
            "--mysql-socket",
            sock.to_str().expect("utf8 socket path"),
        ],
        "MySQL authentication is not implemented",
    );
}

// ===========================================================================
// 4. PostgreSQL Unix socket does not bypass the wire credential
// ===========================================================================

#[cfg(unix)]
mod uds_auth {
    use super::*;
    use std::path::Path;

    /// Wait for the daemon's PostgreSQL Unix socket to appear.
    fn wait_for_socket(daemon: &mut Daemon, path: &Path) {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(status)) = daemon.child.try_wait() {
                panic!("daemon exited during startup with {status}{}", daemon.diagnostics());
            }
            if path.exists() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("the PostgreSQL Unix socket never appeared{}", daemon.diagnostics());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn connect_uds(path: &Path, port: u16, user: &str, password: Option<&str>) -> Result<(), String> {
        // `tokio-postgres` parses a `host=/dir` connection string into a Unix
        // socket path (`<dir>/.s.PGSQL.<port>`), which is exactly what the
        // daemon's `--pg-socket-dir` listener serves.
        let mut conn = format!(
            "host={} port={} user={user} dbname=postgres",
            path.parent().expect("socket parent").display(),
            port
        );
        if let Some(password) = password {
            conn.push_str(&format!(" password={password}"));
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async move {
            let connect = tokio::time::timeout(IO_TIMEOUT, tokio_postgres::connect(&conn, tokio_postgres::NoTls))
                .await
                .map_err(|_| "UDS connect timeout".to_string())?;
            match connect {
                Ok((client, connection)) => {
                    // Keep the connection task alive while proving an actual query
                    // succeeds over the authenticated UDS session.
                    let handle = tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    let row = tokio::time::timeout(IO_TIMEOUT, client.query_one("SELECT 1", &[]))
                        .await
                        .map_err(|_| "UDS query timeout".to_string())?
                        .map_err(|e| format_pg_error("query failed", &e))?;
                    assert_eq!(row.get::<_, i32>(0), 1, "authenticated UDS SELECT 1 result");
                    handle.abort();
                    Ok(())
                }
                Err(e) => Err(format_pg_error("connect failed", &e)),
            }
        })
    }

    fn format_pg_error(context: &str, error: &tokio_postgres::Error) -> String {
        let mut text = format!("{context}: {error}; sqlstate={:?}", error.code().map(|c| c.code()));
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            text.push_str("; source=");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        text
    }

    fn start_uds_daemon() -> Daemon {
        spawn(
            &["--auth", "scram-sha-256", "--password", PASSWORD],
            free_port(),
            free_port(),
            true,
        )
    }

    #[test]
    fn uds_accepts_valid_and_rejects_missing_wrong_and_unknown_credentials() {
        let mut daemon = start_uds_daemon();
        let path = daemon
            .pg_socket_path()
            .expect("the daemon was started with a socket dir");
        wait_for_socket(&mut daemon, &path);
        let port = daemon.pg_port;

        assert!(
            connect_uds(&path, port, "postgres", Some(PASSWORD)).is_ok(),
            "the configured credential must authenticate over the Unix socket{}",
            daemon.diagnostics()
        );
        for (user, password, label) in [
            ("postgres", Some("wrong-password"), "wrong password"),
            ("nobody", Some(PASSWORD), "unknown user"),
        ] {
            let error = connect_uds(&path, port, user, password).expect_err("invalid UDS credentials must fail");
            assert!(
                error.contains("sqlstate=Some(\"28"),
                "{label} must return SQLSTATE class 28, got {error}{}",
                daemon.diagnostics()
            );
        }
        let missing = connect_uds(&path, port, "postgres", None).expect_err("missing UDS password must fail");
        assert!(
            missing.to_ascii_lowercase().contains("password")
                && (missing.to_ascii_lowercase().contains("missing")
                    || missing.to_ascii_lowercase().contains("required")),
            "missing password must be rejected specifically through the error source chain, got {missing}{}",
            daemon.diagnostics()
        );
        assert!(
            connect_uds(&path, port, "postgres", Some(PASSWORD)).is_ok(),
            "a valid UDS query must still succeed after rejected attempts{}",
            daemon.diagnostics()
        );
    }
}
