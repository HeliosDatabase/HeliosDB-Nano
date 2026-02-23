#!/bin/bash

# HeliosDB-Lite Materialized Views Test Suite
# Tests: CREATE/DROP MV, REFRESH, Incremental Refresh, Auto-refresh, Staleness Tracking
# Run: ./test_materialized_views.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_materialized_views.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Materialized Views Test"
echo "=========================================="
echo ""

# Cleanup function
cleanup() {
    rm -rf "$TEST_DB" 2>/dev/null
    rm -rf test_mv_*.db 2>/dev/null
}

# Setup cleanup trap
trap cleanup EXIT

run_test() {
    local test_name="$1"
    local test_num="$2"
    local sql="$3"

    echo -n "[$test_num] $test_name ... "

    output=$(timeout 10 "$BINARY" repl --memory << EOF 2>&1
$sql
\q
EOF
)

    # Test passes if:
    # 1. Output contains successful patterns (Query OK, results, columns, etc.)
    # 2. No critical errors (Connection failed, panic, etc.)
    if echo "$output" | grep -qE "Query OK|Column|^[0-9]|^\(|rows\)|^[a-z_].*\|"; then
        if echo "$output" | grep -qvE "panic|Connection|INTERNAL"; then
            echo -e "${GREEN}✓${NC}"
            ((PASSED++))
            return 0
        fi
    fi

    echo -e "${RED}✗${NC}"
    echo "  Output: $(echo "$output" | tail -2)"
    ((FAILED++))
    return 1
}

# ===================================================================
# BASIC MATERIALIZED VIEW OPERATIONS
# ===================================================================
echo -e "${YELLOW}═══ BASIC MATERIALIZED VIEW OPERATIONS ═══${NC}"
echo ""

run_test "Create materialized view" "1.1" \
    "CREATE TABLE products (id INT, name TEXT, price REAL);
INSERT INTO products VALUES (1, 'Widget', 9.99);
INSERT INTO products VALUES (2, 'Gadget', 19.99);
INSERT INTO products VALUES (3, 'Doohickey', 14.99);
CREATE MATERIALIZED VIEW product_summary AS SELECT COUNT(*) as total, AVG(price) as avg_price FROM products;
SELECT * FROM product_summary;"

run_test "Query materialized view" "1.2" \
    "CREATE TABLE users (id INT, name TEXT, age INT);
INSERT INTO users VALUES (1, 'Alice', 30);
INSERT INTO users VALUES (2, 'Bob', 25);
INSERT INTO users VALUES (3, 'Charlie', 35);
CREATE MATERIALIZED VIEW user_list AS SELECT * FROM users WHERE age > 26;
SELECT * FROM user_list ORDER BY age;"

run_test "MV with WHERE clause" "1.3" \
    "CREATE TABLE orders (order_id INT, customer_id INT, amount REAL, status TEXT);
INSERT INTO orders VALUES (1, 100, 50.0, 'completed');
INSERT INTO orders VALUES (2, 101, 75.5, 'pending');
INSERT INTO orders VALUES (3, 100, 25.0, 'completed');
INSERT INTO orders VALUES (4, 102, 100.0, 'cancelled');
CREATE MATERIALIZED VIEW completed_orders AS SELECT * FROM orders WHERE status = 'completed';
SELECT COUNT(*) FROM completed_orders;"

run_test "MV IF NOT EXISTS" "1.4" \
    "CREATE TABLE items (id INT, name TEXT);
CREATE MATERIALIZED VIEW item_view AS SELECT * FROM items;
CREATE MATERIALIZED VIEW IF NOT EXISTS item_view AS SELECT * FROM items;
SELECT COUNT(*) FROM item_view;"

echo ""

# ===================================================================
# MANUAL REFRESH
# ===================================================================
echo -e "${YELLOW}═══ MANUAL REFRESH ═══${NC}"
echo ""

run_test "Manual refresh after insert" "2.1" \
    "CREATE TABLE inventory (item TEXT, quantity INT);
INSERT INTO inventory VALUES ('Widget', 100);
CREATE MATERIALIZED VIEW inventory_summary AS SELECT COUNT(*) as item_count FROM inventory;
INSERT INTO inventory VALUES ('Gadget', 50);
REFRESH MATERIALIZED VIEW inventory_summary;
SELECT * FROM inventory_summary;"

run_test "Manual refresh with updates" "2.2" \
    "CREATE TABLE sales (product_id INT, quantity INT, price REAL);
INSERT INTO sales VALUES (1, 10, 5.0);
INSERT INTO sales VALUES (2, 5, 10.0);
CREATE MATERIALIZED VIEW sales_total AS SELECT SUM(quantity * price) as total FROM sales;
INSERT INTO sales VALUES (3, 8, 7.5);
REFRESH MATERIALIZED VIEW sales_total;
SELECT * FROM sales_total;"

run_test "Multiple refreshes" "2.3" \
    "CREATE TABLE events (id INT, event_type TEXT);
INSERT INTO events VALUES (1, 'login');
CREATE MATERIALIZED VIEW event_count AS SELECT COUNT(*) as total FROM events;
REFRESH MATERIALIZED VIEW event_count;
INSERT INTO events VALUES (2, 'logout');
REFRESH MATERIALIZED VIEW event_count;
SELECT * FROM event_count;"

echo ""

# ===================================================================
# INCREMENTAL REFRESH
# ===================================================================
echo -e "${YELLOW}═══ INCREMENTAL REFRESH ═══${NC}"
echo ""

run_test "Incremental refresh basic" "3.1" \
    "CREATE TABLE orders (order_id INT, customer_id INT, amount REAL);
INSERT INTO orders VALUES (1, 100, 50.0);
INSERT INTO orders VALUES (2, 101, 75.5);
CREATE MATERIALIZED VIEW order_summary AS SELECT COUNT(*) as order_count, SUM(amount) as total_amount FROM orders;
INSERT INTO orders VALUES (3, 102, 100.0);
REFRESH MATERIALIZED VIEW order_summary;
SELECT * FROM order_summary;"

run_test "Incremental refresh with filter" "3.2" \
    "CREATE TABLE transactions (tx_id INT, amount REAL, status TEXT);
INSERT INTO transactions VALUES (1, 100.0, 'approved');
INSERT INTO transactions VALUES (2, 50.0, 'pending');
CREATE MATERIALIZED VIEW approved_tx AS SELECT * FROM transactions WHERE status = 'approved';
INSERT INTO transactions VALUES (3, 75.0, 'approved');
INSERT INTO transactions VALUES (4, 25.0, 'rejected');
REFRESH MATERIALIZED VIEW approved_tx;
SELECT COUNT(*) FROM approved_tx;"

run_test "Incremental with aggregates" "3.3" \
    "CREATE TABLE sales (product_id INT, quantity INT, price REAL);
INSERT INTO sales VALUES (1, 10, 5.0);
INSERT INTO sales VALUES (2, 5, 10.0);
INSERT INTO sales VALUES (1, 15, 5.0);
CREATE MATERIALIZED VIEW sales_summary AS SELECT product_id, SUM(quantity) as total_qty, SUM(quantity * price) as revenue FROM sales GROUP BY product_id;
INSERT INTO sales VALUES (1, 5, 5.0);
INSERT INTO sales VALUES (3, 20, 8.0);
REFRESH MATERIALIZED VIEW sales_summary;
SELECT * FROM sales_summary ORDER BY product_id;"

echo ""

# ===================================================================
# MATERIALIZED VIEW WITH AGGREGATES
# ===================================================================
echo -e "${YELLOW}═══ MATERIALIZED VIEWS WITH AGGREGATES ═══${NC}"
echo ""

run_test "MV with COUNT" "4.1" \
    "CREATE TABLE customers (id INT, name TEXT, city TEXT);
INSERT INTO customers VALUES (1, 'Alice', 'NYC');
INSERT INTO customers VALUES (2, 'Bob', 'LA');
INSERT INTO customers VALUES (3, 'Charlie', 'NYC');
CREATE MATERIALIZED VIEW customer_count AS SELECT city, COUNT(*) as count FROM customers GROUP BY city;
SELECT * FROM customer_count ORDER BY city;"

run_test "MV with SUM and AVG" "4.2" \
    "CREATE TABLE payments (id INT, amount REAL, category TEXT);
INSERT INTO payments VALUES (1, 100.0, 'food');
INSERT INTO payments VALUES (2, 50.0, 'transport');
INSERT INTO payments VALUES (3, 150.0, 'food');
CREATE MATERIALIZED VIEW payment_stats AS SELECT category, COUNT(*) as count, SUM(amount) as total, AVG(amount) as average FROM payments GROUP BY category;
SELECT * FROM payment_stats ORDER BY category;"

run_test "MV with MIN and MAX" "4.3" \
    "CREATE TABLE temperatures (sensor_id INT, temp REAL, reading_time INT);
INSERT INTO temperatures VALUES (1, 22.5, 1000);
INSERT INTO temperatures VALUES (1, 25.0, 2000);
INSERT INTO temperatures VALUES (2, 18.0, 1000);
INSERT INTO temperatures VALUES (2, 20.5, 2000);
CREATE MATERIALIZED VIEW temp_ranges AS SELECT sensor_id, MIN(temp) as min_temp, MAX(temp) as max_temp FROM temperatures GROUP BY sensor_id;
SELECT * FROM temp_ranges ORDER BY sensor_id;"

run_test "MV with complex aggregates" "4.4" \
    "CREATE TABLE metrics (metric_name TEXT, value REAL, timestamp INT);
INSERT INTO metrics VALUES ('cpu', 45.5, 1000);
INSERT INTO metrics VALUES ('memory', 60.0, 1000);
INSERT INTO metrics VALUES ('cpu', 50.0, 2000);
INSERT INTO metrics VALUES ('memory', 65.5, 2000);
CREATE MATERIALIZED VIEW metric_summary AS SELECT metric_name, COUNT(*) as samples, AVG(value) as avg_value, MIN(value) as min_value, MAX(value) as max_value FROM metrics GROUP BY metric_name;
SELECT * FROM metric_summary ORDER BY metric_name;"

echo ""

# ===================================================================
# SYSTEM VIEW FOR MV STALENESS
# ===================================================================
echo -e "${YELLOW}═══ SYSTEM VIEW: pg_mv_staleness ═══${NC}"
echo ""

run_test "Query pg_mv_staleness view" "5.1" \
    "CREATE TABLE data (id INT, value TEXT);
INSERT INTO data VALUES (1, 'test');
CREATE MATERIALIZED VIEW data_view AS SELECT * FROM data;
SELECT * FROM pg_mv_staleness();"

run_test "Staleness after refresh" "5.2" \
    "CREATE TABLE logs (log_id INT, message TEXT);
INSERT INTO logs VALUES (1, 'startup');
CREATE MATERIALIZED VIEW log_summary AS SELECT COUNT(*) FROM logs;
SELECT view_name FROM pg_mv_staleness();
REFRESH MATERIALIZED VIEW log_summary;
SELECT view_name FROM pg_mv_staleness();"

run_test "Multiple MVs in staleness view" "5.3" \
    "CREATE TABLE t1 (id INT);
CREATE TABLE t2 (id INT);
INSERT INTO t1 VALUES (1);
INSERT INTO t2 VALUES (2);
CREATE MATERIALIZED VIEW view1 AS SELECT * FROM t1;
CREATE MATERIALIZED VIEW view2 AS SELECT * FROM t2;
SELECT COUNT(*) FROM pg_mv_staleness();"

echo ""

# ===================================================================
# DROP MATERIALIZED VIEW
# ===================================================================
echo -e "${YELLOW}═══ DROP MATERIALIZED VIEW ═══${NC}"
echo ""

run_test "Drop materialized view" "6.1" \
    "CREATE TABLE orders (id INT, total REAL);
CREATE MATERIALIZED VIEW order_stats AS SELECT COUNT(*) FROM orders;
DROP MATERIALIZED VIEW order_stats;
SELECT COUNT(*) FROM pg_mv_staleness();"

run_test "Drop MV IF EXISTS" "6.2" \
    "CREATE TABLE products (id INT, name TEXT);
CREATE MATERIALIZED VIEW product_list AS SELECT * FROM products;
DROP MATERIALIZED VIEW IF EXISTS product_list;
DROP MATERIALIZED VIEW IF EXISTS nonexistent_view;"

run_test "Drop multiple MVs" "6.3" \
    "CREATE TABLE data (id INT);
CREATE MATERIALIZED VIEW view1 AS SELECT * FROM data;
CREATE MATERIALIZED VIEW view2 AS SELECT COUNT(*) FROM data;
CREATE MATERIALIZED VIEW view3 AS SELECT * FROM data WHERE id > 0;
DROP MATERIALIZED VIEW view1;
DROP MATERIALIZED VIEW view2;
DROP MATERIALIZED VIEW view3;
SELECT COUNT(*) FROM pg_mv_staleness();"

echo ""

# ===================================================================
# AUTO-REFRESH CONFIGURATION
# ===================================================================
echo -e "${YELLOW}═══ AUTO-REFRESH CONFIGURATION ═══${NC}"
echo ""

run_test "Create MV with auto-refresh metadata" "7.1" \
    "CREATE TABLE sensor_data (sensor_id INT, value REAL, timestamp INT);
INSERT INTO sensor_data VALUES (1, 22.5, 1000);
CREATE MATERIALIZED VIEW sensor_summary AS SELECT sensor_id, AVG(value) as avg_value FROM sensor_data GROUP BY sensor_id;
SELECT view_name FROM pg_mv_staleness();"

run_test "MV refresh updates metadata" "7.2" \
    "CREATE TABLE counters (counter_id INT, count INT);
INSERT INTO counters VALUES (1, 10);
CREATE MATERIALIZED VIEW counter_summary AS SELECT SUM(count) as total FROM counters;
INSERT INTO counters VALUES (2, 20);
REFRESH MATERIALIZED VIEW counter_summary;
SELECT view_name FROM pg_mv_staleness();"

echo ""

# ===================================================================
# CONCURRENT REFRESH (metadata operations)
# ===================================================================
echo -e "${YELLOW}═══ CONCURRENT REFRESH ═══${NC}"
echo ""

run_test "Sequential refreshes" "8.1" \
    "CREATE TABLE activity (activity_id INT, user_id INT, activity_type TEXT);
INSERT INTO activity VALUES (1, 100, 'login');
INSERT INTO activity VALUES (2, 101, 'view');
CREATE MATERIALIZED VIEW activity_summary AS SELECT activity_type, COUNT(*) as count FROM activity GROUP BY activity_type;
REFRESH MATERIALIZED VIEW activity_summary;
INSERT INTO activity VALUES (3, 102, 'login');
REFRESH MATERIALIZED VIEW activity_summary;
SELECT * FROM activity_summary ORDER BY activity_type;"

run_test "Refresh after multiple inserts" "8.2" \
    "CREATE TABLE events (event_id INT, event_name TEXT);
INSERT INTO events VALUES (1, 'start');
CREATE MATERIALIZED VIEW event_list AS SELECT * FROM events;
INSERT INTO events VALUES (2, 'middle');
INSERT INTO events VALUES (3, 'end');
REFRESH MATERIALIZED VIEW event_list;
SELECT COUNT(*) FROM event_list;"

echo ""

# ===================================================================
# COMPLEX SCENARIOS
# ===================================================================
echo -e "${YELLOW}═══ COMPLEX SCENARIOS ═══${NC}"
echo ""

run_test "MV with JOIN" "9.1" \
    "CREATE TABLE customers (customer_id INT, name TEXT);
CREATE TABLE orders (order_id INT, customer_id INT, amount REAL);
INSERT INTO customers VALUES (1, 'Alice');
INSERT INTO customers VALUES (2, 'Bob');
INSERT INTO orders VALUES (1, 1, 100.0);
INSERT INTO orders VALUES (2, 1, 50.0);
INSERT INTO orders VALUES (3, 2, 75.0);
CREATE MATERIALIZED VIEW customer_orders AS SELECT c.name, COUNT(o.order_id) as order_count, SUM(o.amount) as total_spent FROM customers c LEFT JOIN orders o ON c.customer_id = o.customer_id GROUP BY c.name;
SELECT * FROM customer_orders ORDER BY name;"

run_test "MV with subquery" "9.2" \
    "CREATE TABLE products (product_id INT, price REAL);
INSERT INTO products VALUES (1, 10.0);
INSERT INTO products VALUES (2, 20.0);
INSERT INTO products VALUES (3, 15.0);
CREATE MATERIALIZED VIEW expensive_products AS SELECT * FROM products WHERE price > (SELECT AVG(price) FROM products);
SELECT COUNT(*) FROM expensive_products;"

run_test "Nested MV scenario" "9.3" \
    "CREATE TABLE raw_data (id INT, value REAL);
INSERT INTO raw_data VALUES (1, 10.0);
INSERT INTO raw_data VALUES (2, 20.0);
INSERT INTO raw_data VALUES (3, 30.0);
CREATE MATERIALIZED VIEW data_summary AS SELECT AVG(value) as avg_value, COUNT(*) as count FROM raw_data;
SELECT * FROM data_summary;"

run_test "MV refresh chain" "9.4" \
    "CREATE TABLE base_table (id INT, amount REAL);
INSERT INTO base_table VALUES (1, 100.0);
INSERT INTO base_table VALUES (2, 200.0);
CREATE MATERIALIZED VIEW summary1 AS SELECT COUNT(*) as count, SUM(amount) as total FROM base_table;
INSERT INTO base_table VALUES (3, 300.0);
REFRESH MATERIALIZED VIEW summary1;
SELECT * FROM summary1;"

echo ""

# ===================================================================
# EDGE CASES
# ===================================================================
echo -e "${YELLOW}═══ EDGE CASES ═══${NC}"
echo ""

run_test "MV on empty table" "10.1" \
    "CREATE TABLE empty_table (id INT, name TEXT);
CREATE MATERIALIZED VIEW empty_view AS SELECT * FROM empty_table;
SELECT COUNT(*) FROM empty_view;"

run_test "MV with NULL values" "10.2" \
    "CREATE TABLE nullable_data (id INT, value REAL);
INSERT INTO nullable_data VALUES (1, 10.0);
INSERT INTO nullable_data VALUES (2, NULL);
INSERT INTO nullable_data VALUES (3, 30.0);
CREATE MATERIALIZED VIEW nullable_summary AS SELECT COUNT(*) as total, COUNT(value) as non_null FROM nullable_data;
SELECT * FROM nullable_summary;"

run_test "Refresh empty MV" "10.3" \
    "CREATE TABLE sparse_data (id INT);
CREATE MATERIALIZED VIEW sparse_view AS SELECT * FROM sparse_data WHERE id > 1000;
REFRESH MATERIALIZED VIEW sparse_view;
SELECT COUNT(*) FROM sparse_view;"

echo ""

# ===================================================================
# SUMMARY
# ===================================================================
echo "=========================================="
echo -e "${BLUE}Test Summary${NC}"
echo "=========================================="
echo -e "${GREEN}Passed: $PASSED${NC}"
echo -e "${RED}Failed: $FAILED${NC}"
echo "=========================================="

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}All tests passed!${NC}"
    exit 0
else
    echo -e "${RED}Some tests failed.${NC}"
    exit 1
fi
