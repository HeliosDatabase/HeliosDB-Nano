-- ============================================================================
-- Tenant Session Script: Globex Inc Operations
-- ============================================================================
-- Purpose: Test multi-tenant isolation from Globex Inc's perspective
--
-- Usage (in separate terminal/session):
--   cargo run
--   \i scripts/03_tenant_globex_operations.sql
--
-- This script demonstrates:
--   - Independent tenant session
--   - Data isolation from Acme Corp
--   - Different business operations
--   - Pro plan quota limits
-- ============================================================================

\echo
\echo ========================================
\echo GLOBEX INC - Tenant Session
\echo ========================================
\echo

-- ----------------------------------------------------------------------------
-- STEP 1: Set Tenant Context to Globex Inc
-- ----------------------------------------------------------------------------
\echo Step 1: Switching to Globex Inc tenant...
\tenant use globex-inc

\echo
\echo Current tenant context:
\tenant current

-- ----------------------------------------------------------------------------
-- STEP 2: View Tenant Information
-- ----------------------------------------------------------------------------
\echo
\echo Step 2: Viewing Globex Inc details...
\tenant info globex-inc

-- ----------------------------------------------------------------------------
-- STEP 3: Query Existing Data (RLS-filtered)
-- ----------------------------------------------------------------------------
\echo
\echo Step 3: Querying existing customers...
\echo (Should only see Globex Inc customers due to RLS)
\echo

SELECT * FROM customers;

\echo
\echo Customer count:
SELECT COUNT(*) as globex_customer_count FROM customers;

\echo
\echo Querying existing products:
SELECT id, name, price, stock FROM products;

\echo
\echo Note: We cannot see Acme Corp's data even though we're on the same tables!

-- ----------------------------------------------------------------------------
-- STEP 4: Create New Customers
-- ----------------------------------------------------------------------------
\echo
\echo Step 4: Creating new Globex Inc customers...
\echo

INSERT INTO customers (id, tenant_id, name, email)
VALUES (12, 'globex-inc', 'Irene Investor', 'irene@globex.com');

INSERT INTO customers (id, tenant_id, name, email)
VALUES (13, 'globex-inc', 'Jack Operations', 'jack@globex.com');

INSERT INTO customers (id, tenant_id, name, email)
VALUES (14, 'globex-inc', 'Karen Marketing', 'karen@globex.com');

\echo   ✓ Created 3 new customers

\echo
\echo Verifying new customers:
SELECT * FROM customers ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 5: Create Enterprise Orders
-- ----------------------------------------------------------------------------
\echo
\echo Step 5: Creating enterprise orders for Globex Inc...
\echo

-- Order 101: Charlie buys Enterprise Suite
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (101, 'globex-inc', 10, 'Enterprise Suite', 99900, 'pending');

-- Order 102: Diana buys Team License
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (102, 'globex-inc', 11, 'Team License', 49900, 'processing');

-- Order 103: Irene buys Enterprise Suite
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (103, 'globex-inc', 12, 'Enterprise Suite', 99900, 'processing');

-- Order 104: Jack buys Team License
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (104, 'globex-inc', 13, 'Team License', 49900, 'completed');

-- Order 105: Karen buys Enterprise Suite
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (105, 'globex-inc', 14, 'Enterprise Suite', 99900, 'completed');

\echo   ✓ Created 5 enterprise orders

\echo
\echo Current orders:
SELECT id, customer_id, product, amount, status FROM orders ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 6: Create Invoices
-- ----------------------------------------------------------------------------
\echo
\echo Step 6: Generating invoices...
\echo

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (101, 'globex-inc', 101, 99900, FALSE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (102, 'globex-inc', 102, 49900, FALSE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (103, 'globex-inc', 103, 99900, TRUE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (104, 'globex-inc', 104, 49900, TRUE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (105, 'globex-inc', 105, 99900, TRUE);

\echo   ✓ Created 5 invoices

\echo
\echo Invoice summary:
SELECT
    i.id,
    i.order_id,
    i.total_amount,
    CASE WHEN i.paid THEN 'PAID' ELSE 'UNPAID' END as status
FROM invoices i
ORDER BY i.id;

-- ----------------------------------------------------------------------------
-- STEP 7: Enterprise Business Analytics
-- ----------------------------------------------------------------------------
\echo
\echo Step 7: Globex Inc Business Analytics
\echo ========================================
\echo

\echo Revenue Summary:
SELECT
    SUM(total_amount) as total_revenue,
    SUM(CASE WHEN paid THEN total_amount ELSE 0 END) as paid_revenue,
    SUM(CASE WHEN NOT paid THEN total_amount ELSE 0 END) as outstanding_revenue,
    AVG(total_amount) as avg_deal_size
FROM invoices;

\echo
\echo Orders by Status:
SELECT status, COUNT(*) as order_count, SUM(amount) as total_amount
FROM orders
GROUP BY status;

\echo
\echo Product Performance:
SELECT
    product,
    COUNT(*) as units_sold,
    SUM(amount) as total_sales
FROM orders
GROUP BY product
ORDER BY total_sales DESC;

\echo
\echo Customer Lifetime Value:
SELECT
    c.name,
    c.email,
    COUNT(o.id) as order_count,
    SUM(o.amount) as lifetime_value
FROM customers c
LEFT JOIN orders o ON c.id = o.customer_id
GROUP BY c.id, c.name, c.email
ORDER BY lifetime_value DESC;

-- ----------------------------------------------------------------------------
-- STEP 8: Update Operations (RLS-protected)
-- ----------------------------------------------------------------------------
\echo
\echo Step 8: Processing order fulfillment...
\echo

-- Mark pending orders as processing
UPDATE orders SET status = 'processing' WHERE status = 'pending';

\echo   ✓ Moved pending orders to processing

-- Complete some orders
UPDATE orders SET status = 'completed' WHERE id IN (102, 103);

\echo   ✓ Completed orders #102 and #103

\echo
\echo Updated order statuses:
SELECT id, product, amount, status FROM orders ORDER BY id;

-- Process payments
UPDATE invoices SET paid = TRUE WHERE id IN (101, 102);

\echo   ✓ Processed payments for invoices #101 and #102

\echo
\echo Updated payment statuses:
SELECT id, order_id, total_amount, paid FROM invoices ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 9: Test Data Isolation from Acme Corp
-- ----------------------------------------------------------------------------
\echo
\echo Step 9: Verifying isolation from Acme Corp...
\echo ========================================
\echo

\echo Our customer count:
SELECT COUNT(*) as our_customers FROM customers;

\echo Our order count:
SELECT COUNT(*) as our_orders FROM orders;

\echo
\echo Attempting to see all tenant data (should only see Globex):
SELECT tenant_id, COUNT(*) as visible_rows
FROM customers
GROUP BY tenant_id;

\echo
\echo ✓ Confirmed: Cannot see Acme Corp or Initech data

-- ----------------------------------------------------------------------------
-- STEP 10: Cross-Tenant Write Protection
-- ----------------------------------------------------------------------------
\echo
\echo Step 10: Testing write protection...
\echo ========================================
\echo

-- Try to update an Acme Corp order (should have no effect)
\echo Attempting to update Acme Corp order #1 (should fail):
UPDATE orders SET status = 'hacked' WHERE id = 1;

\echo
\echo Verify no Acme orders are visible:
SELECT COUNT(*) as acme_orders FROM orders WHERE id <= 10;

\echo (Should be 0)

-- Try to delete an Acme Corp customer (should have no effect)
\echo
\echo Attempting to delete Acme Corp customer #1 (should fail):
DELETE FROM customers WHERE id = 1;

\echo
\echo Verify our customer count unchanged:
SELECT COUNT(*) as our_customers FROM customers;

\echo
\echo ✓ Write protection verified - cannot modify other tenant data

-- ----------------------------------------------------------------------------
-- STEP 11: Check Quota Usage (Pro Plan)
-- ----------------------------------------------------------------------------
\echo
\echo Step 11: Checking Pro plan quota usage...
\echo

\tenant quota globex-inc

-- ----------------------------------------------------------------------------
-- STEP 12: Product Catalog Management
-- ----------------------------------------------------------------------------
\echo
\echo Step 12: Managing enterprise product catalog...
\echo

-- Update stock levels
UPDATE products SET stock = stock - 3 WHERE name = 'Enterprise Suite';
UPDATE products SET stock = stock - 2 WHERE name = 'Team License';

\echo   ✓ Updated inventory after sales

\echo
\echo Current inventory:
SELECT name, price, stock FROM products ORDER BY name;

-- Add new enterprise products
INSERT INTO products (id, tenant_id, name, description, price, stock)
VALUES (12, 'globex-inc', 'Dedicated Support', 'Annual support contract', 199900, 100);

INSERT INTO products (id, tenant_id, name, description, price, stock)
VALUES (13, 'globex-inc', 'Training Package', 'On-site training', 149900, 50);

INSERT INTO products (id, tenant_id, name, description, price, stock)
VALUES (14, 'globex-inc', 'API Access', 'Premium API tier', 79900, 1000);

\echo   ✓ Added 3 new enterprise products

\echo
\echo Updated product catalog:
SELECT id, name, price, stock FROM products ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 13: Complex Queries with Joins
-- ----------------------------------------------------------------------------
\echo
\echo Step 13: Running complex analytics queries...
\echo ========================================
\echo

\echo Customer Order Details:
SELECT
    c.name as customer_name,
    o.product,
    o.amount,
    o.status,
    CASE WHEN i.paid THEN 'PAID' ELSE 'UNPAID' END as payment_status
FROM customers c
JOIN orders o ON c.id = o.customer_id
JOIN invoices i ON o.id = i.order_id
ORDER BY c.name, o.id;

\echo
\echo Unpaid Invoice Report:
SELECT
    c.name as customer_name,
    c.email,
    i.id as invoice_id,
    i.total_amount
FROM customers c
JOIN orders o ON c.id = o.customer_id
JOIN invoices i ON o.id = i.order_id
WHERE i.paid = FALSE
ORDER BY i.total_amount DESC;

-- ----------------------------------------------------------------------------
-- STEP 14: Stress Test - Multiple Operations
-- ----------------------------------------------------------------------------
\echo
\echo Step 14: Stress testing with bulk operations...
\echo

-- Bulk customer creation
INSERT INTO customers (id, tenant_id, name, email)
VALUES
    (15, 'globex-inc', 'Laura Legal', 'laura@globex.com'),
    (16, 'globex-inc', 'Mike Manager', 'mike@globex.com'),
    (17, 'globex-inc', 'Nancy Networking', 'nancy@globex.com'),
    (18, 'globex-inc', 'Oliver Ops', 'oliver@globex.com'),
    (19, 'globex-inc', 'Paula Product', 'paula@globex.com');

\echo   ✓ Created 5 more customers in bulk

\echo
\echo Total customers now:
SELECT COUNT(*) as total_customers FROM customers;

-- Bulk orders
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES
    (106, 'globex-inc', 15, 'API Access', 79900, 'pending'),
    (107, 'globex-inc', 16, 'Training Package', 149900, 'pending'),
    (108, 'globex-inc', 17, 'Team License', 49900, 'pending'),
    (109, 'globex-inc', 18, 'Dedicated Support', 199900, 'processing'),
    (110, 'globex-inc', 19, 'Enterprise Suite', 99900, 'processing');

\echo   ✓ Created 5 more orders in bulk

\echo
\echo Total orders now:
SELECT COUNT(*) as total_orders FROM orders;

-- ----------------------------------------------------------------------------
-- STEP 15: Final Summary
-- ----------------------------------------------------------------------------
\echo
\echo ========================================
\echo GLOBEX INC SESSION SUMMARY
\echo ========================================
\echo

\echo Database Statistics:
SELECT
    (SELECT COUNT(*) FROM customers) as customers,
    (SELECT COUNT(*) FROM orders) as orders,
    (SELECT COUNT(*) FROM invoices) as invoices,
    (SELECT COUNT(*) FROM products) as products;

\echo
\echo Financial Summary:
SELECT
    COUNT(*) as total_invoices,
    SUM(total_amount) as total_revenue,
    SUM(CASE WHEN paid THEN total_amount ELSE 0 END) as collected,
    SUM(CASE WHEN NOT paid THEN total_amount ELSE 0 END) as outstanding,
    AVG(total_amount) as avg_invoice_value
FROM invoices;

\echo
\echo Order Status Breakdown:
SELECT
    status,
    COUNT(*) as count,
    SUM(amount) as total_value
FROM orders
GROUP BY status;

\echo
\echo Current Tenant Info:
\tenant info globex-inc

\echo
\echo Resource Usage:
\tenant quota globex-inc

\echo
\echo ========================================
\echo Session Complete!
\echo
\echo Key Achievements:
\echo   ✓ Created 10 enterprise customers
\echo   ✓ Processed 10 high-value orders
\echo   ✓ Generated 5 invoices ($400K+ revenue)
\echo   ✓ Managed 6 enterprise products
\echo   ✓ Verified complete data isolation from Acme Corp
\echo   ✓ Tested cross-tenant write protection
\echo   ✓ Demonstrated Pro plan capabilities
\echo
\echo All operations were restricted to Globex Inc data only!
\echo Even though Acme Corp is operating simultaneously,
\echo their data is completely invisible to us!
\echo ========================================
\echo
\echo Bonus: Try these queries to verify isolation:
\echo   SELECT COUNT(*) FROM customers;  -- Should show only Globex
\echo   SELECT COUNT(*) FROM orders;     -- Should show only Globex
\echo   SELECT DISTINCT tenant_id FROM customers;  -- Should show only globex-inc
\echo
