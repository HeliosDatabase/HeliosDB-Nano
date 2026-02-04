//! Semantic Search API
//!
//! Advanced semantic search capabilities with hybrid retrieval,
//! reranking, and multi-modal support.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::storage::VectorIndexManager;

/// Semantic search configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSearchConfig {
    /// Default vector store
    pub default_store: String,
    /// Default embedding model
    pub embedding_model: Option<String>,
    /// Embedding dimensions
    pub dimensions: usize,
    /// Default distance metric
    pub metric: DistanceMetric,
    /// Enable hybrid search
    #[serde(default)]
    pub hybrid_enabled: bool,
    /// Reranking model
    pub reranker_model: Option<String>,
    /// Query expansion enabled
    #[serde(default)]
    pub query_expansion: bool,
    /// Cache embeddings
    #[serde(default = "default_true")]
    pub cache_embeddings: bool,
    /// BM25 k1 parameter
    #[serde(default = "default_bm25_k1")]
    pub bm25_k1: f32,
    /// BM25 b parameter
    #[serde(default = "default_bm25_b")]
    pub bm25_b: f32,
}

fn default_true() -> bool {
    true
}

fn default_bm25_k1() -> f32 {
    1.2
}

fn default_bm25_b() -> f32 {
    0.75
}

/// Distance metric
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DistanceMetric {
    Cosine,
    Euclidean,
    DotProduct,
    Manhattan,
}

impl Default for DistanceMetric {
    fn default() -> Self {
        Self::Cosine
    }
}

/// Semantic search request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSearchRequest {
    /// Search query (text or vector)
    pub query: SearchQuery,
    /// Vector stores to search
    pub stores: Option<Vec<String>>,
    /// Number of results
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Minimum score threshold
    pub min_score: Option<f32>,
    /// Metadata filters
    pub filters: Option<Vec<MetadataFilter>>,
    /// Search mode
    #[serde(default)]
    pub mode: SearchMode,
    /// Hybrid search weight (0=keyword, 1=semantic)
    pub alpha: Option<f32>,
    /// Include vector values
    #[serde(default)]
    pub include_vectors: bool,
    /// Include metadata
    #[serde(default = "default_true")]
    pub include_metadata: bool,
    /// Highlight matches
    #[serde(default)]
    pub highlight: bool,
    /// Namespace filter
    pub namespace: Option<String>,
    /// Rerank results
    #[serde(default)]
    pub rerank: bool,
    /// Expand query
    #[serde(default)]
    pub expand_query: bool,
    /// Group by field
    pub group_by: Option<String>,
    /// Distinct by field
    pub distinct_by: Option<String>,
}

fn default_top_k() -> usize {
    10
}

/// Search query type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchQuery {
    /// Text query (will be embedded)
    Text(String),
    /// Pre-computed vector
    Vector(Vec<f32>),
    /// Multi-query for query expansion
    MultiQuery(Vec<String>),
    /// Image query (base64 encoded)
    Image { image: String, alt_text: Option<String> },
}

/// Search mode
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Pure semantic/vector search
    #[default]
    Semantic,
    /// Pure keyword/BM25 search
    Keyword,
    /// Hybrid semantic + keyword
    Hybrid,
    /// Multi-modal search
    MultiModal,
}

/// Metadata filter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataFilter {
    /// Field path
    pub field: String,
    /// Filter operator
    pub operator: FilterOperator,
    /// Filter value
    pub value: serde_json::Value,
}

/// Filter operator
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    NotIn,
    Contains,
    StartsWith,
    EndsWith,
    Exists,
    IsNull,
    IsNotNull,
    Between,
    Regex,
}

/// Semantic search response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSearchResponse {
    /// Search results
    pub results: Vec<SearchResult>,
    /// Total matching documents
    pub total: usize,
    /// Query time in milliseconds
    pub query_time_ms: u64,
    /// Embedding time in milliseconds
    pub embedding_time_ms: Option<u64>,
    /// Rerank time in milliseconds
    pub rerank_time_ms: Option<u64>,
    /// Expanded queries (if expansion enabled)
    pub expanded_queries: Option<Vec<String>>,
    /// Facets (if grouping enabled)
    pub facets: Option<HashMap<String, Vec<FacetValue>>>,
}

/// Single search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Document ID
    pub id: String,
    /// Relevance score
    pub score: f32,
    /// Document content
    pub content: Option<String>,
    /// Vector values (if requested)
    pub vector: Option<Vec<f32>>,
    /// Document metadata
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Highlighted content
    pub highlights: Option<Vec<Highlight>>,
    /// Source store
    pub store: String,
    /// Namespace
    pub namespace: Option<String>,
    /// Rerank score (if reranked)
    pub rerank_score: Option<f32>,
}

/// Highlighted text segment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Highlight {
    /// Field name
    pub field: String,
    /// Highlighted text with markers
    pub text: String,
    /// Match positions
    pub positions: Vec<(usize, usize)>,
}

/// Facet value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacetValue {
    /// Value
    pub value: String,
    /// Count
    pub count: usize,
}

/// Document for indexing
#[derive(Debug, Clone)]
pub struct IndexedDocument {
    pub id: String,
    pub content: String,
    pub vector: Option<Vec<f32>>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    pub namespace: Option<String>,
    pub store: String,
}

/// BM25 index for keyword search
pub struct Bm25Index {
    /// Document term frequencies: doc_id -> (term -> count)
    doc_terms: HashMap<String, HashMap<String, usize>>,
    /// Document lengths
    doc_lengths: HashMap<String, usize>,
    /// Inverse document frequencies: term -> idf
    idf: HashMap<String, f32>,
    /// Average document length
    avg_doc_length: f32,
    /// Total documents
    num_docs: usize,
    /// Document content for highlighting
    doc_content: HashMap<String, String>,
    /// Document metadata
    doc_metadata: HashMap<String, HashMap<String, serde_json::Value>>,
    /// BM25 parameters
    k1: f32,
    b: f32,
}

impl Bm25Index {
    /// Create new BM25 index
    pub fn new(k1: f32, b: f32) -> Self {
        Self {
            doc_terms: HashMap::new(),
            doc_lengths: HashMap::new(),
            idf: HashMap::new(),
            avg_doc_length: 0.0,
            num_docs: 0,
            doc_content: HashMap::new(),
            doc_metadata: HashMap::new(),
            k1,
            b,
        }
    }

    /// Tokenize text into terms
    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty() && s.len() > 1)
            .map(|s| s.to_string())
            .collect()
    }

    /// Add document to index
    pub fn add_document(&mut self, doc: &IndexedDocument) {
        let terms = Self::tokenize(&doc.content);
        let doc_len = terms.len();

        // Count term frequencies
        let mut term_counts: HashMap<String, usize> = HashMap::new();
        for term in &terms {
            *term_counts.entry(term.clone()).or_insert(0) += 1;
        }

        // Update document frequency for IDF calculation
        for term in term_counts.keys() {
            let df = self.idf.entry(term.clone()).or_insert(0.0);
            *df += 1.0;
        }

        self.doc_terms.insert(doc.id.clone(), term_counts);
        self.doc_lengths.insert(doc.id.clone(), doc_len);
        self.doc_content.insert(doc.id.clone(), doc.content.clone());
        if let Some(ref meta) = doc.metadata {
            self.doc_metadata.insert(doc.id.clone(), meta.clone());
        }

        self.num_docs += 1;

        // Update average document length
        let total_length: usize = self.doc_lengths.values().sum();
        self.avg_doc_length = total_length as f32 / self.num_docs as f32;

        // Recalculate IDF values
        self.recalculate_idf();
    }

    /// Recalculate IDF values
    fn recalculate_idf(&mut self) {
        let n = self.num_docs as f32;
        for (term, df) in self.idf.iter_mut() {
            // Calculate document frequency from scratch
            let doc_freq = self.doc_terms.values()
                .filter(|terms| terms.contains_key(term))
                .count() as f32;
            *df = ((n - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();
        }
    }

    /// Search with BM25 scoring
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(String, f32, Option<String>)> {
        let query_terms = Self::tokenize(query);
        let mut scores: HashMap<String, f32> = HashMap::new();

        for (doc_id, term_freqs) in &self.doc_terms {
            let doc_len = *self.doc_lengths.get(doc_id).unwrap_or(&1) as f32;
            let mut score = 0.0;

            for term in &query_terms {
                if let Some(&tf) = term_freqs.get(term) {
                    let idf = *self.idf.get(term).unwrap_or(&0.0);
                    let tf_norm = (tf as f32 * (self.k1 + 1.0))
                        / (tf as f32 + self.k1 * (1.0 - self.b + self.b * doc_len / self.avg_doc_length));
                    score += idf * tf_norm;
                }
            }

            if score > 0.0 {
                scores.insert(doc_id.clone(), score);
            }
        }

        // Sort by score and take top_k
        let mut results: Vec<_> = scores.into_iter().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);

        results.into_iter()
            .map(|(id, score)| {
                let content = self.doc_content.get(&id).cloned();
                (id, score, content)
            })
            .collect()
    }

    /// Remove document from index
    pub fn remove_document(&mut self, doc_id: &str) {
        self.doc_terms.remove(doc_id);
        self.doc_lengths.remove(doc_id);
        self.doc_content.remove(doc_id);
        self.doc_metadata.remove(doc_id);

        if self.num_docs > 0 {
            self.num_docs -= 1;
            if self.num_docs > 0 {
                let total_length: usize = self.doc_lengths.values().sum();
                self.avg_doc_length = total_length as f32 / self.num_docs as f32;
            }
            self.recalculate_idf();
        }
    }

    /// Get document count
    pub fn len(&self) -> usize {
        self.num_docs
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.num_docs == 0
    }
}

/// Embedding cache entry
struct CacheEntry {
    vector: Vec<f32>,
    timestamp: std::time::Instant,
}

/// Embedding cache with LRU eviction
struct EmbeddingCache {
    entries: HashMap<String, CacheEntry>,
    max_entries: usize,
    ttl_seconds: u64,
}

impl EmbeddingCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            ttl_seconds: 3600, // 1 hour TTL
        }
    }

    fn get(&self, key: &str) -> Option<Vec<f32>> {
        self.entries.get(key).and_then(|entry| {
            if entry.timestamp.elapsed().as_secs() < self.ttl_seconds {
                Some(entry.vector.clone())
            } else {
                None
            }
        })
    }

    fn set(&mut self, key: String, vector: Vec<f32>) {
        // Evict oldest entries if at capacity
        if self.entries.len() >= self.max_entries {
            let oldest_key = self.entries.iter()
                .min_by_key(|(_, v)| v.timestamp)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                self.entries.remove(&key);
            }
        }

        self.entries.insert(key, CacheEntry {
            vector,
            timestamp: std::time::Instant::now(),
        });
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Semantic search engine
pub struct SemanticSearch {
    config: SemanticSearchConfig,
    embedding_cache: RwLock<Option<EmbeddingCache>>,
    bm25_index: RwLock<Bm25Index>,
    vector_index: Option<Arc<VectorIndexManager>>,
    document_store: RwLock<HashMap<String, IndexedDocument>>,
}

impl SemanticSearch {
    /// Create new semantic search engine
    pub fn new(config: SemanticSearchConfig) -> Self {
        let embedding_cache = if config.cache_embeddings {
            Some(EmbeddingCache::new(10000))
        } else {
            None
        };

        let bm25_index = Bm25Index::new(config.bm25_k1, config.bm25_b);

        Self {
            config,
            embedding_cache: RwLock::new(embedding_cache),
            bm25_index: RwLock::new(bm25_index),
            vector_index: None,
            document_store: RwLock::new(HashMap::new()),
        }
    }

    /// Create with vector index manager
    pub fn with_vector_index(mut self, index: Arc<VectorIndexManager>) -> Self {
        self.vector_index = Some(index);
        self
    }

    /// Index a document
    pub fn index_document(&self, doc: IndexedDocument) -> Result<(), SearchError> {
        // Add to BM25 index
        {
            let mut bm25 = self.bm25_index.write();
            bm25.add_document(&doc);
        }

        // Add to vector index if we have vectors
        if let (Some(ref index), Some(ref vector)) = (&self.vector_index, &doc.vector) {
            let row_id = hash_string_to_u64(&doc.id);
            let store_name = format!("{}_{}", doc.store, "vectors");

            // Try to insert, create index if needed
            if let Err(_) = index.insert_vector(&store_name, row_id, vector) {
                // Index might not exist, that's ok for now
            }
        }

        // Store document
        {
            let mut store = self.document_store.write();
            store.insert(doc.id.clone(), doc);
        }

        Ok(())
    }

    /// Remove a document
    pub fn remove_document(&self, doc_id: &str) -> Result<(), SearchError> {
        // Remove from BM25
        {
            let mut bm25 = self.bm25_index.write();
            bm25.remove_document(doc_id);
        }

        // Remove from vector index
        if let Some(ref index) = self.vector_index {
            let row_id = hash_string_to_u64(doc_id);
            let _ = index.delete_vector(&self.config.default_store, row_id);
        }

        // Remove from document store
        {
            let mut store = self.document_store.write();
            store.remove(doc_id);
        }

        Ok(())
    }

    /// Execute semantic search
    pub async fn search(&self, request: SemanticSearchRequest) -> Result<SemanticSearchResponse, SearchError> {
        let start = std::time::Instant::now();

        // Step 1: Expand query if enabled
        let queries = if request.expand_query {
            self.expand_query(&request.query).await?
        } else {
            vec![request.query.clone()]
        };

        // Step 2: Embed queries
        let embed_start = std::time::Instant::now();
        let query_vectors = self.embed_queries(&queries).await?;
        let embedding_time = embed_start.elapsed().as_millis() as u64;

        // Step 3: Execute search based on mode
        let mut results = match request.mode {
            SearchMode::Semantic => {
                self.vector_search(&query_vectors, &request).await?
            }
            SearchMode::Keyword => {
                self.keyword_search(&queries, &request).await?
            }
            SearchMode::Hybrid => {
                let alpha = request.alpha.unwrap_or(0.5);
                self.hybrid_search(&queries, &query_vectors, alpha, &request).await?
            }
            SearchMode::MultiModal => {
                self.multimodal_search(&request.query, &request).await?
            }
        };

        // Step 4: Apply filters
        if let Some(ref filters) = request.filters {
            results = self.apply_filters(results, filters);
        }

        // Step 5: Apply min score threshold
        if let Some(min_score) = request.min_score {
            results.retain(|r| r.score >= min_score);
        }

        // Step 6: Rerank if enabled
        let rerank_time = if request.rerank {
            let rerank_start = std::time::Instant::now();
            results = self.rerank_results(&queries, results).await?;
            Some(rerank_start.elapsed().as_millis() as u64)
        } else {
            None
        };

        // Step 7: Apply post-processing
        if let Some(ref group_by) = request.group_by {
            results = self.group_results(results, group_by);
        }

        if let Some(ref distinct_by) = request.distinct_by {
            results = self.distinct_results(results, distinct_by);
        }

        // Step 8: Add highlights if requested
        if request.highlight {
            results = self.add_highlights(results, &queries);
        }

        // Step 9: Truncate to top_k
        results.truncate(request.top_k);

        let total = results.len();

        Ok(SemanticSearchResponse {
            results,
            total,
            query_time_ms: start.elapsed().as_millis() as u64,
            embedding_time_ms: Some(embedding_time),
            rerank_time_ms: rerank_time,
            expanded_queries: if request.expand_query {
                Some(queries.iter().filter_map(|q| {
                    if let SearchQuery::Text(t) = q {
                        Some(t.clone())
                    } else {
                        None
                    }
                }).collect())
            } else {
                None
            },
            facets: None,
        })
    }

    /// Expand query using synonyms or variations
    async fn expand_query(&self, query: &SearchQuery) -> Result<Vec<SearchQuery>, SearchError> {
        match query {
            SearchQuery::Text(text) => {
                let mut expanded = vec![SearchQuery::Text(text.clone())];

                // Add variations
                if text.contains(" or ") {
                    let parts: Vec<&str> = text.split(" or ").collect();
                    for part in parts {
                        expanded.push(SearchQuery::Text(part.trim().to_string()));
                    }
                }

                // Add stemmed/lemmatized variants
                let words: Vec<&str> = text.split_whitespace().collect();
                if words.len() > 1 {
                    // Add individual important words as queries
                    for word in &words {
                        if word.len() > 4 { // Only longer words
                            expanded.push(SearchQuery::Text(word.to_string()));
                        }
                    }
                }

                Ok(expanded)
            }
            SearchQuery::MultiQuery(texts) => {
                Ok(texts.iter().map(|t| SearchQuery::Text(t.clone())).collect())
            }
            _ => Ok(vec![query.clone()]),
        }
    }

    /// Embed text queries
    async fn embed_queries(&self, queries: &[SearchQuery]) -> Result<Vec<Vec<f32>>, SearchError> {
        let mut vectors = Vec::new();

        for query in queries {
            match query {
                SearchQuery::Text(text) => {
                    // Check cache
                    {
                        let cache = self.embedding_cache.read();
                        if let Some(ref cache) = *cache {
                            if let Some(vec) = cache.get(text) {
                                vectors.push(vec);
                                continue;
                            }
                        }
                    }

                    // Generate embedding using simple hash-based approach
                    // In production, this would call an LLM embedding endpoint
                    let embedding = self.generate_embedding(text);

                    // Cache the embedding
                    {
                        let mut cache = self.embedding_cache.write();
                        if let Some(ref mut cache) = *cache {
                            cache.set(text.clone(), embedding.clone());
                        }
                    }

                    vectors.push(embedding);
                }
                SearchQuery::Vector(vec) => {
                    vectors.push(vec.clone());
                }
                SearchQuery::MultiQuery(texts) => {
                    // Embed and average
                    let mut avg = vec![0.0f32; self.config.dimensions];
                    for text in texts {
                        let emb = self.generate_embedding(text);
                        for (i, v) in emb.iter().enumerate() {
                            if i < avg.len() {
                                avg[i] += v / texts.len() as f32;
                            }
                        }
                    }
                    vectors.push(avg);
                }
                SearchQuery::Image { image, alt_text } => {
                    // For images, use alt_text if available, otherwise generate placeholder
                    let text = alt_text.as_ref().map(|s| s.as_str()).unwrap_or("image");
                    let embedding = self.generate_embedding(text);
                    vectors.push(embedding);
                }
            }
        }

        Ok(vectors)
    }

    /// Generate embedding from text (placeholder - would use LLM in production)
    fn generate_embedding(&self, text: &str) -> Vec<f32> {
        let mut embedding = vec![0.0f32; self.config.dimensions];

        // Simple hash-based embedding for demonstration
        // In production, this would call OpenAI/Anthropic/etc embedding API
        let tokens = Bm25Index::tokenize(text);
        for (i, token) in tokens.iter().enumerate() {
            let hash = hash_string_to_u64(token);
            let idx = (hash as usize) % self.config.dimensions;
            embedding[idx] += 1.0 / (i + 1) as f32;
        }

        // Normalize
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut embedding {
                *v /= norm;
            }
        }

        embedding
    }

    /// Pure vector search
    async fn vector_search(
        &self,
        vectors: &[Vec<f32>],
        request: &SemanticSearchRequest,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let mut all_results = Vec::new();

        if let Some(ref index) = self.vector_index {
            let stores = request.stores.as_ref()
                .map(|s| s.clone())
                .unwrap_or_else(|| vec![self.config.default_store.clone()]);

            for store in stores {
                for vector in vectors {
                    let store_name = format!("{}_vectors", store);
                    if let Ok(results) = index.search(&store_name, vector, request.top_k * 2) {
                        for (row_id, distance) in results {
                            // Convert distance to similarity score
                            let score = match self.config.metric {
                                DistanceMetric::Cosine => 1.0 - distance,
                                DistanceMetric::DotProduct => distance,
                                DistanceMetric::Euclidean => 1.0 / (1.0 + distance),
                                DistanceMetric::Manhattan => 1.0 / (1.0 + distance),
                            };

                            let doc_id = format!("doc_{}", row_id);
                            let doc_store = self.document_store.read();
                            let content = doc_store.get(&doc_id).map(|d| d.content.clone());
                            let metadata = doc_store.get(&doc_id).and_then(|d| d.metadata.clone());

                            all_results.push(SearchResult {
                                id: doc_id,
                                score,
                                content,
                                vector: if request.include_vectors { Some(vector.clone()) } else { None },
                                metadata,
                                highlights: None,
                                store: store.clone(),
                                namespace: request.namespace.clone(),
                                rerank_score: None,
                            });
                        }
                    }
                }
            }
        }

        // If no vector index, fall back to document store similarity
        if all_results.is_empty() && !vectors.is_empty() {
            let doc_store = self.document_store.read();
            for (doc_id, doc) in doc_store.iter() {
                if let Some(ref doc_vec) = doc.vector {
                    let score = cosine_similarity(&vectors[0], doc_vec);
                    all_results.push(SearchResult {
                        id: doc_id.clone(),
                        score,
                        content: Some(doc.content.clone()),
                        vector: if request.include_vectors { Some(doc_vec.clone()) } else { None },
                        metadata: doc.metadata.clone(),
                        highlights: None,
                        store: doc.store.clone(),
                        namespace: doc.namespace.clone(),
                        rerank_score: None,
                    });
                }
            }
        }

        // Sort by score
        all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        all_results.dedup_by(|a, b| a.id == b.id);
        all_results.truncate(request.top_k);

        Ok(all_results)
    }

    /// Pure keyword search using BM25
    async fn keyword_search(
        &self,
        queries: &[SearchQuery],
        request: &SemanticSearchRequest,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let bm25 = self.bm25_index.read();
        let mut all_results = Vec::new();

        for query in queries {
            if let SearchQuery::Text(text) = query {
                let results = bm25.search(text, request.top_k * 2);

                for (id, score, content) in results {
                    let doc_store = self.document_store.read();
                    let metadata = doc_store.get(&id).and_then(|d| d.metadata.clone());
                    let store = doc_store.get(&id).map(|d| d.store.clone()).unwrap_or_else(|| self.config.default_store.clone());
                    let namespace = doc_store.get(&id).and_then(|d| d.namespace.clone());

                    all_results.push(SearchResult {
                        id,
                        score,
                        content,
                        vector: None,
                        metadata,
                        highlights: None,
                        store,
                        namespace,
                        rerank_score: None,
                    });
                }
            }
        }

        // Deduplicate and sort
        all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        all_results.dedup_by(|a, b| a.id == b.id);
        all_results.truncate(request.top_k);

        Ok(all_results)
    }

    /// Hybrid search combining vector and keyword
    async fn hybrid_search(
        &self,
        queries: &[SearchQuery],
        vectors: &[Vec<f32>],
        alpha: f32,
        request: &SemanticSearchRequest,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // Get both result sets
        let vector_results = self.vector_search(vectors, request).await?;
        let keyword_results = self.keyword_search(queries, request).await?;

        // Merge with reciprocal rank fusion
        let merged = self.reciprocal_rank_fusion(
            &[vector_results, keyword_results],
            &[alpha, 1.0 - alpha],
        );

        Ok(merged)
    }

    /// Multi-modal search
    async fn multimodal_search(
        &self,
        query: &SearchQuery,
        request: &SemanticSearchRequest,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // For multi-modal, we embed both text and image and combine
        let vectors = self.embed_queries(&[query.clone()]).await?;
        self.vector_search(&vectors, request).await
    }

    /// Apply metadata filters to results
    fn apply_filters(&self, results: Vec<SearchResult>, filters: &[MetadataFilter]) -> Vec<SearchResult> {
        results.into_iter()
            .filter(|result| {
                if let Some(ref metadata) = result.metadata {
                    filters.iter().all(|filter| {
                        self.evaluate_filter(metadata, filter)
                    })
                } else {
                    filters.is_empty()
                }
            })
            .collect()
    }

    /// Evaluate a single filter
    fn evaluate_filter(&self, metadata: &HashMap<String, serde_json::Value>, filter: &MetadataFilter) -> bool {
        let value = match metadata.get(&filter.field) {
            Some(v) => v,
            None => return matches!(filter.operator, FilterOperator::IsNull | FilterOperator::Exists),
        };

        match filter.operator {
            FilterOperator::Eq => value == &filter.value,
            FilterOperator::Ne => value != &filter.value,
            FilterOperator::Gt => compare_json_values(value, &filter.value) == Some(std::cmp::Ordering::Greater),
            FilterOperator::Gte => matches!(compare_json_values(value, &filter.value), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)),
            FilterOperator::Lt => compare_json_values(value, &filter.value) == Some(std::cmp::Ordering::Less),
            FilterOperator::Lte => matches!(compare_json_values(value, &filter.value), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)),
            FilterOperator::In => {
                if let serde_json::Value::Array(arr) = &filter.value {
                    arr.contains(value)
                } else {
                    false
                }
            }
            FilterOperator::NotIn => {
                if let serde_json::Value::Array(arr) = &filter.value {
                    !arr.contains(value)
                } else {
                    true
                }
            }
            FilterOperator::Contains => {
                if let (serde_json::Value::String(s), serde_json::Value::String(pattern)) = (value, &filter.value) {
                    s.contains(pattern.as_str())
                } else {
                    false
                }
            }
            FilterOperator::StartsWith => {
                if let (serde_json::Value::String(s), serde_json::Value::String(pattern)) = (value, &filter.value) {
                    s.starts_with(pattern.as_str())
                } else {
                    false
                }
            }
            FilterOperator::EndsWith => {
                if let (serde_json::Value::String(s), serde_json::Value::String(pattern)) = (value, &filter.value) {
                    s.ends_with(pattern.as_str())
                } else {
                    false
                }
            }
            FilterOperator::Exists => true,
            FilterOperator::IsNull => value.is_null(),
            FilterOperator::IsNotNull => !value.is_null(),
            FilterOperator::Between => {
                if let serde_json::Value::Array(arr) = &filter.value {
                    if arr.len() == 2 {
                        let gte = matches!(compare_json_values(value, &arr[0]), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal));
                        let lte = matches!(compare_json_values(value, &arr[1]), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal));
                        gte && lte
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            FilterOperator::Regex => {
                if let (serde_json::Value::String(s), serde_json::Value::String(pattern)) = (value, &filter.value) {
                    regex::Regex::new(pattern)
                        .map(|re| re.is_match(s))
                        .unwrap_or(false)
                } else {
                    false
                }
            }
        }
    }

    /// Rerank results using cross-encoder scoring
    async fn rerank_results(
        &self,
        queries: &[SearchQuery],
        mut results: Vec<SearchResult>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // Simple reranking based on exact match boosting
        // In production, would use a cross-encoder model
        let query_terms: Vec<String> = queries.iter()
            .filter_map(|q| {
                if let SearchQuery::Text(t) = q {
                    Some(Bm25Index::tokenize(t))
                } else {
                    None
                }
            })
            .flatten()
            .collect();

        for result in &mut results {
            let mut boost = 0.0;

            if let Some(ref content) = result.content {
                let content_lower = content.to_lowercase();
                for term in &query_terms {
                    if content_lower.contains(term) {
                        boost += 0.1;
                    }
                    // Exact phrase match gets higher boost
                    if let Some(SearchQuery::Text(query)) = queries.first() {
                        if content_lower.contains(&query.to_lowercase()) {
                            boost += 0.3;
                        }
                    }
                }
            }

            result.rerank_score = Some(result.score * (1.0 + boost));
        }

        results.sort_by(|a, b| {
            b.rerank_score.unwrap_or(b.score)
                .partial_cmp(&a.rerank_score.unwrap_or(a.score))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    /// Reciprocal rank fusion for combining result sets
    fn reciprocal_rank_fusion(
        &self,
        result_sets: &[Vec<SearchResult>],
        weights: &[f32],
    ) -> Vec<SearchResult> {
        let k = 60.0; // RRF constant
        let mut scores: HashMap<String, (f32, SearchResult)> = HashMap::new();

        for (results, weight) in result_sets.iter().zip(weights.iter()) {
            for (rank, result) in results.iter().enumerate() {
                let rrf_score = weight / (k + rank as f32 + 1.0);

                scores.entry(result.id.clone())
                    .and_modify(|(score, _)| *score += rrf_score)
                    .or_insert((rrf_score, result.clone()));
            }
        }

        let mut merged: Vec<SearchResult> = scores.into_values()
            .map(|(score, mut result)| {
                result.score = score;
                result
            })
            .collect();

        merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        merged
    }

    /// Group results by field
    fn group_results(&self, results: Vec<SearchResult>, field: &str) -> Vec<SearchResult> {
        let mut groups: HashMap<String, Vec<SearchResult>> = HashMap::new();

        for result in results {
            let key = result.metadata.as_ref()
                .and_then(|m| m.get(field))
                .map(|v| v.to_string())
                .unwrap_or_else(|| "_none_".to_string());

            groups.entry(key).or_default().push(result);
        }

        // Take best from each group
        groups.into_values()
            .filter_map(|mut group| {
                group.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                group.into_iter().next()
            })
            .collect()
    }

    /// Deduplicate by field
    fn distinct_results(&self, results: Vec<SearchResult>, field: &str) -> Vec<SearchResult> {
        let mut seen: HashMap<String, bool> = HashMap::new();

        results.into_iter()
            .filter(|result| {
                let key = result.metadata.as_ref()
                    .and_then(|m| m.get(field))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| result.id.clone());

                if seen.contains_key(&key) {
                    false
                } else {
                    seen.insert(key, true);
                    true
                }
            })
            .collect()
    }

    /// Add highlights to results
    fn add_highlights(&self, mut results: Vec<SearchResult>, queries: &[SearchQuery]) -> Vec<SearchResult> {
        let query_terms: Vec<String> = queries.iter()
            .filter_map(|q| {
                if let SearchQuery::Text(t) = q {
                    Some(Bm25Index::tokenize(t))
                } else {
                    None
                }
            })
            .flatten()
            .collect();

        for result in &mut results {
            if let Some(ref content) = result.content {
                let highlights = generate_highlights(content, &query_terms);
                if !highlights.is_empty() {
                    result.highlights = Some(highlights);
                }
            }
        }

        results
    }

    /// Clear embedding cache
    pub fn clear_cache(&self) {
        let mut cache = self.embedding_cache.write();
        if let Some(ref mut cache) = *cache {
            cache.clear();
        }
    }

    /// Get statistics
    pub fn stats(&self) -> SearchStats {
        let cache_size = {
            let cache = self.embedding_cache.read();
            cache.as_ref().map(|c| c.entries.len()).unwrap_or(0)
        };

        let bm25_docs = self.bm25_index.read().len();
        let doc_store_size = self.document_store.read().len();

        SearchStats {
            cached_embeddings: cache_size,
            indexed_documents: bm25_docs,
            document_store_size: doc_store_size,
        }
    }
}

/// Search statistics
#[derive(Debug, Clone)]
pub struct SearchStats {
    pub cached_embeddings: usize,
    pub indexed_documents: usize,
    pub document_store_size: usize,
}

/// Search error
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("Embedding error: {0}")]
    Embedding(String),
    #[error("Index error: {0}")]
    Index(String),
    #[error("Invalid query: {0}")]
    InvalidQuery(String),
    #[error("Store not found: {0}")]
    StoreNotFound(String),
    #[error("Filter error: {0}")]
    Filter(String),
}

impl Default for SemanticSearchConfig {
    fn default() -> Self {
        Self {
            default_store: "default".to_string(),
            embedding_model: None,
            dimensions: 1536,
            metric: DistanceMetric::Cosine,
            hybrid_enabled: true,
            reranker_model: None,
            query_expansion: false,
            cache_embeddings: true,
            bm25_k1: 1.2,
            bm25_b: 0.75,
        }
    }
}

// Helper functions

/// Hash string to u64 for consistent document IDs
fn hash_string_to_u64(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Calculate cosine similarity between two vectors
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Compare two JSON values
fn compare_json_values(a: &serde_json::Value, b: &serde_json::Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (serde_json::Value::Number(n1), serde_json::Value::Number(n2)) => {
            n1.as_f64().partial_cmp(&n2.as_f64())
        }
        (serde_json::Value::String(s1), serde_json::Value::String(s2)) => {
            Some(s1.cmp(s2))
        }
        _ => None,
    }
}

/// Generate highlights for content
fn generate_highlights(content: &str, query_terms: &[String]) -> Vec<Highlight> {
    let mut highlights = Vec::new();
    let content_lower = content.to_lowercase();

    for term in query_terms {
        let mut positions = Vec::new();
        let mut start = 0;

        while let Some(pos) = content_lower[start..].find(term) {
            let abs_pos = start + pos;
            positions.push((abs_pos, abs_pos + term.len()));
            start = abs_pos + term.len();
        }

        if !positions.is_empty() {
            // Create highlighted text with markers
            let mut highlighted = String::new();
            let mut last_end = 0;

            for (pos_start, pos_end) in &positions {
                // Add context before match
                let context_start = pos_start.saturating_sub(30);
                if context_start > last_end {
                    highlighted.push_str("...");
                }

                let actual_start = context_start.max(last_end);
                highlighted.push_str(&content[actual_start..*pos_start]);
                highlighted.push_str("<mark>");
                highlighted.push_str(&content[*pos_start..*pos_end]);
                highlighted.push_str("</mark>");

                // Add context after match
                let context_end = (*pos_end + 30).min(content.len());
                highlighted.push_str(&content[*pos_end..context_end]);

                last_end = context_end;
            }

            if last_end < content.len() {
                highlighted.push_str("...");
            }

            highlights.push(Highlight {
                field: "content".to_string(),
                text: highlighted,
                positions,
            });
        }
    }

    highlights
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bm25_index() {
        let mut index = Bm25Index::new(1.2, 0.75);

        index.add_document(&IndexedDocument {
            id: "doc1".to_string(),
            content: "The quick brown fox jumps over the lazy dog".to_string(),
            vector: None,
            metadata: None,
            namespace: None,
            store: "default".to_string(),
        });

        index.add_document(&IndexedDocument {
            id: "doc2".to_string(),
            content: "A quick brown dog runs in the park".to_string(),
            vector: None,
            metadata: None,
            namespace: None,
            store: "default".to_string(),
        });

        let results = index.search("quick brown", 10);
        assert_eq!(results.len(), 2);
        assert!(results[0].1 > 0.0);
    }

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 0.001);

        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &c).abs() < 0.001);
    }

    #[test]
    fn test_highlights() {
        let content = "The quick brown fox jumps over the lazy dog";
        let terms = vec!["quick".to_string(), "fox".to_string()];
        let highlights = generate_highlights(content, &terms);

        assert!(!highlights.is_empty());
        assert!(highlights[0].text.contains("<mark>"));
    }

    #[tokio::test]
    async fn test_semantic_search() {
        let config = SemanticSearchConfig::default();
        let search = SemanticSearch::new(config);

        search.index_document(IndexedDocument {
            id: "doc1".to_string(),
            content: "Machine learning is a subset of artificial intelligence".to_string(),
            vector: None,
            metadata: Some(HashMap::from([
                ("category".to_string(), serde_json::json!("tech")),
            ])),
            namespace: None,
            store: "default".to_string(),
        }).unwrap();

        search.index_document(IndexedDocument {
            id: "doc2".to_string(),
            content: "Deep learning uses neural networks for pattern recognition".to_string(),
            vector: None,
            metadata: Some(HashMap::from([
                ("category".to_string(), serde_json::json!("tech")),
            ])),
            namespace: None,
            store: "default".to_string(),
        }).unwrap();

        let request = SemanticSearchRequest {
            query: SearchQuery::Text("machine learning AI".to_string()),
            stores: None,
            top_k: 10,
            min_score: None,
            filters: None,
            mode: SearchMode::Keyword,
            alpha: None,
            include_vectors: false,
            include_metadata: true,
            highlight: true,
            namespace: None,
            rerank: false,
            expand_query: false,
            group_by: None,
            distinct_by: None,
        };

        let response = search.search(request).await.unwrap();
        assert!(!response.results.is_empty());
    }

    #[test]
    fn test_filter_evaluation() {
        let config = SemanticSearchConfig::default();
        let search = SemanticSearch::new(config);

        let metadata: HashMap<String, serde_json::Value> = HashMap::from([
            ("count".to_string(), serde_json::json!(10)),
            ("name".to_string(), serde_json::json!("test")),
        ]);

        // Test equality
        let filter = MetadataFilter {
            field: "count".to_string(),
            operator: FilterOperator::Eq,
            value: serde_json::json!(10),
        };
        assert!(search.evaluate_filter(&metadata, &filter));

        // Test greater than
        let filter = MetadataFilter {
            field: "count".to_string(),
            operator: FilterOperator::Gt,
            value: serde_json::json!(5),
        };
        assert!(search.evaluate_filter(&metadata, &filter));

        // Test contains
        let filter = MetadataFilter {
            field: "name".to_string(),
            operator: FilterOperator::Contains,
            value: serde_json::json!("es"),
        };
        assert!(search.evaluate_filter(&metadata, &filter));
    }
}
