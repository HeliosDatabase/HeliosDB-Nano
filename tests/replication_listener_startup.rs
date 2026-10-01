//! Native replication must reserve its endpoint before startup is advertised.

#![cfg(feature = "ha-tier1")]

use heliosdb_nano::replication::{
    streaming::{StreamingServer, StreamingServerConfig},
    transport::{
        Capabilities, HandshakeRequest, Message, MessageType, NodeRole, ReplicationConnection, SyncModeConfig,
    },
    wal_store::{WalStore, WalStoreConfig},
};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, time::timeout};
use uuid::Uuid;

#[tokio::test]
async fn occupied_endpoint_fails_before_serving_and_prebound_listener_handshakes() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let occupied_addr = occupied.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wal_store = Arc::new(WalStore::new(WalStoreConfig {
        wal_dir: dir.path().join("wal"),
        ..Default::default()
    }));
    wal_store.init().await.unwrap();
    let blocked_server = StreamingServer::new(
        StreamingServerConfig {
            listen_addr: occupied_addr,
            ..Default::default()
        },
        Uuid::new_v4(),
        wal_store.clone(),
    );
    let error = blocked_server.bind_listener().await.unwrap_err().to_string();
    assert!(error.contains(&occupied_addr.to_string()), "{error}");
    assert!(error.contains("native replication listener"), "{error}");
    assert!(error.contains("--replication-port"), "{error}");

    let node_id = Uuid::new_v4();
    let server = Arc::new(StreamingServer::new(
        StreamingServerConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        },
        node_id,
        wal_store.clone(),
    ));
    let listener = server.bind_listener().await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(TcpListener::bind(addr).await.is_err());
    let server_task = {
        let server = server.clone();
        tokio::spawn(async move { server.start_with_listener(listener).await })
    };
    let mut connection = ReplicationConnection::connect(addr, Duration::from_secs(3))
        .await
        .unwrap();
    let response = timeout(
        Duration::from_secs(3),
        connection.handshake_client(HandshakeRequest {
            node_id: Uuid::new_v4(),
            role: NodeRole::Standby,
            sync_mode: SyncModeConfig::Async,
            current_lsn: Some(0),
            slot_name: None,
            capabilities: Capabilities::all(),
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.accepted);
    assert_eq!(response.server_node_id, node_id);
    connection.close().await.unwrap();
    server.shutdown();
    timeout(Duration::from_secs(3), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    wal_store.close().await.unwrap();
}

#[tokio::test]
async fn postgres_endpoint_response_has_actionable_protocol_diagnostic() {
    // A PostgreSQL ErrorResponse with a severity and a message, not a HELI frame.
    let fields = b"SFATAL\0Munsupported frontend protocol\0\0";
    let mut frame = vec![b'E'];
    frame.extend_from_slice(&((fields.len() + 4) as u32).to_be_bytes());
    frame.extend_from_slice(fields);
    let error = Message::read_from(&mut frame.as_slice()).await.unwrap_err().to_string();
    assert!(error.contains("got 45000000"), "{error}");
    assert!(error.contains("PostgreSQL ErrorResponse"), "{error}");
    assert!(error.contains("--primary-host"), "{error}");
    assert!(error.contains("--replication-port"), "{error}");
}

#[tokio::test]
async fn native_protocol_still_roundtrips_and_other_magic_is_rejected() {
    let original = Message::new(MessageType::Heartbeat, bytes::Bytes::from_static(b"payload"), 42);
    let encoded = original.encode();
    let decoded = Message::read_from(&mut encoded.as_ref()).await.unwrap();
    assert_eq!(decoded.header.sequence, 42);
    assert_eq!(decoded.payload, original.payload);

    let mut corrupt = encoded.to_vec();
    corrupt[..4].copy_from_slice(b"HTTP");
    let error = Message::read_from(&mut corrupt.as_slice())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("native HELI replication protocol"), "{error}");
    assert!(!error.contains("PostgreSQL ErrorResponse"), "{error}");
}
