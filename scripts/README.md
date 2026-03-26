# Multi-Tenancy SQL Test Scripts

This directory contains comprehensive SQL scripts for testing HeliosDB Nano's multi-tenancy features with Row-Level Security (RLS) isolation.

## Scripts Overview

### 1. `01_admin_setup_tenants.sql` - Admin Setup Script

**Purpose**: Initialize the multi-tenant database with tenants, shared tables, and sample data.

**What it does**:
- Creates 3 tenants with different plans:
  - **Acme Corp** - Starter Plan (upgraded from Free)
  - **Globex Inc** - Pro Plan
  - **Initech LLC** - Enterprise Plan
- Creates shared tables: `customers`, `orders`, `invoices`, `products`
- Seeds initial data for testing
- Demonstrates tenant management commands
- Shows quota configuration

**Run time**: ~10 seconds

---

### 2. `02_tenant_acme_operations.sql` - Acme Corp Session

**Purpose**: Test operations from Acme Corp's isolated perspective.

**What it does**:
- Switches context to Acme Corp tenant
- Creates customers, orders, and invoices
- Demonstrates RLS data isolation
- Tests cross-tenant write protection
- Shows quota tracking for Starter plan
- Performs business analytics within tenant

**Expected behavior**: Can only see/modify Acme Corp data

**Run time**: ~15 seconds

---

### 3. `03_tenant_globex_operations.sql` - Globex Inc Session

**Purpose**: Test operations from Globex Inc's isolated perspective.

**What it does**:
- Switches context to Globex Inc tenant
- Creates enterprise customers and high-value orders
- Demonstrates complete isolation from Acme Corp
- Tests that concurrent sessions don't interfere
- Shows Pro plan quota limits
- Performs complex joins and analytics

**Expected behavior**: Can only see/modify Globex Inc data

**Run time**: ~20 seconds

---

## Usage Instructions

### Basic Usage (Sequential)

Run scripts one at a time:

```bash
# Terminal 1: Setup
cargo run
# In REPL:
\i scripts/01_admin_setup_tenants.sql

# After setup completes, test tenant A:
\i scripts/02_tenant_acme_operations.sql

# Then test tenant B:
\i scripts/03_tenant_globex_operations.sql
```

### Concurrent Multi-Session Testing (Recommended)

Test true multi-tenant isolation with concurrent sessions:

```bash
# Terminal 1: Admin Setup
cargo run
# In REPL:
\i scripts/01_admin_setup_tenants.sql
\q

# Terminal 2: Acme Corp Session
cargo run
# In REPL:
\i scripts/02_tenant_acme_operations.sql
# Leave this session running...

# Terminal 3: Globex Inc Session (while Terminal 2 is still running)
cargo run
# In REPL:
\i scripts/03_tenant_globex_operations.sql
# Leave this session running...

# Now you have 2 tenants operating simultaneously!
# Each can only see their own data despite being in the same database
```

---

## What to Observe

### 1. **Data Isolation**

When running concurrent sessions:

- **Terminal 2 (Acme)** creates customers with IDs 1-4
- **Terminal 3 (Globex)** creates customers with IDs 10-19
- Neither terminal can see the other's customers
- Queries like `SELECT * FROM customers` return different results in each session

### 2. **RLS Enforcement**

Both scripts attempt cross-tenant operations:

```sql
-- In Acme session, try to insert Globex data:
INSERT INTO customers (id, tenant_id, name, email)
VALUES (99, 'globex-inc', 'Hacker', 'hack@globex.com');
```

**Result**: Insert succeeds but row is immediately invisible due to RLS `using_expr` filtering.

### 3. **Quota Tracking**

Watch quota usage grow as operations execute:

```sql
\tenant quota acme-corp
```

Shows real-time tracking of:
- Storage usage (bytes)
- Active connections
- Queries per second (QPS)

### 4. **Plan-Based Limits**

Different plans have different limits:

| Plan       | Storage | Connections | QPS   |
|------------|---------|-------------|-------|
| Free       | 100 MB  | 5           | 100   |
| Starter    | 1 GB    | 10          | 500   |
| Pro        | 10 GB   | 50          | 5000  |
| Enterprise | 100 GB  | 200         | 50000 |

---

## Expected Output Examples

### Admin Setup Output

```
========================================
Multi-Tenant Database Setup
========================================

Step 1: Checking existing tenants...
No tenants found.

Step 2: Creating tenants with different plans...
  Creating Acme Corp (Free Plan)...
Success: Tenant 'acme-corp' created
  ID: 550e8400-e29b-41d4-a716-446655440000
  Plan: free
  Isolation: SharedSchema (RLS)

Resource Limits:
  Storage: 100 MB
  Connections: 5
  QPS: 100

[... continues ...]
```

### Acme Corp Session Output

```
========================================
ACME CORP - Tenant Session
========================================

Step 1: Switching to Acme Corp tenant...
Success: Now using tenant 'acme-corp'
  ID: 550e8400-e29b-41d4-a716-446655440000
  RLS: enabled

Step 3: Querying existing customers...
(Should only see Acme Corp customers due to RLS)

id | tenant_id  | name        | email           | created_at
---|------------|-------------|-----------------|-------------------
1  | acme-corp  | Alice Admin | alice@acme.com  | 2025-12-12 10:30:00
2  | acme-corp  | Bob Builder | bob@acme.com    | 2025-12-12 10:30:00

Customer count: 2

[... continues ...]
```

---

## Verification Commands

After running all scripts, verify isolation:

### In Acme Session (Terminal 2):

```sql
-- Should only see Acme data
SELECT COUNT(*) FROM customers;  -- Shows ~3-4 customers
SELECT DISTINCT tenant_id FROM customers;  -- Shows only 'acme-corp'

-- Try to see Globex data (should fail)
SELECT * FROM customers WHERE id >= 10;  -- Returns 0 rows
```

### In Globex Session (Terminal 3):

```sql
-- Should only see Globex data
SELECT COUNT(*) FROM customers;  -- Shows ~10 customers
SELECT DISTINCT tenant_id FROM customers;  -- Shows only 'globex-inc'

-- Try to see Acme data (should fail)
SELECT * FROM customers WHERE id <= 5;  -- Returns 0 rows
```

### In Admin Session (No Context):

```sql
-- Admin can see all data when no context set
\tenant clear

SELECT tenant_id, COUNT(*) as count
FROM customers
GROUP BY tenant_id;

-- Result:
-- acme-corp    | 3
-- globex-inc   | 10
-- initech-llc  | 2
```

---

## Troubleshooting

### Issue: "Tenant not found"

**Solution**: Run `01_admin_setup_tenants.sql` first to create tenants.

### Issue: "Can see other tenant's data"

**Cause**: Tenant context not set, or RLS not enabled.

**Solution**:
```sql
\tenant use acme-corp
\tenant current  -- Verify context is set
```

### Issue: "Quota exceeded"

**Cause**: Reached plan limits (especially on Free/Starter plans).

**Solution**:
```sql
-- Upgrade plan
\tenant plan acme-corp pro

-- Or delete some data
DELETE FROM customers WHERE id = X;
```

### Issue: Scripts run slow

**Cause**: Normal for first run (database initialization).

**Solution**: Subsequent runs will be faster due to caching.

---

## Advanced Testing Scenarios

### Scenario 1: Quota Enforcement

Modify script to exceed QPS limits:

```sql
-- Add to any tenant script
-- Execute 1000 queries rapidly
DO $$
DECLARE i INTEGER;
BEGIN
    FOR i IN 1..1000 LOOP
        PERFORM COUNT(*) FROM customers;
    END LOOP;
END $$;
```

**Expected**: QPS quota errors after hitting plan limit.

### Scenario 2: Storage Limits

Insert large datasets to test storage quotas:

```sql
-- Generate 10,000 customers
INSERT INTO customers (id, tenant_id, name, email)
SELECT
    generate_series(1000, 11000),
    'acme-corp',
    'Customer ' || generate_series(1000, 11000),
    'user' || generate_series(1000, 11000) || '@acme.com';
```

**Expected**: Storage quota errors based on plan.

### Scenario 3: Connection Limits

Open multiple REPL sessions with same tenant:

```bash
# Open 6 terminals (exceeds Free plan's 5 connection limit)
for i in {1..6}; do
    cargo run &
    # In each: \tenant use acme-corp
done
```

**Expected**: 6th connection refused if on Free plan.

---

## Script Customization

### Adding Your Own Tenant

Edit `01_admin_setup_tenants.sql`:

```sql
\tenant create my-company pro

INSERT INTO customers (id, tenant_id, name, email)
VALUES (1000, 'my-company', 'My Customer', 'test@mycompany.com');
```

### Testing Different Plans

Compare plan performance:

```sql
-- Create 2 tenants with different plans
\tenant create tenant-a free
\tenant create tenant-b enterprise

-- Time operations
\timing on
\tenant use tenant-a
INSERT INTO customers VALUES (1, 'tenant-a', 'Test', 'test@a.com');

\tenant use tenant-b
INSERT INTO customers VALUES (1, 'tenant-b', 'Test', 'test@b.com');
```

---

## File Structure

```
scripts/
├── README.md                          # This file
├── 01_admin_setup_tenants.sql         # Admin setup (run first)
├── 02_tenant_acme_operations.sql      # Acme Corp session
└── 03_tenant_globex_operations.sql    # Globex Inc session
```

---

## Notes

### RLS Policy Creation

⚠️ **Important**: RLS policies must be created programmatically via Rust API. The REPL does not currently expose RLS policy commands.

To enable full RLS enforcement, you need to:

1. Run the setup script to create tenants and tables
2. Add RLS policies via Rust code:

```rust
// In your application code
db.tenant_manager.create_rls_policy(
    "customers".to_string(),
    "tenant_isolation".to_string(),
    "tenant_id = current_tenant()".to_string(),
    RLSCommand::All,
    "tenant_id = current_tenant()".to_string(),
    Some("tenant_id = current_tenant()".to_string()),
);
```

3. Run tenant operation scripts

### Performance

- **First run**: 30-45 seconds (includes compilation)
- **Subsequent runs**: 5-15 seconds
- **Concurrent sessions**: No performance degradation

### Data Persistence

Scripts use in-memory database by default. To persist data:

1. Modify cargo run to use file-based database
2. Or use `--db-path` flag when starting REPL

---

## Success Criteria

After running all scripts, you should observe:

✅ 3 tenants created with different plans
✅ Multiple shared tables with tenant_id columns
✅ Acme Corp can only see/modify their data
✅ Globex Inc can only see/modify their data
✅ Cross-tenant writes are blocked by RLS
✅ Quota usage tracked per tenant
✅ No data leakage between concurrent sessions

---

## Related Documentation

- **Multi-Tenancy Implementation Report**: `../MULTI_TENANCY_IMPLEMENTATION_REPORT.md`
- **RLS Quick Start Guide**: `../docs/guides/RLS_QUICKSTART.md`
- **Test Coverage Report**: `../docs/testing/MULTI_TENANCY_TEST_COVERAGE.md`

---

**Last Updated**: December 12, 2025
**HeliosDB Nano Version**: v3.7.0
