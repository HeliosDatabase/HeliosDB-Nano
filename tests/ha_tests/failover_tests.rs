//! Failover Tests
//!
//! Tests for automatic and manual failover scenarios.

use std::time::Duration;
use uuid::Uuid;

#[cfg(feature = "ha-tier1")]
use heliosdb_nano::replication::{
    split_brain::{
        ClusterNode, ObserverConfig, ProtectionEvent, ProtectionState, SplitBrainProtector,
    },
    transport::{NodeRole, VoteReason, VoteRequestPayload},
};

/// Create a test protector for a standby node
fn create_standby_protector() -> (SplitBrainProtector, tokio::sync::mpsc::Receiver<ProtectionEvent>) {
    let node_id = Uuid::new_v4();
    let config = ObserverConfig {
        quorum_size: 2,
        vote_timeout: Duration::from_secs(5),
        election_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    SplitBrainProtector::new(node_id, NodeRole::Standby, config)
}

/// Create a test protector for a primary node
fn create_primary_protector() -> (SplitBrainProtector, tokio::sync::mpsc::Receiver<ProtectionEvent>) {
    let node_id = Uuid::new_v4();
    let config = ObserverConfig {
        quorum_size: 2,
        vote_timeout: Duration::from_secs(5),
        election_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    SplitBrainProtector::new(node_id, NodeRole::Primary, config)
}

#[tokio::test]
async fn test_protector_initial_state() {
    let (protector, _rx) = create_primary_protector();

    // Verify initial state
    assert_eq!(protector.current_term(), 1);
    assert_eq!(protector.current_fencing_token(), 1);
    assert_eq!(protector.role().await, NodeRole::Primary);
    assert_eq!(protector.current_state().await, ProtectionState::Normal);
    assert!(protector.known_primary().await.is_none());
}

#[tokio::test]
async fn test_vote_request_handling() {
    let (protector, _rx) = create_standby_protector();

    // Create a vote request from a candidate
    let candidate_id = Uuid::new_v4();
    let request = VoteRequestPayload {
        candidate_id,
        term: 2,
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };

    // Handle the vote request
    let response = protector.handle_vote_request(request).await;

    // Verify vote was granted
    assert!(response.vote_granted);
    assert_eq!(response.term, 2);

    // Term should be updated
    assert_eq!(protector.current_term(), 2);
}

#[tokio::test]
async fn test_vote_rejection_stale_term() {
    let (protector, _rx) = create_standby_protector();

    // First, vote for term 3
    let candidate1 = Uuid::new_v4();
    let request1 = VoteRequestPayload {
        candidate_id: candidate1,
        term: 3,
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response1 = protector.handle_vote_request(request1).await;
    assert!(response1.vote_granted);

    // Now try to vote for term 2 (stale)
    let candidate2 = Uuid::new_v4();
    let request2 = VoteRequestPayload {
        candidate_id: candidate2,
        term: 2, // Stale term
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response2 = protector.handle_vote_request(request2).await;

    // Vote should be rejected
    assert!(!response2.vote_granted);
    assert!(response2.rejection_reason.is_some());
}

#[tokio::test]
async fn test_vote_rejection_already_voted() {
    let (protector, _rx) = create_standby_protector();

    let candidate1 = Uuid::new_v4();
    let candidate2 = Uuid::new_v4();

    // Vote for candidate1 in term 5
    let request1 = VoteRequestPayload {
        candidate_id: candidate1,
        term: 5,
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response1 = protector.handle_vote_request(request1).await;
    assert!(response1.vote_granted);

    // Try to vote for candidate2 in same term
    let request2 = VoteRequestPayload {
        candidate_id: candidate2,
        term: 5, // Same term
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response2 = protector.handle_vote_request(request2).await;

    // Vote should be rejected (already voted for candidate1)
    assert!(!response2.vote_granted);
    assert!(response2.rejection_reason.is_some());
}

#[tokio::test]
async fn test_fencing_token_validation() {
    let (protector, _rx) = create_primary_protector();

    // Initial token is 1
    assert!(protector.validate_fencing_token(1));
    assert!(protector.validate_fencing_token(2));
    assert!(protector.validate_fencing_token(100));

    // Token 0 should be invalid
    assert!(!protector.validate_fencing_token(0));
}

#[tokio::test]
async fn test_fencing_token_update() {
    let (protector, mut rx) = create_standby_protector();

    // Initial fencing token
    let initial_token = protector.current_fencing_token();
    assert_eq!(initial_token, 1);

    // Simulate receiving a higher fencing token from new primary
    let new_primary_id = Uuid::new_v4();
    let payload = heliosdb_nano::replication::transport::FencingTokenPayload {
        token: 5,
        issuer_id: new_primary_id,
        term: 2,
        timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
    };

    protector.handle_fencing_token(payload).await;

    // Token should be updated
    assert_eq!(protector.current_fencing_token(), 5);

    // Known primary should be set
    assert_eq!(protector.known_primary().await, Some(new_primary_id));

    // Check for event
    if let Some(event) = rx.recv().await {
        match event {
            ProtectionEvent::FencingTokenChanged { old_token, new_token } => {
                assert_eq!(old_token, 1);
                assert_eq!(new_token, 5);
            }
            _ => panic!("Expected FencingTokenChanged event"),
        }
    }
}

#[tokio::test]
async fn test_cluster_node_registration() {
    let (protector, _rx) = create_primary_protector();

    // Register some cluster nodes
    let standby1_id = Uuid::new_v4();
    let standby2_id = Uuid::new_v4();

    protector.register_node(ClusterNode {
        node_id: standby1_id,
        role: NodeRole::Standby,
        addr: "192.168.1.2:5433".parse().unwrap(),
        last_lsn: 0,
        last_heartbeat: std::time::Instant::now(),
        is_healthy: true,
        fencing_token: 1,
    }).await;

    protector.register_node(ClusterNode {
        node_id: standby2_id,
        role: NodeRole::Standby,
        addr: "192.168.1.3:5433".parse().unwrap(),
        last_lsn: 0,
        last_heartbeat: std::time::Instant::now(),
        is_healthy: true,
        fencing_token: 1,
    }).await;

    // Update heartbeat
    protector.update_node_heartbeat(standby1_id, 100).await;
    protector.update_node_heartbeat(standby2_id, 95).await;
}

#[tokio::test]
async fn test_primary_stepdown_on_higher_token() {
    let (protector, _rx) = create_primary_protector();

    // Verify we're primary
    assert_eq!(protector.role().await, NodeRole::Primary);

    // Receive higher fencing token from another node claiming primary
    let usurper_id = Uuid::new_v4();
    let payload = heliosdb_nano::replication::transport::FencingTokenPayload {
        token: 10, // Much higher than our 1
        issuer_id: usurper_id,
        term: 5,
        timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
    };

    protector.handle_fencing_token(payload).await;

    // We should have stepped down to standby
    assert_eq!(protector.role().await, NodeRole::Standby);

    // Fencing token should be updated
    assert_eq!(protector.current_fencing_token(), 10);

    // Known primary should be the usurper
    assert_eq!(protector.known_primary().await, Some(usurper_id));
}

#[tokio::test]
async fn test_vote_reason_variants() {
    // Verify all vote reasons are accessible
    let reasons = vec![
        VoteReason::PrimaryFailure,
        VoteReason::NetworkPartition,
        VoteReason::ManualFailover,
        VoteReason::SplitBrainRecovery,
    ];

    for reason in reasons {
        let request = VoteRequestPayload {
            candidate_id: Uuid::new_v4(),
            term: 1,
            last_lsn: 0,
            previous_primary: None,
            reason,
        };

        // Just verify we can create requests with each reason
        assert!(request.candidate_id != Uuid::nil());
    }
}

#[tokio::test]
async fn test_protection_state_transitions() {
    // Verify state enum values
    assert_eq!(ProtectionState::Normal, ProtectionState::Normal);
    assert_ne!(ProtectionState::Normal, ProtectionState::Election);
    assert_ne!(ProtectionState::Election, ProtectionState::Fenced);
    assert_ne!(ProtectionState::Fenced, ProtectionState::SplitBrain);
}
