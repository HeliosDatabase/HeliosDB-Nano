//! PostgreSQL system catalog emulation
//!
//! This module provides minimal emulation of PostgreSQL system catalogs
//! (pg_catalog) and information_schema for client compatibility.
//! Many PostgreSQL clients query these system tables during connection
//! and for introspection.

use crate::{Column, DataType, EmbeddedDatabase, Result, Schema, Tuple, Value};
use std::sync::Arc;

/// PostgreSQL catalog emulator
pub struct PgCatalog {
    /// Reference to the database for real catalog queries
    database: Option<Arc<EmbeddedDatabase>>,
}

impl PgCatalog {
    /// Create a new catalog emulator (without database access - static responses only)
    pub fn new() -> Self {
        Self { database: None }
    }

    /// Create a new catalog emulator with database access for real table/column metadata
    pub fn with_database(database: Arc<EmbeddedDatabase>) -> Self {
        Self {
            database: Some(database),
        }
    }

    /// Handle catalog queries
    ///
    /// Returns Some((schema, rows)) if this is a catalog query,
    /// None if it should be handled by the normal query engine
    pub fn handle_query(&self, query: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        // HDB-011: `to_ascii_lowercase` — NOT `to_lowercase`. Every helper below
        // locates a clause by searching the lowered text and then slices it by
        // byte offset; a Unicode lowering can change a string's byte LENGTH
        // (`İ` is 2 bytes and lowers to 3), which would make those offsets point
        // at different characters in the original. ASCII lowering is
        // length-preserving, so `query_lower[a..b]` and `query_orig[a..b]`
        // always address the same span — which is what lets a literal's VALUE
        // be compared case-SENSITIVELY (from `query_orig`) while keywords and
        // column names keep matching case-insensitively (from `query_lower`).
        // Before this, `WHERE typname = 'INT4'` was compared as `'int4'`.
        //
        // HDB-011 review FIX-1: the lowered copy ALSO folds every ASCII
        // whitespace byte to a plain space. Every clause this router locates —
        // ` where `, ` and `, ` or `, ` order by ` — is found by substring
        // search, so a client that puts its WHERE on the NEXT LINE (what every
        // ORM and every hand-formatted statement does) used to look like a
        // statement with no WHERE at all: `where_clause_is_fully_supported`
        // answered "nothing to misinterpret", and `pg_tables` came back
        // UNFILTERED — the exact widening this fix exists to prevent. Folding
        // is length-preserving (`\t\n\r\x0b\x0c` are one byte each, all
        // < 0x80), so the offsets stay aligned with `query_orig` as above.
        let query_orig = query.trim();
        // Deliberately NOT folded: `strip_literals_and_comments` below needs
        // the real line breaks, because a `--` line comment ends at the next
        // `\n`. Folding first would blank the rest of the statement.
        let query_lowered = query_orig.to_ascii_lowercase();
        let query_lower = Self::fold_ascii_whitespace(&query_lowered);

        // --- F1: statement-kind gate (task #38) --------------------------
        // This handler runs on the RAW, UNPARSED statement text and can only
        // *substring-match*. Every legitimate interception it performs — psql
        // meta-command signatures and client introspection probes — is a
        // read: a `SELECT`, a CTE `WITH`, or a parenthesised `( SELECT … )`.
        // A DML/DDL statement (UPDATE/INSERT/DELETE/CREATE/…) that merely
        // *mentions* a catalog name (in a string literal, a column value, a
        // comment) must NEVER be intercepted here — doing so silently discards
        // the write and hands the client a fake SELECT-shaped result. Gate on
        // the first keyword up front so no downstream substring check can
        // hijack a write. This alone kills the live-verified silent-write-loss
        // class: `UPDATE t SET note='see pg_tables'`,
        // `CREATE TABLE pg_type_registry (…)`,
        // `INSERT … VALUES ('… information_schema.sql_features …')`, and the
        // full-psql-\dt-signature-inside-a-string-literal INSERT.
        let first_word: String = query_lower.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let is_select_like = query_lower.starts_with('(') || first_word == "select" || first_word == "with";
        if !is_select_like {
            return Ok(None);
        }

        // --- F2: literal/comment-stripped view of the statement (task #38) -
        // A raw `contains()` also fires on catalog names that appear INSIDE a
        // single-quoted string literal or a SQL comment (e.g.
        // `SELECT * FROM my_notes WHERE body = 'see pg_type docs'`, or
        // `SELECT * FROM t -- see pg_tables`). Those are ordinary reads of a
        // USER table, not catalog probes. `matchable` blanks out the CONTENTS
        // of literals and comments (see `strip_literals_and_comments`) so the
        // catalog-detection predicates only ever see real SQL. IMPORTANT:
        // `matchable` is routed ONLY to the detection predicates below
        // (`has_information_schema_ref`, `is_catalog_query`, and the pg_*
        // dispatch). `try_psql_metacommand` and every result post-processing
        // helper (`apply_where_filter` / `apply_aggregate` / `project_columns`
        // / `extract_*`) keep receiving the ORIGINAL `query_lower` — they
        // legitimately parse string literals (psql's `'r'` relkind fragment,
        // WHERE filter values).
        // Built from the UNFOLDED copy on purpose: a `--` comment ends at a
        // real `\n` (HDB-011 review FIX-1).
        let matchable = Self::strip_literals_and_comments(&query_lowered);

        // --- F2b: the CLAUSE-CLASSIFICATION view (HDB-011 review FIX-A/B) --
        // A third parallel copy: literal/comment-stripped AND whitespace-
        // folded. Both transforms are byte-for-byte length-preserving, so this
        // still addresses the same characters as `query_lower` / `query_orig`.
        //
        // Every decision about the SHAPE of the WHERE clause —
        // `where_clause_span`, `where_clause_is_fully_supported`,
        // `split_conjunct_spans`, `is_supported_pred_shape`, and the operator /
        // column-name scanning inside `eval_simple_pred` — is made on THIS
        // copy, never on text that still contains literal bodies. Two things
        // follow, and both were live bugs:
        //
        //   1. The interceptor-vs-planner decision becomes independent of
        //      every bound parameter's VALUE. The extended protocol classifies
        //      at Parse (text still holding `$1`) and again at Execute (text
        //      with the value spliced in by `substitute_parameters`). When the
        //      two disagreed, Describe had already sent the interceptor's
        //      5-field RowDescription while Execute answered from the
        //      registry's 8-column `pg_tables` — DataRows with more fields
        //      than the RowDescription announced, which is a protocol
        //      violation (tokio-postgres rejects the row outright; node-postgres
        //      throws). `WHERE tablename = $1` with the value
        //      `'orders and returns'` did exactly that: ` and ` inside the
        //      value split the predicate into two conjuncts, the second of
        //      which matched no supported shape.
        //   2. `' is null'`, `' and '`, `' or '` and `(` INSIDE a literal stop
        //      counting as syntax. `WHERE tablename = 'x is null'` used to be
        //      classified as an IS NULL predicate on the "column"
        //      `tablename = 'x`, which resolves to NULL for every row — so the
        //      client got EVERY table in the database, the exact widening this
        //      guard exists to prevent.
        //
        // Value extraction is deliberately NOT moved here: `eval_simple_pred`
        // still cuts every literal out of `query_orig` at the same offsets, so
        // comparisons stay case-sensitive and see the real text.
        let matchable_folded = Self::fold_ascii_whitespace(&matchable);

        // --- psql meta-command query detection ---------------------------
        // psql sends complex JOINs across pg_class / pg_namespace /
        // pg_attribute that our simple substring matcher can't resolve, so
        // recognise them by signature and synthesise a shaped response.
        if let Some(result) = self.try_psql_metacommand(&query_lower)? {
            return Ok(Some(result));
        }

        // `version()` / `current_database()` / `current_user` / `session_user` /
        // `current_schema()` are deliberately NOT intercepted here. This handler
        // runs on the RAW, UNPARSED query text before the real parser/planner
        // even sees the statement — a `contains()` check can't tell "this
        // substring IS the whole statement" from "this substring occurs
        // somewhere inside a larger expression" (e.g. `current_database() ~ 'x'`,
        // `length(version())`, or a WHERE clause on an UPDATE/DELETE that happens
        // to mention one of these names), so a hardcoded canned row here would
        // silently discard the rest of the statement — including write
        // statements, which would then return a fake SELECT-shaped result
        // instead of executing. Falling through to `Ok(None)` lets the real
        // parser/planner/evaluator answer these correctly and uniformly
        // (session-aware where relevant) for both the wire and embedded paths —
        // see `Evaluator`'s `"version"` / `"current_database"` / `"current_user"`
        // / `"session_user"` / `"current_schema"` scalar-function arms.

        // Check for information_schema queries (table / column listing).
        // Match the TABLE reference (`information_schema.<name>`) over the
        // literal/comment-stripped `matchable` text. Historically this check
        // was hand-rolled to avoid matching the `'information_schema'` string
        // literal that Drizzle / postgres-js / Prisma pass in WHERE clauses
        // like `… WHERE schemaname NOT IN ('pg_catalog','information_schema')`;
        // F2 stripping now blanks that literal's contents generically, so the
        // special-case dodge is no longer needed. The old bare
        // space-delimited ` information_schema ` disjunct was dropped together
        // with F4 (its only consumer was a degenerate empty-result branch that
        // now falls through to the planner).
        let has_information_schema_ref = matchable.contains("information_schema.");
        // HC3 (catalog unification): every information_schema view listed below
        // is served by the planner-backed SystemViewRegistry
        // (src/sql/phase3/system_views.rs). `return Ok(None)` defers to the
        // planner on ALL three routes that reach this function — the PG simple
        // query path, the PG extended/Parse path (handler_extended.rs derives
        // RowDescription from the planner instead), and the MySQL wire (which
        // calls `execute_query` on Ok(None)) — so ONE implementation now answers
        // every interface instead of a wire-only fixed-shape copy plus a
        // divergent registry copy.
        //
        // This deletes a whole class of bug rather than instances of it: the
        // substring router could not filter, project or JOIN, so
        // `… FROM information_schema.columns WHERE table_schema = 'public'` —
        // the most common ORM introspection query in existence — tested a column
        // the wire shape did not have, compared it against NULL and dropped
        // EVERY row; written without spaces around `=` it instead silently
        // dropped `table_schema` from the projection and returned a narrower row
        // than RowDescription had promised. The planner does real filtering,
        // projection, JOINs and aggregates, so all of that simply goes away.
        //
        // NEVER add an interception branch back here. This handler runs on RAW,
        // UNPARSED text and has already caused two silent-write-loss incidents
        // (commits 0c27a30, 4ec06fa / tasks #34, #38); REMOVING interception is
        // the only safe direction. If a wire test fails, fix the registry.
        let result = if has_information_schema_ref {
            if query_lower.contains("information_schema.columns")
                || query_lower.contains("information_schema.tables")
                || query_lower.contains("information_schema.key_column_usage")
                || query_lower.contains("information_schema.table_constraints")
                || query_lower.contains("information_schema.referential_constraints")
                || query_lower.contains("information_schema.constraint_column_usage")
                || query_lower.contains("information_schema.sequences")
                || query_lower.contains("information_schema.schemata")
                || query_lower.contains("information_schema.catalog_name")
                || query_lower.contains("information_schema.check_constraints")
                || query_lower.contains("information_schema.views")
                // HC4 privilege/role views. `table_privileges` and
                // `role_table_grants` are now POPULATED from the persisted ACL
                // catalog, and the other eight are registered shape-correct and
                // empty — all ten in the phase-3 registry, so the embedded /
                // REPL / Python routes stop reporting them as unknown
                // relations. Deferring here means the planner (which can
                // filter, project and JOIN) answers them on the wire too.
                //
                // A ROW IN THESE VIEWS MEANS "SOMEBODY RAN GRANT". It does not
                // mean access is restricted: this build enforces no privilege.
                || query_lower.contains("information_schema.table_privileges")
                || query_lower.contains("information_schema.role_table_grants")
                || query_lower.contains("information_schema.column_privileges")
                || query_lower.contains("information_schema.role_column_grants")
                || query_lower.contains("information_schema.usage_privileges")
                || query_lower.contains("information_schema.role_usage_grants")
                || query_lower.contains("information_schema.role_routine_grants")
                || query_lower.contains("information_schema.applicable_roles")
                || query_lower.contains("information_schema.enabled_roles")
                || query_lower.contains("information_schema.administrable_role_authorizations")
            {
                return Ok(None);
            } else if query_lower.contains("information_schema.routines") {
                Some(Self::query_information_schema_routines())
            } else if let Some(name) = Self::information_schema_view_name(&query_lower) {
                if let Some(empty) = Self::known_empty_information_schema_view(&name) {
                    Some(empty)
                } else {
                    // Keep this list HONEST: "populated" means the view returns rows
                    // reflecting real schema state, measured over the wire. Several
                    // views resolve and report the correct column list but return zero
                    // rows by construction — listing those as implemented is what sent
                    // users looking for their own mistake. See
                    // docs/compatibility/information_schema.md.
                    return Err(crate::Error::QueryExecution(format!(
                        "information_schema.{name} is not a recognised view; \
                         HeliosDB Nano populates catalog_name, tables (base tables AND \
                         views), columns, schemata, views, key_column_usage, \
                         table_constraints, constraint_column_usage, \
                         referential_constraints, check_constraints, sequences, \
                         table_privileges and role_table_grants — the last two report \
                         STORED grants; HeliosDB does NOT enforce SQL privileges. \
                         These resolve but are ALWAYS EMPTY: view_table_usage, \
                         view_column_usage, routines, parameters, triggers, domains, \
                         character_sets, collations, column_privileges, \
                         usage_privileges, role_column_grants, role_usage_grants, \
                         role_routine_grants, applicable_roles, enabled_roles, \
                         administrable_role_authorizations. \
                         Please file an issue if this view is needed."
                    )));
                }
            } else {
                // F4 (task #38): `information_schema.` is present but no view
                // name is extractable (a degenerate trailing dot). The old
                // behaviour returned a zero-column empty result, silently
                // masking the real outcome. Fall through to the planner so a
                // genuine "relation does not exist" surfaces instead of a fake
                // empty rowset.
                return Ok(None);
            }
        } else if !Self::is_catalog_query(&matchable) {
            return Ok(None);
        } else if Self::contains_word(&matchable, "pg_type") {
            // HDB-011: `pg_type` is served by the planner-backed
            // SystemViewRegistry (src/sql/phase3/system_views.rs), which filters,
            // projects, JOINs and aggregates with real SQL semantics over the
            // full PostgreSQL type inventory. The interception this replaces
            // answered every `pg_type` SELECT from a 12-row fixed shape, compared
            // literals against the LOWERCASED statement text (so `= 'INT4'`
            // matched `int4`), and interpreted WHERE by string-splitting — which
            // silently returned EVERY row for `typname='int4'` (no spaces), for
            // any `OR`, and for `count(*) FROM pg_type WHERE typname = 'hstore'`
            // (12, not 0). `pg_type` stays in `is_catalog_query`'s MARKERS: it is
            // harmless there, and the R5.W2 Parse-time probe in
            // handler_extended.rs keys off this `Ok(None)` to cache "engine
            // query", so Describe comes from the shared plan and Bind parameters
            // stay typed.
            return Ok(None);
        } else if matchable.contains("pg_inherits") {
            // KanttBan #22 slice 5 regression carve-out: pg_inherits
            // is registered in the SystemViewRegistry but psql's `\d`
            // sub-queries against it use `c.oid::pg_catalog.regclass`
            // which the planner doesn't yet parse. Short-circuit with
            // an empty 3-col shape so libpq doesn't error and psql's
            // describe panel doesn't render bogus "Inherits" sections.
            // Direct ORM queries against pg_inherits still get the
            // empty rowset via this route — same behaviour as the
            // registry would have produced.
            Some((
                Schema::new(vec![
                    Column::new("oid", DataType::Text),
                    Column::new("relkind", DataType::Char(1)),
                    Column::new("partbound", DataType::Text),
                ]),
                vec![],
            ))
        } else if matchable.contains("pg_publication") {
            // Same carve-out as pg_inherits: psql `\d` joins this with
            // `pg_relation_is_publishable(<oid>)`, which the planner
            // doesn't implement. Empty 1-col `pubname` response.
            Some((Schema::new(vec![Column::new("pubname", DataType::Text)]), vec![]))
        } else if matchable.contains("pg_statistic_ext") {
            // Same carve-out: psql's `\d` query against pg_statistic_ext
            // projects `stxrelid::pg_catalog.regclass` and
            // `stxnamespace::pg_catalog.regnamespace`, both regclass-family
            // type casts the planner doesn't handle. Empty 9-col shape
            // matches the slice 5 registry registration.
            Some((
                Schema::new(vec![
                    Column::new("oid", DataType::Int4),
                    Column::new("stxrelid", DataType::Text),
                    Column::new("nsp", DataType::Text),
                    Column::new("stxname", DataType::Text),
                    Column::new("columns", DataType::Text),
                    Column::new("ndist_enabled", DataType::Boolean),
                    Column::new("deps_enabled", DataType::Boolean),
                    Column::new("mcv_enabled", DataType::Boolean),
                    Column::new("stxstattarget", DataType::Int4),
                ]),
                vec![],
            ))
        } else if Self::contains_word(&matchable, "pg_tables") {
            // Leave until migrated to registry. `contains_word` still matches
            // inside `pg_catalog.pg_tables` (the `.` is a boundary).
            //
            // HDB-011: `pg_tables` is ALSO registered in the planner-backed
            // registry, so when the WHERE clause is not one of the shapes
            // `apply_where_filter` interprets, deferring costs nothing and the
            // planner answers it correctly. Returning the UNFILTERED catalog —
            // what `apply_where_filter`'s "when in doubt, keep the row" fallback
            // did — is the worse answer: `WHERE tablename = 'a' OR tablename =
            // 'zzz'` listed EVERY table.
            //
            // Classified on `matchable_folded`, NOT on `query_lower`: the
            // decision has to be the same at Parse (`$1` still in the text) and
            // at Execute (the value spliced in), and syntax that only occurs
            // inside a literal must not count (HDB-011 review FIX-A/FIX-B).
            if !Self::where_clause_is_fully_supported(&matchable_folded) {
                return Ok(None);
            }
            Some(self.query_pg_tables()?)
        } else if Self::contains_word(&matchable, "pg_settings") {
            // NOT gated on `where_clause_is_fully_supported`: `pg_settings` is
            // the one view here with no registry twin, so deferring would turn
            // psql's and pgAdmin's startup probes into "relation does not
            // exist". Today's behaviour (an uninterpretable WHERE keeps every
            // row) is kept deliberately for this view alone.
            //
            // NIT-C: the canned shape is only `name, setting, unit, category`,
            // and clause detection now sees a predicate written across a line
            // break (the whitespace fold). So a MULTI-LINE predicate on a
            // column this shape lacks — `SELECT name, setting FROM
            // pg_settings\nWHERE context = 'user'` — now yields ZERO rows
            // (`row_value` answers NULL for `context`, and `lit_eq_value`
            // drops the row) where it used to yield all four canned rows.
            // Both answers are fiction; the fold is kept because it fixes the
            // far more common multi-line `WHERE name = …` / `WHERE name IN (…)`
            // spellings. Note the asymmetry: `<>` on an absent column still
            // keeps every row, because `eval_simple_pred` negates the match.
            Some(self.query_pg_settings()?)
        } else {
            // KanttBan #22 (v3.31.0): pg_namespace / pg_class / pg_attribute /
            // pg_index / pg_constraint / pg_user / pg_roles previously had
            // fixed-shape branches here; HC3 added pg_views and pg_indexes to
            // that list (pg_indexes was the LIVE implementation and is ported
            // verbatim into the registry, which also un-errors it on the
            // embedded / REPL / Python routes). They now flow through the
            // regular planner via the SystemViewRegistry (see src/sql/planner.rs
            // dealias_schema + table_factor_to_plan; src/sql/executor/scan.rs
            // handle_scan). Returning None signals the caller to fall through
            // to the planner; the planner handles SELECT projection, column
            // aliases, JOINs, complex WHERE, aggregates — all the things
            // this substring router didn't.
            return Ok(None);
        };

        // Apply WHERE filter + column projection based on the user's
        // SELECT clause. Catalog queries come in from every direction
        // (Drizzle / postgres-js / psycopg introspection), so without
        // these filters we'd send the full table regardless of the
        // predicate — B20 from the TimeTracker report.
        //
        // KanttBan #21A (v3.30.1): if the SELECT contains an aggregate
        // (`count(*)` / `count(col)`) we collapse rows AFTER filtering
        // and BEFORE projection — projection looks for column names in
        // the schema and can't see synthetic aggregate output columns.
        // drizzle-kit's introspection asks for things like
        //   SELECT count(*) FROM pg_namespace WHERE nspname IS NULL;
        //   SELECT table_schema, count(*) FROM information_schema.tables GROUP BY table_schema;
        // Without this stage both queries return the underlying tuples
        // and break tooling that expects scalar shapes.
        match result {
            Some((schema, rows)) => {
                // `matchable_folded` (not `query_lower`) so the filter reads the
                // clause from EXACTLY the text `where_clause_is_fully_supported`
                // classified above; `query_orig` still supplies every literal
                // VALUE, at the same byte offsets (HDB-011 review FIX-A/FIX-B).
                let filtered = Self::apply_where_filter(&matchable_folded, query_orig, &schema, rows);
                if let Some(agg) = Self::apply_aggregate(&query_lower, &schema, &filtered) {
                    return Ok(Some(agg));
                }
                let projected = Self::project_columns(&query_lower, schema, filtered);
                Ok(Some(projected))
            }
            None => Ok(None),
        }
    }

    /// Detect `count(*)` (with optional `GROUP BY <col>`) in the SELECT
    /// clause of a catalog query and collapse the rows accordingly.
    /// Returns `None` when the query is not an aggregate, leaving the
    /// caller to fall through to ordinary projection.
    ///
    /// Only handles the shapes drivers actually emit against catalog
    /// tables — bare `count(*)` and single-column `GROUP BY`. Anything
    /// more complex (multiple GROUP BY columns, HAVING, custom
    /// aggregates) falls through and the caller returns the
    /// underlying rows; that's the same "graceful degradation" path
    /// `apply_where_filter` and `project_columns` use.
    fn apply_aggregate(q: &str, schema: &Schema, rows: &[Tuple]) -> Option<(Schema, Vec<Tuple>)> {
        if !q.contains("count(") {
            return None;
        }

        let select_pos = q.find("select")? + "select".len();
        let from_pos = q.find(" from ")?;
        if select_pos >= from_pos {
            return None;
        }

        // Pull the GROUP BY column (if any). Stop at the next clause
        // keyword so trailing ORDER BY / LIMIT don't bleed in.
        let group_by_col = q.find(" group by ").map(|g| {
            let after = &q[g + " group by ".len()..];
            let mut end = after.len();
            for t in [" order by ", " having ", " limit ", " offset ", ";"] {
                if let Some(p) = after.find(t) {
                    if p < end {
                        end = p;
                    }
                }
            }
            after[..end].trim().to_string()
        });

        if let Some(group_col_raw) = group_by_col {
            // Strip alias prefix (`t.col` → `col`) and quotes.
            let group_col = group_col_raw
                .rsplit('.')
                .next()
                .unwrap_or(&group_col_raw)
                .trim()
                .trim_matches('"')
                .to_lowercase();
            let col_idx = schema.columns.iter().position(|c| c.name.to_lowercase() == group_col)?;

            let mut buckets: Vec<(Value, i64)> = Vec::new();
            for row in rows {
                let v = row.values.get(col_idx).cloned().unwrap_or(Value::Null);
                if let Some(b) = buckets.iter_mut().find(|(bv, _)| bv == &v) {
                    b.1 += 1;
                } else {
                    buckets.push((v, 1));
                }
            }

            // Safety: col_idx came from `position` above.
            #[allow(clippy::indexing_slicing)]
            let group_col_meta = schema.columns[col_idx].clone();
            let out_schema = Schema::new(vec![group_col_meta, Column::new("count", DataType::Int8)]);
            let out_rows: Vec<Tuple> = buckets
                .into_iter()
                .map(|(v, c)| Tuple::new(vec![v, Value::Int8(c)]))
                .collect();
            Some((out_schema, out_rows))
        } else {
            // Bare `count(*)` — collapse to a single scalar row.
            let n = rows.len() as i64;
            let out_schema = Schema::new(vec![Column::new("count", DataType::Int8)]);
            let out_rows = vec![Tuple::new(vec![Value::Int8(n)])];
            Some((out_schema, out_rows))
        }
    }

    /// Apply a small subset of WHERE predicates directly to catalog
    /// rows before we send them back. Supports the common driver
    /// introspection shapes:
    ///   * `col = 'literal'`
    ///   * `col = N`
    ///   * `col IN ('a','b',...)` / `col NOT IN (...)`
    ///   * `col <> 'literal'` / `col != 'literal'`
    ///   * conjunctions (`AND`) — evaluated left-to-right
    ///
    /// Anything more complex (OR, function calls, subqueries) falls
    /// through unchanged; the caller will get all rows, which is
    /// still correct-if-noisy for every driver I've tested — but see
    /// `where_clause_is_fully_supported`, which the caller consults FIRST for
    /// any view the planner can serve, so "keep every row" is now only the
    /// last resort for `pg_settings` (HDB-011).
    ///
    /// `q` is the ASCII-lowered, literal/comment-STRIPPED, whitespace-FOLDED
    /// statement (`matchable_folded`) and `q_orig` the ORIGINAL text at the
    /// same byte offsets. Clause, conjunct and operator detection reads `q` —
    /// so keywords and column names stay case-insensitive AND syntax that only
    /// occurs inside a string literal (`= 'x is null'`, `= 'a and b'`) cannot
    /// be mistaken for syntax (HDB-011 review FIX-A/FIX-B). Every literal VALUE
    /// is taken from `q_orig`, where the literal bodies are intact, so
    /// `typname = 'INT4'` still does not match the row named `int4`.
    fn apply_where_filter(q: &str, q_orig: &str, schema: &Schema, rows: Vec<Tuple>) -> Vec<Tuple> {
        let (start, end) = match Self::where_clause_span(q) {
            Some(span) => span,
            None => return rows,
        };
        let (ps, pe) = Self::trim_span(q, start, end);
        if ps >= pe {
            return rows;
        }
        let predicate = match q.get(ps..pe) {
            Some(p) => p,
            None => return rows,
        };
        // Same span in the original text; `to_ascii_lowercase` is
        // length-preserving so this is the identical run of characters.
        let predicate_orig = q_orig.get(ps..pe).unwrap_or(predicate);

        // Split on " and " at the top level (we don't handle parens). The spans
        // are computed once on the lowered text and applied to BOTH strings so
        // the two halves of every conjunct stay aligned.
        let spans = Self::split_conjunct_spans(predicate);
        rows.into_iter()
            .filter(|row| {
                spans.iter().all(|&(s, e)| {
                    let p = predicate.get(s..e).unwrap_or("");
                    let p_orig = predicate_orig.get(s..e).unwrap_or(p);
                    Self::eval_simple_pred(p, p_orig, schema, row)
                })
            })
            .collect()
    }

    /// Byte span of the WHERE clause body inside the lowered, whitespace-FOLDED
    /// statement text: from just after the `where` keyword up to the next
    /// clause keyword (`order by`, `group by`, `limit`, `offset`, `;`) or the
    /// end of the statement. Factored out of `apply_where_filter` so
    /// `where_clause_is_fully_supported` looks at exactly the same text the
    /// filter will (HDB-011).
    ///
    /// Every keyword is matched through `find_clause_keyword` rather than as
    /// `" where "` / `" order by "`, so a clause that ENDS the statement, or
    /// one written across a line break, is still found — and one that only
    /// occurs inside a string literal is not (HDB-011 review FIX-1).
    fn where_clause_span(q: &str) -> Option<(usize, usize)> {
        let where_at = Self::find_clause_keyword(q, "where", 0)?;
        let mut start = where_at + "where".len();
        // Exactly one space to skip, because whitespace is folded. A statement
        // that ENDS at the keyword leaves an empty predicate, which
        // `where_clause_is_fully_supported` treats as "not understood" rather
        // than as "no WHERE" (HDB-011 review FIX-1).
        if q.as_bytes().get(start) == Some(&b' ') {
            start += 1;
        }
        let mut end = q.len();
        if let Some(rel) = q.get(start..).and_then(|rest| rest.find(';')) {
            end = end.min(start + rel);
        }
        for kw in ["order", "group", "limit", "offset"] {
            let mut cursor = start;
            while let Some(at) = Self::find_clause_keyword(q, kw, cursor) {
                // `ORDER` / `GROUP` only close the predicate as `… BY`.
                let after = at + kw.len();
                let closes = match kw {
                    "order" | "group" => Self::find_clause_keyword(q, "by", after)
                        .and_then(|by| q.get(after..by))
                        .is_some_and(|gap| gap.trim().is_empty()),
                    _ => true,
                };
                if closes {
                    end = end.min(at);
                    break;
                }
                cursor = after;
            }
        }
        Some((start, end.max(start)))
    }

    /// `s` with every ASCII whitespace byte replaced by a plain space.
    ///
    /// Length-preserving: ` \t\n\r\x0b\x0c` are all one byte and all < 0x80,
    /// so no char boundary moves and every byte offset into the result still
    /// addresses the same character of the original statement — the property
    /// the case-sensitive literal comparison rests on. Without it a line break
    /// anywhere near `WHERE` / `AND` / `OR` / `ORDER BY` made this router's
    /// substring clause detection miss the clause entirely (HDB-011 review
    /// FIX-1).
    fn fold_ascii_whitespace(s: &str) -> String {
        let mut bytes = s.as_bytes().to_vec();
        for b in bytes.iter_mut() {
            if b.is_ascii_whitespace() {
                *b = b' ';
            }
        }
        // Only ASCII whitespace was replaced by ASCII space, so this is still
        // valid UTF-8; the fallback keeps the function total either way.
        String::from_utf8(bytes).unwrap_or_else(|_| s.to_string())
    }

    /// Byte offset of clause keyword `word` in `q` at or after `from`.
    ///
    /// `q` is the lowered, whitespace-FOLDED statement, so a real clause
    /// keyword is always preceded by a single space (or starts the statement)
    /// and followed by a single space, an opening parenthesis (`where(x = 1)`)
    /// or the end of the statement. Both sides matter: requiring the space
    /// before rejects `= 'x order by y'`, and requiring one of the three after
    /// rejects `= 'no limit'` — a bare `find` would read either as a clause
    /// keyword and truncate the predicate (HDB-011 review FIX-1).
    fn find_clause_keyword(q: &str, word: &str, from: usize) -> Option<usize> {
        let bytes = q.as_bytes();
        let mut cursor = from;
        while let Some(rel) = q.get(cursor..).and_then(|rest| rest.find(word)) {
            let at = cursor + rel;
            let before_ok = at == 0 || bytes.get(at - 1) == Some(&b' ');
            let after_ok = matches!(bytes.get(at + word.len()), None | Some(&b' ') | Some(&b'('));
            if before_ok && after_ok {
                return Some(at);
            }
            cursor = at + 1;
        }
        None
    }

    /// `(start, end)` of `s[start..end]` with leading/trailing whitespace
    /// removed, as byte offsets into `s`. Returned as offsets rather than a
    /// `&str` so the SAME span can be applied to the parallel original-case
    /// text (HDB-011).
    fn trim_span(s: &str, start: usize, end: usize) -> (usize, usize) {
        let slice = match s.get(start..end) {
            Some(slice) => slice,
            None => return (start, end),
        };
        let lead = slice.len() - slice.trim_start().len();
        let trail = slice.len() - slice.trim_end().len();
        if lead + trail >= slice.len() {
            return (start, start);
        }
        (start + lead, end - trail)
    }

    /// Byte spans of the top-level ` and `-separated conjuncts of `pred_lower`,
    /// each already trimmed. Parentheses are NOT tracked (this router has never
    /// handled them); `where_clause_is_fully_supported` is what refuses the
    /// shapes that would need them.
    fn split_conjunct_spans(pred_lower: &str) -> Vec<(usize, usize)> {
        const AND: &str = " and ";
        let mut spans = Vec::new();
        let mut cursor = 0usize;
        while let Some(rel) = pred_lower.get(cursor..).and_then(|rest| rest.find(AND)) {
            let abs = cursor + rel;
            spans.push(Self::trim_span(pred_lower, cursor, abs));
            cursor = abs + AND.len();
        }
        spans.push(Self::trim_span(pred_lower, cursor, pred_lower.len()));
        spans
    }

    /// True when EVERY top-level conjunct of the WHERE clause is a shape
    /// `eval_simple_pred` actually interprets — so applying the filter answers
    /// the user's question rather than silently widening it (HDB-011).
    ///
    /// `apply_where_filter` keeps a row whose predicate it cannot parse. For a
    /// view the planner can also serve, that "graceful degradation" is a wrong
    /// answer the client cannot detect: `WHERE tablename = 'a' OR tablename =
    /// 'zzz'` came back with every table in the database. The caller consults
    /// this first and defers to the planner (`Ok(None)`) when it is false.
    ///
    /// A clause is supported when it contains no top-level ` or `, and each
    /// ` and `-conjunct is one of: `col IS [NOT] NULL`, `col [NOT] IN (…)`
    /// (whose parentheses are the only ones allowed anywhere), or
    /// `col = / <> / != <literal>`. Anything else — a function call, a cast, a
    /// subquery, a `LIKE`, a range comparison — is not supported.
    ///
    /// The SPACES around the operator are part of the shape: `eval_simple_pred`
    /// looks for ` = `, so `tablename='a'` is NOT evaluable here and is refused
    /// (the planner then answers it correctly). Accepting it would run a filter
    /// that matches nothing.
    fn where_clause_is_fully_supported(q: &str) -> bool {
        let (start, end) = match Self::where_clause_span(q) {
            Some(span) => span,
            // No WHERE at all: there is nothing to misinterpret.
            None => return true,
        };
        let (ps, pe) = Self::trim_span(q, start, end);
        let predicate = match q.get(ps..pe) {
            Some(p) if !p.is_empty() => p,
            // There IS a WHERE keyword but no predicate came out of it (it
            // ended the statement, or the span was empty). Not understood —
            // defer, rather than serve the whole view (HDB-011 review FIX-1).
            _ => return false,
        };
        if predicate.contains(" or ") {
            return false;
        }
        Self::split_conjunct_spans(predicate)
            .iter()
            .all(|&(s, e)| Self::is_supported_pred_shape(predicate.get(s..e).unwrap_or("")))
    }

    /// One conjunct of `where_clause_is_fully_supported`. The order of the
    /// tests mirrors `eval_simple_pred` exactly, so the two can never disagree
    /// about which shape a predicate is.
    ///
    /// `p` is a conjunct of the literal/comment-STRIPPED text
    /// (`matchable_folded`), so every check below sees only real SQL syntax —
    /// `' is null'` or `' and '` sitting inside a string VALUE is already
    /// blanked out by the time it gets here (HDB-011 review FIX-A/FIX-B).
    ///
    /// Two shapes are refused even though `eval_simple_pred` would happily take
    /// them, because it evaluates both to the WRONG answer (review FIX-B):
    ///   * a qualified left-hand side (`t.tablename = 'x'`) — `row_value`
    ///     resolves bare column names against the canned schema and never
    ///     strips an alias or schema prefix, so the conjunct matches NO row and
    ///     an aliased existence check reports the table missing;
    ///   * an IN *subquery* (`tablename IN (SELECT …)`) — `parse_in_list` would
    ///     compare each row against the literal text `select …`, again matching
    ///     nothing.
    ///
    /// Refused, both go to the planner, which resolves aliases and subqueries.
    fn is_supported_pred_shape(p: &str) -> bool {
        let p = p.trim();
        if p.is_empty() {
            return false;
        }
        // `row_value` cannot resolve `alias.col` / `schema.col`.
        let lhs_is_bare_column = |idx: usize| !p.get(..idx).unwrap_or("").contains('.');
        if let Some(idx) = p.find(" is not null") {
            return lhs_is_bare_column(idx) && !p.contains('(');
        }
        if let Some(idx) = p.find(" is null") {
            return lhs_is_bare_column(idx) && !p.contains('(');
        }
        // ` not in (` first, exactly as `eval_simple_pred` tests it, so the
        // left-hand side of a NOT IN is the column and not `col not`.
        for kw in [" not in (", " in ("] {
            if let Some(idx) = p.find(kw) {
                // Exactly the IN list's own parenthesis pair, closing the conjunct.
                if p.matches('(').count() != 1 || p.matches(')').count() != 1 || !p.ends_with(')') {
                    return false;
                }
                // A SELECT inside the list is a subquery, not a literal list.
                if Self::contains_word(p.get(idx + kw.len()..).unwrap_or(""), "select") {
                    return false;
                }
                return lhs_is_bare_column(idx);
            }
        }
        for op in [" = ", " <> ", " != "] {
            if let Some(idx) = p.find(op) {
                return lhs_is_bare_column(idx) && !p.contains('(');
            }
        }
        false
    }

    /// Evaluate one of the predicate shapes supported by
    /// `apply_where_filter`. Returns `true` when the predicate can't
    /// be parsed — matches our "when in doubt, keep the row"
    /// behaviour and avoids silently dropping data for complex
    /// WHEREs we don't yet interpret.
    ///
    /// `pred` is the ASCII-lowered, literal/comment-STRIPPED conjunct and
    /// `pred_orig` the SAME byte span of the original statement. Keywords,
    /// operators and column names are read from `pred` — so a literal whose
    /// TEXT reads like syntax (`= 'x is null'`) is no longer parsed as syntax
    /// (HDB-011 review FIX-B). Every literal VALUE is read from `pred_orig`,
    /// where the literal bodies are intact, so string comparison is
    /// case-sensitive like PostgreSQL's (HDB-011).
    fn eval_simple_pred(pred: &str, pred_orig: &str, schema: &Schema, row: &Tuple) -> bool {
        let (ts, te) = Self::trim_span(pred, 0, pred.len());
        let p = pred.get(ts..te).unwrap_or("");
        let p_orig = pred_orig.get(ts..te).unwrap_or(p);

        // `col is null` / `col is not null` (KanttBan #21A, v3.30.1).
        // Must be tested BEFORE the `=` / `<>` family because these
        // predicates also contain spaces around the column name.
        // No literal involved, so the lowered text is enough.
        if let Some(idx) = p.find(" is not null") {
            let col_name = p.get(..idx).unwrap_or("").trim();
            let val = Self::row_value(schema, row, col_name);
            return !matches!(val, Value::Null);
        }
        if let Some(idx) = p.find(" is null") {
            let col_name = p.get(..idx).unwrap_or("").trim();
            let val = Self::row_value(schema, row, col_name);
            return matches!(val, Value::Null);
        }

        // `col NOT IN (a, b, c)` — must be tested BEFORE plain `IN`.
        if let Some(idx) = p.find(" not in (") {
            let col_name = p.get(..idx).unwrap_or("").trim();
            let items = Self::in_list_items(p, p_orig, idx + " not in (".len());
            let val = Self::row_value(schema, row, col_name);
            return !items.iter().any(|v| Self::lit_eq_value(v, &val));
        }
        if let Some(idx) = p.find(" in (") {
            let col_name = p.get(..idx).unwrap_or("").trim();
            let items = Self::in_list_items(p, p_orig, idx + " in (".len());
            let val = Self::row_value(schema, row, col_name);
            return items.iter().any(|v| Self::lit_eq_value(v, &val));
        }

        // `col = 'lit'`, `col = N`, `col <> 'lit'`, `col != 'lit'`
        for (op, eq) in [(" = ", true), (" <> ", false), (" != ", false)] {
            if let Some(idx) = p.find(op) {
                let col_name = p.get(..idx).unwrap_or("").trim();
                let (rs, re) = Self::trim_span(p, idx + op.len(), p.len());
                // The literal comes from the ORIGINAL text at the same offsets.
                let rhs = p_orig.get(rs..re).unwrap_or("");
                let val = Self::row_value(schema, row, col_name);
                let matches = Self::lit_eq_value(rhs, &val);
                return if eq { matches } else { !matches };
            }
        }

        // Unknown predicate shape — keep the row.
        true
    }

    /// The literals of an `IN (…)` list that starts at byte `open` in the
    /// lowered conjunct `p`. The trailing `)` is located on `p` (lowered) but
    /// the items are cut from `p_orig`, so their case survives (HDB-011).
    fn in_list_items(p: &str, p_orig: &str, open: usize) -> Vec<String> {
        let rest = p.get(open..).unwrap_or("");
        let stripped_len = rest.trim_end_matches(')').len();
        let end = open + stripped_len;
        Self::parse_in_list(p_orig.get(open..end).unwrap_or(""))
    }

    /// Split an `IN (…)` body on commas. Fed the ORIGINAL-case text, since
    /// the items are literal VALUES (HDB-011); `lit_eq_value` still matches the
    /// `null` / `true` / `false` keywords case-insensitively.
    fn parse_in_list(s: &str) -> Vec<String> {
        s.trim()
            .trim_matches(|c: char| c == '(' || c == ')')
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn row_value(schema: &Schema, row: &Tuple, col_name: &str) -> Value {
        let col_lower = col_name.trim().trim_matches('"').to_lowercase();
        if let Some(idx) = schema.columns.iter().position(|c| c.name.to_lowercase() == col_lower) {
            row.values.get(idx).cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    }

    /// Compare a literal (as written in SQL: `'abc'` or `42`) with a
    /// `Value`. Strips single quotes, parses numerics.
    ///
    /// HDB-011: `lit` MUST be the literal as the client wrote it, not a
    /// lowercased copy — string comparison here is byte-exact, so a lowercased
    /// `'INT4'` would match the row named `int4`. Keyword literals
    /// (`null` / `true` / `false`) are still matched case-insensitively.
    fn lit_eq_value(lit: &str, val: &Value) -> bool {
        let lit = lit.trim();
        // String literal
        if (lit.starts_with('\'') && lit.ends_with('\'')) && lit.len() >= 2 {
            let s = &lit[1..lit.len() - 1];
            return match val {
                Value::String(v) => v == s,
                Value::Null => false,
                other => other.to_string() == s,
            };
        }
        // NULL literal
        if lit.eq_ignore_ascii_case("null") {
            return matches!(val, Value::Null);
        }
        // Numeric literal
        if let Ok(n) = lit.parse::<i64>() {
            return match val {
                Value::Int2(v) => (*v as i64) == n,
                Value::Int4(v) => (*v as i64) == n,
                Value::Int8(v) => *v == n,
                _ => false,
            };
        }
        if let Ok(f) = lit.parse::<f64>() {
            return match val {
                Value::Float4(v) => (*v as f64 - f).abs() < 1e-9,
                Value::Float8(v) => (v - f).abs() < 1e-9,
                _ => false,
            };
        }
        // Bool
        if lit.eq_ignore_ascii_case("true") {
            return matches!(val, Value::Boolean(true));
        }
        if lit.eq_ignore_ascii_case("false") {
            return matches!(val, Value::Boolean(false));
        }
        false
    }

    /// Apply column projection based on the SELECT clause
    /// Parses "SELECT col1, col2 FROM ..." and returns only the requested columns
    /// Returns all columns for "SELECT *" or if parsing fails
    fn project_columns(query_lower: &str, schema: Schema, rows: Vec<Tuple>) -> (Schema, Vec<Tuple>) {
        // Extract SELECT column list
        let select_cols = Self::parse_select_columns(query_lower);

        // If no specific columns requested (SELECT * or parse failure), return all
        if select_cols.is_empty() {
            return (schema, rows);
        }

        // Build index map: for each requested column, find its position in the full schema
        let col_indices: Vec<usize> = select_cols
            .iter()
            .filter_map(|requested| schema.columns.iter().position(|c| c.name == *requested))
            .collect();

        // If no columns matched, return all (safety fallback)
        if col_indices.is_empty() {
            return (schema, rows);
        }

        // Build projected schema
        let projected_schema = Schema::new(
            // Safety: col_indices validated against schema.columns.len() above
            #[allow(clippy::indexing_slicing)]
            col_indices.iter().map(|&i| schema.columns[i].clone()).collect(),
        );

        // Build projected rows
        let projected_rows = rows
            .into_iter()
            .map(|row| {
                let values: Vec<Value> = col_indices
                    .iter()
                    .map(|&i| row.values.get(i).cloned().unwrap_or(Value::Null))
                    .collect();
                Tuple::new(values)
            })
            .collect();

        (projected_schema, projected_rows)
    }

    /// Parse SELECT column list from a query string
    /// Returns empty vec for "SELECT *" or if parsing fails
    fn parse_select_columns(query_lower: &str) -> Vec<String> {
        // Find "select" and "from" positions
        let select_pos = match query_lower.find("select") {
            Some(pos) => pos + 6, // skip "select"
            None => return vec![],
        };
        let from_pos = match query_lower.find(" from ") {
            Some(pos) => pos,
            None => return vec![],
        };

        if select_pos >= from_pos {
            return vec![];
        }

        let col_list = query_lower[select_pos..from_pos].trim();

        // SELECT * returns all columns
        if col_list == "*" {
            return vec![];
        }

        // Split by comma, trim, and collect column names
        col_list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Simple SQL LIKE pattern matching (supports % and _ wildcards)
    fn sql_like_match(text: &str, pattern: &str) -> bool {
        let t_chars: Vec<char> = text.chars().collect();
        let p_chars: Vec<char> = pattern.chars().collect();

        Self::like_match_recursive(&t_chars, &p_chars, 0, 0)
    }

    #[allow(clippy::indexing_slicing)] // Safety: pi/ti bounds checked at function entry and before use
    fn like_match_recursive(text: &[char], pattern: &[char], ti: usize, pi: usize) -> bool {
        if pi == pattern.len() {
            return ti == text.len();
        }

        match pattern[pi] {
            '%' => {
                // % matches zero or more characters
                for i in ti..=text.len() {
                    if Self::like_match_recursive(text, pattern, i, pi + 1) {
                        return true;
                    }
                }
                false
            }
            '_' => {
                // _ matches exactly one character
                if ti < text.len() {
                    Self::like_match_recursive(text, pattern, ti + 1, pi + 1)
                } else {
                    false
                }
            }
            c => {
                if ti < text.len() && text[ti] == c {
                    Self::like_match_recursive(text, pattern, ti + 1, pi + 1)
                } else {
                    false
                }
            }
        }
    }

    /// Query pg_class (relation/table information) - returns real tables from catalog
    fn query_pg_class(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("relname", DataType::Text),
            Column::new("relnamespace", DataType::Int4),
            Column::new("relkind", DataType::Text),
            Column::new("relowner", DataType::Int4),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        let catalog = db.storage.catalog();
        let table_names = catalog.list_tables()?;

        let mut rows = Vec::new();
        for (i, name) in table_names.iter().enumerate() {
            rows.push(Tuple::new(vec![
                Value::Int4((16384 + i) as i32), // Start OIDs at 16384 (user tables)
                Value::String(name.clone()),
                Value::Int4(2200),              // public namespace
                Value::String("r".to_string()), // regular table
                Value::Int4(10),                // owner
            ]));
        }

        Ok((schema, rows))
    }

    /// Query pg_namespace (schema information)
    fn query_pg_namespace(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("nspname", DataType::Text),
            Column::new("nspowner", DataType::Int4),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::Int4(11),
                Value::String("pg_catalog".to_string()),
                Value::Int4(10),
            ]),
            Tuple::new(vec![
                Value::Int4(2200),
                Value::String("public".to_string()),
                Value::Int4(10),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_database (database information)
    fn query_pg_database(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("datname", DataType::Text),
            Column::new("datdba", DataType::Int4),
            Column::new("encoding", DataType::Int4),
        ]);

        // Always include the implicit `heliosdb` system database. Then
        // append every tenant registered via `CREATE DATABASE` (the
        // v3.25 wrap of the multi-tenant API). Without this, `\l` and
        // every ORM that calls `pg_database` see only the default DB
        // even after `CREATE DATABASE foo` succeeded — KanttBan #16
        // partial fix against v3.28.0.
        let mut rows = vec![Tuple::new(vec![
            Value::Int4(1),
            Value::String("heliosdb".to_string()),
            Value::Int4(10),
            Value::Int4(6), // UTF8
        ])];
        if let Some(db) = self.database.as_ref() {
            for (i, t) in db.tenant_manager.list_tenants().iter().enumerate() {
                // Skip the implicit system database — already in the list.
                if t.name.eq_ignore_ascii_case("heliosdb") || t.name.eq_ignore_ascii_case("postgres") {
                    continue;
                }
                rows.push(Tuple::new(vec![
                    Value::Int4((100 + i) as i32),
                    Value::String(t.name.clone()),
                    Value::Int4(10),
                    Value::Int4(6),
                ]));
            }
        }

        Ok((schema, rows))
    }

    /// Query pg_settings (configuration parameters)
    fn query_pg_settings(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("name", DataType::Text),
            Column::new("setting", DataType::Text),
            Column::new("unit", DataType::Text),
            Column::new("category", DataType::Text),
        ]);

        let rows = vec![
            Tuple::new(vec![
                Value::String("server_version".to_string()),
                Value::String("17.0".to_string()),
                Value::Null,
                Value::String("Preset Options".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("server_encoding".to_string()),
                Value::String("UTF8".to_string()),
                Value::Null,
                Value::String("Preset Options".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("client_encoding".to_string()),
                Value::String("UTF8".to_string()),
                Value::Null,
                Value::String("Client Connection Defaults".to_string()),
            ]),
            Tuple::new(vec![
                Value::String("max_connections".to_string()),
                Value::String("100".to_string()),
                Value::Null,
                Value::String("Connections and Authentication".to_string()),
            ]),
        ];

        Ok((schema, rows))
    }

    /// Query pg_attribute (column information) - returns real column data from catalog
    fn query_pg_attribute(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("attrelid", DataType::Int4),
            Column::new("attname", DataType::Text),
            Column::new("atttypid", DataType::Int4),
            Column::new("attnum", DataType::Int2),
            Column::new("attlen", DataType::Int2),
        ]);

        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };

        let storage_catalog = db.storage.catalog();
        let table_names = storage_catalog.list_tables()?;

        let mut rows = Vec::new();
        for (ti, table_name) in table_names.iter().enumerate() {
            let oid = (16384 + ti) as i32;
            if let Ok(table_schema) = storage_catalog.get_table_schema(table_name) {
                for (ci, col) in table_schema.columns.iter().enumerate() {
                    let type_oid = Self::datatype_to_oid(&col.data_type);
                    let type_len = Self::datatype_to_len(&col.data_type);
                    rows.push(Tuple::new(vec![
                        Value::Int4(oid),
                        Value::String(col.name.clone()),
                        Value::Int4(type_oid),
                        Value::Int2((ci + 1) as i16),
                        Value::Int2(type_len),
                    ]));
                }
            }
        }

        Ok((schema, rows))
    }

    /// Map DataType to PostgreSQL type OID
    fn datatype_to_oid(dt: &DataType) -> i32 {
        match dt {
            DataType::Boolean => 16,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Int8 => 20,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Numeric => 1700,
            DataType::Varchar(_) => 1043,
            DataType::Text => 25,
            DataType::Char(_) => 1042,
            DataType::Bytea => 17,
            DataType::Date => 1082,
            DataType::Time => 1083,
            DataType::Timestamp => 1114,
            DataType::Timestamptz => 1184,
            DataType::Interval => 1186,
            DataType::Uuid => 2950,
            DataType::Json => 114,
            DataType::Jsonb => 3802,
            DataType::Array(_) => 2277,
            // HDB-002: `vector` used to report 25 here, 1000 on the wire and
            // 3614 in `pg_type`. `pg_type` and this map now use its private OID
            // 16385 (which frees PostgreSQL's real 3614 for `tsvector`); the PG
            // wire RowDescription deliberately keeps advertising `text` (25) —
            // see `handler::datatype_to_oid` for why.
            DataType::Vector(_) => 16385,
            DataType::TsVector => 3614,
            DataType::TsQuery => 3615,
        }
    }

    /// Detect the canonical queries that `psql` sends for its meta-commands
    /// (`\dt`, `\d table`, `\di`, `\dn`, `\du`, `\l`) and synthesise a shaped
    /// response. Returns `Ok(None)` if the query doesn't match any known
    /// psql signature — the caller should then fall through to the generic
    /// catalog handler.
    fn try_psql_metacommand(&self, q: &str) -> Result<Option<(Schema, Vec<Tuple>)>> {
        let db = match &self.database {
            Some(db) => db,
            None => return Ok(None),
        };
        let catalog = db.storage.catalog();

        // ---- \d <name> first sub-query: relation OID lookup ---------------------
        // psql resolves the target with a regex match:
        //
        //   SELECT c.oid, n.nspname, c.relname
        //   FROM pg_catalog.pg_class c
        //     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        //   WHERE c.relname OPERATOR(pg_catalog.~) '^(<name>)$' COLLATE pg_catalog.default
        //     AND pg_catalog.pg_table_is_visible(c.oid)
        //   ORDER BY 2, 3;
        //
        // The 5-col query_pg_class fallback returned every table, so
        // psql then iterated `\d` over each one in turn. Filter to
        // exactly the matching relation here (KanttBan #7 follow-up,
        // v3.30.1 smoke).
        if q.contains("operator(pg_catalog.~)")
            && q.contains("c.oid")
            && q.contains("c.relname")
            && q.contains("pg_table_is_visible")
        {
            let schema = Schema::new(vec![
                Column::new("oid", DataType::Int4),
                Column::new("nspname", DataType::Text),
                Column::new("relname", DataType::Text),
            ]);
            let pat = Self::extract_psql_regex_relname(q);
            let mut rows = Vec::new();
            for (ti, name) in catalog.list_tables()?.iter().enumerate() {
                if let Some(ref p) = pat {
                    if name != p {
                        continue;
                    }
                }
                rows.push(Tuple::new(vec![
                    Value::Int4((16384 + ti) as i32),
                    Value::String("public".into()),
                    Value::String(name.clone()),
                ]));
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \l (list databases) ------------------------------------------------
        // psql sends a multi-column SELECT joining pg_database to
        // pg_authid + pg_tablespace + pg_shdescription. v3.31.0 slice 4
        // wrinkle: the previous signature (`pg_database` + `d.datname`)
        // false-fired on drizzle-kit-style queries like
        // `SELECT d.datname AS db_name FROM pg_database d WHERE …`.
        // Tightened to require the multi-column shape psql actually
        // sends — `pg_get_userbyid(d.datdba)` (the owner column) is a
        // good discriminator since no ORM emits it.
        if q.contains("pg_database")
            && q.contains("pg_catalog.pg_database")
            && q.contains("d.datname")
            && q.contains("pg_get_userbyid(d.datdba)")
        {
            let schema = Schema::new(vec![
                Column::new("Name", DataType::Text),
                Column::new("Owner", DataType::Text),
                Column::new("Encoding", DataType::Text),
                Column::new("Collate", DataType::Text),
                Column::new("Ctype", DataType::Text),
                Column::new("Access privileges", DataType::Text),
            ]);
            let rows = vec![Tuple::new(vec![
                Value::String("heliosdb".into()),
                Value::String("heliosdb".into()),
                Value::String("UTF8".into()),
                Value::String("C.UTF-8".into()),
                Value::String("C.UTF-8".into()),
                Value::Null,
            ])];
            return Ok(Some((schema, rows)));
        }

        // ---- \du / \dg (list roles) --------------------------------------------
        // psql sends a SELECT of 11 columns from pg_catalog.pg_roles.
        // Mirror its exact shape so psql's client-side formatter accepts it.
        //
        // HC4: these rows used to be two hardcoded all-privilege superusers, so
        // `\du` reported a privilege posture nobody had configured. Shape and
        // rows now come from `sql::acl_views` — the SAME builders the phase-3
        // registry uses for `pg_roles` / `pg_user` / `pg_authid` — so `\du` and
        // `SELECT * FROM pg_roles` can never disagree. The two virtual built-ins
        // are still listed (compatibility), followed by every persisted role
        // with its REAL attribute bits. Those bits are RECORDED, NOT ENFORCED.
        if q.contains("pg_catalog.pg_roles") && q.contains("rolname") && q.contains("rolsuper") {
            let schema = Schema::new(crate::sql::acl_views::psql_du_columns());
            let rows = crate::sql::acl_views::psql_du_rows(&catalog)?;
            return Ok(Some((schema, rows)));
        }

        // ---- \dn (list schemas) -------------------------------------------------
        // Must NOT match \dt / \di / \d — those also JOIN pg_namespace.
        if q.contains("pg_catalog.pg_namespace")
            && q.contains("nspname")
            && q.contains("pg_get_userbyid")
            && !q.contains("pg_catalog.pg_class")
            && !q.contains("pg_class c")
        {
            let schema = Schema::new(vec![
                Column::new("Name", DataType::Text),
                Column::new("Owner", DataType::Text),
            ]);
            let rows = vec![Tuple::new(vec![
                Value::String("public".into()),
                Value::String("heliosdb".into()),
            ])];
            return Ok(Some((schema, rows)));
        }

        // ---- \dt / \d (list tables) --------------------------------------------
        // Signature: SELECT n.nspname, c.relname, ..., pg_get_userbyid(c.relowner)
        // FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ...
        // WHERE c.relkind IN ('r', ...)
        let is_dt = q.contains("pg_catalog.pg_class")
            && q.contains("pg_catalog.pg_namespace")
            && q.contains("pg_get_userbyid")
            && (q.contains("'r'") || q.contains("relkind in ('r"))
            && !q.contains("pg_index ");
        if is_dt {
            let schema = Schema::new(vec![
                Column::new("Schema", DataType::Text),
                Column::new("Name", DataType::Text),
                Column::new("Type", DataType::Text),
                Column::new("Owner", DataType::Text),
            ]);
            let mut rows = Vec::new();
            let name_filter = Self::extract_psql_relname_filter(q);
            for name in catalog.list_tables()? {
                if let Some(ref pat) = name_filter {
                    if !Self::sql_like_match(&name, pat) {
                        continue;
                    }
                }
                rows.push(Tuple::new(vec![
                    Value::String("public".into()),
                    Value::String(name),
                    Value::String("table".into()),
                    Value::String("heliosdb".into()),
                ]));
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \d table_name (KanttBan #7, v3.30.1 follow-up) ------------
        // The first query psql sends for `\d <name>` after resolving
        // the relation OID is a 15-column pg_class header pull:
        //
        //   SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules,
        //          c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity,
        //          false AS relhasoids, c.relispartition, '',
        //          c.reltablespace,
        //          CASE WHEN c.reloftype = 0 THEN '' ELSE … END,
        //          c.relpersistence, c.relreplident, am.amname
        //   FROM pg_catalog.pg_class c
        //     LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid)
        //     LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid)
        //   WHERE c.oid = '<oid>';
        //
        // The generic `pg_class` matcher returns only 5 columns, so
        // psql's libpq errors with "column number 5 is out of range
        // 0..4" — the exact message KanttBan reported in the v3.30
        // re-test. We special-case the shape and emit the 15 columns
        // psql's client formatter expects.
        if q.contains("pg_catalog.pg_class")
            && q.contains("relchecks")
            && q.contains("relhasindex")
            && q.contains("c.oid = '")
        {
            let schema = Schema::new(vec![
                Column::new("relchecks", DataType::Int2),
                Column::new("relkind", DataType::Char(1)),
                Column::new("relhasindex", DataType::Boolean),
                Column::new("relhasrules", DataType::Boolean),
                Column::new("relhastriggers", DataType::Boolean),
                Column::new("relrowsecurity", DataType::Boolean),
                Column::new("relforcerowsecurity", DataType::Boolean),
                Column::new("relhasoids", DataType::Boolean),
                Column::new("relispartition", DataType::Boolean),
                Column::new("reltoasttable", DataType::Text),
                Column::new("reltablespace", DataType::Int4),
                Column::new("reloftype", DataType::Text),
                Column::new("relpersistence", DataType::Char(1)),
                Column::new("relreplident", DataType::Char(1)),
                Column::new("amname", DataType::Text),
            ]);
            let target_oid = Self::extract_relchecks_oid(q);
            let table_names = catalog.list_tables()?;
            let mut rows = Vec::new();
            for (ti, name) in table_names.iter().enumerate() {
                let table_oid = (16384 + ti) as i32;
                if let Some(t) = target_oid {
                    if t != table_oid {
                        continue;
                    }
                }
                let has_index = catalog
                    .get_table_schema(name)
                    .map(|s| s.columns.iter().any(|c| c.primary_key || c.unique))
                    .unwrap_or(false);
                rows.push(Tuple::new(vec![
                    Value::Int2(0),               // relchecks
                    Value::String("r".into()),    // relkind = ordinary table
                    Value::Boolean(has_index),    // relhasindex
                    Value::Boolean(false),        // relhasrules
                    Value::Boolean(false),        // relhastriggers
                    Value::Boolean(false),        // relrowsecurity
                    Value::Boolean(false),        // relforcerowsecurity
                    Value::Boolean(false),        // relhasoids
                    Value::Boolean(false),        // relispartition
                    Value::String(String::new()), // (literal '' from psql query)
                    Value::Int4(0),               // reltablespace = pg_default
                    Value::String(String::new()), // CASE reloftype → ''
                    Value::String("p".into()),    // relpersistence = permanent
                    Value::String("d".into()),    // relreplident = default
                    Value::String("heap".into()), // am.amname
                ]));
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \d table_name (KanttBan #7, deferred from v3.28) ----------
        // psql's `\d <name>` sends several catalog queries; the one that
        // libpq error-rejects with "column number 5 is out of range 0..4"
        // is the per-column descriptor:
        //
        //   SELECT a.attname,
        //          pg_catalog.format_type(a.atttypid, a.atttypmod),
        //          (default-expr subquery),
        //          a.attnotnull,
        //          (collation subquery),
        //          a.attidentity,
        //          a.attgenerated
        //   FROM pg_catalog.pg_attribute a
        //   WHERE a.attrelid = '<oid>' AND a.attnum > 0 AND NOT a.attisdropped
        //   ORDER BY a.attnum;
        //
        // Match on the telltale `attnum > 0` + `attisdropped` combination
        // and emit the 7-column shape filled from our internal schema —
        // identity / generated / collation default to empty since Nano
        // doesn't expose them.
        //
        // KanttBan #7 follow-up (v3.30.1 smoke): the previous matcher
        // false-fired on `pg_statistic_ext` queries which JOIN
        // `pg_catalog.pg_attribute` in a subquery. Tightened to require
        // the OUTER `FROM pg_catalog.pg_attribute a` plus the
        // `a.attrelid = '<oid>'` WHERE predicate that only the
        // descriptor query emits.
        if q.contains("from pg_catalog.pg_attribute a")
            && q.contains("a.attrelid = '")
            && q.contains("a.attnum > 0")
            && q.contains("attisdropped")
        {
            let schema = Schema::new(vec![
                Column::new("attname", DataType::Text),
                Column::new("format_type", DataType::Text),
                Column::new("default_expr", DataType::Text),
                Column::new("attnotnull", DataType::Boolean),
                Column::new("collation", DataType::Text),
                Column::new("attidentity", DataType::Char(1)),
                Column::new("attgenerated", DataType::Char(1)),
            ]);
            // Extract the OID literal so we can find the matching table.
            // psql formats it as `a.attrelid = '<oid>'`. Any single OID
            // literal in the query is the target.
            let oid_literal = Self::extract_attrelid(q);
            let table_names = catalog.list_tables()?;
            let mut rows = Vec::new();
            for (ti, table_name) in table_names.iter().enumerate() {
                let table_oid = (16384 + ti) as i32;
                if let Some(target_oid) = oid_literal {
                    if target_oid != table_oid {
                        continue;
                    }
                }
                if let Ok(table_schema) = catalog.get_table_schema(table_name) {
                    for col in &table_schema.columns {
                        rows.push(Tuple::new(vec![
                            Value::String(col.name.clone()),
                            Value::String(Self::pg_format_type(&col.data_type)),
                            col.default_expr
                                .as_ref()
                                .map(|d| Value::String(d.clone()))
                                .unwrap_or(Value::Null),
                            Value::Boolean(!col.nullable),
                            Value::Null, // collation
                            Value::String(if col.primary_key {
                                "d".to_string()
                            } else {
                                "".to_string()
                            }),
                            Value::String(String::new()), // attgenerated — Nano has no GENERATED columns
                        ]));
                    }
                }
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \d <name> index list (12 columns) -----------------------------
        // psql sends:
        //
        //   SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered,
        //          i.indisvalid, pg_catalog.pg_get_indexdef(...),
        //          pg_catalog.pg_get_constraintdef(con.oid, true), contype,
        //          condeferrable, condeferred, i.indisreplident, c2.reltablespace
        //   FROM pg_catalog.pg_class c, pg_catalog.pg_class c2,
        //        pg_catalog.pg_index i
        //     LEFT JOIN pg_catalog.pg_constraint con ON …
        //   WHERE c.oid = '<oid>' AND c.oid = i.indrelid AND i.indexrelid = c2.oid
        //
        // The generic pg_index handler returns 5 cols; psql expected 12,
        // hence "column number 7 is out of range 0..4" on the v3.30.1
        // smoke (KanttBan #7 follow-up). Emit one row per PRIMARY KEY
        // and per UNIQUE column on the target relation.
        if q.contains("pg_get_indexdef") && q.contains("pg_get_constraintdef") && q.contains("c2.relname") {
            let schema = Schema::new(vec![
                Column::new("relname", DataType::Text),
                Column::new("indisprimary", DataType::Boolean),
                Column::new("indisunique", DataType::Boolean),
                Column::new("indisclustered", DataType::Boolean),
                Column::new("indisvalid", DataType::Boolean),
                Column::new("indexdef", DataType::Text),
                Column::new("constraintdef", DataType::Text),
                Column::new("contype", DataType::Char(1)),
                Column::new("condeferrable", DataType::Boolean),
                Column::new("condeferred", DataType::Boolean),
                Column::new("indisreplident", DataType::Boolean),
                Column::new("reltablespace", DataType::Int4),
            ]);
            let target_oid = Self::extract_relchecks_oid(q);
            let mut rows = Vec::new();
            for (ti, name) in catalog.list_tables()?.iter().enumerate() {
                let table_oid = (16384 + ti) as i32;
                if let Some(t) = target_oid {
                    if t != table_oid {
                        continue;
                    }
                }
                if let Ok(ts) = catalog.get_table_schema(name) {
                    let pk_cols: Vec<&str> = ts
                        .columns
                        .iter()
                        .filter(|c| c.primary_key)
                        .map(|c| c.name.as_str())
                        .collect();
                    if !pk_cols.is_empty() {
                        let cols = pk_cols.join(", ");
                        rows.push(Tuple::new(vec![
                            Value::String(format!("{}_pkey", name)),
                            Value::Boolean(true),  // indisprimary
                            Value::Boolean(true),  // indisunique
                            Value::Boolean(false), // indisclustered
                            Value::Boolean(true),  // indisvalid
                            Value::String(format!(
                                "CREATE UNIQUE INDEX {}_pkey ON public.{} USING btree ({})",
                                name, name, cols,
                            )),
                            Value::String(format!("PRIMARY KEY ({})", cols)),
                            Value::String("p".into()),
                            Value::Boolean(false),
                            Value::Boolean(false),
                            Value::Boolean(false),
                            Value::Int4(0),
                        ]));
                    }
                    for col in &ts.columns {
                        if col.unique && !col.primary_key {
                            rows.push(Tuple::new(vec![
                                Value::String(format!("{}_{}_key", name, col.name)),
                                Value::Boolean(false),
                                Value::Boolean(true),
                                Value::Boolean(false),
                                Value::Boolean(true),
                                Value::String(format!(
                                    "CREATE UNIQUE INDEX {0}_{1}_key ON public.{0} USING btree ({1})",
                                    name, col.name,
                                )),
                                Value::String(format!("UNIQUE ({})", col.name)),
                                Value::String("u".into()),
                                Value::Boolean(false),
                                Value::Boolean(false),
                                Value::Boolean(false),
                                Value::Int4(0),
                            ]));
                        }
                    }
                }
            }
            return Ok(Some((schema, rows)));
        }

        // ---- \di (list indexes) ------------------------------------------------
        let is_di = q.contains("pg_catalog.pg_class")
            && q.contains("pg_catalog.pg_namespace")
            && q.contains("pg_get_userbyid")
            && (q.contains("'i'") || q.contains("relkind in ('i"));
        if is_di {
            let schema = Schema::new(vec![
                Column::new("Schema", DataType::Text),
                Column::new("Name", DataType::Text),
                Column::new("Type", DataType::Text),
                Column::new("Owner", DataType::Text),
                Column::new("Table", DataType::Text),
            ]);
            let mut rows = Vec::new();
            for name in catalog.list_tables()? {
                if let Ok(ts) = catalog.get_table_schema(&name) {
                    if ts.columns.iter().any(|c| c.primary_key) {
                        rows.push(Tuple::new(vec![
                            Value::String("public".into()),
                            Value::String(format!("{}_pkey", name)),
                            Value::String("index".into()),
                            Value::String("heliosdb".into()),
                            Value::String(name.clone()),
                        ]));
                    }
                    for col in &ts.columns {
                        if col.unique && !col.primary_key {
                            rows.push(Tuple::new(vec![
                                Value::String("public".into()),
                                Value::String(format!("{}_{}_key", name, col.name)),
                                Value::String("index".into()),
                                Value::String("heliosdb".into()),
                                Value::String(name.clone()),
                            ]));
                        }
                    }
                }
            }
            return Ok(Some((schema, rows)));
        }

        Ok(None)
    }

    /// Extract the table OID literal from psql's
    /// `WHERE a.attrelid = '<oid>'` shape used by `\d <table>`.
    fn extract_attrelid(q: &str) -> Option<i32> {
        let marker = "attrelid = '";
        let start = q.find(marker)?;
        let after = q.get(start + marker.len()..)?;
        let end = after.find('\'')?;
        after.get(..end)?.parse::<i32>().ok()
    }

    /// Extract the table OID literal from psql's
    /// `WHERE c.oid = '<oid>'` shape used by `\d <table>`'s
    /// 15-column pg_class header pull.
    fn extract_relchecks_oid(q: &str) -> Option<i32> {
        let marker = "c.oid = '";
        let start = q.find(marker)?;
        let after = q.get(start + marker.len()..)?;
        let end = after.find('\'')?;
        after.get(..end)?.parse::<i32>().ok()
    }

    /// Extract the relation name from psql's `\d <name>` regex-match
    /// shape `c.relname OPERATOR(pg_catalog.~) '^(<name>)$' COLLATE …`.
    /// Returns None when the regex isn't a plain anchored name (e.g.
    /// the user passed a pattern with metacharacters), in which case
    /// the caller falls back to "return all tables".
    fn extract_psql_regex_relname(q: &str) -> Option<String> {
        let marker = "operator(pg_catalog.~) '^(";
        let start = q.find(marker)?;
        let after = q.get(start + marker.len()..)?;
        let end = after.find(")$'")?;
        let name = after.get(..end)?;
        if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            Some(name.to_string())
        } else {
            None
        }
    }

    /// Render a `DataType` in the long form `pg_catalog.format_type`
    /// produces for psql `\d`. Lossy but human-readable enough for the
    /// describe panel; `integer` / `text` / `timestamp without time zone`
    /// match how stock PG renders the corresponding columns.
    fn pg_format_type(dt: &DataType) -> String {
        match dt {
            DataType::Boolean => "boolean".into(),
            DataType::Int2 => "smallint".into(),
            DataType::Int4 => "integer".into(),
            DataType::Int8 => "bigint".into(),
            DataType::Float4 => "real".into(),
            DataType::Float8 => "double precision".into(),
            DataType::Numeric => "numeric".into(),
            DataType::Varchar(n) => match n {
                Some(len) => format!("character varying({len})"),
                None => "character varying".into(),
            },
            DataType::Char(n) => format!("character({n})"),
            DataType::Text => "text".into(),
            DataType::Bytea => "bytea".into(),
            DataType::Date => "date".into(),
            DataType::Time => "time without time zone".into(),
            DataType::Timestamp => "timestamp without time zone".into(),
            DataType::Timestamptz => "timestamp with time zone".into(),
            DataType::Interval => "interval".into(),
            DataType::Uuid => "uuid".into(),
            DataType::Json => "json".into(),
            DataType::Jsonb => "jsonb".into(),
            DataType::Array(inner) => format!("{}[]", Self::pg_format_type(inner)),
            DataType::Vector(n) => format!("vector({n})"),
            DataType::TsVector => "tsvector".into(),
            DataType::TsQuery => "tsquery".into(),
        }
    }

    /// Extract a `relname ~ '^(pattern)$'` filter from a psql \d query.
    fn extract_psql_relname_filter(q: &str) -> Option<String> {
        let marker = "relname ~ '^(";
        if let Some(start) = q.find(marker) {
            let after = q.get(start + marker.len()..)?;
            if let Some(end) = after.find(")$") {
                let pat = after.get(..end)?;
                // Convert regex anchor to LIKE-style pattern (approx): leave as-is for exact match.
                return Some(pat.to_string());
            }
        }
        None
    }

    /// Check whether a query touches any pg_catalog table we emulate.
    fn is_catalog_query(q: &str) -> bool {
        const MARKERS: &[&str] = &[
            "pg_catalog",
            "pg_type",
            "pg_class",
            "pg_namespace",
            "pg_attribute",
            "pg_database",
            "pg_index",
            "pg_indexes",
            "pg_sequences",
            "pg_tables",
            "pg_views",
            "pg_constraint",
            "pg_description",
            "pg_roles",
            "pg_user",
            "pg_proc",
            "pg_settings",
            "pg_policies",
            "pg_matviews",
        ];
        // Word-boundary match (task #38 F3): a marker must be a whole
        // identifier token, not a substring of a larger name. Without this a
        // user table like `app_pg_settings` or `my_pg_tables_backup` would be
        // permanently shadowed by the canned catalog response. `contains_word`
        // still matches qualified references (`pg_catalog.pg_class`) because
        // `.` is a boundary character. Caller passes the literal/comment
        // stripped `matchable` text so markers inside string literals /
        // comments don't count either.
        MARKERS.iter().any(|m| Self::contains_word(q, m))
    }

    /// Replace the CONTENTS of single-quoted string literals, line comments
    /// (`-- … EOL`) and block comments (`/* … */`, non-nested) with spaces,
    /// preserving every other byte verbatim (task #38 F2). This yields a
    /// "matchable" view of the statement in which catalog-name substring
    /// checks can't be fooled by a marker that only appears inside a literal
    /// or a comment. Doubled `''` inside a literal is an escaped quote and
    /// keeps us INSIDE the literal. Delimiter bytes (`'`, `-`, `/`, `*`,
    /// newline) are all ASCII (<0x80) and so never collide with a UTF-8
    /// continuation byte, making the byte scan safe for multibyte input.
    fn strip_literals_and_comments(q: &str) -> String {
        let bytes = q.as_bytes();
        let n = bytes.len();
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let mut i = 0;
        while i < n {
            let c = bytes[i];
            // Line comment: `--` to end of line.
            if c == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
                out.push(b' ');
                out.push(b' ');
                i += 2;
                while i < n && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
                continue;
            }
            // Block comment: `/* … */` (non-nested).
            if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                out.push(b' ');
                out.push(b' ');
                i += 2;
                while i < n {
                    if bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                        out.push(b' ');
                        out.push(b' ');
                        i += 2;
                        break;
                    }
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
                continue;
            }
            // Single-quoted string literal (with `''` escape).
            if c == b'\'' {
                out.push(b'\''); // preserve the opening quote position
                i += 1;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            // Escaped quote: stay inside the literal.
                            out.push(b' ');
                            out.push(b' ');
                            i += 2;
                            continue;
                        }
                        out.push(b'\''); // closing quote
                        i += 1;
                        break;
                    }
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
                continue;
            }
            out.push(c);
            i += 1;
        }
        // Every emitted byte is either a verbatim source byte or an ASCII
        // space/newline; no multibyte sequence is ever split, so the result is
        // valid UTF-8. Fall back to the original on the impossible error path.
        String::from_utf8(out).unwrap_or_else(|_| q.to_string())
    }

    /// True iff `needle` occurs in `haystack` at an identifier-token boundary:
    /// the character immediately before and after the match (if any) must NOT
    /// be an identifier byte (`[a-z0-9_]`) (task #38 F3). This is what stops a
    /// marker like `pg_settings` from matching inside `app_pg_settings`, while
    /// still matching inside `pg_catalog.pg_settings` (the `.` is a boundary).
    /// Operates on the already-lowercased text.
    fn contains_word(haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let hb = haystack.as_bytes();
        let nlen = needle.len();
        let mut start = 0;
        while let Some(rel) = haystack[start..].find(needle) {
            let abs = start + rel;
            let before_ok = abs == 0 || !Self::is_ident_byte(hb[abs - 1]);
            let after_idx = abs + nlen;
            let after_ok = after_idx >= hb.len() || !Self::is_ident_byte(hb[after_idx]);
            if before_ok && after_ok {
                return true;
            }
            start = abs + 1;
        }
        false
    }

    /// Identifier byte for `contains_word`: `[a-z0-9_]` (lowercased input).
    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
    }

    /// Query pg_index — per-table primary key and unique indexes.
    /// Columns: indexrelid, indrelid, indisunique, indisprimary, indkey.
    fn query_pg_index(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("indexrelid", DataType::Int4),
            Column::new("indrelid", DataType::Int4),
            Column::new("indisunique", DataType::Boolean),
            Column::new("indisprimary", DataType::Boolean),
            Column::new("indkey", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let tables = catalog.list_tables()?;
        let mut rows = Vec::new();
        for (ti, name) in tables.iter().enumerate() {
            let table_oid = (16384 + ti) as i32;
            if let Ok(tschema) = catalog.get_table_schema(name) {
                // Primary key: any column flagged primary_key
                let pk_cols: Vec<String> = tschema
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key)
                    .map(|(i, _)| (i + 1).to_string())
                    .collect();
                if !pk_cols.is_empty() {
                    rows.push(Tuple::new(vec![
                        Value::Int4(table_oid + 100_000), // synthetic index oid
                        Value::Int4(table_oid),
                        Value::Boolean(true), // indisunique
                        Value::Boolean(true), // indisprimary
                        Value::String(pk_cols.join(" ")),
                    ]));
                }
                // Unique indexes: any column flagged unique (non-PK)
                for (ci, col) in tschema.columns.iter().enumerate() {
                    if col.unique && !col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::Int4(table_oid + 100_000 + ci as i32 + 1),
                            Value::Int4(table_oid),
                            Value::Boolean(true),
                            Value::Boolean(false),
                            Value::String((ci + 1).to_string()),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    /// Query pg_tables (view) — 5 cols (schemaname, tablename, tableowner, tablespace, hasindexes).
    fn query_pg_tables(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("schemaname", DataType::Text),
            Column::new("tablename", DataType::Text),
            Column::new("tableowner", DataType::Text),
            Column::new("tablespace", DataType::Text),
            Column::new("hasindexes", DataType::Boolean),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let tables = db.storage.catalog().list_tables()?;
        let rows = tables
            .into_iter()
            .map(|t| {
                Tuple::new(vec![
                    Value::String("public".into()),
                    Value::String(t),
                    Value::String("heliosdb".into()),
                    Value::Null,
                    Value::Boolean(true),
                ])
            })
            .collect();
        Ok((schema, rows))
    }

    /// Query pg_constraint — primary key + unique constraints per table.
    fn query_pg_constraint(&self) -> Result<(Schema, Vec<Tuple>)> {
        let schema = Schema::new(vec![
            Column::new("oid", DataType::Int4),
            Column::new("conname", DataType::Text),
            Column::new("contype", DataType::Text), // 'p' PK, 'u' unique
            Column::new("conrelid", DataType::Int4),
            Column::new("conkey", DataType::Text),
        ]);
        let db = match &self.database {
            Some(db) => db,
            None => return Ok((schema, vec![])),
        };
        let catalog = db.storage.catalog();
        let tables = catalog.list_tables()?;
        let mut rows = Vec::new();
        for (ti, name) in tables.iter().enumerate() {
            let table_oid = (16384 + ti) as i32;
            if let Ok(tschema) = catalog.get_table_schema(name) {
                let pk_cols: Vec<String> = tschema
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key)
                    .map(|(i, _)| (i + 1).to_string())
                    .collect();
                if !pk_cols.is_empty() {
                    rows.push(Tuple::new(vec![
                        Value::Int4(table_oid + 200_000),
                        Value::String(format!("{}_pkey", name)),
                        Value::String("p".into()),
                        Value::Int4(table_oid),
                        Value::String(format!("{{{}}}", pk_cols.join(","))),
                    ]));
                }
                for (ci, col) in tschema.columns.iter().enumerate() {
                    if col.unique && !col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::Int4(table_oid + 200_000 + ci as i32 + 1),
                            Value::String(format!("{}_{}_key", name, col.name)),
                            Value::String("u".into()),
                            Value::Int4(table_oid),
                            Value::String(format!("{{{}}}", ci + 1)),
                        ]));
                    }
                }
            }
        }
        Ok((schema, rows))
    }

    // HC4: `query_pg_roles` (two hardcoded all-privilege superusers) is gone.
    // `pg_roles` / `pg_user` / `pg_authid` have no branch in this substring
    // router at all — they fall through to the planner and are answered by the
    // phase-3 registry from `sql::acl_views`, which reads the persisted role
    // catalog. The only role rows still built on this file's side are psql's
    // `\du` / `\dg` meta-command response, and those come from the same
    // `acl_views` builders (see `try_psql_metacommand`).

    /// Extract the view name from an `information_schema.<view>` reference.
    /// Returns the lowercase name on the first match, or `None` if the
    /// query references `information_schema` without naming a view.
    fn information_schema_view_name(q: &str) -> Option<String> {
        let marker = "information_schema.";
        let idx = q.find(marker)?;
        let tail = q.get(idx + marker.len()..)?;
        // Stop at the first non-identifier character.
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        let name = tail.get(..end)?.to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    /// Whitelist of SQL-standard `information_schema` view names that Nano
    /// recognises but legitimately doesn't populate. Returns a stable
    /// schema-only response (zero rows) so ORM probes get a well-formed
    /// reply rather than an error.
    fn known_empty_information_schema_view(name: &str) -> Option<(Schema, Vec<Tuple>)> {
        let cols: &[(&str, DataType)] = match name {
            "triggers" => &[
                ("trigger_catalog", DataType::Text),
                ("trigger_schema", DataType::Text),
                ("trigger_name", DataType::Text),
                ("event_manipulation", DataType::Text),
                ("event_object_catalog", DataType::Text),
                ("event_object_schema", DataType::Text),
                ("event_object_table", DataType::Text),
                ("action_statement", DataType::Text),
                ("action_orientation", DataType::Text),
                ("action_timing", DataType::Text),
            ],
            "parameters" => &[
                ("specific_catalog", DataType::Text),
                ("specific_schema", DataType::Text),
                ("specific_name", DataType::Text),
                ("ordinal_position", DataType::Int4),
                ("parameter_mode", DataType::Text),
                ("parameter_name", DataType::Text),
                ("data_type", DataType::Text),
            ],
            "sequences" => &[
                ("sequence_catalog", DataType::Text),
                ("sequence_schema", DataType::Text),
                ("sequence_name", DataType::Text),
                ("data_type", DataType::Text),
                ("start_value", DataType::Text),
                ("minimum_value", DataType::Text),
                ("maximum_value", DataType::Text),
                ("increment", DataType::Text),
            ],
            "domains" => &[
                ("domain_catalog", DataType::Text),
                ("domain_schema", DataType::Text),
                ("domain_name", DataType::Text),
                ("data_type", DataType::Text),
            ],
            "character_sets" => &[
                ("character_set_catalog", DataType::Text),
                ("character_set_schema", DataType::Text),
                ("character_set_name", DataType::Text),
                ("default_collate_name", DataType::Text),
            ],
            "collations" => &[
                ("collation_catalog", DataType::Text),
                ("collation_schema", DataType::Text),
                ("collation_name", DataType::Text),
            ],
            // HC4: table_privileges / column_privileges / usage_privileges /
            // role_*_grants / applicable_roles / enabled_roles /
            // administrable_role_authorizations are NOT listed here any more.
            // All ten are registered in the phase-3 registry (two populated
            // from the stored ACL catalog, eight shape-correct empty) and the
            // caller defers them to the planner, so one implementation answers
            // every route. Do not re-add a wire-side copy.
            "constraint_column_usage" | "constraint_table_usage" => &[
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("column_name", DataType::Text),
                ("constraint_catalog", DataType::Text),
                ("constraint_schema", DataType::Text),
                ("constraint_name", DataType::Text),
            ],
            "view_column_usage" | "view_table_usage" => &[
                ("view_catalog", DataType::Text),
                ("view_schema", DataType::Text),
                ("view_name", DataType::Text),
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
            ],
            "element_types" => &[
                ("object_catalog", DataType::Text),
                ("object_schema", DataType::Text),
                ("object_name", DataType::Text),
                ("data_type", DataType::Text),
            ],
            _ => return None,
        };
        let columns = cols.iter().map(|(n, dt)| Column::new(*n, dt.clone())).collect();
        Some((Schema::new(columns), vec![]))
    }

    /// information_schema.routines — SQL-standard schema, zero rows.
    /// Nano supports CREATE FUNCTION but does not currently expose its
    /// runtime function catalog through this view; ORM probes that look
    /// up routine names will see an empty set, which is correct (it
    /// signals "no user-defined routines visible").
    fn query_information_schema_routines() -> (Schema, Vec<Tuple>) {
        let schema = Schema::new(vec![
            Column::new("specific_catalog", DataType::Text),
            Column::new("specific_schema", DataType::Text),
            Column::new("specific_name", DataType::Text),
            Column::new("routine_catalog", DataType::Text),
            Column::new("routine_schema", DataType::Text),
            Column::new("routine_name", DataType::Text),
            Column::new("routine_type", DataType::Text),
            Column::new("data_type", DataType::Text),
            Column::new("type_udt_catalog", DataType::Text),
            Column::new("type_udt_schema", DataType::Text),
            Column::new("type_udt_name", DataType::Text),
            Column::new("routine_body", DataType::Text),
            Column::new("routine_definition", DataType::Text),
            Column::new("external_language", DataType::Text),
            Column::new("is_deterministic", DataType::Text),
            Column::new("security_type", DataType::Text),
        ]);
        (schema, vec![])
    }

    /// Bug 5 — validate a StartupMessage `database` parameter. Thin
    /// associated-function wrapper around `EmbeddedDatabase::database_name_is_valid`
    /// so the PG-wire handler doesn't need to peek at internals.
    pub fn is_valid_database_name(db: &EmbeddedDatabase, name: &str) -> bool {
        db.database_name_is_valid(name)
    }

    /// Map DataType to PostgreSQL type length
    fn datatype_to_len(dt: &DataType) -> i16 {
        match dt {
            DataType::Boolean => 1,
            DataType::Int2 => 2,
            DataType::Int4 => 4,
            DataType::Int8 => 8,
            DataType::Float4 => 4,
            DataType::Float8 => 8,
            DataType::Timestamp | DataType::Timestamptz => 8,
            DataType::Uuid => 16,
            _ => -1, // variable length
        }
    }
}

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// HDB-011: every `pg_type` shape defers to the planner. The router used
    /// to answer all of these from a 12-row fixed shape whose WHERE handling was
    /// string-splitting over LOWERCASED text — so `typname='int4'` (no spaces),
    /// any `OR`, and `count(*) … WHERE typname = 'hstore'` all returned the
    /// whole table. Row-level behaviour is asserted in tests/security_hdb_011.rs.
    #[test]
    fn hdb011_pg_type_is_deferred_to_the_planner() {
        let catalog = PgCatalog::new();
        for q in &[
            "SELECT * FROM pg_type",
            "SELECT typname FROM pg_type WHERE typname = 'int4'",
            "SELECT typname FROM pg_type WHERE typname='int4'",
            "SELECT typname FROM pg_type WHERE typname = 'INT4'",
            "SELECT count(*) FROM pg_type WHERE typname = 'hstore'",
            "SELECT typname FROM pg_type WHERE typname = 'int4' OR typname = 'text'",
            "SELECT t.typname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace",
        ] {
            let result = catalog.handle_query(q).expect("a pg_type SELECT must never error here");
            assert!(result.is_none(), "`{q}` must defer to the planner, got {result:?}");
        }
    }

    #[test]
    fn test_pg_namespace_query() {
        let catalog = PgCatalog::new();
        let result = catalog.query_pg_namespace();
        assert!(result.is_ok());

        let (schema, rows) = result.unwrap();
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_handle_query_non_catalog() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM users");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    /// HDB-011: `pg_type` moved to the planner, so the "this router still
    /// answers something" smoke test now uses `pg_settings` — the one view left
    /// here with no registry twin.
    #[test]
    fn test_handle_query_catalog() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM pg_settings");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    /// HC3: every registry-backed catalog view DEFERS to the planner
    /// (`Ok(None)`), including a plain single-view SELECT. The fixed-shape wire
    /// copies are deleted: they could not filter, project or JOIN, which is why
    /// `WHERE table_schema = 'public'` — the query every ORM opens with — used
    /// to return zero rows on `columns`. Pin the deferral so nobody
    /// "helpfully" re-adds an interception branch when a wire test fails.
    #[test]
    fn hc3_registry_backed_catalog_views_defer_to_planner() {
        let catalog = PgCatalog::new();
        for q in &[
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'my_notes'",
            "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default \
             FROM information_schema.columns WHERE table_schema = 'public'",
            "SELECT * FROM information_schema.schemata",
            "SELECT * FROM information_schema.catalog_name",
            "SELECT * FROM information_schema.views",
            "SELECT * FROM information_schema.check_constraints",
            "SELECT * FROM information_schema.key_column_usage",
            "SELECT * FROM information_schema.table_constraints",
            "SELECT * FROM information_schema.referential_constraints",
            "SELECT * FROM information_schema.constraint_column_usage",
            "SELECT * FROM information_schema.sequences",
            "SELECT * FROM pg_views",
            "SELECT * FROM pg_indexes",
        ] {
            let result = catalog.handle_query(q).unwrap();
            assert!(
                result.is_none(),
                "`{q}` must DEFER to the planner-backed SystemViewRegistry (Ok(None)); got {result:?}"
            );
        }
    }

    // -------------------------------------------------------------------
    // Task #38 — wire-protocol substring-hijack closure.
    //
    // `handle_query` runs on the RAW, lowercased statement text for EVERY
    // statement on the PG wire path. Before this fix, a `contains()` marker
    // check would intercept ANY statement mentioning a catalog name — even
    // inside a string literal, a comment, or as a substring of a user
    // identifier — silently discarding writes and shadowing user tables.
    // F1 (statement-kind gate), F2 (literal/comment stripping) and F3
    // (word-boundary matching) close that surface. These tests pin the
    // exact live-verified hijacks from the audit.
    // -------------------------------------------------------------------

    /// F1: a write whose literal mentions `pg_tables` must NOT be intercepted
    /// (it would silently never execute). Falls through to the real engine.
    #[test]
    fn task38_update_with_pg_tables_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("UPDATE inventory SET note='see pg_tables' WHERE id=1")
            .unwrap();
        assert!(result.is_none(), "UPDATE must fall through, got {result:?}");
    }

    /// F1: a write whose literal mentions `pg_settings` must fall through.
    #[test]
    fn task38_update_with_pg_settings_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("UPDATE inventory SET note='pg_settings changed' WHERE id=1")
            .unwrap();
        assert!(result.is_none(), "UPDATE must fall through, got {result:?}");
    }

    /// F1: `CREATE TABLE pg_type_registry` (marker as an identifier substring)
    /// must fall through so the table is actually created.
    #[test]
    fn task38_create_table_pg_type_substring_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("CREATE TABLE pg_type_registry (id int)").unwrap();
        assert!(result.is_none(), "CREATE TABLE must fall through, got {result:?}");
    }

    /// F1: `CREATE TABLE pg_views_cache` must fall through.
    #[test]
    fn task38_create_table_pg_views_substring_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("CREATE TABLE pg_views_cache (id int)").unwrap();
        assert!(result.is_none(), "CREATE TABLE must fall through, got {result:?}");
    }

    /// F2: a SELECT of a USER table whose literal mentions `pg_type` must NOT
    /// be intercepted by the pg_type dispatch — the marker is inside a string.
    #[test]
    fn task38_select_user_table_with_pg_type_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT * FROM my_notes WHERE body = 'see pg_type docs'")
            .unwrap();
        assert!(
            result.is_none(),
            "SELECT of user table must fall through, got {result:?}"
        );
    }

    /// F3: a user table named `app_pg_settings` must NOT be shadowed by the
    /// pg_settings canned response (word boundary: `_` before the marker).
    #[test]
    fn task38_select_word_boundary_app_pg_settings_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM app_pg_settings").unwrap();
        assert!(result.is_none(), "app_pg_settings must not be shadowed, got {result:?}");
    }

    /// F1/F2: a write whose literal mentions `information_schema.columns` must
    /// fall through (the write must execute).
    #[test]
    fn task38_update_with_information_schema_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("UPDATE inventory SET note='check information_schema.columns' WHERE id=1")
            .unwrap();
        assert!(result.is_none(), "UPDATE must fall through, got {result:?}");
    }

    /// F1: an INSERT mentioning an unknown information_schema view in a literal
    /// must fall through as Ok(None) — NOT raise the spurious unknown-view
    /// ERROR the old bare-branch produced.
    #[test]
    fn task38_insert_with_unknown_information_schema_literal_is_none_not_err() {
        let catalog = PgCatalog::new();
        let result =
            catalog.handle_query("INSERT INTO my_notes VALUES (9, 'read information_schema.sql_features spec')");
        assert!(
            matches!(result, Ok(None)),
            "INSERT with information_schema literal must be Ok(None), got {result:?}"
        );
    }

    /// F2/F4: a SELECT of a user table whose literal contains the bare word
    /// `information_schema` must fall through (no degenerate empty result).
    #[test]
    fn task38_select_user_table_with_bare_information_schema_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT * FROM my_notes WHERE body = 'the information_schema is useful'")
            .unwrap();
        assert!(
            result.is_none(),
            "bare information_schema literal must fall through, got {result:?}"
        );
    }

    /// F1: an INSERT whose literal contains the verbatim psql `\dt` catalog
    /// query must fall through — the psql signature must not intercept a write.
    #[test]
    fn task38_insert_with_psql_dt_signature_in_literal_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query(
                "INSERT INTO query_log VALUES (1, 'SELECT n.nspname, c.relname FROM pg_catalog.pg_class c \
                 LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN (''r'') AND pg_catalog.pg_get_userbyid(c.relowner) = x')",
            )
            .unwrap();
        assert!(
            result.is_none(),
            "INSERT with psql signature literal must fall through, got {result:?}"
        );
    }

    /// F2: a trailing line comment mentioning `pg_tables` must not hijack a
    /// plain user-table SELECT.
    #[test]
    fn task38_select_with_trailing_comment_marker_falls_through() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT * FROM t -- see pg_tables").unwrap();
        assert!(result.is_none(), "comment marker must not hijack, got {result:?}");
    }

    // ---- The introspection contract these branches exist for still holds ---

    /// A real `pg_tables` reference is still intercepted.
    #[test]
    fn task38_real_pg_tables_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT tablename FROM pg_tables").unwrap();
        assert!(result.is_some(), "real pg_tables SELECT must still be served");
    }

    /// The drizzle shape: markers inside ITS OWN literals get stripped, but the
    /// real `FROM pg_tables` reference remains and must still be served.
    #[test]
    fn task38_drizzle_pg_tables_shape_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query(
                "SELECT schemaname, tablename FROM pg_tables \
                 WHERE schemaname NOT IN ('pg_catalog','information_schema')",
            )
            .unwrap();
        assert!(result.is_some(), "drizzle pg_tables shape must still be served");
    }

    /// A schema-qualified `pg_catalog.pg_type` reference must survive
    /// `contains_word` (the `.` is a token boundary) — and, since HDB-011, be
    /// DEFERRED to the planner rather than intercepted. The planner collapses
    /// `pg_catalog.pg_type` to `pg_type` and serves it from the registry.
    #[test]
    fn task38_qualified_pg_type_is_deferred_to_the_planner() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT oid, typname FROM pg_catalog.pg_type")
            .unwrap();
        assert!(result.is_none(), "qualified pg_catalog.pg_type must reach the planner");
    }

    /// A real `information_schema.columns` SELECT is answered — by the planner
    /// after HC3, not by this router. The task-#38 contract that matters here is
    /// that it neither ERRORS nor gets hijacked: `Ok(None)` is the "the real
    /// engine handles this" signal, and the engine has the view registered.
    /// Row-level behaviour is asserted in tests/catalog_introspection_tests.rs
    /// and the wire tests.
    #[test]
    fn task38_real_information_schema_columns_reaches_the_engine() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("SELECT column_name FROM information_schema.columns WHERE table_name = 'my_notes'")
            .expect("a real information_schema.columns SELECT must never error here");
        assert!(
            result.is_none(),
            "real information_schema.columns SELECT must reach the planner, got {result:?}"
        );
    }

    /// The verbatim psql `\dt` query still returns the 4-column
    /// Schema/Name/Type/Owner shape (needs a live database handle).
    #[test]
    fn task38_psql_dt_still_returns_four_column_shape() {
        use std::sync::Arc;
        let db = crate::EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE widgets (id INT PRIMARY KEY)").unwrap();
        let catalog = PgCatalog::with_database(Arc::new(db));
        // The query psql sends for `\dt` (modern form, `!~` not OPERATOR()).
        let dt = "SELECT n.nspname as \"Schema\", c.relname as \"Name\", \
                  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' \
                  WHEN 'm' THEN 'materialized view' WHEN 'S' THEN 'sequence' \
                  WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' END as \"Type\", \
                  pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\" \
                  FROM pg_catalog.pg_class c \
                  LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.relkind IN ('r','p','') AND n.nspname <> 'pg_catalog' \
                  AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema' \
                  AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 1,2";
        let (schema, _rows) = catalog.handle_query(dt).unwrap().expect("psql \\dt must be served");
        let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Schema", "Name", "Type", "Owner"],
            "psql \\dt must return the 4-column Schema/Name/Type/Owner shape"
        );
    }

    /// A real `pg_settings` reference is still intercepted.
    #[test]
    fn task38_real_pg_settings_still_intercepted() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("SELECT name, setting FROM pg_settings").unwrap();
        assert!(result.is_some(), "real pg_settings SELECT must still be served");
    }

    // ---- Direct unit coverage of the F2/F3 helpers ---------------------

    #[test]
    fn task38_strip_literals_and_comments_blanks_contents() {
        // Literal contents blanked, quote positions preserved, `''` escape kept
        // inside the literal, structure outside literals intact.
        let out = PgCatalog::strip_literals_and_comments("select * from t where c='pg_tables' and d=1");
        assert!(!out.contains("pg_tables"), "literal contents must be blanked: {out}");
        assert!(
            out.contains("select * from t where c="),
            "outside-literal text intact: {out}"
        );
        assert!(out.contains("and d=1"), "trailing predicate intact: {out}");

        // Line comment blanked.
        let out = PgCatalog::strip_literals_and_comments("select * from t -- see pg_tables");
        assert!(!out.contains("pg_tables"), "line comment must be blanked: {out}");

        // Block comment blanked.
        let out = PgCatalog::strip_literals_and_comments("select /* pg_settings */ 1");
        assert!(!out.contains("pg_settings"), "block comment must be blanked: {out}");

        // Doubled '' escape keeps us inside the literal (no marker leaks).
        let out = PgCatalog::strip_literals_and_comments("x 'a''pg_type''b' y");
        assert!(
            !out.contains("pg_type"),
            "escaped-quote literal must stay blanked: {out}"
        );
        assert!(out.contains('x') && out.contains('y'), "surrounding text intact: {out}");
    }

    #[test]
    fn task38_contains_word_respects_boundaries() {
        assert!(PgCatalog::contains_word("select * from pg_tables", "pg_tables"));
        // Qualified reference: `.` is a boundary.
        assert!(PgCatalog::contains_word("from pg_catalog.pg_tables x", "pg_tables"));
        // Substring of a longer identifier must NOT match.
        assert!(!PgCatalog::contains_word(
            "select * from app_pg_settings",
            "pg_settings"
        ));
        assert!(!PgCatalog::contains_word("select * from pg_tables_backup", "pg_tables"));
        // Trailing/leading boundary at string ends.
        assert!(PgCatalog::contains_word("pg_type", "pg_type"));
        assert!(!PgCatalog::contains_word("pg_typeof(x)", "pg_type"));
    }

    #[test]
    fn test_like_match() {
        assert!(PgCatalog::sql_like_match("tenant_abc__users", "tenant_abc__%"));
        assert!(PgCatalog::sql_like_match("tenant_abc__orders", "tenant_abc__%"));
        assert!(!PgCatalog::sql_like_match("other_table", "tenant_abc__%"));
        assert!(PgCatalog::sql_like_match("hello", "hel%"));
        assert!(PgCatalog::sql_like_match("hello", "h_llo"));
        assert!(!PgCatalog::sql_like_match("hello", "h_lo"));
    }

    #[test]
    fn test_information_schema_columns_filter_distinguishes_tables() {
        // Regression for the a2h v3.60.3 report. With multiple tables each having
        // a `nextval` default, `information_schema.columns` read back the WRONG
        // table's default: the `table_name='t'`/`column_name='c'` filter (no
        // spaces around `=`, as psycopg emits) was dropped, the handler returned
        // every table's columns, and a client `fetchone()` got the first table's
        // first defaulted column. The stored defaults were always correct.
        // HC3: the hand-rolled `extract_eq_filter` this used to exercise is gone
        // along with the whole wire-side copy of the view; the planner now
        // evaluates the predicate. The USER-VISIBLE contract is unchanged and is
        // what this test pins — asserted through the engine, which is exactly
        // where the wire now routes it.
        let db = crate::EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE SEQUENCE actor_actor_id_seq").unwrap();
        db.execute("CREATE TABLE actor (actor_id INT DEFAULT nextval('actor_actor_id_seq'), first_name TEXT)")
            .unwrap();
        db.execute("CREATE SEQUENCE harden_seq").unwrap();
        db.execute("CREATE TABLE harden_t (id INT DEFAULT nextval('harden_seq'), v TEXT)")
            .unwrap();

        let default_of = |sql: &str| -> String {
            let (rows, _cols) = db.query_with_columns(sql).unwrap();
            assert_eq!(
                rows.len(),
                1,
                "expected exactly one row for `{sql}`, got {}",
                rows.len()
            );
            match rows[0].values.first() {
                Some(Value::String(s)) => s.clone(),
                other => panic!("expected a string column_default, got {other:?}"),
            }
        };

        // a2h's exact no-space query must return each table's OWN sequence default.
        let h = default_of(
            "select column_default from information_schema.columns where table_name='harden_t' and column_name='id'",
        );
        assert!(
            h.contains("harden_seq"),
            "harden_t.id default should be harden_seq, got {h}"
        );
        assert!(
            !h.contains("actor"),
            "harden_t.id default must NOT leak actor's sequence, got {h}"
        );

        let a = default_of(
            "select column_default from information_schema.columns where table_name='actor' and column_name='actor_id'",
        );
        assert!(
            a.contains("actor_actor_id_seq"),
            "actor.actor_id default should be actor_actor_id_seq, got {a}"
        );
    }

    // -------------------------------------------------------------------
    // KanttBan #21A (v3.30.1) — aggregates / WHERE IS NULL on pg_catalog.
    //
    // v3.30.1 implemented these in a custom `apply_aggregate` post-filter
    // stage inside the catalog handler. v3.31.0 (KanttBan #22) moved the
    // catalog reads through the regular planner — these queries now
    // return `Ok(None)` from `handle_query` and the planner's aggregate
    // operator takes over. The contract these tests assert flipped:
    //     v3.30.1: Some((schema=[count], rows=[Int8(n)]))
    //     v3.31.0: None  (fall through to planner)
    // End-to-end behaviour for the user is identical (smoked via psql);
    // tested here at the handler boundary.
    // -------------------------------------------------------------------

    #[test]
    fn count_star_pg_namespace_falls_through_to_planner() {
        let catalog = PgCatalog::new();
        let result = catalog.handle_query("select count(*) from pg_namespace").unwrap();
        assert!(
            result.is_none(),
            "pg_namespace should fall through to planner; got {result:?}"
        );
    }

    #[test]
    fn count_star_with_is_null_filter_falls_through_to_planner() {
        // Original KanttBan #21A shape:
        // SELECT count(*) FROM pg_namespace WHERE nspname IS NULL;
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select count(*) from pg_namespace where nspname is null")
            .unwrap();
        assert!(
            result.is_none(),
            "pg_namespace WHERE IS NULL should fall through; got {result:?}"
        );
    }

    #[test]
    fn count_star_with_is_not_null_filter_falls_through_to_planner() {
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select count(*) from pg_namespace where nspname is not null")
            .unwrap();
        assert!(
            result.is_none(),
            "pg_namespace WHERE IS NOT NULL should fall through; got {result:?}"
        );
    }

    #[test]
    fn group_by_information_schema_tables_falls_through_to_planner() {
        // v3.31.0 slice 4: information_schema.tables migrated to the
        // SystemViewRegistry, so this query now falls through to the
        // planner exactly like the pg_namespace variants above.
        // End-to-end behaviour is preserved (smoked via psql); the
        // aggregate is now applied by the planner's aggregate
        // operator, not the catalog handler's apply_aggregate.
        let catalog = PgCatalog::new();
        let result = catalog
            .handle_query("select table_schema, count(*) from information_schema.tables group by table_schema")
            .unwrap();
        assert!(
            result.is_none(),
            "information_schema.tables should fall through; got {result:?}"
        );
    }

    #[test]
    fn is_null_eval_simple_pred_drops_non_null_row() {
        let schema = Schema::new(vec![Column::new("c", DataType::Text)]);
        let row_text = Tuple::new(vec![Value::String("x".into())]);
        let row_null = Tuple::new(vec![Value::Null]);
        // HDB-011: `eval_simple_pred` now takes the lowered conjunct AND the
        // original-case text at the same offsets. IS NULL reads no literal, so
        // the two are the same string here.
        assert!(!PgCatalog::eval_simple_pred(
            "c is null",
            "c is null",
            &schema,
            &row_text
        ));
        assert!(PgCatalog::eval_simple_pred(
            "c is null",
            "c is null",
            &schema,
            &row_null
        ));
        assert!(PgCatalog::eval_simple_pred(
            "c is not null",
            "c is not null",
            &schema,
            &row_text
        ));
        assert!(!PgCatalog::eval_simple_pred(
            "c is not null",
            "c is not null",
            &schema,
            &row_null
        ));
    }

    /// HDB-011: a literal's VALUE is compared exactly as the client wrote it.
    /// Before the fix the whole statement was lowercased before it reached this
    /// code, so `= 'INT4'` matched a row holding `int4`.
    #[test]
    fn hdb011_eval_simple_pred_compares_literals_case_sensitively() {
        let schema = Schema::new(vec![Column::new("typname", DataType::Text)]);
        let row = Tuple::new(vec![Value::String("int4".into())]);

        // Keyword/column case still does not matter; the literal's does.
        assert!(PgCatalog::eval_simple_pred(
            "typname = 'int4'",
            "TYPNAME = 'int4'",
            &schema,
            &row
        ));
        assert!(!PgCatalog::eval_simple_pred(
            "typname = 'int4'",
            "typname = 'INT4'",
            &schema,
            &row
        ));
        assert!(PgCatalog::eval_simple_pred(
            "typname <> 'int4'",
            "typname <> 'INT4'",
            &schema,
            &row
        ));
        // IN lists take their items from the original text too.
        assert!(PgCatalog::eval_simple_pred(
            "typname in ('int4','int8')",
            "typname in ('int4','int8')",
            &schema,
            &row
        ));
        assert!(!PgCatalog::eval_simple_pred(
            "typname in ('int4','int8')",
            "typname in ('INT4','INT8')",
            &schema,
            &row
        ));
    }

    /// HDB-011: the WHERE shapes this router can actually evaluate. Anything
    /// else must be reported unsupported so `handle_query` defers to the
    /// planner instead of returning the unfiltered catalog.
    #[test]
    fn hdb011_where_clause_support_matches_what_we_can_evaluate() {
        for q in &[
            "select tablename from pg_tables",
            "select tablename from pg_tables where tablename = 'a'",
            "select tablename from pg_tables where schemaname = 'public' and tablename <> 'a'",
            "select tablename from pg_tables where schemaname not in ('pg_catalog','information_schema')",
            "select tablename from pg_tables where tablename is not null",
            "select tablename from pg_tables where tablename = 'a' order by tablename",
        ] {
            assert!(
                PgCatalog::where_clause_is_fully_supported(q),
                "`{q}` should be evaluable here"
            );
        }
        for q in &[
            "select tablename from pg_tables where tablename = 'a' or tablename = 'zzz'",
            // `tablename='a'` — no spaces around the operator — is NOT a shape
            // `eval_simple_pred` evaluates (it looks for ` = `, with spaces),
            // so it must be REFUSED here, not accepted: accepting it would run
            // a filter that matches nothing and answer `a` with zero rows.
            // Refused, the planner answers it, which is the correct result.
            "select tablename from pg_tables where tablename='a'",
            "select tablename from pg_tables where tablename like 'a%'",
            "select tablename from pg_tables where length(tablename) > 3",
            "select tablename from pg_tables where (tablename = 'a')",
            "select tablename from pg_tables where tablename = (select 'a')",
        ] {
            assert!(
                !PgCatalog::where_clause_is_fully_supported(q),
                "`{q}` must be refused so the planner answers it"
            );
        }
    }

    /// HDB-011 review FIX-1: clause keywords are located AFTER every ASCII
    /// whitespace byte has been folded to a space, so a WHERE (or an OR, an
    /// AND, an ORDER BY) on its own line reads exactly like the one-line
    /// spelling. Before the fold, `\nWHERE …` was invisible to
    /// `where_clause_span`, `where_clause_is_fully_supported` answered "no
    /// WHERE at all → supported", and `pg_tables` came back UNFILTERED.
    #[test]
    fn hdb011_line_breaks_do_not_defeat_the_where_guard() {
        let fold = |q: &str| PgCatalog::fold_ascii_whitespace(&q.to_ascii_lowercase());
        let predicate = |q: &str| {
            let folded = fold(q);
            let (start, end) = PgCatalog::where_clause_span(&folded).expect("a WHERE clause");
            folded.get(start..end).unwrap_or("").trim().to_string()
        };

        // The clause is FOUND across a line break, a tab, and a statement that
        // ends right after the predicate.
        assert_eq!(
            predicate("SELECT tablename FROM pg_tables\nWHERE tablename = 'a'"),
            "tablename = 'a'"
        );
        assert_eq!(
            predicate("SELECT tablename FROM pg_tables\n\tWHERE\ttablename = 'a'\nORDER BY tablename"),
            "tablename = 'a'"
        );
        assert_eq!(
            predicate("SELECT tablename FROM pg_tables\nWHERE tablename = 'a'\nLIMIT 1;"),
            "tablename = 'a'"
        );

        // …and then judged on its merits, exactly as the one-line spelling is.
        for q in &[
            "SELECT tablename FROM pg_tables\nWHERE tablename = 'a' OR tablename = 'zzz'",
            "SELECT tablename FROM pg_tables WHERE tablename = 'a'\nOR tablename = 'zzz'",
            "SELECT tablename FROM pg_tables WHERE tablename = 'a'\tOR\ttablename = 'zzz'",
            "SELECT tablename FROM pg_tables WHERE tablename = 'a'\nAND length(tablename) > 1",
            "SELECT tablename FROM pg_tables\nWHERE tablename='a'",
        ] {
            assert!(
                !PgCatalog::where_clause_is_fully_supported(&fold(q)),
                "`{q}` must be refused so the planner answers it"
            );
        }
        for q in &[
            "SELECT tablename FROM pg_tables\nWHERE tablename = 'a'",
            "SELECT tablename FROM pg_tables\nWHERE schemaname = 'public'\nAND tablename <> 'a'",
            "SELECT tablename FROM pg_tables\nWHERE tablename IN ('a','b')\nORDER BY tablename",
        ] {
            assert!(
                PgCatalog::where_clause_is_fully_supported(&fold(q)),
                "`{q}` should be evaluable here"
            );
        }

        // A clause keyword INSIDE a literal is not a clause keyword: the whole
        // predicate must survive, not just the text before the quote.
        assert_eq!(
            predicate("SELECT tablename FROM pg_tables WHERE tablename = 'order by me'"),
            "tablename = 'order by me'"
        );
    }

    /// HDB-011: `pg_tables` has a registry twin, so an uninterpretable WHERE
    /// defers instead of silently returning every table. `pg_settings` has no
    /// twin, so it deliberately keeps the old keep-every-row behaviour rather
    /// than breaking psql / pgAdmin startup.
    #[test]
    fn hdb011_unsupported_where_defers_only_for_registry_served_views() {
        let catalog = PgCatalog::new();
        let deferred = catalog
            .handle_query("SELECT tablename FROM pg_tables WHERE tablename = 'a' OR tablename = 'zzz'")
            .unwrap();
        assert!(
            deferred.is_none(),
            "an OR predicate on pg_tables must reach the planner, got {deferred:?}"
        );

        // Review FIX-1: the same statement with the WHERE on its own line —
        // the way every ORM and every formatted client writes it — must defer
        // as well. Before the whitespace fold this returned every table.
        let deferred_two_line = catalog
            .handle_query("SELECT tablename FROM pg_tables\nWHERE tablename = 'a' OR tablename = 'zzz'")
            .unwrap();
        assert!(
            deferred_two_line.is_none(),
            "an OR predicate on the next line must reach the planner, got {deferred_two_line:?}"
        );

        let still_served = catalog
            .handle_query("SELECT name FROM pg_settings WHERE name = 'a' OR name = 'zzz'")
            .unwrap();
        assert!(
            still_served.is_some(),
            "pg_settings has no registry twin and must still answer"
        );
    }

    /// HDB-011 review FIX-A/FIX-B: the WHERE clause is classified on the
    /// literal/comment-STRIPPED, whitespace-FOLDED copy of the statement
    /// (`matchable_folded` in `handle_query`), so text that only LOOKS like
    /// syntax because it sits inside a string VALUE is not syntax — while the
    /// value itself is still read, case-intact, out of the original statement.
    ///
    /// Before this, `WHERE tablename = 'x is null'` was classified as an
    /// `IS NULL` predicate on the "column" `tablename = 'x`, which `row_value`
    /// resolves to NULL for EVERY row — so the client got every table in the
    /// database, the exact widening the guard exists to prevent.
    #[test]
    fn hdb011_literal_text_is_not_treated_as_syntax() {
        // Exactly the pipeline `handle_query` builds: lower → strip → fold.
        // All three are byte-for-byte length-preserving, so the result
        // addresses the same characters as the original statement.
        fn classify(q: &str) -> String {
            PgCatalog::fold_ascii_whitespace(&PgCatalog::strip_literals_and_comments(&q.to_ascii_lowercase()))
        }
        // The two columns of `query_pg_tables` this test needs.
        fn pg_tables_rows(names: &[&str]) -> (Schema, Vec<Tuple>) {
            let schema = Schema::new(vec![
                Column::new("schemaname", DataType::Text),
                Column::new("tablename", DataType::Text),
            ]);
            let rows = names
                .iter()
                .map(|n| Tuple::new(vec![Value::String("public".into()), Value::String((*n).to_string())]))
                .collect();
            (schema, rows)
        }
        // `tablename` of every row the filter keeps. `q` is passed as the
        // ORIGINAL text, so literal VALUES are read from it.
        fn selected(q: &str, names: &[&str]) -> Vec<String> {
            let (schema, rows) = pg_tables_rows(names);
            PgCatalog::apply_where_filter(&classify(q), q, &schema, rows)
                .into_iter()
                .filter_map(|row| match row.values.get(1) {
                    Some(Value::String(name)) => Some(name.clone()),
                    _ => None,
                })
                .collect()
        }

        // 1. `' is null'` INSIDE a literal is a VALUE. The shape is a plain
        //    equality, and it must select the one row actually named
        //    `x is null` — not every row.
        const IS_NULL_LITERAL: &str = "SELECT tablename FROM pg_tables WHERE tablename = 'x is null'";
        assert!(
            PgCatalog::where_clause_is_fully_supported(&classify(IS_NULL_LITERAL)),
            "`= 'x is null'` is an equality predicate, not an IS NULL predicate"
        );
        assert_eq!(
            selected(IS_NULL_LITERAL, &["a", "x is null", "zzz"]),
            vec!["x is null".to_string()],
            "the literal must be COMPARED, not parsed — one row, never the whole catalog"
        );

        // 2. A literal containing `' and '`, written across two lines (what an
        //    ORM emits). The fold finds the clause; the strip stops ` and `
        //    from splitting the predicate into two bogus conjuncts; the value
        //    still comes from the original text.
        const AND_LITERAL: &str = "SELECT tablename FROM pg_tables\nWHERE tablename = 'orders and returns'";
        assert!(
            PgCatalog::where_clause_is_fully_supported(&classify(AND_LITERAL)),
            "` and ` inside a literal must not split the predicate"
        );
        assert_eq!(
            selected(AND_LITERAL, &["orders", "orders and returns", "returns"]),
            vec!["orders and returns".to_string()],
            "the whole literal is the value being compared"
        );

        // 3. `' or '` inside a literal is not a top-level OR either.
        const OR_LITERAL: &str = "SELECT tablename FROM pg_tables WHERE tablename = 'this or that'";
        assert!(
            PgCatalog::where_clause_is_fully_supported(&classify(OR_LITERAL)),
            "` or ` inside a literal must not read as a disjunction"
        );
        assert_eq!(
            selected(OR_LITERAL, &["a", "this or that"]),
            vec!["this or that".to_string()],
            "one row, not both"
        );

        // 4. A `(` inside a literal is not a parenthesis: the shape tests
        //    reject `(`, and before the strip this deferred a predicate the
        //    router evaluates perfectly well.
        const PAREN_LITERAL: &str = "SELECT tablename FROM pg_tables WHERE tablename = 'orders (eu)'";
        assert!(
            PgCatalog::where_clause_is_fully_supported(&classify(PAREN_LITERAL)),
            "`(` inside a literal is not a parenthesis"
        );
        assert_eq!(
            selected(PAREN_LITERAL, &["orders", "orders (eu)"]),
            vec!["orders (eu)".to_string()],
            "one row, not both"
        );

        // 5. Case survives the round trip: the classification copy is lowered,
        //    the VALUE is not.
        const UPPER_LITERAL: &str = "SELECT tablename FROM pg_tables WHERE tablename = 'Orders'";
        assert_eq!(
            selected(UPPER_LITERAL, &["orders", "Orders"]),
            vec!["Orders".to_string()],
            "the literal is compared case-sensitively, from the ORIGINAL text"
        );
    }

    /// HDB-011 review FIX-B: two shapes `eval_simple_pred` accepts but
    /// provably mis-evaluates (both to ZERO rows) are now refused, so the
    /// planner — which resolves aliases and subqueries — answers them.
    #[test]
    fn hdb011_shapes_the_evaluator_gets_wrong_are_refused() {
        fn classify(q: &str) -> String {
            PgCatalog::fold_ascii_whitespace(&PgCatalog::strip_literals_and_comments(&q.to_ascii_lowercase()))
        }

        for q in &[
            // `row_value` never strips an alias/schema prefix, so `t.tablename`
            // resolves to NULL and the conjunct matches no row at all — an
            // aliased existence check reported the table MISSING.
            "SELECT t.tablename FROM pg_tables t WHERE t.tablename = 'users'",
            "SELECT tablename FROM pg_tables WHERE pg_tables.tablename = 'users'",
            "SELECT tablename FROM pg_tables t WHERE t.tablename IN ('a','b')",
            "SELECT tablename FROM pg_tables t WHERE t.tablename IS NOT NULL",
            // `parse_in_list` would treat `select …` as a single literal.
            "SELECT tablename FROM pg_tables WHERE tablename IN (SELECT tablename FROM pg_tables)",
            "SELECT tablename FROM pg_tables WHERE tablename NOT IN (SELECT tablename FROM pg_tables)",
        ] {
            assert!(
                !PgCatalog::where_clause_is_fully_supported(&classify(q)),
                "`{q}` must be refused so the planner answers it"
            );
        }

        // A quoted (but UNqualified) column still works — `row_value` trims the
        // quotes — and so does a literal that merely CONTAINS a dot or the word
        // `select`, because both are blanked before classification.
        for q in &[
            "SELECT tablename FROM pg_tables WHERE \"tablename\" = 'users'",
            "SELECT tablename FROM pg_tables WHERE tablename = 'schema.users'",
            "SELECT tablename FROM pg_tables WHERE tablename IN ('select','a')",
        ] {
            assert!(
                PgCatalog::where_clause_is_fully_supported(&classify(q)),
                "`{q}` is evaluable here"
            );
        }
    }

    /// HDB-011 review FIX-A: a bound parameter's VALUE can no longer change
    /// the interceptor-vs-planner decision.
    ///
    /// The extended protocol classifies TWICE: at Parse, on text that still
    /// holds `$1` (that decision fixes the RowDescription Describe sends), and
    /// again at Execute, on the text `substitute_parameters` spliced the value
    /// into. When the two disagreed, Describe had announced the interceptor's
    /// 5-column `pg_tables` while Execute answered from the registry's
    /// 8-column one — DataRows with more fields than the RowDescription, which
    /// is a protocol violation (tokio-postgres rejects the row; node-postgres
    /// throws). Classifying on the literal-stripped copy makes the two
    /// decisions identical by construction: the parameter's value lives
    /// entirely inside a literal, and literal bodies are blanked.
    #[test]
    fn hdb011_a_parameter_value_cannot_change_the_route() {
        fn classify(q: &str) -> String {
            PgCatalog::fold_ascii_whitespace(&PgCatalog::strip_literals_and_comments(&q.to_ascii_lowercase()))
        }

        const PARSE_TIME: &str = "SELECT * FROM pg_tables WHERE tablename = $1";
        let at_parse = PgCatalog::where_clause_is_fully_supported(&classify(PARSE_TIME));

        // Every one of these is `substitute_parameters`' output for some value
        // of `$1`; each contains a fragment that USED to re-classify the
        // statement at Execute time.
        for executed in &[
            "SELECT * FROM pg_tables WHERE tablename = 'a'",
            "SELECT * FROM pg_tables WHERE tablename = 'orders and returns'",
            "SELECT * FROM pg_tables WHERE tablename = 'this or that'",
            "SELECT * FROM pg_tables WHERE tablename = 'x is null'",
            "SELECT * FROM pg_tables WHERE tablename = 'orders (eu)'",
            "SELECT * FROM pg_tables WHERE tablename = 'a; drop'",
            "SELECT * FROM pg_tables WHERE tablename = 'order by me'",
        ] {
            assert_eq!(
                PgCatalog::where_clause_is_fully_supported(&classify(executed)),
                at_parse,
                "`{executed}` must take the SAME route as the unsubstituted `{PARSE_TIME}` — \
                 otherwise Execute contradicts the RowDescription Describe already sent"
            );
        }

        // And the route the two agree on is the interceptor, whose answer is
        // correctly filtered — the whole point of keeping the shape supported.
        assert!(at_parse, "`tablename = $1` is a plain equality shape");
    }

    #[test]
    fn extract_relchecks_oid_parses_psql_d_query() {
        // KanttBan #7 (v3.30.1): the literal 15-column header that
        // psql `\d <name>` sends after resolving the relation OID.
        let q = "select c.relchecks, c.relkind, c.relhasindex, c.relhasrules, \
                 c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity, \
                 false as relhasoids, c.relispartition, '', c.reltablespace, \
                 case when c.reloftype = 0 then '' else \
                 c.reloftype::pg_catalog.regtype::pg_catalog.text end, \
                 c.relpersistence, c.relreplident, am.amname \
                 from pg_catalog.pg_class c \
                 left join pg_catalog.pg_class tc on (c.reltoastrelid = tc.oid) \
                 left join pg_catalog.pg_am am on (c.relam = am.oid) \
                 where c.oid = '16384';";
        assert_eq!(PgCatalog::extract_relchecks_oid(q), Some(16384));
    }
}
