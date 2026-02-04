-- ============================================================================
-- Admin Script: Multi-Tenant Setup
-- ============================================================================
-- Purpose: Create tenants, configure plans, and prepare database for
--          multi-tenant operations with RLS isolation
--
-- Usage:
--   cargo run
--   \i scripts/01_admin_setup_tenants.sql
--
-- This script creates:
--   - 3 tenants with different plans (Acme Corp, Globex Inc, Initech LLC)
--   - Shared tables with tenant_id columns
--   - Sample data structure for testing
-- ============================================================================

\echo
\echo ========================================
\echo Multi-Tenant Database Setup
\echo ========================================
\echo

-- ----------------------------------------------------------------------------
-- STEP 1: List existing tenants (should be empty on fresh install)
-- ----------------------------------------------------------------------------
\echo Step 1: Checking existing tenants...
\tenants

-- ----------------------------------------------------------------------------
-- STEP 2: Create Tenants with Different Plans
-- ----------------------------------------------------------------------------
\echo
\echo Step 2: Creating tenants with different plans...
\echo

-- Tenant 1: Acme Corp (Free Plan)
\echo   Creating Acme Corp (Free Plan)...
\tenant create acme-corp free

-- Tenant 2: Globex Inc (Pro Plan)
\echo   Creating Globex Inc (Pro Plan)...
\tenant create globex-inc pro

-- Tenant 3: Initech LLC (Enterprise Plan)
\echo   Creating Initech LLC (Enterprise Plan)...
\tenant create initech-llc enterprise

-- List all created tenants
\echo
\echo Step 3: Verifying tenant creation...
\tenants

-- ----------------------------------------------------------------------------
-- STEP 4: Show detailed info for each tenant
-- ----------------------------------------------------------------------------
\echo
\echo Step 4: Tenant Details
\echo ========================================
\echo

\echo Acme Corp Details:
\tenant info acme-corp

\echo Globex Inc Details:
\tenant info globex-inc

\echo Initech LLC Details:
\tenant info initech-llc

-- ----------------------------------------------------------------------------
-- STEP 5: Create Shared Tables (Multi-Tenant Schema)
-- ----------------------------------------------------------------------------
\echo
\echo Step 5: Creating shared multi-tenant tables...
\echo

-- Clear any existing context to create tables as admin
\tenant clear

-- Customers table (shared across all tenants)
CREATE TABLE IF NOT EXISTS customers (
    id INTEGER PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

\echo   ✓ Created: customers table

-- Orders table (shared across all tenants)
CREATE TABLE IF NOT EXISTS orders (
    id INTEGER PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id INTEGER NOT NULL,
    product TEXT NOT NULL,
    amount INTEGER NOT NULL,
    status TEXT DEFAULT 'pending',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

\echo   ✓ Created: orders table

-- Invoices table (shared across all tenants)
CREATE TABLE IF NOT EXISTS invoices (
    id INTEGER PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    order_id INTEGER NOT NULL,
    total_amount INTEGER NOT NULL,
    paid BOOLEAN DEFAULT FALSE,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

\echo   ✓ Created: invoices table

-- Products table (shared catalog with tenant-specific pricing)
CREATE TABLE IF NOT EXISTS products (
    id INTEGER PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    price INTEGER NOT NULL,
    stock INTEGER DEFAULT 0
);

\echo   ✓ Created: products table

-- Show created tables
\echo
\echo Verifying table creation:
\dt

-- ----------------------------------------------------------------------------
-- STEP 6: Test Plan Modifications
-- ----------------------------------------------------------------------------
\echo
\echo Step 6: Testing plan modifications...
\echo

-- Show current plan for Acme Corp
\echo Current Acme Corp plan:
\tenant info acme-corp

-- Upgrade Acme Corp from free to starter
\echo
\echo Upgrading Acme Corp to Starter plan...
\tenant plan acme-corp starter

-- Verify upgrade
\echo
\echo Verifying upgrade:
\tenant info acme-corp

-- ----------------------------------------------------------------------------
-- STEP 7: Show Quota Usage (should be minimal at this point)
-- ----------------------------------------------------------------------------
\echo
\echo Step 7: Initial quota usage
\echo ========================================
\echo

\tenant quota acme-corp
\tenant quota globex-inc
\tenant quota initech-llc

-- ----------------------------------------------------------------------------
-- STEP 8: Create Sample Data for Admin Testing
-- ----------------------------------------------------------------------------
\echo
\echo Step 8: Creating sample admin data...
\echo

-- NOTE: Without tenant context, we need to manually specify tenant_id
-- In production, RLS policies would enforce this automatically

-- Sample data for Acme Corp
INSERT INTO customers VALUES (1, 'acme-corp', 'Alice Admin', 'alice@acme.com', CURRENT_TIMESTAMP);
INSERT INTO customers VALUES (2, 'acme-corp', 'Bob Builder', 'bob@acme.com', CURRENT_TIMESTAMP);

-- Sample data for Globex Inc
INSERT INTO customers VALUES (10, 'globex-inc', 'Charlie CEO', 'charlie@globex.com', CURRENT_TIMESTAMP);
INSERT INTO customers VALUES (11, 'globex-inc', 'Diana Director', 'diana@globex.com', CURRENT_TIMESTAMP);

-- Sample data for Initech LLC
INSERT INTO customers VALUES (20, 'initech-llc', 'Eve Engineer', 'eve@initech.com', CURRENT_TIMESTAMP);
INSERT INTO customers VALUES (21, 'initech-llc', 'Frank Finance', 'frank@initech.com', CURRENT_TIMESTAMP);

\echo   ✓ Created 6 customers across 3 tenants

-- Sample products for each tenant
INSERT INTO products VALUES (1, 'acme-corp', 'Widget Pro', 'Professional widget', 29900, 100);
INSERT INTO products VALUES (2, 'acme-corp', 'Widget Basic', 'Basic widget', 9900, 500);

INSERT INTO products VALUES (10, 'globex-inc', 'Enterprise Suite', 'Full enterprise platform', 99900, 50);
INSERT INTO products VALUES (11, 'globex-inc', 'Team License', 'Team collaboration', 49900, 200);

INSERT INTO products VALUES (20, 'initech-llc', 'Cloud Storage Pro', '1TB storage', 19900, 1000);
INSERT INTO products VALUES (21, 'initech-llc', 'Cloud Storage Basic', '100GB storage', 4900, 5000);

\echo   ✓ Created 6 products across 3 tenants

-- Verify data
\echo
\echo Verifying data (admin view - can see all tenants):
SELECT COUNT(*) as total_customers FROM customers;
SELECT tenant_id, COUNT(*) as customer_count FROM customers GROUP BY tenant_id;

SELECT COUNT(*) as total_products FROM products;
SELECT tenant_id, COUNT(*) as product_count FROM products GROUP BY tenant_id;

-- ----------------------------------------------------------------------------
-- STEP 9: Summary
-- ----------------------------------------------------------------------------
\echo
\echo ========================================
\echo Setup Complete!
\echo ========================================
\echo
\echo Created Tenants:
\echo   1. Acme Corp      - Starter Plan (upgraded from Free)
\echo   2. Globex Inc     - Pro Plan
\echo   3. Initech LLC    - Enterprise Plan
\echo
\echo Created Tables:
\echo   - customers (6 rows)
\echo   - orders (0 rows)
\echo   - invoices (0 rows)
\echo   - products (6 rows)
\echo
\echo Next Steps:
\echo   1. Run: cargo run
\echo      Execute: \i scripts/02_tenant_acme_operations.sql
\echo
\echo   2. In separate terminal:
\echo      Run: cargo run
\echo      Execute: \i scripts/03_tenant_globex_operations.sql
\echo
\echo   Note: Each tenant will only see their own data due to RLS!
\echo
\echo ========================================

-- Save current state
\echo
\echo Final tenant listing:
\tenants
