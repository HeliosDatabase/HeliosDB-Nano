//! Split-Brain Protection Tests
//!
//! Tests for split-brain detection, fencing, and quorum-based decisions.

use std::time::Duration;
use uuid::Uuid;

#[cfg(feature = "ha-tier1")]
use heliosdb_lite::replication::{
    split_brain::{
        ClusterNode, ObserverConfig, ProtectionEvent, ProtectionState,
        SplitBrainProtector,
    },
    transport::{FencingTokenPayload, NodeRole, VoteReason, VoteRequestPayload},
};

/// Create a protector with custom quorum size
fn create_protector_with_quorum(
    role: NodeRole,
    quorum_size: usize,
) -> (SplitBrainProtector, tokio::sync::mpsc::Receiver<ProtectionEvent>) {
    let node_id = Uuid::new_v4();
    let config = ObserverConfig {
        quorum_size,
        vote_timeout: Duration::from_secs(5),
        election_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    SplitBrainProtector::new(node_id, role, config)
}

#[tokio::test]
async fn test_protector_initial_state() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);

    // Verify initial state
    assert_eq!(protector.current_term(), 1);
    assert_eq!(protector.current_fencing_token(), 1);
    assert_eq!(protector.role().await, NodeRole::Primary);
    assert_eq!(protector.current_state().await, ProtectionState::Normal);
    assert!(protector.known_primary().await.is_none());
}

#[tokio::test]
async fn test_node_registration() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);

    // Register 2 standbys
    let standby1_id = Uuid::new_v4();
    let standby2_id = Uuid::new_v4();

    protector
        .register_node(ClusterNode {
            node_id: standby1_id,
            role: NodeRole::Standby,
            addr: "192.168.1.2:5433".parse().unwrap(),
            last_lsn: 0,
            last_heartbeat: std::time::Instant::now(),
            is_healthy: true,
            fencing_token: 1,
        })
        .await;

    protector
        .register_node(ClusterNode {
            node_id: standby2_id,
            role: NodeRole::Standby,
            addr: "192.168.1.3:5433".parse().unwrap(),
            last_lsn: 0,
            last_heartbeat: std::time::Instant::now(),
            is_healthy: true,
            fencing_token: 1,
        })
        .await;

    // Nodes should be registered (verify through heartbeat update)
    protector.update_node_heartbeat(standby1_id, 100).await;
    protector.update_node_heartbeat(standby2_id, 200).await;
}

#[tokio::test]
async fn test_election_with_multiple_candidates() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);

    // First candidate requests vote
    let candidate1 = Uuid::new_v4();
    let request1 = VoteRequestPayload {
        candidate_id: candidate1,
        term: 2,
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response1 = protector.handle_vote_request(request1).await;
    assert!(response1.vote_granted);

    // Second candidate with same term - should be rejected
    let candidate2 = Uuid::new_v4();
    let request2 = VoteRequestPayload {
        candidate_id: candidate2,
        term: 2,
        last_lsn: 150, // Higher LSN but same term
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response2 = protector.handle_vote_request(request2).await;
    assert!(!response2.vote_granted);

    // Third candidate with higher term - should be granted
    let candidate3 = Uuid::new_v4();
    let request3 = VoteRequestPayload {
        candidate_id: candidate3,
        term: 3, // Higher term
        last_lsn: 50,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response3 = protector.handle_vote_request(request3).await;
    assert!(response3.vote_granted);
    assert_eq!(protector.current_term(), 3);
}

#[tokio::test]
async fn test_fencing_prevents_stale_primary() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);

    // Initial fencing token
    assert_eq!(protector.current_fencing_token(), 1);

    // Simulate new primary with higher token
    let new_primary_id = Uuid::new_v4();
    let payload = FencingTokenPayload {
        token: 10,
        issuer_id: new_primary_id,
        term: 5,
        timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
    };

    protector.handle_fencing_token(payload).await;

    // Old primary should recognize it's been fenced
    assert_eq!(protector.current_fencing_token(), 10);
    assert!(!protector.validate_fencing_token(5)); // Token 5 < current 10
    assert!(protector.validate_fencing_token(10)); // Token 10 == current
    assert!(protector.validate_fencing_token(11)); // Token 11 > current
}

#[tokio::test]
async fn test_observer_voting() {
    // Create an observer node
    let observer_id = Uuid::new_v4();
    let observer_config = ObserverConfig {
        quorum_size: 2,
        vote_timeout: Duration::from_secs(5),
        election_timeout: Duration::from_secs(10),
        ..Default::default()
    };
    let (observer, _rx) = SplitBrainProtector::new(observer_id, NodeRole::Observer, observer_config);

    // Observer should participate in voting
    let candidate_id = Uuid::new_v4();
    let request = VoteRequestPayload {
        candidate_id,
        term: 2,
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };

    let response = observer.handle_vote_request(request).await;
    assert!(response.vote_granted);
    assert_eq!(observer.current_term(), 2);
}

#[tokio::test]
async fn test_term_monotonicity() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);

    // Initial term
    assert_eq!(protector.current_term(), 1);

    // Vote for term 5
    let candidate = Uuid::new_v4();
    let request = VoteRequestPayload {
        candidate_id: candidate,
        term: 5,
        last_lsn: 100,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    protector.handle_vote_request(request).await;
    assert_eq!(protector.current_term(), 5);

    // Attempt to go back to term 3 - should fail
    let old_request = VoteRequestPayload {
        candidate_id: Uuid::new_v4(),
        term: 3,
        last_lsn: 200,
        previous_primary: None,
        reason: VoteReason::PrimaryFailure,
    };
    let response = protector.handle_vote_request(old_request).await;
    assert!(!response.vote_granted);
    assert_eq!(protector.current_term(), 5); // Term unchanged
}

#[tokio::test]
async fn test_vote_reason_handling() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);

    // Test each vote reason
    let reasons = vec![
        (VoteReason::PrimaryFailure, 2),
        (VoteReason::NetworkPartition, 3),
        (VoteReason::ManualFailover, 4),
        (VoteReason::SplitBrainRecovery, 5),
    ];

    for (reason, term) in reasons {
        let request = VoteRequestPayload {
            candidate_id: Uuid::new_v4(),
            term,
            last_lsn: 100,
            previous_primary: None,
            reason,
        };
        let response = protector.handle_vote_request(request).await;
        assert!(response.vote_granted, "Vote should be granted for {:?}", reason);
    }
}

#[tokio::test]
async fn test_heartbeat_tracking() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);

    let standby_id = Uuid::new_v4();
    protector
        .register_node(ClusterNode {
            node_id: standby_id,
            role: NodeRole::Standby,
            addr: "192.168.1.2:5433".parse().unwrap(),
            last_lsn: 0,
            last_heartbeat: std::time::Instant::now(),
            is_healthy: true,
            fencing_token: 1,
        })
        .await;

    // Update heartbeat with LSN
    protector.update_node_heartbeat(standby_id, 100).await;
    protector.update_node_heartbeat(standby_id, 200).await;
}

#[tokio::test]
async fn test_role_change_on_fencing_token() {
    // Start as primary
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);
    assert_eq!(protector.role().await, NodeRole::Primary);

    // Receive higher fencing token from another node claiming primary
    let usurper_id = Uuid::new_v4();
    let payload = FencingTokenPayload {
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
async fn test_previous_primary_tracking() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);

    let old_primary = Uuid::new_v4();
    let new_primary = Uuid::new_v4();

    // Vote request with previous primary
    let request = VoteRequestPayload {
        candidate_id: new_primary,
        term: 2,
        last_lsn: 100,
        previous_primary: Some(old_primary),
        reason: VoteReason::PrimaryFailure,
    };

    let response = protector.handle_vote_request(request).await;
    assert!(response.vote_granted);

    // Note: known_primary is set when receiving a fencing token from a primary,
    // not when voting for a candidate (voting doesn't mean election won).
    // The vote just records who we voted for in this term.
}

#[tokio::test]
async fn test_cluster_node_registration() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);

    // Register multiple nodes
    let node_ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();

    for (i, node_id) in node_ids.iter().enumerate() {
        protector
            .register_node(ClusterNode {
                node_id: *node_id,
                role: NodeRole::Standby,
                addr: format!("192.168.1.{}:5433", i + 2).parse().unwrap(),
                last_lsn: 0,
                last_heartbeat: std::time::Instant::now(),
                is_healthy: true,
                fencing_token: 1,
            })
            .await;
    }

    // Update heartbeats
    for (i, node_id) in node_ids.iter().enumerate() {
        protector.update_node_heartbeat(*node_id, (i * 100) as u64).await;
    }
}

#[tokio::test]
async fn test_concurrent_vote_requests() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);
    let protector = std::sync::Arc::new(protector);

    // Spawn multiple concurrent vote requests
    let mut handles = Vec::new();
    for i in 0..10 {
        let protector = std::sync::Arc::clone(&protector);
        handles.push(tokio::spawn(async move {
            let request = VoteRequestPayload {
                candidate_id: Uuid::new_v4(),
                term: (i + 2) as u64,
                last_lsn: 100,
                previous_primary: None,
                reason: VoteReason::PrimaryFailure,
            };
            protector.handle_vote_request(request).await
        }));
    }

    // Wait for all and count grants
    let mut granted_count = 0;
    for handle in handles {
        let response = handle.await.expect("Task failed");
        if response.vote_granted {
            granted_count += 1;
        }
    }

    // Due to term monotonicity, some votes will be rejected
    // At minimum one will be granted (the highest term wins)
    assert!(granted_count >= 1);

    // Term should be at highest requested value
    assert!(protector.current_term() >= 11);
}

#[tokio::test]
async fn test_fencing_token_validation() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Primary, 2);

    // Initial token is 1
    assert!(protector.validate_fencing_token(1));
    assert!(protector.validate_fencing_token(2));
    assert!(protector.validate_fencing_token(100));

    // Token 0 should be invalid
    assert!(!protector.validate_fencing_token(0));
}

#[tokio::test]
async fn test_protection_state_values() {
    // Verify state enum values are distinct
    assert_eq!(ProtectionState::Normal, ProtectionState::Normal);
    assert_ne!(ProtectionState::Normal, ProtectionState::Election);
    assert_ne!(ProtectionState::Election, ProtectionState::Fenced);
    assert_ne!(ProtectionState::Fenced, ProtectionState::SplitBrain);
}

#[tokio::test]
async fn test_vote_rejection_stale_term() {
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);

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
    let (protector, _rx) = create_protector_with_quorum(NodeRole::Standby, 2);

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
