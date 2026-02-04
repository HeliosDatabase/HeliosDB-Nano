//! Hash Synchronization - HA-Dedup
//!
//! Synchronizes content hash metadata across nodes without transferring content.
//! Enables deduplication-aware replication where only missing content is fetched.

use super::content_dedup::{ContentHash, ContentReference};
use super::{ReplicationError, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Hash manifest for a node
#[derive(Debug, Clone)]
pub struct HashManifest {
    /// Node ID
    pub node_id: Uuid,
    /// Content hashes available on this node
    pub hashes: HashSet<ContentHash>,
    /// Hash sizes (for bandwidth estimation)
    pub sizes: HashMap<ContentHash, usize>,
    /// Manifest generation timestamp
    pub generated_at: chrono::DateTime<chrono::Utc>,
    /// Manifest version (for incremental updates)
    pub version: u64,
}

/// Hash delta (changes since last sync)
#[derive(Debug, Clone)]
pub struct HashDelta {
    /// Source node
    pub source: Uuid,
    /// Added hashes
    pub added: Vec<ContentHash>,
    /// Removed hashes
    pub removed: Vec<ContentHash>,
    /// Base version this delta applies to
    pub base_version: u64,
    /// New version after applying delta
    pub new_version: u64,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Sync request for content
#[derive(Debug, Clone)]
pub struct SyncRequest {
    /// Request ID
    pub id: Uuid,
    /// Hashes we need
    pub missing_hashes: Vec<ContentHash>,
    /// Priority order (first = most urgent)
    pub priority_order: Vec<ContentHash>,
    /// Requester node
    pub requester: Uuid,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Sync response with content
#[derive(Debug, Clone)]
pub struct SyncResponse {
    /// Request ID being responded to
    pub request_id: Uuid,
    /// Contents being sent (hash -> data)
    pub contents: HashMap<ContentHash, Vec<u8>>,
    /// Hashes that couldn't be sent (not found, too large, etc.)
    pub unavailable: Vec<ContentHash>,
    /// Sender node
    pub sender: Uuid,
    /// Total bytes in response
    pub total_bytes: usize,
}

/// Sync event for monitoring
#[derive(Debug, Clone)]
pub enum HashSyncEvent {
    /// Manifest received from peer
    ManifestReceived { node_id: Uuid, hash_count: usize },
    /// Delta received from peer
    DeltaReceived { node_id: Uuid, added: usize, removed: usize },
    /// Content sync started
    SyncStarted { missing_count: usize },
    /// Content fetched
    ContentFetched { hash: ContentHash, size: usize, source: Uuid },
    /// Sync completed
    SyncCompleted { fetched_count: usize, total_bytes: u64 },
    /// Sync error
    SyncError { error: String },
}

/// Hash Synchronization Manager
pub struct HashSync {
    /// This node's ID
    node_id: Uuid,
    /// Local hash manifest
    local_manifest: Arc<RwLock<HashManifest>>,
    /// Remote manifests
    remote_manifests: Arc<RwLock<HashMap<Uuid, HashManifest>>>,
    /// Pending sync requests
    pending_requests: Arc<RwLock<HashMap<Uuid, SyncRequest>>>,
    /// Event channel sender
    event_tx: mpsc::Sender<HashSyncEvent>,
    /// Event channel receiver
    event_rx: Option<mpsc::Receiver<HashSyncEvent>>,
    /// Max batch size for sync requests
    max_batch_size: usize,
    /// Max bytes per sync response
    max_bytes_per_response: usize,
}

impl HashSync {
    /// Create a new hash sync manager
    pub fn new(node_id: Uuid) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);

        Self {
            node_id,
            local_manifest: Arc::new(RwLock::new(HashManifest {
                node_id,
                hashes: HashSet::new(),
                sizes: HashMap::new(),
                generated_at: chrono::Utc::now(),
                version: 0,
            })),
            remote_manifests: Arc::new(RwLock::new(HashMap::new())),
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            event_rx: Some(event_rx),
            max_batch_size: 1000,
            max_bytes_per_response: 64 * 1024 * 1024, // 64MB
        }
    }

    /// Configure max batch size
    pub fn with_max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = size;
        self
    }

    /// Configure max bytes per response
    pub fn with_max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes_per_response = bytes;
        self
    }

    /// Register a local hash
    pub async fn register_hash(&self, hash: ContentHash, size: usize) {
        let mut manifest = self.local_manifest.write().await;
        manifest.hashes.insert(hash);
        manifest.sizes.insert(hash, size);
        manifest.version += 1;
        manifest.generated_at = chrono::Utc::now();
    }

    /// Remove a local hash
    pub async fn remove_hash(&self, hash: &ContentHash) {
        let mut manifest = self.local_manifest.write().await;
        manifest.hashes.remove(hash);
        manifest.sizes.remove(hash);
        manifest.version += 1;
        manifest.generated_at = chrono::Utc::now();
    }

    /// Get the local manifest
    pub async fn local_manifest(&self) -> HashManifest {
        self.local_manifest.read().await.clone()
    }

    /// Update remote manifest
    pub async fn update_remote_manifest(&self, manifest: HashManifest) {
        let node_id = manifest.node_id;
        let hash_count = manifest.hashes.len();

        self.remote_manifests.write().await.insert(node_id, manifest);

        let _ = self.event_tx.send(HashSyncEvent::ManifestReceived {
            node_id,
            hash_count,
        }).await;
    }

    /// Apply a delta to remote manifest
    pub async fn apply_delta(&self, delta: HashDelta) -> Result<()> {
        let mut manifests = self.remote_manifests.write().await;

        let manifest = manifests.get_mut(&delta.source).ok_or_else(|| {
            ReplicationError::ContentSync(format!(
                "No manifest for node {}",
                delta.source
            ))
        })?;

        if manifest.version != delta.base_version {
            return Err(ReplicationError::ContentSync(format!(
                "Delta base version {} doesn't match manifest version {}",
                delta.base_version, manifest.version
            )));
        }

        // Apply changes
        for hash in &delta.added {
            manifest.hashes.insert(*hash);
        }
        for hash in &delta.removed {
            manifest.hashes.remove(hash);
        }

        manifest.version = delta.new_version;
        manifest.generated_at = delta.timestamp;

        let _ = self.event_tx.send(HashSyncEvent::DeltaReceived {
            node_id: delta.source,
            added: delta.added.len(),
            removed: delta.removed.len(),
        }).await;

        Ok(())
    }

    /// Create a delta from current local manifest
    pub async fn create_delta(&self, base_version: u64) -> HashDelta {
        let manifest = self.local_manifest.read().await;

        // In a real implementation, we'd track changes since base_version
        // For skeleton, we just send the full set as "added"
        HashDelta {
            source: self.node_id,
            added: manifest.hashes.iter().cloned().collect(),
            removed: vec![],
            base_version,
            new_version: manifest.version,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Find hashes we need from a specific node
    pub async fn find_missing_from(&self, node_id: &Uuid) -> Vec<ContentHash> {
        let local = self.local_manifest.read().await;
        let remotes = self.remote_manifests.read().await;

        if let Some(remote) = remotes.get(node_id) {
            remote
                .hashes
                .difference(&local.hashes)
                .cloned()
                .collect()
        } else {
            vec![]
        }
    }

    /// Find all hashes we're missing across all nodes
    pub async fn find_all_missing(&self) -> HashMap<ContentHash, Vec<Uuid>> {
        let local = self.local_manifest.read().await;
        let remotes = self.remote_manifests.read().await;

        let mut missing: HashMap<ContentHash, Vec<Uuid>> = HashMap::new();

        for (node_id, manifest) in remotes.iter() {
            for hash in manifest.hashes.difference(&local.hashes) {
                missing.entry(*hash).or_default().push(*node_id);
            }
        }

        missing
    }

    /// Create a sync request for missing hashes
    pub async fn create_sync_request(&self, hashes: Vec<ContentHash>) -> SyncRequest {
        let request = SyncRequest {
            id: Uuid::new_v4(),
            missing_hashes: hashes.clone(),
            priority_order: hashes, // Could prioritize based on access patterns
            requester: self.node_id,
            created_at: chrono::Utc::now(),
        };

        self.pending_requests.write().await.insert(request.id, request.clone());

        let _ = self.event_tx.send(HashSyncEvent::SyncStarted {
            missing_count: request.missing_hashes.len(),
        }).await;

        request
    }

    /// Handle a sync request (generate response)
    pub async fn handle_request<F>(&self, request: &SyncRequest, get_content: F) -> SyncResponse
    where
        F: Fn(&ContentHash) -> Option<Vec<u8>>,
    {
        let mut contents = HashMap::new();
        let mut unavailable = Vec::new();
        let mut total_bytes = 0;

        for hash in &request.priority_order {
            if total_bytes >= self.max_bytes_per_response {
                unavailable.push(*hash);
                continue;
            }

            if let Some(content) = get_content(hash) {
                total_bytes += content.len();
                contents.insert(*hash, content);
            } else {
                unavailable.push(*hash);
            }

            if contents.len() >= self.max_batch_size {
                // Add remaining to unavailable
                unavailable.extend(
                    request.priority_order
                        .iter()
                        .skip(contents.len() + unavailable.len())
                        .cloned()
                );
                break;
            }
        }

        SyncResponse {
            request_id: request.id,
            contents,
            unavailable,
            sender: self.node_id,
            total_bytes,
        }
    }

    /// Process a sync response
    pub async fn process_response<F>(&self, response: SyncResponse, store_content: F) -> Result<usize>
    where
        F: Fn(ContentHash, Vec<u8>) -> Result<()>,
    {
        // Remove from pending
        self.pending_requests.write().await.remove(&response.request_id);

        let mut stored = 0;
        let mut total_bytes = 0u64;

        for (hash, content) in response.contents {
            let size = content.len();
            store_content(hash, content)?;

            self.register_hash(hash, size).await;

            let _ = self.event_tx.send(HashSyncEvent::ContentFetched {
                hash,
                size,
                source: response.sender,
            }).await;

            stored += 1;
            total_bytes += size as u64;
        }

        let _ = self.event_tx.send(HashSyncEvent::SyncCompleted {
            fetched_count: stored,
            total_bytes,
        }).await;

        Ok(stored)
    }

    /// Get sync statistics
    pub async fn stats(&self) -> SyncStats {
        let local = self.local_manifest.read().await;
        let remotes = self.remote_manifests.read().await;

        let local_hashes = local.hashes.len();
        let local_bytes: usize = local.sizes.values().sum();

        let mut unique_remote_hashes = HashSet::new();
        let mut total_remote_bytes = 0usize;

        for manifest in remotes.values() {
            for hash in &manifest.hashes {
                unique_remote_hashes.insert(*hash);
                if let Some(&size) = manifest.sizes.get(hash) {
                    total_remote_bytes += size;
                }
            }
        }

        let missing = unique_remote_hashes.difference(&local.hashes).count();

        SyncStats {
            local_hashes,
            local_bytes: local_bytes as u64,
            remote_nodes: remotes.len(),
            unique_remote_hashes: unique_remote_hashes.len(),
            missing_hashes: missing,
            pending_requests: self.pending_requests.read().await.len(),
        }
    }

    /// Take the event receiver
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashSyncEvent>> {
        self.event_rx.take()
    }
}

/// Sync statistics
#[derive(Debug, Clone)]
pub struct SyncStats {
    /// Number of local hashes
    pub local_hashes: usize,
    /// Total local bytes
    pub local_bytes: u64,
    /// Number of remote nodes
    pub remote_nodes: usize,
    /// Unique hashes across all remotes
    pub unique_remote_hashes: usize,
    /// Hashes we're missing
    pub missing_hashes: usize,
    /// Pending sync requests
    pub pending_requests: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(id: u8) -> ContentHash {
        let mut hash = [0u8; 32];
        hash[0] = id;
        hash
    }

    #[tokio::test]
    async fn test_register_hash() {
        let sync = HashSync::new(Uuid::new_v4());

        let hash = make_hash(1);
        sync.register_hash(hash, 1024).await;

        let manifest = sync.local_manifest().await;
        assert!(manifest.hashes.contains(&hash));
        assert_eq!(manifest.sizes.get(&hash), Some(&1024));
    }

    #[tokio::test]
    async fn test_find_missing() {
        let local_id = Uuid::new_v4();
        let remote_id = Uuid::new_v4();

        let sync = HashSync::new(local_id);

        // Register local hash
        let hash1 = make_hash(1);
        sync.register_hash(hash1, 100).await;

        // Create remote manifest with different hash
        let hash2 = make_hash(2);
        let mut remote_hashes = HashSet::new();
        remote_hashes.insert(hash2);

        let remote_manifest = HashManifest {
            node_id: remote_id,
            hashes: remote_hashes,
            sizes: [(hash2, 200)].into_iter().collect(),
            generated_at: chrono::Utc::now(),
            version: 1,
        };

        sync.update_remote_manifest(remote_manifest).await;

        // Find missing
        let missing = sync.find_missing_from(&remote_id).await;
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], hash2);
    }

    #[tokio::test]
    async fn test_sync_request_response() {
        let sync = HashSync::new(Uuid::new_v4());

        // Create request
        let hashes = vec![make_hash(1), make_hash(2)];
        let request = sync.create_sync_request(hashes.clone()).await;

        assert_eq!(request.missing_hashes.len(), 2);

        // Simulate response handling
        let response = sync.handle_request(&request, |hash| {
            Some(vec![hash[0]; 100]) // Return 100 bytes with hash[0] value
        }).await;

        assert_eq!(response.contents.len(), 2);
        assert!(response.unavailable.is_empty());
    }

    #[tokio::test]
    async fn test_delta() {
        let node_id = Uuid::new_v4();
        let sync = HashSync::new(node_id);

        // Register some hashes
        sync.register_hash(make_hash(1), 100).await;
        sync.register_hash(make_hash(2), 200).await;

        // Create delta from version 0
        let delta = sync.create_delta(0).await;

        assert_eq!(delta.source, node_id);
        assert_eq!(delta.added.len(), 2);
        assert!(delta.removed.is_empty());
    }

    #[tokio::test]
    async fn test_stats() {
        let local_id = Uuid::new_v4();
        let sync = HashSync::new(local_id);

        // Add local hashes
        sync.register_hash(make_hash(1), 100).await;
        sync.register_hash(make_hash(2), 200).await;

        let stats = sync.stats().await;
        assert_eq!(stats.local_hashes, 2);
        assert_eq!(stats.local_bytes, 300);
    }
}
