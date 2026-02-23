#!/bin/bash
#
# HeliosDB-Lite HNSW Vector Search Demo
#
# Demonstrates high-performance vector similarity search using HNSW indexes.
# Features:
#   - 500 vectors with 64 dimensions
#   - Category-based metadata filtering
#   - Cosine similarity search
#   - Persistent storage
#
# Usage: ./demo_hnsw_vector_search.sh
#

set -e

# Configuration
BINARY="./target/release/heliosdb-nano"
if [ ! -f "$BINARY" ]; then
    BINARY="./target/debug/heliosdb-nano"
fi

# Data directory (gitignored)
DATA_DIR="./tmp/pq_demo_data"

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
    echo -e "${RED}Error: heliosdb-nano binary not found${NC}"
    echo "Build with: cargo build --release"
    exit 1
fi

echo -e "${CYAN}${BOLD}"
echo "╔════════════════════════════════════════════════════════════════════╗"
echo "║       HeliosDB-Lite HNSW Vector Search Demo                        ║"
echo "║       High-Performance Approximate Nearest Neighbor Search         ║"
echo "╚════════════════════════════════════════════════════════════════════╝"
echo -e "${NC}"

# Setup data directory
echo -e "${YELLOW}Setting up data directory: ${DATA_DIR}${NC}"
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

# Run the demo
echo ""
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${CYAN}  Running HNSW Vector Search Demo${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""

# Execute using persistent database
echo -e "${YELLOW}Initializing database at: ${DATA_DIR}/heliosdb${NC}"

# Phase 1: Create table (no index yet - PQ needs data first for training)
echo -e "${BLUE}Phase 1: Creating table...${NC}"
timeout 60 "$BINARY" repl -d "$DATA_DIR/heliosdb" << 'EOF'
CREATE TABLE pq_documents (id INTEGER PRIMARY KEY, title TEXT, category TEXT, embedding VECTOR(64));
\q
EOF

# Phase 2: Insert all vectors
echo ""
echo -e "${BLUE}Phase 2: Inserting 500 vectors...${NC}"

# Generate inserts only (no CREATE TABLE)
INSERTS_FILE="$DATA_DIR/inserts.sql"
python3 << 'PYGEN' > "$INSERTS_FILE"
import math

for i in range(1, 501):
    category_num = i % 5
    categories = ["science", "technology", "business", "health", "sports"]
    category = categories[category_num]

    # Generate 64-dimensional vector with structure based on category
    vec = []
    for j in range(64):
        base = math.sin((i + j) * 0.1) * 0.3 + category_num * 0.15 + 0.2
        variation = math.cos((i * 7 + j * 13) * 0.05) * 0.1
        value = round(base + variation, 4)
        vec.append(value)

    vec_str = "[" + ",".join(str(v) for v in vec) + "]"
    print(f"INSERT INTO pq_documents VALUES ({i}, 'Document {i}', '{category}', '{vec_str}');")
PYGEN

timeout 120 "$BINARY" repl -d "$DATA_DIR/heliosdb" << EOF
$(cat "$INSERTS_FILE")
\q
EOF

# Phase 3: Create PQ index (now that we have 500 vectors for training)
echo ""
echo -e "${BLUE}Phase 3: Creating Product Quantization index (requires 256+ vectors)...${NC}"
timeout 120 "$BINARY" repl -d "$DATA_DIR/heliosdb" << 'EOF'
CREATE INDEX idx_pq_embedding ON pq_documents(embedding) USING hnsw WITH (quantization='product', pq_subquantizers=8);
\q
EOF

# Phase 4: Verify and run queries
echo ""
echo -e "${BLUE}Phase 4: Verifying data and running similarity searches...${NC}"
timeout 60 "$BINARY" repl -d "$DATA_DIR/heliosdb" << 'EOF'
SELECT COUNT(*) AS total_documents FROM pq_documents;
SELECT category, COUNT(*) AS count FROM pq_documents GROUP BY category ORDER BY category;
SELECT * FROM pg_vector_index_stats();
SELECT id, title, category, embedding <=> '[0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3]' AS distance FROM pq_documents ORDER BY distance LIMIT 10;
SELECT id, title, category, embedding <=> '[0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5]' AS distance FROM pq_documents WHERE category = 'technology' ORDER BY distance LIMIT 5;
\q
EOF

echo ""
echo -e "${CYAN}${BOLD}"
echo "╔════════════════════════════════════════════════════════════════════╗"
echo "║                    DEMO COMPLETE                                   ║"
echo "╚════════════════════════════════════════════════════════════════════╝"
echo -e "${NC}"

echo -e "${GREEN}${BOLD}HNSW Index Benefits:${NC}"
echo ""
echo -e "  ${BLUE}Performance${NC}"
echo "    - O(log n) search complexity"
echo "    - Sub-millisecond queries on millions of vectors"
echo "    - Maintains 95%+ recall@10"
echo ""
echo -e "  ${BLUE}Features Demonstrated${NC}"
echo "    - 64-dimensional vectors with cosine similarity"
echo "    - Category-based metadata filtering"
echo "    - Persistent storage with RocksDB backend"
echo ""
echo -e "  ${BLUE}Product Quantization (Available)${NC}"
echo "    - Use: WITH (quantization='product', pq_subquantizers=8)"
echo "    - Compresses vectors by 32-375x"
echo "    - Best for datasets with 100K+ vectors"
echo "    - Requires 256+ vectors for training"
echo ""
echo -e "${YELLOW}Database persisted at: ${DATA_DIR}/heliosdb${NC}"
echo -e "${YELLOW}To query interactively: ${BINARY} repl -d ${DATA_DIR}/heliosdb${NC}"
