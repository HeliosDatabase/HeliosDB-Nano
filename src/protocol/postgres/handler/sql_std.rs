//! The wire side of the statements PostgreSQL defines and the generic paths
//! do not answer with the right tag or row description: `MERGE INTO` and the
//! cursor statements (`DECLARE … CURSOR`, `FETCH`, `MOVE`, `CLOSE`).

use super::PgConnectionHandler;
use crate::protocol::postgres::messages::BackendMessage;
use crate::protocol::postgres::prepared::{Portal, PortalState, PreparedStatement};
use crate::{Result, Value};
use tokio::io::{AsyncRead, AsyncWrite};

impl<S: AsyncRead + AsyncWrite + Unpin> PgConnectionHandler<S> {
    /// Is this a statement this module answers on the simple protocol?
    pub(super) fn is_sql_std_statement(query: &str) -> bool {
        crate::merge_into::is_merge_into(query) || crate::cursor::is_cursor_statement(query)
    }

    /// Simple protocol: run the statement, send its rows (if it returns any)
    /// and its own command tag, then ReadyForQuery.
    pub(super) async fn handle_sql_std_simple(&mut self, query: &str) -> Result<()> {
        let db = std::sync::Arc::clone(&self.database);
        let sid = self.session_id;
        if crate::cursor::is_cursor_statement(query) {
            let outcome = super::run_guarded(|| db.try_cursor_statement_for_session(sid, query, &[]))?
                .ok_or_else(|| crate::Error::internal("a cursor statement was not answered"))?;
            if let Some((schema, rows)) = &outcome.rows {
                let plan = crate::protocol::postgres::codec::wire_plan(schema, &[]);
                let fields = crate::protocol::postgres::codec::field_descriptions(schema, &plan);
                self.send_message(BackendMessage::RowDescription { fields }).await?;
                self.send_data_rows_direct(rows, &plan).await?;
            }
            self.send_command_complete(&outcome.tag).await?;
            return self.send_ready_for_query().await;
        }
        let schema = super::run_guarded(|| db.merge_returning_schema(query))?;
        let (affected, rows) = super::run_guarded(|| db.execute_returning_for_session(sid, query))?;
        if let Some(schema) = schema {
            let plan = crate::protocol::postgres::codec::wire_plan(&schema, &[]);
            let fields = crate::protocol::postgres::codec::field_descriptions(&schema, &plan);
            self.send_message(BackendMessage::RowDescription { fields }).await?;
            self.send_data_rows_direct(&rows, &plan).await?;
        }
        self.send_command_complete(&format!("MERGE {affected}")).await?;
        self.send_ready_for_query().await
    }

    /// Extended protocol, Parse: the prepared statement for a statement this
    /// module answers, or `None` for any other. Parameters the client left
    /// untyped are described as `unknown`; the result shape is the
    /// statement's RETURNING list.
    pub(in crate::protocol::postgres) fn sql_std_prepared_statement(
        &self,
        name: &str,
        query: &str,
        param_types: &[i32],
    ) -> Result<Option<PreparedStatement>> {
        if !Self::is_sql_std_statement(query) {
            return Ok(None);
        }
        let unknown = crate::protocol::postgres::param_infer::UNKNOWN_OID;
        // What the statement itself says about its parameters (a cast, a
        // comparison with a column), where the SQL parser can read it: a
        // cursor's query, or the MERGE as written.
        let count = Self::count_parameters(query).max(param_types.len());
        let inferred: Vec<i32> = if count == 0 {
            Vec::new()
        } else {
            let text = crate::cursor::declared_query(query).unwrap_or_else(|| query.to_string());
            let dialect = sqlparser::dialect::PostgreSqlDialect {};
            match sqlparser::parser::Parser::parse_sql(&dialect, &text) {
                Ok(statements) => match statements.as_slice() {
                    [statement] => self.infer_param_types(statement, count),
                    _ => Vec::new(),
                },
                Err(_) => Vec::new(),
            }
        };
        let param_types: Vec<i32> = (0..count)
            .map(|i| match param_types.get(i).copied() {
                Some(oid) if oid != 0 => oid,
                _ => inferred.get(i).copied().unwrap_or(unknown),
            })
            .collect();
        let db = std::sync::Arc::clone(&self.database);
        let sid = self.session_id;
        let result_schema = if crate::cursor::is_cursor_statement(query) {
            // A FETCH is described with the columns of its cursor as it is
            // open now; Execute checks it is still that cursor.
            super::run_guarded(|| db.describe_cursor_statement_for_session(sid, query))?.flatten()
        } else {
            super::run_guarded(|| db.merge_returning_schema(query))?
        };
        Ok(Some(PreparedStatement {
            name: name.to_string(),
            query: query.to_string(),
            param_types,
            result_schema,
            cached_plan: None,
            cached_plan_epoch: 0,
            cached_plan_layout: crate::storage::LayoutStamp::empty(),
            is_catalog: Some(false),
            result_shape: crate::protocol::postgres::prepared::ResultShapeState {
                described: None,
                stamp: self.result_schema_stamp(),
                scope: self.session_scope(),
                // The shape is this statement's own; the generic re-derivation
                // has no grammar for it.
                synthesised: true,
            },
        }))
    }

    /// Extended protocol, Execute: `true` when the statement was one of this
    /// module's and has been answered.
    pub(in crate::protocol::postgres) async fn execute_sql_std_extended(
        &mut self,
        statement: &PreparedStatement,
        portal: &Portal,
        params: &[Value],
    ) -> Result<bool> {
        if !Self::is_sql_std_statement(&statement.query) {
            return Ok(false);
        }
        let db = std::sync::Arc::clone(&self.database);
        let sid = self.session_id;
        if crate::cursor::is_cursor_statement(&statement.query) {
            // The client was told this statement's columns at Parse. A FETCH
            // whose cursor did not exist then, or has other columns now,
            // cannot be answered in that shape.
            let now = super::run_guarded(|| db.describe_cursor_statement_for_session(sid, &statement.query))?.flatten();
            if let Some(now) = &now {
                let same = statement.result_schema.as_ref().is_some_and(|then| {
                    then.columns.len() == now.columns.len()
                        && then
                            .columns
                            .iter()
                            .zip(&now.columns)
                            .all(|(a, b)| a.name == b.name && a.data_type == b.data_type)
                });
                if !same {
                    return Err(crate::Error::sql_state(
                        "0A000",
                        "cached plan must not change result type: this FETCH was prepared before its cursor was \
                         declared, or for a cursor with other columns; prepare it again",
                    ));
                }
            }
            let outcome = super::run_guarded(|| db.try_cursor_statement_for_session(sid, &statement.query, params))?
                .ok_or_else(|| crate::Error::internal("a cursor statement was not answered"))?;
            self.prepared_statements
                .update_portal_state(&portal.name, PortalState::Complete)?;
            if let Some((schema, rows)) = &outcome.rows {
                let plan = crate::protocol::postgres::codec::wire_plan(schema, &portal.result_formats);
                self.send_data_rows_with_formats(rows, &plan).await?;
            }
            self.send_command_complete(&outcome.tag).await?;
            return Ok(true);
        }
        let (affected, rows) =
            super::run_guarded(|| db.execute_params_returning_for_session(sid, &statement.query, params))?;
        self.prepared_statements
            .update_portal_state(&portal.name, PortalState::Complete)?;
        if let Some(schema) = &statement.result_schema {
            let plan = crate::protocol::postgres::codec::wire_plan(schema, &portal.result_formats);
            self.send_data_rows_with_formats(&rows, &plan).await?;
        }
        self.send_command_complete(&format!("MERGE {affected}")).await?;
        Ok(true)
    }
}
