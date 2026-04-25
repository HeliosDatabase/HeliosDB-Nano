//! Unix-domain-socket MCP server.
//!
//! Same JSON-RPC line protocol as the stdio transport, but bound to a
//! filesystem path with peer-credential auth — i.e. authentication is
//! whatever filesystem permissions the caller put on the socket. A
//! socket created mode `0600` is only readable by its owner; an
//! `0660` socket plus a dedicated group restricts access to that
//! group; world-readable sockets allow any local user to connect.
//!
//! Wire frame: one JSON-RPC message per line (`\n`-terminated). A
//! single connection can drive many request/response pairs and can
//! interleave requests, but the server processes them serially per
//! connection. Multiple concurrent connections get their own task.
//!
//! Linux/macOS only — the module compiles on Unix targets and is a
//! no-op shim elsewhere.

#[cfg(unix)]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tracing::{debug, error, info, warn};

    use crate::EmbeddedDatabase;

    use super::super::rpc::{handle_rpc_with_db, RpcRequest};

    /// A Unix-domain MCP server. `path` is the socket file path —
    /// it's removed on `bind` if it already exists, and on shutdown.
    pub struct UnixSocketServer {
        path: PathBuf,
        db: Arc<EmbeddedDatabase>,
    }

    impl UnixSocketServer {
        pub fn new(path: impl Into<PathBuf>, db: Arc<EmbeddedDatabase>) -> Self {
            Self { path: path.into(), db }
        }

        /// Bind to the configured path and serve until the future is
        /// dropped. Best-effort cleanup of the socket file on the way
        /// out.
        pub async fn run(&self) -> crate::Result<()> {
            // Remove any stale socket file from a prior crash.
            if self.path.exists() {
                let _ = std::fs::remove_file(&self.path);
            }
            let listener = UnixListener::bind(&self.path).map_err(|e| {
                crate::Error::Generic(format!(
                    "MCP unix socket bind failed at {}: {e}",
                    self.path.display()
                ))
            })?;
            info!(path = %self.path.display(), "mcp unix-socket server listening");

            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let db = self.db.clone();
                        let path = self.path.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, db).await {
                                warn!(path = %path.display(), "mcp uds connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!("mcp uds accept failed: {e}");
                        break;
                    }
                }
            }

            // Best-effort cleanup.
            let _ = std::fs::remove_file(&self.path);
            Ok(())
        }
    }

    impl Drop for UnixSocketServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn serve_connection(stream: UnixStream, db: Arc<EmbeddedDatabase>) -> std::io::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read).lines();
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            debug!("mcp uds << {line}");
            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": serde_json::Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    });
                    let mut out = err.to_string();
                    out.push('\n');
                    write.write_all(out.as_bytes()).await?;
                    write.flush().await?;
                    continue;
                }
            };
            // MCP notification — no response.
            if req.method == "initialized" {
                continue;
            }
            let resp = handle_rpc_with_db(db.as_ref(), req);
            let mut json = serde_json::to_string(&resp).unwrap_or_default();
            json.push('\n');
            write.write_all(json.as_bytes()).await?;
            write.flush().await?;
        }
        Ok(())
    }

    /// Convenience: tighten the socket file's permissions to 0600
    /// (owner-only). Call after `bind` if you want to lock down a
    /// socket created with looser umask defaults. Best-effort —
    /// returns Ok if the platform doesn't support `chmod`.
    pub fn lock_down_to_owner(path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::{json, Value};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        fn temp_socket_path() -> PathBuf {
            let mut p = std::env::temp_dir();
            p.push(format!("helios-mcp-test-{}.sock", uuid::Uuid::new_v4()));
            p
        }

        #[tokio::test]
        async fn unix_socket_round_trip() {
            let path = temp_socket_path();
            let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
            db.execute("CREATE TABLE t (id INT4)").unwrap();
            db.execute("INSERT INTO t VALUES (5)").unwrap();
            let server = Arc::new(UnixSocketServer::new(path.clone(), db));

            let server_for_task = server.clone();
            let handle = tokio::spawn(async move {
                let _ = server_for_task.run().await;
            });
            // Give the listener a tick to come up.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let stream = UnixStream::connect(&path).await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read).lines();

            let req = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "heliosdb_query",
                    "arguments": { "sql": "SELECT id FROM t" }
                }
            });
            let mut wire = req.to_string();
            wire.push('\n');
            write.write_all(wire.as_bytes()).await.unwrap();
            write.flush().await.unwrap();

            let line = reader.next_line().await.unwrap().expect("response");
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["result"]["isError"], false, "got {v}");
            let inner = v["result"]["content"][0]["text"].as_str().unwrap();
            assert!(inner.contains("\"row_count\":1"));

            handle.abort();
        }

        #[tokio::test]
        async fn lock_down_sets_0600() {
            let path = temp_socket_path();
            std::fs::write(&path, b"").unwrap();
            lock_down_to_owner(&path).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o600);
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(unix)]
pub use imp::{lock_down_to_owner, UnixSocketServer};

#[cfg(not(unix))]
pub mod imp {
    //! Non-Unix stub. The Unix-domain server is only meaningful on
    //! Unix targets; on Windows / WASM we expose the type so the
    //! API compiles but `run` errors out.
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::EmbeddedDatabase;

    pub struct UnixSocketServer {
        _path: PathBuf,
        _db: Arc<EmbeddedDatabase>,
    }

    impl UnixSocketServer {
        pub fn new(path: impl Into<PathBuf>, db: Arc<EmbeddedDatabase>) -> Self {
            Self { _path: path.into(), _db: db }
        }
        pub async fn run(&self) -> crate::Result<()> {
            Err(crate::Error::Generic(
                "MCP unix-domain server not supported on this platform".into(),
            ))
        }
    }
}

#[cfg(not(unix))]
pub use imp::UnixSocketServer;
