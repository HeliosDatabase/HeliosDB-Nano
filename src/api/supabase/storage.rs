//! Supabase Storage Compatible API
//!
//! File storage API compatible with Supabase Storage.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Storage bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bucket {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    pub public: bool,
    pub created_at: String,
    pub updated_at: String,
    pub file_size_limit: Option<u64>,
    pub allowed_mime_types: Option<Vec<String>>,
}

/// File object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileObject {
    pub id: String,
    pub name: String,
    pub bucket_id: String,
    pub owner: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed_at: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// File metadata for listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub name: String,
    pub id: Option<String>,
    pub updated_at: Option<String>,
    pub created_at: Option<String>,
    pub last_accessed_at: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Create bucket request
#[derive(Debug, Clone, Deserialize)]
pub struct CreateBucketRequest {
    pub name: String,
    pub id: Option<String>,
    pub public: Option<bool>,
    pub file_size_limit: Option<u64>,
    pub allowed_mime_types: Option<Vec<String>>,
}

/// Update bucket request
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateBucketRequest {
    pub public: Option<bool>,
    pub file_size_limit: Option<u64>,
    pub allowed_mime_types: Option<Vec<String>>,
}

/// List files options
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListFilesOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub sort_by: Option<SortBy>,
    pub search: Option<String>,
}

/// Sort by options
#[derive(Debug, Clone, Deserialize)]
pub struct SortBy {
    pub column: String,
    pub order: String,
}

/// Upload response
#[derive(Debug, Clone, Serialize)]
pub struct UploadResponse {
    pub id: String,
    pub path: String,
    #[serde(rename = "fullPath")]
    pub full_path: String,
}

/// Signed URL response
#[derive(Debug, Clone, Serialize)]
pub struct SignedUrlResponse {
    #[serde(rename = "signedUrl")]
    pub signed_url: String,
    pub path: String,
    pub token: Option<String>,
}

/// Storage error
#[derive(Debug, Clone, Serialize)]
pub struct StorageError {
    pub status_code: u16,
    pub error: String,
    pub message: String,
}

/// Storage service
pub struct StorageService {
    buckets: HashMap<String, Bucket>,
    objects: HashMap<String, Vec<FileObject>>, // bucket_id -> objects
    base_url: String,
}

impl StorageService {
    pub fn new(base_url: &str) -> Self {
        Self {
            buckets: HashMap::new(),
            objects: HashMap::new(),
            base_url: base_url.to_string(),
        }
    }

    /// Create a new bucket
    pub fn create_bucket(&mut self, request: CreateBucketRequest, owner: Option<&str>) -> Result<Bucket, StorageError> {
        let id = request.id.unwrap_or_else(|| request.name.clone());

        if self.buckets.contains_key(&id) {
            return Err(StorageError {
                status_code: 409,
                error: "Duplicate".to_string(),
                message: "Bucket already exists".to_string(),
            });
        }

        let now = chrono_now();
        let bucket = Bucket {
            id: id.clone(),
            name: request.name,
            owner: owner.map(|s| s.to_string()),
            public: request.public.unwrap_or(false),
            created_at: now.clone(),
            updated_at: now,
            file_size_limit: request.file_size_limit,
            allowed_mime_types: request.allowed_mime_types,
        };

        self.buckets.insert(id.clone(), bucket.clone());
        self.objects.insert(id, Vec::new());

        Ok(bucket)
    }

    /// Get bucket by ID
    pub fn get_bucket(&self, id: &str) -> Result<&Bucket, StorageError> {
        self.buckets.get(id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", id),
        })
    }

    /// List all buckets
    pub fn list_buckets(&self) -> Vec<&Bucket> {
        self.buckets.values().collect()
    }

    /// Update bucket
    pub fn update_bucket(&mut self, id: &str, request: UpdateBucketRequest) -> Result<Bucket, StorageError> {
        let bucket = self.buckets.get_mut(id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", id),
        })?;

        if let Some(public) = request.public {
            bucket.public = public;
        }
        if let Some(limit) = request.file_size_limit {
            bucket.file_size_limit = Some(limit);
        }
        if let Some(types) = request.allowed_mime_types {
            bucket.allowed_mime_types = Some(types);
        }

        bucket.updated_at = chrono_now();
        Ok(bucket.clone())
    }

    /// Delete bucket
    pub fn delete_bucket(&mut self, id: &str) -> Result<(), StorageError> {
        if !self.buckets.contains_key(id) {
            return Err(StorageError {
                status_code: 404,
                error: "Not found".to_string(),
                message: format!("Bucket {} not found", id),
            });
        }

        // Check if bucket is empty
        if let Some(objects) = self.objects.get(id) {
            if !objects.is_empty() {
                return Err(StorageError {
                    status_code: 409,
                    error: "Conflict".to_string(),
                    message: "Bucket is not empty".to_string(),
                });
            }
        }

        self.buckets.remove(id);
        self.objects.remove(id);
        Ok(())
    }

    /// Empty bucket
    pub fn empty_bucket(&mut self, id: &str) -> Result<(), StorageError> {
        if !self.buckets.contains_key(id) {
            return Err(StorageError {
                status_code: 404,
                error: "Not found".to_string(),
                message: format!("Bucket {} not found", id),
            });
        }

        self.objects.insert(id.to_string(), Vec::new());
        Ok(())
    }

    /// Upload file
    pub fn upload(
        &mut self,
        bucket_id: &str,
        path: &str,
        _content: Vec<u8>,
        _content_type: Option<&str>,
        owner: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<UploadResponse, StorageError> {
        // Validate bucket exists
        let bucket = self.get_bucket(bucket_id)?.clone();

        // Check file size limit
        if let Some(limit) = bucket.file_size_limit {
            if _content.len() as u64 > limit {
                return Err(StorageError {
                    status_code: 413,
                    error: "Payload too large".to_string(),
                    message: format!("File exceeds maximum size of {} bytes", limit),
                });
            }
        }

        // Check mime type if restricted
        if let Some(ref allowed) = bucket.allowed_mime_types {
            if let Some(ct) = _content_type {
                if !allowed.iter().any(|t| t == ct || t == "*/*") {
                    return Err(StorageError {
                        status_code: 415,
                        error: "Unsupported media type".to_string(),
                        message: format!("Content type {} not allowed", ct),
                    });
                }
            }
        }

        let now = chrono_now();
        let id = generate_uuid();

        let file = FileObject {
            id: id.clone(),
            name: path.to_string(),
            bucket_id: bucket_id.to_string(),
            owner: owner.map(|s| s.to_string()),
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            metadata,
        };

        // Store file object
        self.objects
            .entry(bucket_id.to_string())
            .or_default()
            .push(file);

        Ok(UploadResponse {
            id,
            path: path.to_string(),
            full_path: format!("{}/{}", bucket_id, path),
        })
    }

    /// Download file (returns file content and metadata)
    pub fn download(&self, bucket_id: &str, path: &str) -> Result<(Vec<u8>, &FileObject), StorageError> {
        let objects = self.objects.get(bucket_id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", bucket_id),
        })?;

        let file = objects.iter().find(|f| f.name == path).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("File {} not found", path),
        })?;

        // Placeholder - would return actual file content
        Ok((Vec::new(), file))
    }

    /// List files in bucket
    pub fn list(
        &self,
        bucket_id: &str,
        prefix: Option<&str>,
        options: ListFilesOptions,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        let objects = self.objects.get(bucket_id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", bucket_id),
        })?;

        let mut files: Vec<FileMetadata> = objects
            .iter()
            .filter(|f| {
                if let Some(p) = prefix {
                    f.name.starts_with(p)
                } else {
                    true
                }
            })
            .filter(|f| {
                if let Some(ref search) = options.search {
                    f.name.contains(search)
                } else {
                    true
                }
            })
            .map(|f| FileMetadata {
                name: f.name.clone(),
                id: Some(f.id.clone()),
                updated_at: Some(f.updated_at.clone()),
                created_at: Some(f.created_at.clone()),
                last_accessed_at: f.last_accessed_at.clone(),
                metadata: f.metadata.clone(),
            })
            .collect();

        // Sort
        if let Some(ref sort) = options.sort_by {
            files.sort_by(|a, b| {
                let cmp = match sort.column.as_str() {
                    "name" => a.name.cmp(&b.name),
                    "created_at" => a.created_at.cmp(&b.created_at),
                    "updated_at" => a.updated_at.cmp(&b.updated_at),
                    _ => std::cmp::Ordering::Equal,
                };

                if sort.order == "desc" {
                    cmp.reverse()
                } else {
                    cmp
                }
            });
        }

        // Pagination
        let offset = options.offset.unwrap_or(0);
        let limit = options.limit.unwrap_or(100);

        let files: Vec<FileMetadata> = files
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect();

        Ok(files)
    }

    /// Move/rename file
    pub fn move_file(
        &mut self,
        bucket_id: &str,
        from_path: &str,
        to_path: &str,
    ) -> Result<(), StorageError> {
        let objects = self.objects.get_mut(bucket_id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", bucket_id),
        })?;

        let file = objects.iter_mut().find(|f| f.name == from_path).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("File {} not found", from_path),
        })?;

        file.name = to_path.to_string();
        file.updated_at = chrono_now();

        Ok(())
    }

    /// Copy file
    pub fn copy_file(
        &mut self,
        bucket_id: &str,
        from_path: &str,
        to_path: &str,
    ) -> Result<String, StorageError> {
        let objects = self.objects.get(bucket_id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", bucket_id),
        })?;

        let source = objects.iter().find(|f| f.name == from_path).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("File {} not found", from_path),
        })?.clone();

        let now = chrono_now();
        let new_id = generate_uuid();

        let new_file = FileObject {
            id: new_id.clone(),
            name: to_path.to_string(),
            bucket_id: bucket_id.to_string(),
            owner: source.owner,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            metadata: source.metadata,
        };

        self.objects
            .entry(bucket_id.to_string())
            .or_default()
            .push(new_file);

        Ok(new_id)
    }

    /// Delete file
    pub fn remove(&mut self, bucket_id: &str, paths: Vec<&str>) -> Result<Vec<FileMetadata>, StorageError> {
        let objects = self.objects.get_mut(bucket_id).ok_or(StorageError {
            status_code: 404,
            error: "Not found".to_string(),
            message: format!("Bucket {} not found", bucket_id),
        })?;

        let mut removed = Vec::new();

        for path in paths {
            if let Some(pos) = objects.iter().position(|f| f.name == path) {
                let file = objects.remove(pos);
                removed.push(FileMetadata {
                    name: file.name,
                    id: Some(file.id),
                    updated_at: Some(file.updated_at),
                    created_at: Some(file.created_at),
                    last_accessed_at: file.last_accessed_at,
                    metadata: file.metadata,
                });
            }
        }

        Ok(removed)
    }

    /// Create signed URL for private file
    pub fn create_signed_url(
        &self,
        bucket_id: &str,
        path: &str,
        expires_in: u64,
    ) -> Result<SignedUrlResponse, StorageError> {
        // Verify file exists
        let _ = self.download(bucket_id, path)?;

        let token = generate_token();
        let signed_url = format!(
            "{}/storage/v1/object/sign/{}/{}?token={}",
            self.base_url, bucket_id, path, token
        );

        let _ = expires_in; // Would be used for token expiration

        Ok(SignedUrlResponse {
            signed_url,
            path: path.to_string(),
            token: Some(token),
        })
    }

    /// Get public URL for public bucket file
    pub fn get_public_url(&self, bucket_id: &str, path: &str) -> Result<String, StorageError> {
        let bucket = self.get_bucket(bucket_id)?;

        if !bucket.public {
            return Err(StorageError {
                status_code: 403,
                error: "Forbidden".to_string(),
                message: "Bucket is not public".to_string(),
            });
        }

        Ok(format!("{}/storage/v1/object/public/{}/{}", self.base_url, bucket_id, path))
    }
}

// Helper functions

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("2024-01-01T{:02}:{:02}:{:02}Z", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

fn generate_uuid() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);

    let hash = hasher.finish();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hash >> 32) as u32,
        (hash >> 16) as u16,
        hash as u16,
        (hash >> 48) as u16,
        hash & 0xFFFFFFFFFFFF
    )
}

fn generate_token() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);

    format!("{:x}{:x}", hasher.finish(), hasher.finish())
}
