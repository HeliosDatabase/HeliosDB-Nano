//! `SHOW` routing, `SHOW DATABASES`, `SHOW PROCESSLIST`, `KILL` and the
//! account statements of a MySQL connection.
//!
//! A `SHOW` this listener has no answer of its own for goes to the engine, so
//! the engine's statements (`SHOW BRANCHES`, `SHOW TOPOLOGY`, ...) are run and
//! an unknown one is an error, not an empty `OK`.

use super::*;
use crate::protocol::mysql::accounts::{self, AccountStatement};
use crate::protocol::mysql::kill::{self, KillTarget};
use crate::protocol::mysql::vars::{self, VarError};

/// What a `SHOW` statement asks for, from its first keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ShowKind {
    Databases,
    TableStatus,
    Tables,
    Index,
    Columns,
    CreateTable,
    Variables { global: bool, filter: String },
    Status { filter: String },
    Warnings,
    Errors,
    Collation,
    CharacterSet,
    Engines,
    Processlist,
    Plugins,
    Privileges,
    Grants,
    /// Replication introspection: an empty answer, as a server without it gives.
    Replication,
    /// Not one of this listener's: the engine runs it.
    Engine,
}

/// Classify a `SHOW` statement by its leading keywords (never by a word that
/// merely appears in it: `SHOW COLUMNS FROM my_tables` is about columns).
pub(super) fn classify_show(sql: &str) -> ShowKind {
    let view = super::super::translator::code_view(sql);
    let text = view.trim().trim_end_matches(';').trim();
    let mut rest = vars::strip_keyword(text, "SHOW").unwrap_or(text);
    let mut global = false;
    loop {
        if let Some(after) = vars::strip_keyword(rest, "GLOBAL") {
            global = true;
            rest = after;
        } else if let Some(after) = ["FULL", "EXTENDED", "SESSION", "LOCAL"]
            .iter()
            .find_map(|word| vars::strip_keyword(rest, word))
        {
            rest = after;
        } else {
            break;
        }
    }
    // The filter is taken from the statement itself, at the same offset.
    let tail_of = |after: &str| -> String {
        let offset = view.len() - view.trim_start().len() + (text.len() - after.len());
        sql.get(offset..).unwrap_or("").trim().trim_end_matches(';').to_string()
    };
    let mut words = rest.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next().unwrap_or("");
    match (first, second) {
        ("DATABASES" | "SCHEMAS", _) => ShowKind::Databases,
        ("TABLE", "STATUS") => ShowKind::TableStatus,
        ("TABLES", _) => ShowKind::Tables,
        ("INDEX" | "INDEXES" | "KEYS", _) => ShowKind::Index,
        ("COLUMNS" | "FIELDS", _) => ShowKind::Columns,
        ("CREATE", "TABLE") => ShowKind::CreateTable,
        ("VARIABLES", _) => ShowKind::Variables {
            global,
            filter: vars::strip_keyword(rest, "VARIABLES").map(tail_of).unwrap_or_default(),
        },
        ("STATUS", _) => ShowKind::Status {
            filter: vars::strip_keyword(rest, "STATUS").map(tail_of).unwrap_or_default(),
        },
        ("WARNINGS", _) => ShowKind::Warnings,
        ("ERRORS", _) => ShowKind::Errors,
        ("COLLATION", _) => ShowKind::Collation,
        ("CHARACTER", "SET") | ("CHARSET", _) => ShowKind::CharacterSet,
        ("ENGINES", _) => ShowKind::Engines,
        ("PROCESSLIST", _) => ShowKind::Processlist,
        ("PLUGINS", _) => ShowKind::Plugins,
        ("PRIVILEGES", _) => ShowKind::Privileges,
        ("GRANTS", _) => ShowKind::Grants,
        ("MASTER" | "REPLICA" | "SLAVE", "STATUS") | ("BINARY", "LOGS") => ShowKind::Replication,
        _ => ShowKind::Engine,
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> MySqlHandler<S> {
    /// `SHOW DATABASES [LIKE 'pattern']`: the databases that exist.
    pub(super) async fn show_databases(&mut self, sql: &str) -> Result<()> {
        let listed = self
            .database
            .query_with_columns_for_session(self.session_id, "SELECT datname FROM pg_database ORDER BY datname");
        let rows = match listed {
            Ok((rows, _)) => rows,
            Err(e) => return self.send_statement_error(&e.to_string()).await,
        };
        let pattern = extract_like_pattern(sql);
        let names: Vec<String> = rows
            .iter()
            .filter_map(|row| row.values.first())
            .map(value_to_mysql_string)
            .filter(|name| pattern.as_deref().is_none_or(|p| vars::like_match(name, p)))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.show_single_column("Database", &refs).await
    }

    /// `SHOW [FULL] PROCESSLIST`: the live sessions of this server, both
    /// wires, as `pg_stat_activity` lists them. `Id` is the number `KILL` takes.
    pub(super) async fn show_processlist(&mut self) -> Result<()> {
        let cols: Vec<String> = ["Id", "User", "Host", "db", "Command", "Time", "State", "Info"]
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        let listed = self
            .database
            .query_with_columns_for_session(self.session_id, "SELECT * FROM pg_stat_activity ORDER BY pid");
        let (activity, names) = match listed {
            Ok(result) => result,
            Err(e) => return self.send_statement_error(&e.to_string()).await,
        };
        let column = |name: &str| names.iter().position(|n| n.eq_ignore_ascii_case(name));
        let (pid_at, user_at, addr_at, port_at, db_at, state_at, query_at) = (
            column("pid"),
            column("usename"),
            column("client_addr"),
            column("client_port"),
            column("datname"),
            column("state"),
            column("query"),
        );
        let cell = |row: &Tuple, at: Option<usize>| -> Value {
            at.and_then(|i| row.values.get(i)).cloned().unwrap_or(Value::Null)
        };
        let own = i64::from(self.backend_pid);
        let mut rows = Vec::with_capacity(activity.len());
        for row in &activity {
            let pid = value_as_i64(&cell(row, pid_at));
            let mine = pid == own;
            let host = match (cell(row, addr_at), cell(row, port_at)) {
                (Value::Null, _) => Value::String("localhost".to_string()),
                (addr, Value::Null) => Value::String(value_to_mysql_string(&addr)),
                (addr, port) => Value::String(format!(
                    "{}:{}",
                    value_to_mysql_string(&addr),
                    value_to_mysql_string(&port)
                )),
            };
            let database = if mine {
                self.current_database.clone().map_or(cell(row, db_at), Value::String)
            } else {
                cell(row, db_at)
            };
            rows.push(Tuple::new(vec![
                Value::Int8(pid),
                cell(row, user_at),
                host,
                database,
                Value::String(if mine { "Query" } else { "" }.to_string()),
                Value::Int8(0),
                if mine {
                    Value::String("executing".to_string())
                } else {
                    cell(row, state_at)
                },
                if mine {
                    Value::String("SHOW PROCESSLIST".to_string())
                } else {
                    cell(row, query_at)
                },
            ]));
        }
        self.send_result_set(&cols, &rows).await
    }

    /// May this session end or interrupt the session of `owner`?
    fn may_kill(&self, owner: &str) -> bool {
        let me = self.username.clone().unwrap_or_default();
        if self.auth.is_none() || me == owner {
            // Without authentication every name is only asserted.
            return true;
        }
        // The server's own accounts are superusers, as on the other wire.
        if me == "postgres" || me == "helios" {
            return true;
        }
        self.database
            .storage
            .catalog()
            .get_role(&me)
            .ok()
            .flatten()
            .is_some_and(|role| role.rolsuper)
    }

    /// `KILL [CONNECTION | QUERY] id`.
    pub(super) async fn handle_kill(&mut self, target: KillTarget, id: i64) -> Result<()> {
        let engine = self.database.storage.instance_id();
        let unknown = |id: i64| VarError {
            code: 1094,
            state: "HY000",
            message: format!("Unknown thread id: {id}"),
        };
        let Some(state) = i32::try_from(id)
            .ok()
            .and_then(|pid| crate::session::scoped::live_backend(engine, pid))
        else {
            return self.send_var_error(&unknown(id)).await;
        };
        if !self.may_kill(&state.username()) {
            let denied = VarError {
                code: 1095,
                state: "HY000",
                message: format!("You are not owner of thread {id}"),
            };
            return self.send_var_error(&denied).await;
        }
        let pid = state.backend_pid();
        match target {
            KillTarget::Query => state.request_cancel_now(),
            KillTarget::Connection => {
                let Some(switch) = kill::find(pid, engine) else {
                    let refused = VarError::not_supported(
                        "KILL CONNECTION for a session that is not on the MySQL wire (KILL QUERY interrupts its statement)",
                    );
                    return self.send_var_error(&refused).await;
                };
                switch.kill();
                state.request_cancel_now();
                if pid == self.backend_pid {
                    // MySQL: killing your own connection ends it with this error.
                    self.send_error(1317, "70100", "Query execution was interrupted").await?;
                    return Err(MySqlError::ConnectionClosed);
                }
            }
        }
        self.send_ok(0, 0).await
    }

    /// Carry out an account statement on the server's roles.
    pub(super) fn run_account_statement(&mut self, statement: AccountStatement) -> std::result::Result<(), VarError> {
        let role_exists = |handler: &Self, name: &str| {
            handler
                .database
                .storage
                .catalog()
                .get_role(name)
                .ok()
                .flatten()
                .is_some()
        };
        let failed = |operation: &str, name: &str, error: &crate::Error| VarError {
            code: 1396,
            state: "HY000",
            message: format!("Operation {operation} failed for '{name}'@'%': {error}"),
        };
        match statement {
            AccountStatement::Create {
                if_not_exists,
                accounts,
            } => {
                for (name, password) in accounts {
                    if role_exists(self, &name) {
                        if if_not_exists {
                            continue;
                        }
                        return Err(VarError {
                            code: 1396,
                            state: "HY000",
                            message: format!("Operation CREATE USER failed for '{name}'@'%'"),
                        });
                    }
                    self.database
                        .execute_for_session(self.session_id, &accounts::create_role_sql(&name, password.as_deref()))
                        .map_err(|e| failed("CREATE USER", &name, &e))?;
                }
            }
            AccountStatement::SetPassword { if_exists, accounts } => {
                for (name, password) in accounts {
                    let name = match name {
                        Some(name) => name,
                        None => self.username.clone().unwrap_or_default(),
                    };
                    if !role_exists(self, &name) {
                        if if_exists {
                            continue;
                        }
                        return Err(VarError {
                            code: 1396,
                            state: "HY000",
                            message: format!(
                                "Operation ALTER USER failed for '{name}'@'%': there is no role of that name \
                                 (the server's start-up accounts keep the password they were started with)"
                            ),
                        });
                    }
                    self.database
                        .execute_for_session(self.session_id, &accounts::alter_role_password_sql(&name, &password))
                        .map_err(|e| failed("ALTER USER", &name, &e))?;
                }
            }
            AccountStatement::Drop { if_exists, accounts } => {
                for name in accounts {
                    if !role_exists(self, &name) {
                        if if_exists {
                            continue;
                        }
                        return Err(VarError {
                            code: 1396,
                            state: "HY000",
                            message: format!("Operation DROP USER failed for '{name}'@'%'"),
                        });
                    }
                    self.database
                        .execute_for_session(self.session_id, &accounts::drop_role_sql(&name))
                        .map_err(|e| failed("DROP USER", &name, &e))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn show_is_classified_by_its_leading_keywords() {
        assert_eq!(classify_show("SHOW DATABASES"), ShowKind::Databases);
        assert_eq!(classify_show("show full tables from heliosdb like 'wp_%'"), ShowKind::Tables);
        assert_eq!(classify_show("SHOW TABLE STATUS LIKE 'x'"), ShowKind::TableStatus);
        // A table whose name holds another SHOW keyword.
        assert_eq!(classify_show("SHOW COLUMNS FROM wp_woocommerce_api_keys"), ShowKind::Columns);
        assert_eq!(classify_show("SHOW FULL COLUMNS FROM `my_tables`"), ShowKind::Columns);
        assert_eq!(classify_show("SHOW INDEX FROM databases"), ShowKind::Index);
        assert_eq!(classify_show("SHOW CREATE TABLE variables"), ShowKind::CreateTable);
        assert_eq!(
            classify_show("SHOW GLOBAL VARIABLES LIKE 'Wait_Timeout'"),
            ShowKind::Variables {
                global: true,
                filter: "LIKE 'Wait_Timeout'".into()
            }
        );
        assert_eq!(
            classify_show("/* c */ SHOW SESSION VARIABLES;"),
            ShowKind::Variables {
                global: false,
                filter: String::new()
            }
        );
        assert_eq!(classify_show("SHOW STATUS"), ShowKind::Status { filter: String::new() });
        assert_eq!(
            classify_show("SHOW SESSION STATUS LIKE 'Ssl%'"),
            ShowKind::Status {
                filter: "LIKE 'Ssl%'".into()
            }
        );
        assert_eq!(classify_show("SHOW FULL PROCESSLIST"), ShowKind::Processlist);
        assert_eq!(classify_show("SHOW MASTER STATUS"), ShowKind::Replication);
        assert_eq!(classify_show("SHOW BRANCHES"), ShowKind::Engine);
        assert_eq!(classify_show("SHOW TOPOLOGY"), ShowKind::Engine);
        assert_eq!(classify_show("SHOW statement_timeout"), ShowKind::Engine);
    }
}
