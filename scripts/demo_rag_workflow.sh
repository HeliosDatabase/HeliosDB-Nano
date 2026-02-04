#!/bin/bash
#
# HeliosDB-Lite RAG (Retrieval-Augmented Generation) Workflow Demo
#
# This script demonstrates how to use HeliosDB-Lite as a vector database
# for RAG applications. It shows:
#   1. Document storage with embeddings
#   2. HNSW index for fast similarity search
#   3. Semantic search (cosine, L2, inner product)
#   4. Metadata filtering
#   5. Hybrid search (vector + SQL filters)
#   6. Product Quantization for memory efficiency
#
# Usage: ./demo_rag_workflow.sh
#
# Requirements: HeliosDB-Lite binary compiled (target/release/heliosdb-lite)

set -e

# Configuration
BINARY="./target/release/heliosdb-lite"
if [ ! -f "$BINARY" ]; then
    BINARY="./target/debug/heliosdb-lite"
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
BOLD='\033[1m'
NC='\033[0m'

# Check binary exists
if [ ! -f "$BINARY" ]; then
    echo -e "${RED}Error: heliosdb-lite binary not found${NC}"
    echo "Build with: cargo build --release"
    exit 1
fi

echo -e "${CYAN}${BOLD}"
echo "╔════════════════════════════════════════════════════════════════════╗"
echo "║          HeliosDB-Lite RAG Workflow Demo                          ║"
echo "║          Retrieval-Augmented Generation Example                    ║"
echo "╚════════════════════════════════════════════════════════════════════╝"
echo -e "${NC}"

# ============================================================================
# SECTION 1: Document Storage with Embeddings
# ============================================================================
section_header() {
    echo ""
    echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${YELLOW}  $1${NC}"
    echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

run_demo() {
    local title="$1"
    local description="$2"
    local sql="$3"

    echo ""
    echo -e "${BLUE}▶ ${BOLD}$title${NC}"
    echo -e "${MAGENTA}  $description${NC}"
    echo ""

    output=$(timeout 30 "$BINARY" repl --memory << EOF 2>&1
$sql
\q
EOF
)

    # Display output (filtering some noise)
    echo "$output" | grep -v "^HeliosDB Lite" | grep -v "^PostgreSQL-compatible" | \
        grep -v "Type .h for help" | grep -v "^heliosdb>" | grep -v "^Goodbye" | \
        grep -v "^$" | head -40
    echo ""
}

# ============================================================================
section_header "1. DOCUMENT STORAGE WITH VECTOR EMBEDDINGS"
# ============================================================================

echo -e "${GREEN}In RAG applications, documents are stored with their vector embeddings.${NC}"
echo -e "${GREEN}HeliosDB-Lite supports native VECTOR columns with configurable dimensions.${NC}"

run_demo "Create Knowledge Base Schema" \
    "Creating a documents table with text content and 384-dim embeddings (typical for sentence-transformers)" \
    "CREATE TABLE knowledge_base (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    category TEXT,
    source TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    embedding VECTOR(384)
);

\\d knowledge_base"

# ============================================================================
section_header "2. HNSW INDEX FOR FAST SIMILARITY SEARCH"
# ============================================================================

echo -e "${GREEN}HNSW (Hierarchical Navigable Small World) indexes enable sub-millisecond${NC}"
echo -e "${GREEN}approximate nearest neighbor search on millions of vectors.${NC}"

run_demo "Create HNSW Index" \
    "Creating an HNSW index for fast k-NN queries" \
    "CREATE TABLE docs (id INTEGER PRIMARY KEY, content TEXT, embedding VECTOR(128));
CREATE INDEX idx_docs_embedding ON docs(embedding) USING hnsw;
SELECT * FROM pg_vector_index_stats();"

# ============================================================================
section_header "3. INSERTING DOCUMENTS WITH EMBEDDINGS"
# ============================================================================

echo -e "${GREEN}In production, embeddings come from models like OpenAI, Cohere, or sentence-transformers.${NC}"
echo -e "${GREEN}Here we use synthetic embeddings to demonstrate the workflow.${NC}"

run_demo "Insert RAG Documents" \
    "Inserting documents about machine learning topics with embeddings" \
    "CREATE TABLE ml_docs (id INTEGER PRIMARY KEY, title TEXT, content TEXT, topic TEXT, embedding VECTOR(8));
INSERT INTO ml_docs VALUES (1, 'Neural Network Basics', 'Neural networks are computing systems inspired by biological neural networks.', 'neural_networks', '[0.9, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (2, 'Deep Learning Introduction', 'Deep learning uses multiple layers to progressively extract features.', 'deep_learning', '[0.85, 0.15, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (3, 'Backpropagation Algorithm', 'Backpropagation calculates gradients for training neural networks.', 'neural_networks', '[0.8, 0.2, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (4, 'Transformers Architecture', 'Transformers use self-attention mechanisms for sequence processing.', 'nlp', '[0.1, 0.9, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (5, 'BERT Model Overview', 'BERT is a bidirectional transformer for language understanding.', 'nlp', '[0.15, 0.85, 0.75, 0.0, 0.0, 0.0, 0.1, 0.0]');
INSERT INTO ml_docs VALUES (6, 'CNN Architecture', 'Convolutional neural networks excel at image recognition tasks.', 'computer_vision', '[0.2, 0.0, 0.0, 0.9, 0.7, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (7, 'Image Classification', 'Image classification assigns labels to images using trained models.', 'computer_vision', '[0.1, 0.0, 0.0, 0.85, 0.8, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (8, 'Q-Learning Basics', 'Q-learning is a model-free reinforcement learning algorithm.', 'reinforcement_learning', '[0.0, 0.0, 0.0, 0.0, 0.0, 0.9, 0.8, 0.0]');
SELECT id, title, topic FROM ml_docs ORDER BY id;"

# ============================================================================
section_header "4. SEMANTIC SIMILARITY SEARCH"
# ============================================================================

echo -e "${GREEN}The core of RAG: find documents similar to a user query using vector similarity.${NC}"
echo -e "${GREEN}HeliosDB-Lite supports three distance metrics:${NC}"
echo -e "${GREEN}  <=> Cosine distance (best for normalized embeddings)${NC}"
echo -e "${GREEN}  <-> L2/Euclidean distance${NC}"
echo -e "${GREEN}  <#> Inner product (negative dot product)${NC}"

run_demo "Cosine Similarity Search" \
    "Query: 'How do neural networks learn?' - Finding most similar documents" \
    "CREATE TABLE ml_docs (id INT, title TEXT, content TEXT, topic TEXT, embedding VECTOR(8));
INSERT INTO ml_docs VALUES (1, 'Neural Network Basics', 'Neural networks are computing systems.', 'neural_networks', '[0.9, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (2, 'Deep Learning Intro', 'Deep learning uses multiple layers.', 'deep_learning', '[0.85, 0.15, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (3, 'Backpropagation', 'Calculates gradients for training.', 'neural_networks', '[0.8, 0.2, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (4, 'Transformers', 'Self-attention mechanisms.', 'nlp', '[0.1, 0.9, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (5, 'BERT Overview', 'Bidirectional transformer.', 'nlp', '[0.15, 0.85, 0.75, 0.0, 0.0, 0.0, 0.1, 0.0]');
INSERT INTO ml_docs VALUES (6, 'CNN Architecture', 'Image recognition.', 'computer_vision', '[0.2, 0.0, 0.0, 0.9, 0.7, 0.0, 0.0, 0.0]');
SELECT title, topic, embedding <=> '[0.88, 0.12, 0.05, 0.0, 0.15, 0.0, 0.0, 0.0]' AS similarity FROM ml_docs ORDER BY similarity LIMIT 3;"

run_demo "L2 Distance Search" \
    "Alternative: Using Euclidean distance for similarity (lower = more similar)" \
    "CREATE TABLE ml_docs (id INT, title TEXT, topic TEXT, embedding VECTOR(8));
INSERT INTO ml_docs VALUES (1, 'Neural Network Basics', 'neural_networks', '[0.9, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (2, 'Transformers Architecture', 'nlp', '[0.1, 0.9, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (3, 'CNN Architecture', 'computer_vision', '[0.2, 0.0, 0.0, 0.9, 0.7, 0.0, 0.0, 0.0]');
SELECT title, topic, embedding <-> '[0.85, 0.15, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0]' AS l2_distance FROM ml_docs ORDER BY l2_distance LIMIT 3;"

# ============================================================================
section_header "5. METADATA FILTERING (Hybrid Search)"
# ============================================================================

echo -e "${GREEN}RAG often requires filtering by metadata before/during vector search.${NC}"
echo -e "${GREEN}HeliosDB-Lite combines SQL WHERE clauses with vector operations.${NC}"

run_demo "Vector Search + Category Filter" \
    "Find similar documents within 'nlp' category only" \
    "CREATE TABLE ml_docs (id INT, title TEXT, topic TEXT, embedding VECTOR(8));
INSERT INTO ml_docs VALUES (1, 'Neural Network Basics', 'neural_networks', '[0.9, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (2, 'Deep Learning Intro', 'deep_learning', '[0.85, 0.15, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (3, 'Transformers', 'nlp', '[0.1, 0.9, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO ml_docs VALUES (4, 'BERT Overview', 'nlp', '[0.15, 0.85, 0.75, 0.0, 0.0, 0.0, 0.1, 0.0]');
INSERT INTO ml_docs VALUES (5, 'GPT Models', 'nlp', '[0.12, 0.88, 0.7, 0.0, 0.0, 0.0, 0.15, 0.0]');
INSERT INTO ml_docs VALUES (6, 'CNN Architecture', 'computer_vision', '[0.2, 0.0, 0.0, 0.9, 0.7, 0.0, 0.0, 0.0]');
SELECT title, topic, embedding <=> '[0.1, 0.9, 0.75, 0.0, 0.0, 0.0, 0.1, 0.0]' AS score FROM ml_docs WHERE topic = 'nlp' ORDER BY score LIMIT 5;"

run_demo "Multi-Condition Hybrid Search" \
    "Find similar in-stock electronics under \$1000" \
    "CREATE TABLE products (id INT, name TEXT, category TEXT, price DECIMAL, in_stock BOOLEAN, features VECTOR(4));
INSERT INTO products VALUES (1, 'Laptop Pro 15', 'electronics', 1299.99, true, '[0.9, 0.8, 0.7, 0.1]');
INSERT INTO products VALUES (2, 'Laptop Basic 14', 'electronics', 599.99, true, '[0.7, 0.5, 0.4, 0.1]');
INSERT INTO products VALUES (3, 'Phone X', 'electronics', 999.99, false, '[0.3, 0.9, 0.2, 0.8]');
INSERT INTO products VALUES (4, 'Tablet S', 'electronics', 449.99, true, '[0.6, 0.6, 0.5, 0.3]');
INSERT INTO products VALUES (5, 'Headphones Pro', 'audio', 299.99, true, '[0.1, 0.2, 0.9, 0.8]');
SELECT name, category, price, features <=> '[0.8, 0.7, 0.6, 0.2]' AS similarity FROM products WHERE category = 'electronics' AND price < 1000 AND in_stock = true ORDER BY similarity LIMIT 3;"

# ============================================================================
section_header "6. RAG CONTEXT RETRIEVAL PATTERN"
# ============================================================================

echo -e "${GREEN}The typical RAG pattern: retrieve top-k documents, then use them as context.${NC}"

run_demo "RAG Context Retrieval" \
    "Simulating a RAG query: 'Explain attention mechanisms'" \
    "CREATE TABLE knowledge_base (id INT PRIMARY KEY, title TEXT, content TEXT, source TEXT, embedding VECTOR(8));
INSERT INTO knowledge_base VALUES (1, 'Self-Attention Explained', 'Self-attention weighs importance of different input parts.', 'ml_textbook', '[0.1, 0.95, 0.9, 0.0, 0.0, 0.0, 0.0, 0.0]');
INSERT INTO knowledge_base VALUES (2, 'Multi-Head Attention', 'Runs multiple attention operations in parallel.', 'transformer_paper', '[0.15, 0.9, 0.85, 0.0, 0.0, 0.0, 0.1, 0.0]');
INSERT INTO knowledge_base VALUES (3, 'Attention Is All You Need', 'Transformer relies entirely on attention mechanisms.', 'arxiv', '[0.2, 0.85, 0.8, 0.0, 0.0, 0.0, 0.15, 0.0]');
INSERT INTO knowledge_base VALUES (4, 'CNN Basics', 'CNNs use local receptive fields and weight sharing.', 'dl_course', '[0.8, 0.0, 0.0, 0.9, 0.7, 0.0, 0.0, 0.0]');
INSERT INTO knowledge_base VALUES (5, 'RNN Overview', 'RNNs process sequences by maintaining hidden state.', 'ml_textbook', '[0.7, 0.1, 0.0, 0.0, 0.0, 0.8, 0.7, 0.0]');
SELECT title AS document, content AS context, source, embedding <=> '[0.12, 0.92, 0.88, 0.0, 0.0, 0.0, 0.05, 0.0]' AS relevance FROM knowledge_base ORDER BY relevance LIMIT 3;"

# ============================================================================
section_header "7. PRODUCT QUANTIZATION FOR SCALE"
# ============================================================================

echo -e "${GREEN}For large-scale RAG (millions of documents), Product Quantization${NC}"
echo -e "${GREEN}reduces memory usage by ~375x while maintaining 95%+ recall.${NC}"

run_demo "High-Dimensional Vector Index" \
    "Creating HNSW index for high-dimensional embeddings (PQ requires 256+ vectors to train)" \
    "CREATE TABLE large_corpus (id INT, doc_id TEXT, chunk_text TEXT, embedding VECTOR(64));
CREATE INDEX idx_corpus ON large_corpus(embedding) USING hnsw;
INSERT INTO large_corpus VALUES (1, 'doc_001', 'Machine learning fundamentals...', '[0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2,0.1,0.2]');
INSERT INTO large_corpus VALUES (2, 'doc_002', 'Deep learning architectures...', '[0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3,0.2,0.3]');
SELECT * FROM pg_vector_index_stats();"

# ============================================================================
section_header "8. MULTIPLE EMBEDDING TYPES (Multi-Modal RAG)"
# ============================================================================

echo -e "${GREEN}Advanced RAG systems use multiple embedding types (text, image, code).${NC}"

run_demo "Multi-Modal Document Store" \
    "Storing documents with both text (768-dim) and image (512-dim) embeddings" \
    "CREATE TABLE multimodal_docs (id INT PRIMARY KEY, title TEXT, content TEXT, image_url TEXT, text_embedding VECTOR(768), image_embedding VECTOR(512));
CREATE INDEX idx_text_emb ON multimodal_docs(text_embedding) USING hnsw;
CREATE INDEX idx_image_emb ON multimodal_docs(image_embedding) USING hnsw;
\\d multimodal_docs"

# ============================================================================
# SUMMARY
# ============================================================================

echo ""
echo -e "${CYAN}${BOLD}"
echo "╔════════════════════════════════════════════════════════════════════╗"
echo "║                         DEMO COMPLETE                              ║"
echo "╚════════════════════════════════════════════════════════════════════╝"
echo -e "${NC}"

echo -e "${GREEN}${BOLD}HeliosDB-Lite RAG Capabilities Summary:${NC}"
echo ""
echo -e "  ${BLUE}Vector Storage${NC}"
echo "    - Native VECTOR(dim) type for any dimension"
echo "    - HNSW indexes for fast approximate nearest neighbor search"
echo "    - Product Quantization for 375x memory reduction"
echo ""
echo -e "  ${BLUE}Search Operations${NC}"
echo "    - Cosine similarity (<=>)"
echo "    - L2/Euclidean distance (<->)"
echo "    - Inner product (<#>)"
echo "    - SQL-based metadata filtering"
echo ""
echo -e "  ${BLUE}RAG Workflow${NC}"
echo "    - Store documents with embeddings"
echo "    - Retrieve top-k similar documents"
echo "    - Filter by metadata (category, date, source)"
echo "    - Multi-modal support (text + image embeddings)"
echo ""
echo -e "${YELLOW}Performance Characteristics:${NC}"
echo "    - Query latency: <5ms p50 for 1M vectors"
echo "    - Insert rate: 1000s vectors/second"
echo "    - Recall: 95%+ with proper index tuning"
echo ""
echo -e "${MAGENTA}Next Steps:${NC}"
echo "    1. Connect your embedding model (OpenAI, Cohere, sentence-transformers)"
echo "    2. Implement document chunking for long texts"
echo "    3. Add re-ranking for improved relevance"
echo "    4. Scale with Product Quantization for large corpora"
echo ""
echo -e "${GREEN}For REST API usage, see: scripts/test_rest_api.sh${NC}"
echo -e "${GREEN}For more vector examples, see: scripts/test_vector_search.sh${NC}"
