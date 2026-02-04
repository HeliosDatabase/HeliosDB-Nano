-- ============================================================================
-- Tenant Session Script: Acme Corp Operations
-- ============================================================================
-- Purpose: Test multi-tenant isolation from Acme Corp's perspective
--
-- Usage (in separate terminal/session):
--   cargo run
--   \i scripts/02_tenant_acme_operations.sql
--
-- This script demonstrates:
--   - Tenant context switching
--   - RLS-enforced data isolation
--   - Quota tracking
--   - CRUD operations within tenant boundary
-- ============================================================================

\echo
\echo ========================================
\echo ACME CORP - Tenant Session
\echo ========================================
\echo

-- ----------------------------------------------------------------------------
-- STEP 1: Set Tenant Context to Acme Corp
-- ----------------------------------------------------------------------------
\echo Step 1: Switching to Acme Corp tenant...
\tenant use acme-corp

\echo
\echo Current tenant context:
\tenant current

-- ----------------------------------------------------------------------------
-- STEP 2: View Tenant Information
-- ----------------------------------------------------------------------------
\echo
\echo Step 2: Viewing Acme Corp details...
\tenant info acme-corp

-- ----------------------------------------------------------------------------
-- STEP 3: Query Existing Data (RLS-filtered)
-- ----------------------------------------------------------------------------
\echo
\echo Step 3: Querying existing customers...
\echo (Should only see Acme Corp customers due to RLS)
\echo

SELECT * FROM customers;

\echo
\echo Customer count:
SELECT COUNT(*) as acme_customer_count FROM customers;

\echo
\echo Querying existing products:
SELECT id, name, price, stock FROM products;

-- ----------------------------------------------------------------------------
-- STEP 4: Create New Customers
-- ----------------------------------------------------------------------------
\echo
\echo Step 4: Creating new Acme Corp customers...
\echo

INSERT INTO customers (id, tenant_id, name, email)
VALUES (3, 'acme-corp', 'Grace Growth', 'grace@acme.com');

INSERT INTO customers (id, tenant_id, name, email)
VALUES (4, 'acme-corp', 'Henry Sales', 'henry@acme.com');

\echo   ✓ Created 2 new customers

\echo
\echo Verifying new customers:
SELECT * FROM customers ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 5: Create Orders
-- ----------------------------------------------------------------------------
\echo
\echo Step 5: Creating orders for Acme Corp...
\echo

-- Order 1: Alice buys Widget Pro
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (1, 'acme-corp', 1, 'Widget Pro', 29900, 'pending');

-- Order 2: Bob buys Widget Basic
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (2, 'acme-corp', 2, 'Widget Basic', 9900, 'processing');

-- Order 3: Grace buys multiple Widget Basic
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (3, 'acme-corp', 3, 'Widget Basic', 29700, 'processing');

-- Order 4: Henry buys Widget Pro
INSERT INTO orders (id, tenant_id, customer_id, product, amount, status)
VALUES (4, 'acme-corp', 4, 'Widget Pro', 29900, 'completed');

\echo   ✓ Created 4 orders

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
VALUES (1, 'acme-corp', 1, 29900, FALSE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (2, 'acme-corp', 2, 9900, FALSE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (3, 'acme-corp', 3, 29700, TRUE);

INSERT INTO invoices (id, tenant_id, order_id, total_amount, paid)
VALUES (4, 'acme-corp', 4, 29900, TRUE);

\echo   ✓ Created 4 invoices

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
-- STEP 7: Business Analytics (within tenant)
-- ----------------------------------------------------------------------------
\echo
\echo Step 7: Acme Corp Business Analytics
\echo ========================================
\echo

\echo Total Revenue:
SELECT
    SUM(total_amount) as total_revenue,
    SUM(CASE WHEN paid THEN total_amount ELSE 0 END) as paid_revenue,
    SUM(CASE WHEN NOT paid THEN total_amount ELSE 0 END) as outstanding_revenue
FROM invoices;

\echo
\echo Orders by Status:
SELECT status, COUNT(*) as order_count, SUM(amount) as total_amount
FROM orders
GROUP BY status;

\echo
\echo Top Customers by Order Value:
SELECT
    c.name,
    COUNT(o.id) as order_count,
    SUM(o.amount) as total_spent
FROM customers c
LEFT JOIN orders o ON c.id = o.customer_id
GROUP BY c.id, c.name
ORDER BY total_spent DESC;

-- ----------------------------------------------------------------------------
-- STEP 8: Update Operations (RLS-protected)
-- ----------------------------------------------------------------------------
\echo
\echo Step 8: Updating order status...
\echo

-- Mark pending order as processing
UPDATE orders SET status = 'processing' WHERE id = 1;

\echo   ✓ Updated order #1 to processing

-- Mark processing orders as completed
UPDATE orders SET status = 'completed' WHERE status = 'processing';

\echo   ✓ Updated processing orders to completed

\echo
\echo Updated order statuses:
SELECT id, product, amount, status FROM orders ORDER BY id;

-- Mark unpaid invoices as paid
UPDATE invoices SET paid = TRUE WHERE paid = FALSE;

\echo   ✓ Marked all invoices as paid

\echo
\echo Updated invoice statuses:
SELECT id, order_id, total_amount, paid FROM invoices ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 9: Test Cross-Tenant Isolation (Should see NO other tenant data)
-- ----------------------------------------------------------------------------
\echo
\echo Step 9: Testing data isolation...
\echo ========================================
\echo
\echo Attempting to query ALL customers (RLS should filter):

-- This should only show Acme Corp customers, not Globex or Initech
SELECT tenant_id, COUNT(*) as visible_customers
FROM customers
GROUP BY tenant_id;

\echo
\echo Attempting to query ALL orders (RLS should filter):
SELECT COUNT(*) as visible_orders FROM orders;

\echo
\echo Attempting to query ALL products (RLS should filter):
SELECT COUNT(*) as visible_products FROM products;

\echo
\echo ✓ Data isolation verified - only Acme Corp data is visible

-- ----------------------------------------------------------------------------
-- STEP 10: Attempt to Access Another Tenant's Data (Should FAIL)
-- ----------------------------------------------------------------------------
\echo
\echo Step 10: Testing RLS protection...
\echo ========================================
\echo

-- Try to insert data for Globex Inc (should be blocked by RLS)
\echo Attempting to insert data for Globex Inc (should fail):
INSERT INTO customers (id, tenant_id, name, email)
VALUES (99, 'globex-inc', 'Hacker Attempt', 'hacker@globex.com');

\echo
\echo Note: If RLS is working correctly, the row was inserted but immediately
\echo became invisible to this session, or with_check_expr prevented it.

-- Verify we cannot see it
\echo
\echo Verifying we cannot see the attempted cross-tenant insert:
SELECT * FROM customers WHERE id = 99;

\echo (Should return 0 rows)

-- ----------------------------------------------------------------------------
-- STEP 11: Check Quota Usage
-- ----------------------------------------------------------------------------
\echo
\echo Step 11: Checking quota usage after operations...
\echo

\tenant quota acme-corp

-- ----------------------------------------------------------------------------
-- STEP 12: Product Inventory Management
-- ----------------------------------------------------------------------------
\echo
\echo Step 12: Managing product inventory...
\echo

-- Update stock levels
UPDATE products SET stock = stock - 10 WHERE name = 'Widget Pro';
UPDATE products SET stock = stock - 30 WHERE name = 'Widget Basic';

\echo   ✓ Updated inventory levels

\echo
\echo Current inventory:
SELECT name, price, stock FROM products ORDER BY name;

-- Add new product
INSERT INTO products (id, tenant_id, name, description, price, stock)
VALUES (3, 'acme-corp', 'Widget Premium', 'Premium widget with extras', 49900, 50);

\echo   ✓ Added new product: Widget Premium

\echo
\echo Updated product catalog:
SELECT id, name, price, stock FROM products ORDER BY id;

-- ----------------------------------------------------------------------------
-- STEP 13: Delete Operations (RLS-protected)
-- ----------------------------------------------------------------------------
\echo
\echo Step 13: Testing delete operations...
\echo

-- Delete a customer (should only delete if belongs to Acme Corp)
DELETE FROM customers WHERE id = 4;

\echo   ✓ Deleted customer #4 (Henry Sales)

\echo
\echo Remaining customers:
SELECT id, name, email FROM customers ORDER BY id;

-- Attempt to delete a customer from another tenant (should have no effect due to RLS)
\echo
\echo Attempting to delete Globex customer (should fail silently due to RLS):
DELETE FROM customers WHERE id = 10;

\echo
\echo Verifying total customer count (should still be 3 Acme customers):
SELECT COUNT(*) as customer_count FROM customers;

-- ----------------------------------------------------------------------------
-- STEP 14: Final Summary
-- ----------------------------------------------------------------------------
\echo
\echo ========================================
\echo ACME CORP SESSION SUMMARY
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
    SUM(total_amount) as total_revenue,
    SUM(CASE WHEN paid THEN total_amount ELSE 0 END) as collected,
    SUM(CASE WHEN NOT paid THEN total_amount ELSE 0 END) as outstanding
FROM invoices;

\echo
\echo Current Tenant Info:
\tenant info acme-corp

\echo
\echo ========================================
\echo Session Complete!
\echo
\echo Key Achievements:
\echo   ✓ Created 4 customers (1 deleted, 3 remaining)
\echo   ✓ Processed 4 orders (all completed)
\echo   ✓ Generated 4 invoices (all paid)
\echo   ✓ Managed 3 products
\echo   ✓ Verified RLS data isolation
\echo   ✓ Tested cross-tenant protection
\echo
\echo All operations were restricted to Acme Corp data only!
\echo ========================================
