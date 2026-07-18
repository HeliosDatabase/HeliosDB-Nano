//! Extended query protocol handler additions
//!
//! This module extends the PgConnectionHandler with full extended query protocol support.

use super::handler::PgConnectionHandler;
use super::messages::{BackendMessage, FieldDescription};
use super::prepared::{decode_parameter, substitute_parameters, Portal, PortalState, PreparedStatement};
use crate::{Error, Result, Value};
use tokio::io::{AsyncRead, AsyncWrite};

impl<S: AsyncRead + AsyncWrite + Unpin> PgConnectionHandler<S> {
    #[allow(clippy::similar_names)]
    /// Handle Parse message (extended protocol) with full implementation
    pub async fn handle_parse_extended(
        &mut self,
        statement_name: String,
        query: String,
        param_types: Vec<i32>,
    ) -> Result<()> {
        tracing::debug!("Parse statement '{}': {}", statement_name, query);

        // KanttBan #23 (v3.31.1 phase 2.2): rewrite PG's empty
        // `SELECT FROM …` projection shorthand before parsing.
        // See the matching note in `handle_single_query`.
        let query = super::handler::pg_rewrite_empty_projection(&query);

        // Parse and validate query syntax
        let parser = crate::sql::Parser::new();
        let statement = parser.parse_one(&query)?;

        // If param_types is empty, infer parameter count from query placeholders ($1, $2, etc)
        // Use OID 0 (unknown) which tells the client to use its preferred type for each parameter
        let inferred_param_types = if param_types.is_empty() {
            let param_count = Self::count_parameters(&query);
            if param_count > 0 {
                tracing::debug!("Inferred {} parameters, using unknown type (OID 0)", param_count);
                vec![0i32; param_count] // OID 0 = unknown/unspecified
            } else {
                vec![]
            }
        } else {
            param_types.clone()
        };

        // Derive result schema from query plan (gracefully handle failures).
        // If planning fails (e.g., table doesn't exist, unknown function), we
        // still create the prepared statement. For `SELECT` specifically,
        // fall back to a best-effort schema derived straight from the
        // projection list so Describe never sends `NoData` for a query that
        // actually returns rows — SQLAlchemy's psycopg dialect calls
        // `_row_as_tuple_getter` on that metadata and raises
        // NotImplementedError if it's missing.
        // Catalog-first derivation: if this is a SELECT against
        // pg_catalog / information_schema, get the schema from our
        // catalog emulator so Describe returns the right
        // RowDescription. Otherwise fall through to plan-based
        // derivation + the AST-based fallback.
        // R5.W2: this Parse-time probe doubles as the cached
        // is-catalog-query decision, so Execute no longer re-runs the
        // ~31-pattern catalog scan (nor `substitute_parameters`, which only
        // existed to feed it) on every Execute of an engine query. The
        // decision keys off table references (`pg_catalog.` /
        // `information_schema.` / `version()` …), which parameter
        // substitution cannot change — if anything, deciding on the
        // unsubstituted text is safer, since a parameter *value* containing
        // "pg_catalog." can no longer flip an engine query into the
        // catalog dispatcher.
        let (is_catalog, catalog_schema) = match self.catalog.handle_query(&query) {
            Ok(Some((schema, _rows))) => (Some(true), Some(schema)),
            Ok(None) => (Some(false), None),
            // Probe failed — leave the decision open; Execute keeps the
            // legacy per-Execute scan for this statement.
            Err(_) => (None, None),
        };
        // W2.3: for a plain row-returning query, take the Describe schema from
        // the SHARED parameterized plan cache — the very `Arc<LogicalPlan>` the
        // Execute path pins — and seed the prepared statement's `cached_plan`
        // with it. This replaces the throwaway private plan `derive_result_schema`
        // built here AND removes the redundant first-Execute plan build (the plan
        // is now built once, at Parse, and reused). Catalog queries keep the
        // catalog schema; anything the shared planner cannot pre-plan (DML — even
        // with RETURNING — DDL, unknown/catalog-only tables, or any planner bail)
        // falls through to the private `derive_result_schema` + AST-synthesis
        // path, which preserves the exact RETURNING RowDescription / NoData
        // behavior. OID parity is exact: both paths plan the SAME parsed statement
        // through `Planner::with_catalog` and read `LogicalPlan::schema()`, so
        // `datatype_to_oid` sees identical types (numeric → 1700, text → 25,
        // int2/4/8 → 21/23/20, …).
        let (result_schema, cached_plan, cached_plan_epoch) = if let Some(s) = catalog_schema {
            (Some(s), None, 0)
        } else if let Some((schema, plan, epoch)) = self.shared_query_schema(&statement, &query) {
            (Some(schema), Some(plan), epoch)
        } else {
            let schema = match self.derive_result_schema(&statement) {
                Ok(schema) => schema,
                Err(e) => {
                    tracing::debug!("Schema derivation failed (falling back to AST): {}", e);
                    Self::synthesise_schema_from_ast(&statement)
                }
            };
            (schema, None, 0)
        };

        tracing::debug!(
            "Derived schema for '{}': {} columns, {} parameters",
            statement_name,
            result_schema.as_ref().map_or(0, |s| s.columns.len()),
            inferred_param_types.len()
        );

        // Create prepared statement with cached plan for faster execution
        let prepared = PreparedStatement {
            name: statement_name.clone(),
            query: query.clone(),
            param_types: inferred_param_types,
            result_schema,
            // W2.3: seeded from the shared parameterized plan for SELECTs
            // (None/0 for catalog / DML / DDL / planner-bail fallback, which
            // populate `cached_plan` lazily at first Execute via R5.W2).
            cached_plan,
            cached_plan_epoch,
            is_catalog,
        };

        // Store in prepared statement manager
        self.prepared_statements.store_statement(prepared)?;

        tracing::debug!("Stored prepared statement '{}'", statement_name);

        // Send ParseComplete
        self.send_message(BackendMessage::ParseComplete).await?;
        Ok(())
    }

    /// Handle Bind message (extended protocol) with full implementation
    pub async fn handle_bind_extended(
        &mut self,
        portal_name: String,
        statement_name: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    ) -> Result<()> {
        tracing::debug!("Bind portal '{}' to statement '{}'", portal_name, statement_name);

        // Get prepared statement
        let statement = self
            .prepared_statements
            .get_statement(&statement_name)?
            .ok_or_else(|| Error::query_execution(format!("Prepared statement '{}' not found", statement_name)))?;

        // Validate parameter count
        if !params.is_empty() && !statement.param_types.is_empty() && params.len() != statement.param_types.len() {
            return Err(Error::query_execution(format!(
                "Parameter count mismatch: expected {}, got {}",
                statement.param_types.len(),
                params.len()
            )));
        }

        // Create portal
        let portal = Portal {
            name: portal_name.clone(),
            statement_name: statement_name.clone(),
            params,
            param_formats,
            result_formats,
            state: PortalState::Ready,
        };

        // Store portal
        self.prepared_statements.store_portal(portal)?;

        tracing::debug!("Created portal '{}' bound to '{}'", portal_name, statement_name);

        // Send BindComplete
        self.send_message(BackendMessage::BindComplete).await?;
        Ok(())
    }

    /// Handle Execute message (extended protocol) with full implementation
    // SAFETY: param_formats[i] and param_types[i] are guarded by i < len checks.
    #[allow(clippy::indexing_slicing)]
    pub async fn handle_execute_extended(&mut self, portal_name: String, max_rows: i32) -> Result<()> {
        tracing::debug!("Execute portal '{}' (max_rows: {})", portal_name, max_rows);

        // Get portal
        let portal = self
            .prepared_statements
            .get_portal(&portal_name)?
            .ok_or_else(|| Error::query_execution(format!("Portal '{}' not found", portal_name)))?;

        // Check portal state
        if portal.state == PortalState::Complete {
            return Err(Error::query_execution(format!(
                "Portal '{}' already complete",
                portal_name
            )));
        }

        // Get statement
        let statement = self
            .prepared_statements
            .get_statement(&portal.statement_name)?
            .ok_or_else(|| Error::query_execution(format!("Statement '{}' not found", portal.statement_name)))?;

        // Transaction-control statements need the connection-level state
        // machine from the simple-query path. Letting extended Execute send
        // BEGIN/COMMIT/ROLLBACK straight to the engine leaves the handler's
        // TransactionStatus out of sync with the embedded transaction.
        let trimmed_query = statement.query.trim();
        let is_transaction_control = trimmed_query.eq_ignore_ascii_case("BEGIN")
            || super::handler::starts_with_icase(trimmed_query, "BEGIN ")
            || trimmed_query.eq_ignore_ascii_case("START TRANSACTION")
            || super::handler::starts_with_icase(trimmed_query, "START TRANSACTION ")
            || trimmed_query.eq_ignore_ascii_case("COMMIT")
            || trimmed_query.eq_ignore_ascii_case("ROLLBACK");
        if is_transaction_control {
            let previous_suppress_ready = self.suppress_ready_for_query;
            self.suppress_ready_for_query = true;
            let result = self.handle_single_query(&statement.query).await;
            self.suppress_ready_for_query = previous_suppress_ready;
            result?;
            self.prepared_statements
                .update_portal_state(&portal_name, PortalState::Complete)?;
            return Ok(());
        }

        if self.transaction_failed() {
            return self.send_extended_failed_transaction_error().await;
        }

        // Convert parameters from wire format to Value
        let mut param_values = Vec::new();
        for (i, param_data) in portal.params.iter().enumerate() {
            if let Some(data) = param_data {
                let format = if i < portal.param_formats.len() {
                    portal.param_formats[i]
                } else {
                    0 // Default to text format
                };

                let type_oid = if i < statement.param_types.len() {
                    statement.param_types[i]
                } else {
                    25 // Default to TEXT
                };

                let value = decode_parameter(data, format, type_oid)?;
                param_values.push(value);
            } else {
                param_values.push(Value::Null);
            }
        }

        // Quirks B and E from the dashboard cutover: the previous
        // implementation textually substituted parameter values into
        // the SQL before running it, which (a) re-fed values through
        // sqlparser — strings with control chars or huge length blew
        // up parsing — and (b) made parameterised CTEs subtly fragile
        // because some shapes of `substitute_parameters` output didn't
        // round-trip through the planner identically to the
        // `query_params`-threaded form. Now: keep the substituted
        // form ONLY for the catalog dispatcher (which is regex-based
        // and predates parameters), and route everything else through
        // `query_params` / `execute_params_returning` / `execute_params`.
        // R5.W2: the is-catalog decision was made once at Parse. For engine
        // queries (`Some(false)`) both the substitution and the catalog scan
        // are skipped on every Execute; for catalog queries (and undecided
        // legacy statements) the substituted text is still rebuilt per
        // Execute because the catalog handlers extract WHERE filters from
        // the query text, which depends on the bound values.
        let maybe_catalog = statement.is_catalog != Some(false);

        tracing::debug!("Executing query: {} (params: {})", statement.query, param_values.len());

        // Execute query. A CTE (`WITH … SELECT …`) is row-returning but does
        // not start with SELECT; route it through the params-aware query path
        // too, otherwise it falls to execute_params and silently returns a
        // command tag with zero rows over the wire (Token Dashboard #3).
        let is_row_returning_query = {
            let t = statement.query.trim();
            (t.len() >= 6 && t.as_bytes()[..6].eq_ignore_ascii_case(b"SELECT"))
                || super::handler::starts_with_cte(t)
                || crate::sql::Parser::is_show_branches(t)
        };

        if is_row_returning_query {
            // Catalog fast path — `pg_catalog.pg_type` and friends must
            // resolve on the extended query protocol too, not just on
            // simple-Q. `postgres-js`, `pg`, `psycopg` all do their
            // connect-time type introspection through Parse / Bind /
            // Execute; without this route, every driver gets a
            // spurious `Table 'pg_catalog.pg_type' does not exist`.
            // Catalog is regex-driven and doesn't speak parameters, so
            // it sees the substituted form. Skipped entirely when Parse
            // already decided this is an engine query (R5.W2).
            if maybe_catalog {
                let substituted_for_catalog = if param_values.is_empty() {
                    statement.query.clone()
                } else {
                    substitute_parameters(&statement.query, &param_values)?
                };
                if let Some(catalog_result) = self.catalog.handle_query(&substituted_for_catalog)? {
                    // Mark the portal complete and emit DataRows + CommandComplete
                    // directly against the catalog-emulated result.
                    self.prepared_statements
                        .update_portal_state(&portal_name, PortalState::Complete)?;
                    self.send_data_rows_with_formats(&catalog_result.1, &portal.result_formats)
                        .await?;
                    let tag = format!("SELECT {}", catalog_result.1.len());
                    self.send_command_complete(&tag).await?;
                    return Ok(());
                }
            }

            // SELECT query — pass parameters to the planner via
            // `query_params` so values stay value-shaped instead of
            // being spliced back into SQL. The pinned plan (populated at
            // first Execute, revalidated against the plan-cache epoch)
            // skips the per-Execute plan-cache lookup (R5.W2).
            // Guard the synchronous planner/executor call so a panic (e.g. an
            // arithmetic-overflow panic under overflow-checks) becomes a
            // recoverable XX000 error instead of dropping the connection.
            let results = super::handler::run_guarded(|| match self.pinned_plan_for(&statement) {
                Some(plan) => self.database.query_params_for_session_with_plan(
                    self.session_id,
                    &statement.query,
                    &param_values,
                    &plan,
                ),
                None => self
                    .database
                    .query_params_for_session(self.session_id, &statement.query, &param_values),
            })?;

            // Handle max_rows limit
            let results_to_send = if max_rows > 0 {
                let max = max_rows as usize;
                if results.len() > max {
                    // Portal suspended - cache remaining results
                    let (to_send, remaining) = results.split_at(max);
                    self.prepared_statements.update_portal_state(
                        &portal_name,
                        PortalState::Suspended {
                            rows_returned: max,
                            cached_results: Some(remaining.to_vec()),
                        },
                    )?;
                    to_send.to_vec()
                } else {
                    self.prepared_statements
                        .update_portal_state(&portal_name, PortalState::Complete)?;
                    results
                }
            } else {
                self.prepared_statements
                    .update_portal_state(&portal_name, PortalState::Complete)?;
                results
            };

            // In extended protocol, RowDescription was already sent during Describe.
            // Execute only sends DataRows and CommandComplete.

            // Send DataRows — direct encoder for text formats (R5.W1),
            // legacy per-row conversion when binary formats are requested.
            self.send_data_rows_with_formats(&results_to_send, &portal.result_formats)
                .await?;

            // Send CommandComplete
            let tag = format!("SELECT {}", results_to_send.len());
            self.send_command_complete(&tag).await?;
        } else {
            // INSERT / UPDATE / DELETE with RETURNING must route through
            // the params-aware path so the executor evaluates `$N`
            // against `param_values` rather than relying on textual
            // substitution. Detection mirrors `handle_query`'s
            // `is_dml_returning` check, but uses the original (not
            // substituted) query.
            let upper = statement.query.to_uppercase();
            let trimmed = statement.query.trim();
            let is_insert = super::handler::starts_with_icase(trimmed, "INSERT");
            let is_update = super::handler::starts_with_icase(trimmed, "UPDATE");
            let is_delete = super::handler::starts_with_icase(trimmed, "DELETE");
            let is_dml_returning = upper.contains("RETURNING") && (is_insert || is_update || is_delete);
            if is_dml_returning {
                // Guard the synchronous DML-RETURNING execution too (the third
                // execution entry on the extended path, alongside the SELECT
                // and non-SELECT param arms below): a panic in the
                // planner/evaluator becomes a recoverable XX000 error instead
                // of unwinding the connection task and dropping the client.
                let (affected, tuples) = super::handler::run_guarded(|| {
                    self.database
                        .execute_params_returning(&statement.query, &param_values)
                })?;
                self.prepared_statements
                    .update_portal_state(&portal_name, PortalState::Complete)?;
                // RowDescription was already sent during Describe; Execute
                // only emits DataRows + CommandComplete.
                self.send_data_rows_with_formats(&tuples, &portal.result_formats)
                    .await?;
                let tag = if is_insert {
                    format!("INSERT 0 {}", affected)
                } else if is_update {
                    format!("UPDATE {}", affected)
                } else {
                    format!("DELETE {}", affected)
                };
                self.send_command_complete(&tag).await?;
                return Ok(());
            }

            // Non-SELECT query — params-aware execution.
            // Guard the synchronous planner/executor call (see the SELECT arm
            // above): convert a panic into a recoverable XX000 error. W3.3:
            // wrap the autocommit DML in the same same-row write-conflict retry
            // as the simple-query path. The pinned plan is resolved ONCE up
            // front (unchanged across retries), and `op` captures only
            // `&self.database` (not `&self`) so the future stays Send.
            let policy = self.database.statement_retry_policy();
            let retriable = policy.is_enabled() && !self.database.session_in_transaction(self.session_id);
            // Resolve the pinned plan once, still panic-guarded (it is stable
            // across retries, so it stays out of the retried `op`).
            let pinned = super::handler::run_guarded(|| Ok::<_, Error>(self.pinned_plan_for(&statement)))?;
            let db = &self.database;
            let sid = self.session_id;
            let query = &statement.query;
            let params = &param_values;
            let affected = super::handler::run_with_retry(policy, retriable, || match &pinned {
                Some(plan) => db.execute_params_for_session_with_plan(sid, query, params, plan),
                None => db.execute_params_for_session(sid, query, params),
            })
            .await?;
            let tag = self.get_command_tag(&statement.query, affected);
            self.send_command_complete(&tag).await?;

            // Mark portal complete
            self.prepared_statements
                .update_portal_state(&portal_name, PortalState::Complete)?;
        }

        Ok(())
    }

    /// Plan pinning for prepared statements (R5.W2): reuse the statement's
    /// cached `Arc<LogicalPlan>` while the engine's plan-cache epoch is
    /// unchanged; on first Execute (or after DDL cleared the plan cache)
    /// fetch the plan once and store it back on the statement. The plan is
    /// the unmodified `parameterized_plan_cached` output — no extra
    /// optimizer passes (W3 is out of scope; DML must keep the 3.37.3
    /// index-probe behavior). Returns `None` when planning fails so the
    /// caller falls back to the legacy path, which either surfaces the real
    /// error or handles shapes that don't pre-plan (e.g. code-graph
    /// rewrites, SHOW BRANCHES).
    fn pinned_plan_for(&self, statement: &PreparedStatement) -> Option<std::sync::Arc<crate::sql::LogicalPlan>> {
        // A pinned plan is built at Parse time against the SHARED (public) plan
        // cache with no per-session `search_path` installed, so under a
        // non-`public` session it would resolve bare names to the wrong schema.
        // Fall back (`None`) to `query_params_for_session` / `execute_params_…`,
        // which re-plan with THIS session's schema installed.
        if self.database.session_schema_active(self.session_id) {
            return None;
        }
        let current_epoch = self.database.wire_plan_epoch();
        if let Some(plan) = &statement.cached_plan {
            if statement.cached_plan_epoch == current_epoch {
                return Some(std::sync::Arc::clone(plan));
            }
        }
        let (plan, epoch) = self.database.wire_parameterized_plan(&statement.query).ok()?;
        let _ = self
            .prepared_statements
            .set_cached_plan(&statement.name, std::sync::Arc::clone(&plan), epoch);
        Some(plan)
    }

    /// W2.3: derive the Describe result schema for a plain row-returning query
    /// (SELECT / CTE / set-op) from the SHARED parameterized plan cache,
    /// returning the schema together with the pinned `Arc<LogicalPlan>` and the
    /// plan-cache epoch so Parse can seed `cached_plan`. The Execute path then
    /// reuses that pin via `pinned_plan_for`, so the plan is built once (at
    /// Parse) instead of twice (a throwaway private plan at Parse + a fresh one
    /// at first Execute).
    ///
    /// Returns `None` — deferring to the private `derive_result_schema` path —
    /// for:
    ///   * non-`Query` statements: INSERT / UPDATE / DELETE produce an EMPTY
    ///     `LogicalPlan::schema()` even with a RETURNING clause, so the shared
    ///     path cannot distinguish "returns the RETURNING columns" from "returns
    ///     nothing"; the private path computes the RETURNING schema explicitly
    ///     (and correctly emits `NoData` for plain DML / DDL).
    ///   * planner bail-outs (unknown table, catalog-only relation, unsupported
    ///     shape): `wire_parameterized_plan` returns `Err` → fall back.
    ///   * an empty result schema (defensive; a plain `Query` should not hit it).
    ///
    /// OID parity with the pre-W2.3 path is exact: both build the plan with
    /// `Planner::with_catalog(&catalog)` from the SAME parsed statement and read
    /// `LogicalPlan::schema()`. The only extra input on the shared path,
    /// `with_sql`, feeds AS-OF / column-storage parsing only — never result
    /// column types — so `datatype_to_oid` sees identical `DataType`s.
    fn shared_query_schema(
        &self,
        statement: &sqlparser::ast::Statement,
        query: &str,
    ) -> Option<(crate::Schema, std::sync::Arc<crate::sql::LogicalPlan>, u64)> {
        if !matches!(statement, sqlparser::ast::Statement::Query(_)) {
            return None;
        }
        // Under a non-`public` session, the shared (public) plan would derive
        // the wrong table's columns; defer to the private schema path.
        if self.database.session_schema_active(self.session_id) {
            return None;
        }
        let (plan, epoch) = self.database.wire_parameterized_plan(query).ok()?;
        let schema = (*plan.schema()).clone();
        if schema.columns.is_empty() {
            return None;
        }
        Some((schema, plan, epoch))
    }

    /// Handle Describe message (extended protocol) with full implementation
    pub async fn handle_describe_extended(
        &mut self,
        target: super::messages::DescribeTarget,
        name: String,
    ) -> Result<()> {
        tracing::debug!("Describe {:?} '{}'", target, name);

        use super::messages::DescribeTarget;

        match target {
            DescribeTarget::Statement => {
                // Describe a prepared statement
                let statement = self
                    .prepared_statements
                    .get_statement(&name)?
                    .ok_or_else(|| Error::query_execution(format!("Statement '{}' not found", name)))?;

                // Send ParameterDescription
                if !statement.param_types.is_empty() {
                    self.send_message(BackendMessage::ParameterDescription {
                        param_types: statement.param_types.clone(),
                    })
                    .await?;
                } else {
                    self.send_message(BackendMessage::ParameterDescription { param_types: vec![] })
                        .await?;
                }

                // Send RowDescription or NoData based on derived schema
                if let Some(schema) = &statement.result_schema {
                    let fields: Vec<FieldDescription> = schema
                        .columns
                        .iter()
                        .map(|col| FieldDescription {
                            name: col.name.clone(),
                            table_oid: 0,
                            column_attr_num: 0,
                            data_type_oid: super::handler::datatype_to_oid(&col.data_type),
                            data_type_size: super::handler::datatype_to_size(&col.data_type),
                            type_modifier: -1,
                            format_code: 0,
                        })
                        .collect();
                    self.send_message(BackendMessage::RowDescription { fields }).await?;
                } else {
                    // Statement doesn't return results (INSERT, UPDATE, DELETE, DDL)
                    self.send_message(BackendMessage::NoData).await?;
                }
            }
            DescribeTarget::Portal => {
                // Describe a portal
                let portal = self
                    .prepared_statements
                    .get_portal(&name)?
                    .ok_or_else(|| Error::query_execution(format!("Portal '{}' not found", name)))?;

                let statement = self
                    .prepared_statements
                    .get_statement(&portal.statement_name)?
                    .ok_or_else(|| {
                        Error::query_execution(format!("Statement '{}' not found", portal.statement_name))
                    })?;

                // Send RowDescription or NoData
                if let Some(schema) = &statement.result_schema {
                    let fields =
                        super::handler::schema_to_field_descriptions_with_formats(schema, &portal.result_formats);
                    self.send_message(BackendMessage::RowDescription { fields }).await?;
                } else {
                    self.send_message(BackendMessage::NoData).await?;
                }
            }
        }

        Ok(())
    }

    /// Handle Close message (close statement or portal)
    pub async fn handle_close(&mut self, target: super::messages::DescribeTarget, name: String) -> Result<()> {
        tracing::debug!("Close {:?} '{}'", target, name);

        use super::messages::DescribeTarget;

        match target {
            DescribeTarget::Statement => {
                self.prepared_statements.remove_statement(&name)?;
            }
            DescribeTarget::Portal => {
                self.prepared_statements.remove_portal(&name)?;
            }
        }

        self.send_message(BackendMessage::CloseComplete).await?;
        Ok(())
    }

    /// Derive result schema from SQL statement for prepared statements
    ///
    /// This method analyzes the SQL statement and extracts the result schema
    /// that will be returned when the statement is executed. It uses the query
    /// planner to build a logical plan and extract schema information.
    ///
    /// # Returns
    ///
    /// - `Some(Schema)` for queries that return results (SELECT, RETURNING)
    /// - `None` for queries that don't return results (INSERT, UPDATE, DELETE, DDL)
    fn derive_result_schema(&self, statement: &sqlparser::ast::Statement) -> Result<Option<crate::Schema>> {
        use sqlparser::ast::Statement;

        // Resolve bare names against THIS session's `search_path` so Describe
        // reports the schema-scoped relation's columns (and doesn't fail on a
        // relation that exists only in the session's schema, not `public`).
        let current_schema = self.database.session_current_schema(self.session_id).unwrap_or(None);

        // Only derive schema for queries that return results
        match statement {
            Statement::Query(_) => {
                // SELECT and other queries - derive schema from logical plan
                let catalog = self.database.storage.catalog();
                let planner = crate::sql::planner::Planner::with_catalog(&catalog)
                    .with_current_schema(current_schema.clone());

                // Convert statement to logical plan
                let logical_plan = planner.statement_to_plan(statement.clone())?;

                // Extract schema from the logical plan
                let schema_arc = logical_plan.schema();
                let schema = (*schema_arc).clone();

                // Only return schema if it has columns
                if schema.columns.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(schema))
                }
            }
            // INSERT, UPDATE, DELETE - check for RETURNING clause
            Statement::Insert(ref insert) => {
                if insert.returning.is_some() {
                    // Has RETURNING clause - derive schema
                    let catalog = self.database.storage.catalog();
                    let planner = crate::sql::planner::Planner::with_catalog(&catalog)
                        .with_current_schema(current_schema.clone());
                    let plan = planner.statement_to_plan(statement.clone())?;
                    if let crate::sql::LogicalPlan::Insert {
                        table_name,
                        returning: Some(ref items),
                        ..
                    }
                    | crate::sql::LogicalPlan::InsertSelect {
                        table_name,
                        returning: Some(ref items),
                        ..
                    } = plan
                    {
                        let table_schema = catalog.get_table_schema(&table_name)?;
                        Ok(Some(crate::EmbeddedDatabase::returning_schema(&table_schema, items)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            Statement::Update { ref returning, .. } => {
                if returning.is_some() {
                    let catalog = self.database.storage.catalog();
                    let planner = crate::sql::planner::Planner::with_catalog(&catalog)
                        .with_current_schema(current_schema.clone());
                    let plan = planner.statement_to_plan(statement.clone())?;
                    if let crate::sql::LogicalPlan::Update {
                        table_name,
                        returning: Some(ref items),
                        ..
                    } = plan
                    {
                        let table_schema = catalog.get_table_schema(&table_name)?;
                        Ok(Some(crate::EmbeddedDatabase::returning_schema(&table_schema, items)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            Statement::Delete(ref del) => {
                if del.returning.is_some() {
                    let catalog = self.database.storage.catalog();
                    let planner = crate::sql::planner::Planner::with_catalog(&catalog)
                        .with_current_schema(current_schema.clone());
                    let plan = planner.statement_to_plan(statement.clone())?;
                    if let crate::sql::LogicalPlan::Delete {
                        table_name,
                        returning: Some(ref items),
                        ..
                    } = plan
                    {
                        let table_schema = catalog.get_table_schema(&table_name)?;
                        Ok(Some(crate::EmbeddedDatabase::returning_schema(&table_schema, items)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            // DDL statements (CREATE, DROP, ALTER) don't return results
            Statement::CreateTable(_)
            | Statement::Drop { .. }
            | Statement::CreateIndex(_)
            | Statement::AlterTable { .. }
            | Statement::Truncate { .. } => Ok(None),
            // For any other statement types, assume no result schema
            _ => Ok(None),
        }
    }

    /// Best-effort schema extraction straight from the `Statement::Query`
    /// projection list, used as a fallback when full planner-based schema
    /// derivation fails. Returns `None` for non-queries or when no
    /// projection names can be extracted.
    ///
    /// All columns are typed as `Text` because we don't know the real
    /// types without planning; psycopg accepts that and will coerce on
    /// the client side. The goal is purely to keep `Describe` from
    /// degrading to `NoData` for a SELECT, which breaks SQLAlchemy's
    /// `_row_as_tuple_getter`.
    fn synthesise_schema_from_ast(statement: &sqlparser::ast::Statement) -> Option<crate::Schema> {
        use sqlparser::ast::{SelectItem, SetExpr, Statement};

        let query = if let Statement::Query(q) = statement {
            q
        } else {
            return None;
        };
        let select = if let SetExpr::Select(s) = &*query.body {
            s
        } else {
            return None;
        };

        let columns: Vec<crate::Column> = select
            .projection
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let name = match item {
                    SelectItem::UnnamedExpr(expr) => {
                        Self::expr_column_label(expr).unwrap_or_else(|| format!("column{}", i + 1))
                    }
                    SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                    SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => format!("column{}", i + 1),
                };
                crate::Column::new(name, crate::DataType::Text)
            })
            .collect();

        if columns.is_empty() {
            None
        } else {
            Some(crate::Schema::new(columns))
        }
    }

    /// Pick a reasonable label for an unaliased SELECT expression — column
    /// name for `Expr::Identifier`, rightmost part of a compound
    /// identifier, function name for a call, or None otherwise.
    fn expr_column_label(expr: &sqlparser::ast::Expr) -> Option<String> {
        use sqlparser::ast::Expr;
        match expr {
            Expr::Identifier(ident) => Some(ident.value.clone()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|p| p.value.clone()),
            Expr::Function(f) => f.name.to_string().split('.').last().map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Count the number of $N placeholders in a SQL query
    /// Returns the highest parameter number found (e.g., "$7" means 7 parameters)
    fn count_parameters(query: &str) -> usize {
        let mut max_param = 0;
        let mut chars = query.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '$' {
                // Collect digits following the $
                let mut num_str = String::new();
                while let Some(&digit) = chars.peek() {
                    if digit.is_ascii_digit() {
                        num_str.push(digit);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !num_str.is_empty() {
                    if let Ok(num) = num_str.parse::<usize>() {
                        max_param = max_param.max(num);
                    }
                }
            }
        }

        max_param
    }
}
