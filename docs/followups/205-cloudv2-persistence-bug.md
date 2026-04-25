# Task 205 — CloudV2 admin_db persistence bug

## Reference

`/home/app/Helios/CloudV2/docs/PERSISTENCE-BUG-INVESTIGATION.md`

## Status — FIXED

The bug is repaired and both reproductions now pass:

```
$ cargo test --test persistence_repro
test insert_then_select_visible_on_same_connection ... ok
$ cargo test --test uuid_where_repro
test int_pk_round_trips_through_where  ... ok
test uuid_pk_round_trips_through_where ... ok
```

## Root cause (post-investigation)

Not at all what the CloudV2 doc theorised. Theories A / B / C / D
were red herrings — neither implicit transactions nor deadpool
recycling nor tokio-postgres streaming nor a 3.14.9 regression
were involved.

The actual bug: **the planner's literal-typing path**.

When a SQL statement uses a quoted UUID literal —
`WHERE id = '550e8400-e29b-41d4-a716-446655440000'` — the parser
emits a `Value::String("550e8400…")` regardless of the
comparison column's declared type. The point-lookup path (ART
PK index) then encodes the search key by Value variant, and
`Value::String("uuid-str")` and `Value::Uuid(uuid)` produce
different encoded keys → the lookup misses → the row appears
invisible.

CloudV2 hits this every time it does
`SELECT … WHERE id = '<uuid>'` against a UUID PK because the
admin_db's `id` columns are UUIDs. The reason every CloudV2
INSERT *appeared* to vanish was that the follow-up SELECT was
always WHERE-id-on-UUID — and that path missed.

The bug is the same regardless of:
- whether there's an explicit BEGIN / COMMIT (no, there isn't —
  Nano writes auto-commit at statement boundary in implicit mode).
- whether deadpool recycles `Fast` or `Verified`.
- whether the connection is a fresh tokio-postgres connect or a
  pooled one.

## Fix

Three complementary patches:

1. **`src/sql/executor/mod.rs::try_index_point_lookup`**
   — coerce the literal to the PK column's declared type before
   the ART index lookup. New helper
   `coerce_literal_to_column_type` handles String→UUID,
   String→Date, String→Timestamp; everything else passes
   through unchanged.

2. **`src/lib.rs::fast_parse_one_value`** — the SQL-text-level
   fast-select parser had the same issue for `SELECT *`
   queries. Same coercion applied at parse time.

3. **`src/storage/simd_filter.rs::compare_eq`** — the SIMD
   pushdown filter's equality comparison gained the
   Value::Uuid↔Value::String cross-type case so any post-walk
   filter through that path also matches correctly. (Belt and
   braces — the index-lookup path is the hot one.)

## Verification

- `tests/uuid_where_repro.rs` — direct-API repro covering both
  `SELECT id WHERE id = '<uuid>'` and
  `SELECT * WHERE id = '<uuid>'`, plus parameterised forms with
  `Value::Uuid` and `Value::String` parameters.
- `tests/persistence_repro.rs` — wire-protocol repro that mirrors
  CloudV2's `admin_db::simple_execute` shape. The
  `#[ignore]` annotation is removed; runs in the default test
  suite.
- All 1842 lib tests + every prior integration suite stays green;
  no regressions from the coercion.

## CloudV2 disposition

Once this Nano release lands in `heliosdb-nano-v2`, CloudV2's
`admin_db.rs` workarounds become unnecessary:

- `get_database` / `get_organization` SELECT-all + filter
  workaround at `src/admin_db.rs:1348-1364, 2692-2706` can revert
  to direct WHERE.
- The daily `heliosdb-nano-v2` restart cron can be dropped.
- `JwtCookie::is_token_revoked` no longer needs to fail open.
- `cloud-v2.heliosdb.com` graduates from staging to production.

The 36 `dashboard_features_test_v2.sh` failures and the 1/16
revocation-enforcement failure should clear on their own.
