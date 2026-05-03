---
name: heliosdb-nano-tenant
description: Multi-tenant database isolation in HeliosDB-Nano. Create tenants with one of three isolation modes (SharedSchema with RLS, SchemaPerTenant, DatabasePerTenant), assign tiered plans (free / starter / pro / enterprise) with per-tenant resource limits (storage / connections / QPS), define row-level-security policies that auto-inject `current_tenant()` predicates, and switch the active tenant per session. Use this when the user mentions "multi-tenant", "tenancy", "tenant", "RLS", "row-level security", or wants to provision per-tenant isolation without standing up separate database instances.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Multi-tenancy & RLS

## When to use
- SaaS application serving multiple customers from one database.
- Per-customer data isolation without spinning up multiple Nano instances.
- Compliance scenarios that require auditable RLS policies.
- Resource-quota enforcement (storage / connections / QPS) per customer.

## Architecture

Three orthogonal axes:

| Axis | Options | Trade-off |
|------|---------|-----------|
| **Isolation mode** | `SharedSchema` (default; RLS) · `SchemaPerTenant` · `DatabasePerTenant` | More isolation = more storage overhead, fewer cross-tenant queries |
| **Plan** | `free` · `starter` · `pro` · `enterprise` (custom plans creatable) | Tiered resource limits; auto-downgrade when a plan is deleted |
| **RLS** | per-table policies on `SELECT/INSERT/UPDATE/DELETE` | Enforced even when the app forgets to filter by `tenant_id` |

The active tenant for a session lives in thread-local storage (`src/tenant/mod.rs:25–28`), set by `\tenant use` (REPL) or by the SDK at connect time. Every query against an RLS-enabled table is rewritten to inject the `current_tenant()` predicate.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create tenant | REPL | `\tenant create acme-corp pro schema` |
| switch active tenant | REPL | `\tenant use acme-corp` |
| current context | REPL | `\tenant current` |
| list tenants | REPL | `\tenants` |
| describe tenant | REPL | `\tenant info acme-corp` |
| set quota | REPL | `\tenant quota set acme-corp 5000 25 10000` (5GB · 25 conn · 10K qps) |
| delete tenant | REPL | `\tenant delete acme-corp` |
| list plans | REPL | `\tenant plans` |
| describe plan | REPL | `\tenant plan info pro` |
| create plan | REPL | `\tenant plan create custom1 pro 10240 50 5000` |
| change plan | REPL | `\tenant plan acme-corp custom1` |
| RLS policy | SQL | `CREATE POLICY p_tenant ON orders FOR SELECT USING (tenant_id = current_tenant())` |
| current tenant (SQL) | SQL | `SELECT current_tenant()` |
| current user (SQL) | SQL | `SELECT current_user_id()` |
| library: register tenant | Rust | `db.tenant_manager.register_tenant(name, IsolationMode::DatabasePerTenant)` |
| library: get current | Rust | `tenant::get_current_tenant_id()` |

## Recipes

### Recipe 1: Spin up a per-tenant SaaS schema (RLS mode)

```sql
-- 1. Tenant table is already provisioned by the binary
SELECT name FROM pg_tenants;        -- system view

-- 2. Create your business tables with a tenant_id column
CREATE TABLE orders (
    id          SERIAL PRIMARY KEY,
    tenant_id   UUID NOT NULL,
    customer    TEXT,
    amount      DECIMAL(10,2)
);

-- 3. Enable RLS and add a policy
ALTER TABLE orders ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON orders
  FOR ALL
  USING (tenant_id = current_tenant());
```
Once a session calls `\tenant use acme-corp`, every `SELECT/INSERT/UPDATE/DELETE` on `orders` is auto-scoped to `acme-corp`'s data.

### Recipe 2: Database-per-tenant isolation (the strongest mode)

```
heliosdb> \tenant create acme-corp enterprise db
   Tenant 'acme-corp' created (id=…, plan=enterprise, isolation=DatabasePerTenant)

heliosdb> \tenant use acme-corp
   Switched to tenant 'acme-corp' — queries route to its private namespace.

heliosdb> CREATE TABLE customers (id SERIAL PRIMARY KEY, name TEXT);
   OK
```
Tables created here are physically isolated under the tenant's own key prefix and **never visible** from other tenants — even if their app misses the `current_tenant()` filter. Trade-off: you get N copies of any cross-tenant lookup table.

### Recipe 3: Switch tenant context per HTTP request (SDK pattern)

```python
# Pseudocode — actual SDK shape varies (psycopg2, node-pg, etc.)
async def handle_request(req):
    tenant_id = req.user.tenant_id
    async with pool.acquire() as conn:
        await conn.execute("SET LOCAL helios.tenant_id = $1", tenant_id)
        # … queries from this conn now scoped to tenant_id …
```
`SET LOCAL` is preferred over `SET` so the binding clears on transaction end — important for connection pools.

### Recipe 4: Resource-quota enforcement

```
heliosdb> \tenant quota set acme-corp 10240 50 5000
   acme-corp:
     storage: 10 GB max (currently 2.3 GB used, 23 %)
     connections: 50 max (currently 12 in use)
     qps:       5 000 max (last 60s avg: 42 qps)
```
When a tenant exceeds its quota, the engine returns a SQLSTATE-shaped error (e.g., `53300 too_many_connections`) — same shape PostgreSQL uses, so any retry-aware client handles it.

### Recipe 5: Custom plans

```
heliosdb> \tenant plan create eu-residency pro 51200 100 10000
   Plan 'eu-residency' created:
     tier: pro
     storage:       50 GB
     connections:   100
     qps:           10 000

heliosdb> \tenant plan acme-corp eu-residency
   Tenant 'acme-corp' moved to plan 'eu-residency' (was: enterprise)
```
Useful for jurisdiction-pinned tenants or contractual plan customisations beyond the default tiers.

### Recipe 6: Library API (Rust embedded)

```rust
use heliosdb_nano::{EmbeddedDatabase, tenant::IsolationMode};

let db = EmbeddedDatabase::new("./mydata")?;
let tenant = db.tenant_manager.register_tenant(
    "acme-corp".into(),
    IsolationMode::DatabasePerTenant,
);
db.tenant_manager.set_current_context(Some(tenant.id))?;
db.execute("CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)")?;
```

## Default plans

| Plan | Storage | Connections | QPS | Use case |
|------|---------|-------------|-----|----------|
| free | 100 MB | 5 | 100 | Free tier; aggressive limits |
| starter | 1 GB | 10 | 500 | Hobby / early-stage |
| pro | 10 GB | 50 | 5 000 | SMB / production |
| enterprise | 100 GB | 200 | 50 000 | Large customers / negotiated |

Limits per plan are at `src/tenant/mod.rs:494–510` (`ResourceLimits`); they're starting points — override per-tenant via `\tenant quota set`.

## Pitfalls

- **`SharedSchema` without an RLS policy is not isolation.** A tenant can read another's rows. Always enable RLS + a `current_tenant()` policy on every business table.
- **Connection pools and tenant context.** Use `SET LOCAL` (transaction-scoped) not `SET` (session-scoped) — without `LOCAL`, a recycled connection can leak the previous request's tenant context.
- **`DatabasePerTenant` multiplies storage of cross-tenant lookup tables.** A 100 KB country-code table × 1 000 tenants = 100 MB of duplicated reference data. Use `SharedSchema` for catalogue-style data, `DatabasePerTenant` only for customer-scoped data.
- **Plan deletion auto-downgrades.** If you delete the `pro` plan, every tenant on `pro` gets moved to the next-lower tier (`starter`). Read the cascade rules at `src/tenant/mod.rs:169+` (`PlanManager`) before deleting plans in production.
- **`CREATE DATABASE` SQL DDL is not yet wired** to the tenant API as of v3.23.0 (tracked under `BUGS_DASHBOARD_MIGRATION_TRIAGE.md` Bug 1). Until that lands, use `\tenant create … db` (REPL) or the library API.
- **`current_tenant()` returns NULL when no context is set.** RLS predicates against NULL behave per SQL three-valued logic — typically meaning "no rows visible". For an unauthenticated public path, set a sentinel tenant or skip the policy on those tables.
- **Cross-tenant queries.** RLS prevents them by default. Admin/reporting workflows that need cross-tenant aggregation must run as a privileged role that bypasses RLS — there is no per-query bypass syntax.

## See also
- `heliosdb-nano-server` — production deployment, auth, TLS for multi-tenant servers.
- `heliosdb-nano-schema` — RLS DDL syntax (`CREATE POLICY`, `ALTER TABLE … ENABLE ROW LEVEL SECURITY`).
- `heliosdb-nano-branches` — branches are an orthogonal isolation tool (per-experiment, not per-customer); don't confuse the two.
- `heliosdb-nano-deploy` — Fly.io / Railway / Render specifics for multi-tenant deployments.
- `src/tenant/mod.rs` — authoritative source for plans, isolation modes, RLS policies.
- `BUGS_DASHBOARD_MIGRATION_TRIAGE.md` Bug 1 — the SQL DDL surface (`CREATE DATABASE foo`) is queued for v3.26.0 and will map to this skill's tenant API.
