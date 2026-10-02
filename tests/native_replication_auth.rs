//! Native logical replication token validation (archive issue 1; one-secret R6).
//!
//! One owner-only 32-byte token file authenticates BOTH replication channels.
//! A primary configured with a token MUST reject a standby handshake BEFORE any
//! registration or WAL streaming:
//!   * correct token  -> accepted and registered
//!   * wrong token    -> rejected (`authentication failed`), nothing registered
//!   * missing token  -> rejected (`authentication required`), nothing registered
//! With no token configured, the metadata-only handshake is preserved.

#![cfg(feature = "ha-tier1")]

use heliosdb_nano::replication::{
    physical_wire::TransferToken,
    streaming::{StreamingServer, StreamingServerConfig},
    transport::{Capabilities, HandshakeRequest, NodeRole, ReplicationConnection, SyncModeConfig},
    wal_store::{WalStore, WalStoreConfig},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::time::timeout;
use uuid::Uuid;

/// The one token used on both channels in these tests.
const TOKEN: [u8; 32] = [0x5A; 32];

/// The HA broadcast channel is process-global; tests that create a server or
/// broadcast WAL must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn token() -> TransferToken {
    TransferToken::new(TOKEN).unwrap()
}

struct Harness {
    addr: SocketAddr,
    server: Arc<StreamingServer>,
    task: tokio::task::JoinHandle<heliosdb_nano::replication::Result<()>>,
    wal_store: Arc<WalStore>,
    _dir: tempfile::TempDir,
}

async fn start_server_with(auth: Option<TransferToken>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let wal_store = Arc::new(WalStore::new(WalStoreConfig {
        wal_dir: dir.path().join("wal"),
        ..Default::default()
    }));
    wal_store.init().await.unwrap();
    let server = Arc::new(
        StreamingServer::new(
            StreamingServerConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                auth,
                ..Default::default()
            },
            Uuid::new_v4(),
            Uuid::new_v4(),
            wal_store.clone(),
        )
        .unwrap(),
    );
    let listener = server.bind_listener().await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = {
        let server = server.clone();
        tokio::spawn(async move { server.start_with_listener(listener).await })
    };
    Harness {
        addr,
        server,
        task,
        wal_store,
        _dir: dir,
    }
}

async fn start_server(auth: Option<TransferToken>) -> Harness {
    start_server_with(auth).await
}

fn request(auth: Option<[u8; 32]>) -> HandshakeRequest {
    HandshakeRequest {
        expected_history: None,
        node_id: Uuid::new_v4(),
        role: NodeRole::Standby,
        sync_mode: SyncModeConfig::Async,
        current_lsn: Some(0),
        slot_name: None,
        capabilities: Capabilities::all(),
        auth,
    }
}

async fn connect(addr: SocketAddr) -> ReplicationConnection {
    ReplicationConnection::connect(addr, Duration::from_secs(3))
        .await
        .unwrap()
}

async fn standby_count_settled(server: &StreamingServer, want: usize) -> usize {
    let mut seen = 0;
    for _ in 0..50 {
        seen = server.connected_standby_count().await;
        if seen == want {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    seen
}

/// Stop the server and PROVE every owned task ends before the temp dir is dropped.
/// A timed-out join is aborted and awaited (never detached), and the timeout is
/// asserted rather than swallowed.
async fn stop(h: Harness) {
    h.server.shutdown();
    let mut task = h.task;
    match timeout(Duration::from_secs(5), &mut task).await {
        Ok(joined) => {
            joined
                .expect("server task panicked")
                .expect("server task returned an error");
        }
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("server task did not stop within timeout; owned tasks were still running");
        }
    }
    h.wal_store.close().await.expect("wal store close failed");
    // h._dir (TempDir) drops here, after the task is joined.
}

#[tokio::test]
async fn configured_token_accepts_correct_token_and_registers() {
    let _serial = SERIAL.lock().await;
    let h = start_server(Some(token())).await;
    let mut conn = connect(h.addr).await;
    let response = timeout(
        Duration::from_secs(3),
        conn.handshake_client(request(Some(TOKEN))),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.accepted, "correct token must be accepted");
    assert_eq!(
        standby_count_settled(&h.server, 1).await,
        1,
        "authenticated standby must be registered"
    );
    let _ = conn.close().await;
    stop(h).await;
}

#[tokio::test]
async fn configured_token_rejects_wrong_token_before_registration() {
    let _serial = SERIAL.lock().await;
    let h = start_server(Some(token())).await;
    let mut conn = connect(h.addr).await;
    let error = timeout(
        Duration::from_secs(3),
        conn.handshake_client(request(Some([0x11; 32]))),
    )
    .await
    .unwrap()
    .unwrap_err()
    .to_string();
    assert!(error.contains("authentication failed"), "{error}");
    assert_never_registered_and_closed(&h.server, conn).await;
    stop(h).await;
}

/// A rejected handshake must (a) never register a standby across a real window,
/// and (b) close the socket so no further native frame can be requested. A single
/// immediate count poll is not sufficient evidence of "never registered".
async fn assert_never_registered_and_closed(server: &StreamingServer, mut conn: ReplicationConnection) {
    let mut max_seen = 0;
    for _ in 0..25 {
        max_seen = max_seen.max(server.connected_standby_count().await);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(max_seen, 0, "a rejected token must never register a standby");
    let closed = match timeout(Duration::from_secs(2), conn.recv()).await {
        Ok(Ok(_)) => false,  // server kept serving native frames after rejection
        Ok(Err(_)) => true,  // socket closed/reset
        Err(_) => false,     // still open and silently waiting
    };
    assert!(closed, "rejected connection was not closed by the server");
}

#[tokio::test]
async fn configured_token_rejects_missing_token_before_registration() {
    let _serial = SERIAL.lock().await;
    let h = start_server(Some(token())).await;
    let mut conn = connect(h.addr).await;
    let error = timeout(Duration::from_secs(3), conn.handshake_client(request(None)))
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(error.contains("authentication required"), "{error}");
    assert_never_registered_and_closed(&h.server, conn).await;
    stop(h).await;
}

/// R1: an older-layout peer (v1/v2) is refused at the header, before any payload
/// decode or registration. The server closes the socket rather than serving a
/// frame to a peer whose positional bincode layout it cannot trust.
#[tokio::test]
async fn old_protocol_version_is_refused_before_registration() {
    let _serial = SERIAL.lock().await;
    use heliosdb_nano::replication::transport::{Message, MessageType};
    let h = start_server(None).await;
    let payload = bincode::serialize(&request(None)).expect("serialize handshake");
    let mut msg = Message::new(MessageType::HandshakeRequest, bytes::Bytes::from(payload), 1);
    msg.header.version = 1; // simulate a v1 peer
    let mut stream = tokio::net::TcpStream::connect(h.addr).await.expect("connect");
    msg.write_to(&mut stream).await.expect("write v1 handshake");
    // The merged server refuses at MessageHeader::decode, so the connection is
    // closed with no decodable response. Accept either an EOF/reset or an
    // explicit rejection, but never an accepted handshake.
    let served = match timeout(Duration::from_secs(2), Message::read_from(&mut stream)).await {
        Ok(Ok(response)) => {
            let decoded: heliosdb_nano::replication::transport::HandshakeResponse =
                bincode::deserialize(&response.payload).expect("decode response");
            decoded.accepted
        }
        Ok(Err(_)) => false,
        Err(_) => false,
    };
    assert!(!served, "v1 handshake must be refused before decode/registration");
    let mut max_seen = 0;
    for _ in 0..20 {
        max_seen = max_seen.max(h.server.connected_standby_count().await);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(max_seen, 0, "a version-mismatched peer must never register");
    stop(h).await;
}

#[tokio::test]
async fn unconfigured_primary_preserves_metadata_only_handshake() {
    let _serial = SERIAL.lock().await;
    let h = start_server(None).await;
    let mut conn = connect(h.addr).await;
    let response = timeout(Duration::from_secs(3), conn.handshake_client(request(None)))
        .await
        .unwrap()
        .unwrap();
    assert!(response.accepted, "no configured token preserves the legacy handshake");
    let _ = conn.close().await;
    stop(h).await;
}

#[tokio::test]
async fn authenticated_connection_receives_broadcast_wal_entry() {
    use heliosdb_nano::replication::ha_state::ha_state;
    use heliosdb_nano::replication::transport::{MessageType, WalEntryPayload};
    use heliosdb_nano::replication::wal_replicator::{WalEntry, WalEntryType};
    let _serial = SERIAL.lock().await;
    let h = start_server(Some(token())).await;
    let mut conn = connect(h.addr).await;
    let response = timeout(Duration::from_secs(3), conn.handshake_client(request(Some(TOKEN))))
        .await
        .unwrap()
        .unwrap();
    assert!(response.accepted);
    assert_eq!(standby_count_settled(&h.server, 1).await, 1);
    let entry = WalEntry {
        lsn: 42,
        tx_id: None,
        entry_type: WalEntryType::Insert,
        data: b"wal-socket-control".to_vec(),
        checksum: 0,
    };
    let mut sent = false;
    for _ in 0..50 {
        if ha_state().broadcast_wal_entry(entry.clone()).is_some() {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(sent, "no HA broadcast channel to deliver a WAL entry");
    let mut received = None;
    for _ in 0..50 {
        match timeout(Duration::from_millis(200), conn.recv()).await {
            Ok(Ok(msg)) if msg.header.msg_type == MessageType::WalEntry => {
                received = Some(msg);
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    let msg = received.expect("authenticated standby did not receive the broadcast WAL entry");
    let payload: WalEntryPayload = bincode::deserialize(&msg.payload).expect("decode WAL payload");
    assert_eq!(payload.lsn, 42, "wrong WAL entry LSN");
    assert_eq!(payload.data, b"wal-socket-control", "wrong WAL entry contents");
    stop(h).await;
}

#[tokio::test]
async fn reconnect_with_the_same_token_is_accepted() {
    let _serial = SERIAL.lock().await;
    let h = start_server(Some(token())).await;
    for attempt in 0..2 {
        let mut conn = connect(h.addr).await;
        let response = timeout(Duration::from_secs(3), conn.handshake_client(request(Some(TOKEN))))
            .await
            .unwrap()
            .unwrap();
        assert!(response.accepted, "reconnect {attempt} with the same token must be accepted");
        let _ = conn.close().await;
    }
    stop(h).await;
}

#[tokio::test]
async fn malformed_handshake_frame_is_refused_without_registration() {
    use heliosdb_nano::replication::transport::{Message, MessageType};
    let _serial = SERIAL.lock().await;
    let h = start_server(None).await;
    let msg = Message::new(
        MessageType::HandshakeRequest,
        bytes::Bytes::from_static(b"not-a-bincode-handshake"),
        1,
    );
    let mut stream = tokio::net::TcpStream::connect(h.addr).await.expect("connect");
    msg.write_to(&mut stream).await.expect("write malformed frame");
    let closed = match timeout(Duration::from_secs(2), Message::read_from(&mut stream)).await {
        Ok(Ok(_)) => false,
        Ok(Err(_)) => true,
        Err(_) => false,
    };
    assert!(closed, "malformed handshake did not close the connection");
    let mut max_seen = 0;
    for _ in 0..20 {
        max_seen = max_seen.max(h.server.connected_standby_count().await);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(max_seen, 0, "malformed handshake must never register");
    stop(h).await;
}

#[tokio::test]
async fn new_client_refuses_an_old_version_server() {
    use heliosdb_nano::replication::transport::{HandshakeResponse, Message, MessageType};
    let _serial = SERIAL.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _request = Message::read_from(&mut stream).await.unwrap();
        let response = HandshakeResponse {
            history_id: Uuid::nil(),
            accepted: true,
            server_node_id: Uuid::new_v4(),
            sync_mode: SyncModeConfig::Async,
            primary_lsn: 0,
            slot_name: None,
            fencing_token: 1,
            capabilities: Capabilities::all(),
            error: None,
        };
        let payload = bincode::serialize(&response).unwrap();
        let mut msg = Message::new(MessageType::HandshakeResponse, bytes::Bytes::from(payload), 1);
        msg.header.version = 1; // simulate an old (v1) server
        msg.write_to(&mut stream).await.unwrap();
    });
    let mut conn = connect(addr).await;
    let error = timeout(Duration::from_secs(3), conn.handshake_client(request(None)))
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    // R1: the client refuses at the header, before decoding the old layout.
    assert!(
        error.contains("Unsupported native replication protocol version"),
        "new client must refuse an old server: {error}"
    );
    server.await.unwrap();
}

/// R6: the same token file authenticates the native channel and the physical
/// channel. Loading once yields a `TransferToken`; the native handshake presents
/// `wire_bytes()` and the physical wire verifies the same bytes with `accepts`.
#[tokio::test]
async fn one_token_file_authenticates_native_and_physical_channels() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replication.token");
    std::fs::write(&path, TOKEN).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let loaded = heliosdb_nano::replication::read_replication_token(&path).unwrap();

    // Physical channel consumes the same loaded token.
    assert!(loaded.accepts(&TOKEN), "physical wire must accept the token file bytes");

    // Native channel presents the same bytes and is accepted.
    let h = start_server(Some(loaded.clone())).await;
    let mut conn = connect(h.addr).await;
    let response = timeout(
        Duration::from_secs(3),
        conn.handshake_client(request(Some(loaded.wire_bytes()))),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.accepted, "native handshake must accept the token file bytes");
    let _ = conn.close().await;
    stop(h).await;
}
