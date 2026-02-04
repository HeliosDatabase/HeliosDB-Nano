//! Content Deduplication - HA-Dedup
//!
//! Provides zero-copy replication for large values using content-addressed storage.
//! When replicating data, only references (hashes) are synced; actual content is
//! fetched on-demand if not already present locally.
//!
//! # How It Works
//!
//! ```text
//! NODE A                                      NODE B
//! ┌─────────────────────────────────┐        ┌─────────────────────────────────┐
//! │ Table: documents                │        │ Table: documents                │
//! │ ┌─────────────────────────────┐ │        │ ┌─────────────────────────────┐ │
//! │ │ id=1, content_hash=0xABC123│ │        │ │ id=1, content_hash=0xABC123│ │
//! │ └─────────────────────────────┘ │        │ └─────────────────────────────┘ │
//! │                                 │        │                                 │
//! │ Content-Addressed Store         │        │ Content-Addressed Store         │
//! │ ┌─────────────────────────────┐ │        │ ┌─────────────────────────────┐ │
//! │ │ 0xABC123 → [10MB blob]      │ │══════► │ │ 0xABC123 → [10MB blob]      │ │
//! │ └─────────────────────────────┘ │  SYNC  │ └─────────────────────────────┘ │
//! └─────────────────────────────────┘  ONCE  └─────────────────────────────────┘
//! ```

use super::{ReplicationError, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Content hash type (Blake3 produces 32-byte hashes)
pub type ContentHash = [u8; 32];

/// Content-addressed entry
#[derive(Debug, Clone)]
pub struct ContentEntry {
    /// Content hash
    pub hash: ContentHash,
    /// Content size in bytes
    pub size: usize,
    /// Reference count (how many rows reference this content)
    pub ref_count: usize,
    /// Is the content stored locally?
    pub is_local: bool,
    /// Node IDs that have this content
    pub locations: HashSet<Uuid>,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Content reference in a row
#[derive(Debug, Clone)]
pub struct ContentReference {
    /// Table name
    pub table: String,
    /// Row ID
    pub row_id: Vec<u8>,
    /// Column name
    pub column: String,
    /// Content hash
    pub hash: ContentHash,
}

/// Deduplication statistics
#[derive(Debug, Clone, Default)]
pub struct DedupStats {
    /// Total content entries
    pub total_entries: usize,
    /// Total bytes stored
    pub total_bytes: u64,
    /// Bytes saved through deduplication
    pub bytes_saved: u64,
    /// Total references to content
    pub total_references: usize,
    /// Average reference count per entry
    pub avg_ref_count: f64,
    /// Entries with multiple references
    pub deduplicated_entries: usize,
}

/// Content request for fetching from remote
#[derive(Debug, Clone)]
pub struct ContentRequest {
    /// Request ID
    pub id: Uuid,
    /// Content hash to fetch
    pub hash: ContentHash,
    /// Requesting node
    pub requester: Uuid,
    /// Priority (higher = more urgent)
    pub priority: u32,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Content fetch result
#[derive(Debug, Clone)]
pub struct ContentFetchResult {
    /// Content hash
    pub hash: ContentHash,
    /// Content data (if fetched)
    pub content: Option<Vec<u8>>,
    /// Source node
    pub source: Uuid,
    /// Fetch duration
    pub fetch_time_ms: u64,
    /// Error message (if failed)
    pub error: Option<String>,
}

/// Content Deduplication Manager
pub struct ContentDedup {
    /// This node's ID
    node_id: Uuid,
    /// Content index (hash -> entry)
    index: Arc<RwLock<HashMap<ContentHash, ContentEntry>>>,
    /// Pending fetch requests
    pending_requests: Arc<RwLock<HashMap<ContentHash, ContentRequest>>>,
    /// Local content store (in-memory for now, would be backed by storage)
    local_store: Arc<RwLock<HashMap<ContentHash, Vec<u8>>>>,
    /// References to content (for reference counting)
    references: Arc<RwLock<Vec<ContentReference>>>,
}

impl ContentDedup {
    /// Create a new content deduplication manager
    pub fn new(node_id: Uuid) -> Self {
        Self {
            node_id,
            index: Arc::new(RwLock::new(HashMap::new())),
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
            local_store: Arc::new(RwLock::new(HashMap::new())),
            references: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Store content and return its hash
    pub async fn store(&self, content: &[u8]) -> Result<ContentHash> {
        let hash = self.compute_hash(content);

        let mut index = self.index.write().await;
        let mut store = self.local_store.write().await;

        if let Some(entry) = index.get_mut(&hash) {
            // Content already exists, increment ref count
            entry.ref_count += 1;
            return Ok(hash);
        }

        // Store new content
        store.insert(hash, content.to_vec());

        let mut locations = HashSet::new();
        locations.insert(self.node_id);

        index.insert(hash, ContentEntry {
            hash,
            size: content.len(),
            ref_count: 1,
            is_local: true,
            locations,
            created_at: chrono::Utc::now(),
        });

        Ok(hash)
    }

    /// Get content by hash
    pub async fn get(&self, hash: &ContentHash) -> Option<Vec<u8>> {
        self.local_store.read().await.get(hash).cloned()
    }

    /// Check if content exists locally
    pub async fn has_local(&self, hash: &ContentHash) -> bool {
        self.local_store.read().await.contains_key(hash)
    }

    /// Register a content reference
    pub async fn add_reference(&self, reference: ContentReference) -> Result<()> {
        let hash = reference.hash;

        // Update ref count
        {
            let mut index = self.index.write().await;
            if let Some(entry) = index.get_mut(&hash) {
                entry.ref_count += 1;
            } else {
                // Content not in index yet - create placeholder
                index.insert(hash, ContentEntry {
                    hash,
                    size: 0,
                    ref_count: 1,
                    is_local: false,
                    locations: HashSet::new(),
                    created_at: chrono::Utc::now(),
                });
            }
        }

        // Store reference
        self.references.write().await.push(reference);

        Ok(())
    }

    /// Remove a content reference
    pub async fn remove_reference(&self, hash: &ContentHash) -> Result<bool> {
        let mut index = self.index.write().await;

        if let Some(entry) = index.get_mut(hash) {
            entry.ref_count = entry.ref_count.saturating_sub(1);

            if entry.ref_count == 0 {
                // No more references - can be garbage collected
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Register that a remote node has content
    pub async fn register_remote_location(&self, hash: &ContentHash, node_id: Uuid) {
        let mut index = self.index.write().await;
        if let Some(entry) = index.get_mut(hash) {
            entry.locations.insert(node_id);
        } else {
            let mut locations = HashSet::new();
            locations.insert(node_id);

            index.insert(*hash, ContentEntry {
                hash: *hash,
                size: 0,
                ref_count: 0,
                is_local: false,
                locations,
                created_at: chrono::Utc::now(),
            });
        }
    }

    /// Request content from a remote node
    pub async fn request_content(&self, hash: ContentHash, priority: u32) -> Result<ContentRequest> {
        // Check if already requested
        let pending = self.pending_requests.read().await;
        if let Some(request) = pending.get(&hash) {
            return Ok(request.clone());
        }
        drop(pending);

        let request = ContentRequest {
            id: Uuid::new_v4(),
            hash,
            requester: self.node_id,
            priority,
            created_at: chrono::Utc::now(),
        };

        self.pending_requests.write().await.insert(hash, request.clone());

        Ok(request)
    }

    /// Complete a content fetch
    pub async fn complete_fetch(&self, result: ContentFetchResult) -> Result<()> {
        // Remove from pending
        self.pending_requests.write().await.remove(&result.hash);

        if let Some(content) = result.content {
            // Verify hash
            let computed_hash = self.compute_hash(&content);
            if computed_hash != result.hash {
                return Err(ReplicationError::ContentSync(
                    "Content hash mismatch".to_string(),
                ));
            }

            // Store content
            self.local_store.write().await.insert(result.hash, content.clone());

            // Update index
            let mut index = self.index.write().await;
            if let Some(entry) = index.get_mut(&result.hash) {
                entry.is_local = true;
                entry.size = content.len();
                entry.locations.insert(self.node_id);
            }
        }

        Ok(())
    }

    /// Get pending content requests
    pub async fn pending_requests(&self) -> Vec<ContentRequest> {
        self.pending_requests.read().await.values().cloned().collect()
    }

    /// Get nodes that have specific content
    pub async fn get_content_locations(&self, hash: &ContentHash) -> Vec<Uuid> {
        self.index
            .read()
            .await
            .get(hash)
            .map(|e| e.locations.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get hashes that need to be fetched
    pub async fn missing_content(&self) -> Vec<ContentHash> {
        self.index
            .read()
            .await
            .iter()
            .filter(|(_, entry)| !entry.is_local && entry.ref_count > 0)
            .map(|(hash, _)| *hash)
            .collect()
    }

    /// Get hashes that can be shared with other nodes
    pub async fn shareable_content(&self) -> Vec<ContentHash> {
        self.index
            .read()
            .await
            .iter()
            .filter(|(_, entry)| entry.is_local)
            .map(|(hash, _)| *hash)
            .collect()
    }

    /// Compute deduplication statistics
    pub async fn stats(&self) -> DedupStats {
        let index = self.index.read().await;
        let store = self.local_store.read().await;

        let total_entries = index.len();
        let total_bytes: u64 = store.values().map(|v| v.len() as u64).sum();
        let total_references: usize = index.values().map(|e| e.ref_count).sum();

        let deduplicated_entries = index.values().filter(|e| e.ref_count > 1).count();

        // Bytes saved = (total_refs - entries) * avg_size
        let bytes_saved = if total_entries > 0 && total_references > total_entries {
            let avg_size = total_bytes as f64 / total_entries as f64;
            ((total_references - total_entries) as f64 * avg_size) as u64
        } else {
            0
        };

        let avg_ref_count = if total_entries > 0 {
            total_references as f64 / total_entries as f64
        } else {
            0.0
        };

        DedupStats {
            total_entries,
            total_bytes,
            bytes_saved,
            total_references,
            avg_ref_count,
            deduplicated_entries,
        }
    }

    /// Garbage collect unreferenced content
    pub async fn garbage_collect(&self) -> usize {
        let mut index = self.index.write().await;
        let mut store = self.local_store.write().await;

        let to_remove: Vec<ContentHash> = index
            .iter()
            .filter(|(_, entry)| entry.ref_count == 0)
            .map(|(hash, _)| *hash)
            .collect();

        for hash in &to_remove {
            index.remove(hash);
            store.remove(hash);
        }

        to_remove.len()
    }

    /// Compute hash of content (using Blake3)
    fn compute_hash(&self, content: &[u8]) -> ContentHash {
        // In production, use blake3::hash(content).as_bytes()
        // For skeleton, use simple hash
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let hash64 = hasher.finish();

        let mut hash = [0u8; 32];
        hash[0..8].copy_from_slice(&hash64.to_le_bytes());
        hash[8..16].copy_from_slice(&hash64.to_be_bytes());
        hash[16..24].copy_from_slice(&(content.len() as u64).to_le_bytes());
        hash[24..32].copy_from_slice(&hash64.wrapping_mul(31).to_le_bytes());
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_and_get() {
        let dedup = ContentDedup::new(Uuid::new_v4());

        let content = b"Hello, World!";
        let hash = dedup.store(content).await.expect("store failed");

        let retrieved = dedup.get(&hash).await.expect("content not found");
        assert_eq!(retrieved, content);
    }

    #[tokio::test]
    async fn test_deduplication() {
        let dedup = ContentDedup::new(Uuid::new_v4());

        let content = b"Duplicate content";

        // Store same content twice
        let hash1 = dedup.store(content).await.expect("store failed");
        let hash2 = dedup.store(content).await.expect("store failed");

        // Hashes should be identical
        assert_eq!(hash1, hash2);

        // Ref count should be 2
        let index = dedup.index.read().await;
        let entry = index.get(&hash1).expect("entry not found");
        assert_eq!(entry.ref_count, 2);
    }

    #[tokio::test]
    async fn test_reference_counting() {
        let dedup = ContentDedup::new(Uuid::new_v4());

        let content = b"Referenced content";
        let hash = dedup.store(content).await.expect("store failed");

        // Add references
        dedup.add_reference(ContentReference {
            table: "docs".to_string(),
            row_id: vec![1],
            column: "content".to_string(),
            hash,
        }).await.expect("add ref failed");

        // Ref count should be 2 (1 from store, 1 from add_reference)
        let index = dedup.index.read().await;
        assert_eq!(index.get(&hash).unwrap().ref_count, 2);
        drop(index);

        // Remove references
        dedup.remove_reference(&hash).await.expect("remove ref failed");
        let can_gc = dedup.remove_reference(&hash).await.expect("remove ref failed");
        assert!(can_gc); // Should be ready for GC
    }

    #[tokio::test]
    async fn test_stats() {
        let dedup = ContentDedup::new(Uuid::new_v4());

        // Store some content
        dedup.store(b"Content A").await.expect("store failed");
        dedup.store(b"Content B").await.expect("store failed");
        dedup.store(b"Content A").await.expect("store failed"); // Duplicate

        let stats = dedup.stats().await;
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.total_references, 3);
        assert_eq!(stats.deduplicated_entries, 1);
        assert!(stats.bytes_saved > 0);
    }

    #[tokio::test]
    async fn test_garbage_collection() {
        let dedup = ContentDedup::new(Uuid::new_v4());

        let content = b"Temporary content";
        let hash = dedup.store(content).await.expect("store failed");

        // Remove the reference
        dedup.remove_reference(&hash).await.expect("remove failed");

        // GC should remove it
        let removed = dedup.garbage_collect().await;
        assert_eq!(removed, 1);

        // Content should be gone
        assert!(dedup.get(&hash).await.is_none());
    }
}
