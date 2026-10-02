//! Native v2 contains history mixing before readiness, registration or WAL dispatch.
#![cfg(feature = "ha-tier1")]

use bytes::Bytes;
use heliosdb_nano::replication::{
    history::{prepare_receiver_history, ReceiverHistory, STANDBY_BINDING_FILE},
    streaming::{
        StreamingClient, StreamingClientConfig, StreamingClientStatus, StreamingServer, StreamingServerConfig,
    },
    transport::{
        Capabilities, HandshakeRequest, HandshakeResponse, Message, MessageType, NodeRole, ReplicationConnection,
        SyncModeConfig, WalEntryPayload, WalEntryType, PROTOCOL_VERSION,
    },
    wal_store::{WalStore, WalStoreConfig},
    ReplicationError,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};
use uuid::Uuid;

const LIMIT: Duration = Duration::from_secs(5);

fn config(addr: SocketAddr) -> StreamingClientConfig {
    StreamingClientConfig {
        node_id: Uuid::new_v4(),
        primary_addr: addr,
        sync_mode: SyncModeConfig::Async,
        connect_timeout: Duration::from_secs(2),
        reconnect_delay: Duration::from_millis(10),
        max_reconnect_attempts: 0,
        auth: None,
    }
}

fn request(expected: Option<Uuid>) -> HandshakeRequest {
    HandshakeRequest {
        expected_history: expected,
        node_id: Uuid::new_v4(),
        role: NodeRole::Standby,
        sync_mode: SyncModeConfig::Async,
        current_lsn: Some(0),
        slot_name: None,
        capabilities: Capabilities::all(),
        auth: None,
    }
}

async fn mock_handshake(stream: &mut TcpStream, history_id: Uuid) -> HandshakeRequest {
    let msg = timeout(LIMIT, Message::read_from(stream)).await.unwrap().unwrap();
    assert_eq!(msg.header.msg_type, MessageType::HandshakeRequest);
    let req: HandshakeRequest = bincode::deserialize(&msg.payload).unwrap();
    // Deliberately claim acceptance even if the peer identity does not match:
    // client-side verification must independently reject this response.
    let response = HandshakeResponse {
        history_id,
        accepted: true,
        server_node_id: Uuid::new_v4(),
        sync_mode: SyncModeConfig::Async,
        primary_lsn: 0,
        slot_name: None,
        fencing_token: 1,
        capabilities: Capabilities::all(),
        error: None,
    };
    let msg = Message::new(
        MessageType::HandshakeResponse,
        Bytes::from(bincode::serialize(&response).unwrap()),
        1,
    );
    timeout(LIMIT, msg.write_to(stream)).await.unwrap().unwrap();
    req
}

fn wal_message(lsn: u64) -> Message {
    let data = format!("private fixture {lsn}").into_bytes();
    let payload = WalEntryPayload {
        lsn,
        tx_id: None,
        entry_type: WalEntryType::Insert,
        checksum: crc32fast::hash(&data),
        data,
        timestamp_us: 0,
    };
    Message::new(
        MessageType::WalEntry,
        Bytes::from(bincode::serialize(&payload).unwrap()),
        lsn,
    )
}

#[tokio::test]
async fn rejects_old_and_future_headers_before_payload_read() {
    for version in [1, PROTOCOL_VERSION + 1] {
        let mut frame = Message::new(MessageType::HandshakeResponse, Bytes::new(), 0);
        frame.header.version = version;
        // Header alone is enough; otherwise the reader would fail with short payload.
        frame.header.length = 100;
        let bytes = frame.encode();
        let error = Message::read_from(&mut bytes.as_ref()).await.unwrap_err();
        assert!(
            matches!(error, ReplicationError::ProtocolVersion { expected: PROTOCOL_VERSION, actual } if actual == version)
        );
    }
}

#[tokio::test]
async fn streaming_server_rejects_mismatch_before_registration() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(WalStore::new(WalStoreConfig {
        wal_dir: dir.path().join("wal"),
        ..Default::default()
    }));
    store.init().await.unwrap();
    let actual = Uuid::new_v4();
    let server = Arc::new(
        StreamingServer::new(
            StreamingServerConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                ..Default::default()
            },
            Uuid::new_v4(),
            actual,
            store.clone(),
        )
        .unwrap(),
    );
    let listener = server.bind_listener().await.unwrap();
    let addr = listener.local_addr().unwrap();
    let run = {
        let server = server.clone();
        tokio::spawn(async move { server.start_with_listener(listener).await })
    };
    let mut conn = ReplicationConnection::connect(addr, LIMIT).await.unwrap();
    let expected = Uuid::new_v4();
    let result = timeout(LIMIT, conn.handshake_client(request(Some(expected)))).await;
    server.shutdown();
    timeout(LIMIT, run).await.unwrap().unwrap().unwrap();
    assert!(
        matches!(result.unwrap(), Err(ReplicationError::HistoryMismatch { expected: e, actual: a }) if e == expected && a == actual)
    );
    assert_eq!(server.standby_count().await, 0);
    store.close().await.unwrap();
}

#[tokio::test]
async fn durable_first_binding_precedes_readiness_and_first_wal() {
    let dir = tempfile::tempdir().unwrap();
    let receiver_path = dir.path().join("receiver");
    let history = Arc::new(prepare_receiver_history(&receiver_path).unwrap());
    // Simulate only RocksDB's identity file; this transport test does not open a DB.
    std::fs::write(receiver_path.join("IDENTITY"), format!("{}\n", Uuid::new_v4())).unwrap();
    let expected = Uuid::new_v4();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        assert_eq!(mock_handshake(&mut stream, expected).await.expected_history, None);
        wal_message(1).write_to(&mut stream).await.unwrap();
        let _ = timeout(LIMIT, release_rx).await;
    });
    let (client, mut entries) = StreamingClient::new(config(addr), history.clone());
    let client = Arc::new(client);
    let mut status = client.subscribe_status();
    let run = {
        let client = client.clone();
        tokio::spawn(async move { client.start().await })
    };
    let verified = timeout(LIMIT, async {
        loop {
            if matches!(*status.borrow_and_update(), StreamingClientStatus::Verified { .. }) {
                break;
            }
            status.changed().await.unwrap();
        }
    })
    .await;
    let durable_at_readiness = prepare_receiver_history(&receiver_path)
        .unwrap()
        .expected_history()
        .unwrap();
    let entry = timeout(LIMIT, entries.recv()).await;
    client.shutdown();
    let result = timeout(LIMIT, run).await;
    let _ = release_tx.send(());
    timeout(LIMIT, peer).await.unwrap().unwrap();
    verified.unwrap();
    result.unwrap().unwrap().unwrap();
    assert_eq!(durable_at_readiness, Some(expected));
    assert_eq!(entry.unwrap().unwrap().lsn, 1);
    assert_eq!(history.expected_history().unwrap(), Some(expected));
}

#[tokio::test]
async fn binding_failure_never_publishes_verified_or_enqueues_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receiver");
    let history = Arc::new(prepare_receiver_history(&path).unwrap());
    std::fs::write(path.join("IDENTITY"), format!("{}\n", Uuid::new_v4())).unwrap();
    // The startup context is fresh, but another file now occupies its binding:
    // create_new must fail without overwrite, even after an accepted handshake.
    std::fs::write(path.join(STANDBY_BINDING_FILE), b"do not overwrite").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        mock_handshake(&mut stream, Uuid::new_v4()).await;
        // Peer may already have queued data, but the receiver must not dispatch it.
        let _ = wal_message(1).write_to(&mut stream).await;
    });
    let (client, mut entries) = StreamingClient::new(config(addr), history);
    let status = client.subscribe_status();
    let result = timeout(LIMIT, client.start()).await.unwrap();
    timeout(LIMIT, peer).await.unwrap().unwrap();
    assert!(matches!(result, Err(ReplicationError::HistoryMetadata(_))));
    assert!(matches!(*status.borrow(), StreamingClientStatus::Failed { .. }));
    assert!(entries.try_recv().is_err());
    assert_eq!(
        std::fs::read(path.join(STANDBY_BINDING_FILE)).unwrap(),
        b"do not overwrite"
    );
    assert!(matches!(client.start().await, Err(ReplicationError::Configuration(_))));
}

#[tokio::test]
async fn reconnect_to_other_history_is_terminal_before_second_wal() {
    let expected = Uuid::new_v4();
    let actual = Uuid::new_v4();
    let history = Arc::new(ReceiverHistory::in_memory());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut first, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        assert_eq!(mock_handshake(&mut first, expected).await.expected_history, None);
        wal_message(1).write_to(&mut first).await.unwrap();
        timeout(LIMIT, release_rx).await.unwrap().unwrap();
        drop(first);
        let (mut second, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        assert_eq!(
            mock_handshake(&mut second, actual).await.expected_history,
            Some(expected)
        );
        let _ = wal_message(2).write_to(&mut second).await;
    });
    let (client, mut entries) = StreamingClient::new(config(addr), history.clone());
    let client = Arc::new(client);
    let status = client.subscribe_status();
    let run = {
        let client = client.clone();
        tokio::spawn(async move { client.start().await })
    };
    let first = timeout(LIMIT, entries.recv()).await;
    let _ = release_tx.send(());
    let outcome = timeout(LIMIT, run).await;
    timeout(LIMIT, peer).await.unwrap().unwrap();
    assert_eq!(first.unwrap().unwrap().lsn, 1);
    assert!(
        matches!(outcome.unwrap().unwrap(), Err(ReplicationError::HistoryMismatch { expected: e, actual: a }) if e == expected && a == actual)
    );
    assert!(matches!(*status.borrow(), StreamingClientStatus::Failed { .. }));
    assert!(entries.try_recv().is_err());
    assert_eq!(history.expected_history().unwrap(), Some(expected));
    assert!(client.start().await.is_err());
}

#[tokio::test]
async fn legacy_peer_version_is_terminal_even_with_unlimited_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        Message::read_from(&mut stream).await.unwrap();
        let mut response = Message::new(MessageType::HandshakeResponse, Bytes::new(), 1);
        response.header.version = 1;
        response.write_to(&mut stream).await.unwrap();
    });
    let (client, mut entries) = StreamingClient::new(config(addr), Arc::new(ReceiverHistory::in_memory()));
    let status = client.subscribe_status();
    let result = timeout(LIMIT, client.start()).await;
    timeout(LIMIT, peer).await.unwrap().unwrap();
    assert!(matches!(
        result.unwrap(),
        Err(ReplicationError::ProtocolVersion { actual: 1, .. })
    ));
    assert!(matches!(*status.borrow(), StreamingClientStatus::Failed { .. }));
    assert!(entries.try_recv().is_err());
}

#[test]
fn public_servers_refuse_nil_history_instead_of_fabricating_identity() {
    use heliosdb_nano::replication::transport::ReplicationServer;
    assert!(matches!(
        ReplicationServer::new("127.0.0.1:0".parse().unwrap(), Uuid::nil()),
        Err(ReplicationError::HistoryMetadata(_))
    ));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(WalStore::new(WalStoreConfig {
        wal_dir: dir.path().join("wal"),
        ..Default::default()
    }));
    assert!(matches!(
        StreamingServer::new(StreamingServerConfig::default(), Uuid::new_v4(), Uuid::nil(), store),
        Err(ReplicationError::HistoryMetadata(_))
    ));
}
