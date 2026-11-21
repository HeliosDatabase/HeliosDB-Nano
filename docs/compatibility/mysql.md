# MySQL wire compatibility: logins, session state, statements

What a MySQL client gets from the `--mysql` and `--mysql-socket` listeners.
The SQL dialect itself (types, functions, DDL rewrites) is the translator's;
this page covers the connection: how a client logs in, which session
variables exist and what each one does, how transactions behave, and the
statements the listener answers itself.

## Logging in

| Server started with | The listener asks for | Plugin in the greeting |
|---|---|---|
| `--auth trust` (the default) | nothing: every login is accepted | `mysql_native_password` |
| `--auth password`, `md5` or `scram-sha-256` | the password of the account | `caching_sha2_password` |

The accounts are the same as on the PostgreSQL wire: the server's own
(`postgres`, `helios`, with the start-up password) and every role that may log
in (`CREATE ROLE … LOGIN PASSWORD '…'`; under `md5` only the server's own
accounts, as on the PostgreSQL wire).

| Transport | How the password travels | Client setting |
|---|---|---|
| TLS (`--mysql-tls-cert`, `--mysql-tls-key`) | as it is, inside the encrypted stream | `--ssl-mode=REQUIRED`, `sslMode=REQUIRED`, `ssl={…}` |
| Unix socket (`--mysql-socket`) | as it is, on the local socket | `--socket=…`, `unix_socket=…` |
| Plain TCP | encrypted with the server's RSA public key, which the client asks for | `--get-server-public-key`, `allowPublicKeyRetrieval=true` (PHP `mysqli`, PyMySQL with `cryptography`, `mysql2` and Go's driver do it by themselves) |

- After one full login the server accepts the plugin's scramble for that
  account from memory (the "fast" path) until the account's password changes
  or the account is dropped. Nothing MySQL-specific is written to disk.
- The RSA key pair (2048 bits) is generated when the server starts. To keep
  one key across restarts, for clients that pin it, pass
  `--mysql-rsa-private-key <file>` (PKCS#8 PEM, 2048 bits or more).
- A password sent in the clear on a connection without TLS is refused with
  error 1045 and is never checked.
- `mysql_native_password` is not offered. A client that starts with it (or
  with any other plugin) is switched to `caching_sha2_password`. A client
  that can only do `mysql_native_password` cannot log in to a server that asks
  for a password. `mysql_clear_password` is accepted on TLS and on the Unix
  socket.
- A wrong password and an unknown user get the same error 1045 after the same
  work. An empty password never logs in.
- `COM_CHANGE_USER` authenticates again and starts a new session; a refusal
  closes the connection. `COM_RESET_CONNECTION` resets the session.

### Account statements

`CREATE USER 'u'@'%' IDENTIFIED BY 'p'`, `ALTER USER 'u'@'%' IDENTIFIED BY 'p'`,
`SET PASSWORD [FOR 'u'@'%'] = 'p'` and `DROP USER 'u'@'%'` create, change and
drop roles: the account can log in on both wires. `IF [NOT] EXISTS` is
honoured. Accounts are not bound to a host: the host must be `%` or
`localhost` (or be left out). Another host, `IDENTIFIED … AS '<hash>'`,
`REQUIRE`, `PASSWORD EXPIRE`, `ACCOUNT LOCK` and resource options are refused
with error 1396 instead of being dropped. The server's own start-up accounts
keep the password the server was started with.

## Transactions

- `autocommit` is on for a new connection. With `SET autocommit = 0` the first
  statement that reads or writes a table opens a transaction, which stays open
  until `COMMIT` or `ROLLBACK`.
- DDL (`CREATE`, `ALTER`, `DROP`, `RENAME`, `TRUNCATE`, except on a temporary
  table), `BEGIN` / `START TRANSACTION` while a transaction is open, and
  `SET autocommit = 1` commit the open transaction first, as in MySQL.
- The status flags of every OK and end-of-rows packet say whether autocommit
  is on and whether a transaction is open.
- `START TRANSACTION [READ ONLY | READ WRITE] [, WITH CONSISTENT SNAPSHOT]`,
  `COMMIT`/`ROLLBACK [WORK] [AND [NO] CHAIN]`. `RELEASE` is refused (1235).
- `SET [SESSION] TRANSACTION ISOLATION LEVEL …`: `READ COMMITTED` (the
  default here; MySQL's default is `REPEATABLE READ`), `REPEATABLE READ`,
  `SERIALIZABLE` (served as snapshot isolation). `READ UNCOMMITTED` is served
  and reported as `READ COMMITTED`. Without `SESSION` it applies to the next
  transaction only, and is error 1568 inside one.
- `SET [SESSION] TRANSACTION READ ONLY`: writes are refused with error 1792.
- A failed statement does not end the transaction (MySQL's statement-level
  behaviour).

## Session variables

`SET name = value`, `SET SESSION name = …`, `SET @@session.name = …`,
several assignments in one `SET`, `SET NAMES`, `SET CHARACTER SET` and
`/*!40101 SET … */` are run. `SET GLOBAL` / `SET PERSIST` are refused (1235):
a server-wide value is configuration. An unknown variable is error 1193, a
value this server cannot honour is error 1231, a read-only variable is 1238.

`SELECT @@name`, `SHOW [SESSION | GLOBAL] VARIABLES [LIKE 'pattern' | WHERE
Variable_name = …]` read the same values; `GLOBAL` shows the defaults. The
engine's own parameters (`statement_timeout`, `lock_timeout`, `work_mem`, …)
are listed under their own names and can be set and read the same way.

Kinds: **effect** (the value changes behaviour), **one value** (this server
behaves as one value; setting the other is error 1231), **restricted** (the
values that describe what this server is are stored; any other is error 1231),
**stored** (kept and read back, no effect here), **read only**.

| Variable | Default | Kind | What it does here |
|---|---|---|---|
| `auto_increment_increment` | `1` | one value | only 1: generated keys step by one |
| `auto_increment_offset` | `1` | one value | only 1: generated keys start at one |
| `autocommit` | `ON` | effect | OFF: the first statement that reads or writes a table opens a transaction that stays open until COMMIT or ROLLBACK |
| `big_tables` | `OFF` | stored | no effect in Nano: stored and read back |
| `bulk_insert_buffer_size` | `8388608` | stored | no effect in Nano: stored and read back |
| `character_set_client` | `utf8mb4` | restricted | UTF-8 character sets only; statements are read as UTF-8 |
| `character_set_connection` | `utf8mb4` | restricted | UTF-8 character sets only |
| `character_set_database` | `utf8mb4` | restricted | UTF-8 character sets only |
| `character_set_results` | `utf8mb4` | restricted | UTF-8 character sets only (NULL and binary are accepted: results are sent as stored) |
| `character_set_server` | `utf8mb4` | read only | read only |
| `character_set_system` | `utf8mb3` | read only | read only |
| `collation_connection` | `utf8mb4_general_ci` | restricted | a UTF-8 collation is stored; comparisons keep this server's rules |
| `collation_database` | `utf8mb4_general_ci` | restricted | a UTF-8 collation is stored; comparisons keep this server's rules |
| `collation_server` | `utf8mb4_general_ci` | read only | read only |
| `default_storage_engine` | `InnoDB` | stored | no effect in Nano: stored and read back |
| `default_week_format` | `0` | one value | only 0 |
| `div_precision_increment` | `4` | one value | only 4 |
| `explicit_defaults_for_timestamp` | `ON` | one value | only ON |
| `foreign_key_checks` | `ON` | effect | OFF: this session's statements skip foreign-key checking |
| `group_concat_max_len` | `1024` | stored | no effect in Nano: GROUP_CONCAT results are never truncated |
| `have_openssl` | `NO` | read only | read only: YES when the listener has TLS configured |
| `have_ssl` | `NO` | read only | read only: YES when the listener has TLS configured |
| `hostname` | `localhost` | read only | read only |
| `identity` | `0` | read only | read only: the last generated key of this session |
| `information_schema_stats_expiry` | `86400` | stored | no effect in Nano: stored and read back |
| `init_connect` | (empty) | read only | read only |
| `innodb_strict_mode` | `ON` | stored | no effect in Nano: stored and read back |
| `interactive_timeout` | `28800` | stored | no effect in Nano: wait_timeout is the idle limit of every connection |
| `join_buffer_size` | `262144` | stored | no effect in Nano: stored and read back |
| `last_insert_id` | `0` | read only | read only: the last generated key of this session |
| `lc_time_names` | `en_US` | one value | only en_US |
| `license` | `Apache-2.0` | read only | read only |
| `long_query_time` | `10` | stored | no effect in Nano: stored and read back |
| `low_priority_updates` | `OFF` | stored | no effect in Nano: stored and read back |
| `lower_case_table_names` | `0` | read only | read only |
| `max_allowed_packet` | `67108864` | read only | read only |
| `max_execution_time` | `0` | effect | milliseconds; this session's statement timeout (every statement, not only SELECT) |
| `max_heap_table_size` | `16777216` | stored | no effect in Nano: stored and read back |
| `max_join_size` | `18446744073709551615` | stored | no effect in Nano: stored and read back |
| `net_buffer_length` | `16384` | read only | read only |
| `net_read_timeout` | `30` | stored | no effect in Nano: stored and read back |
| `net_write_timeout` | `60` | stored | no effect in Nano: stored and read back |
| `optimizer_search_depth` | `62` | stored | no effect in Nano: stored and read back |
| `optimizer_switch` | (empty) | stored | no effect in Nano: stored and read back |
| `performance_schema` | `OFF` | read only | read only |
| `profiling` | `OFF` | one value | only OFF |
| `protocol_version` | `10` | read only | read only |
| `pseudo_thread_id` | `0` | read only | read only: this connection's id |
| `query_cache_size` | `0` | read only | read only |
| `query_cache_type` | `OFF` | read only | read only |
| `read_buffer_size` | `131072` | stored | no effect in Nano: stored and read back |
| `session_track_schema` | `ON` | stored | no effect in Nano: stored and read back |
| `session_track_state_change` | `OFF` | stored | no effect in Nano: stored and read back |
| `session_track_system_variables` | (empty) | stored | no effect in Nano: stored and read back |
| `sort_buffer_size` | `262144` | stored | no effect in Nano: stored and read back |
| `sql_auto_is_null` | `OFF` | one value | only OFF: `col IS NULL` never finds the last inserted row |
| `sql_big_selects` | `ON` | stored | no effect in Nano: stored and read back |
| `sql_buffer_result` | `OFF` | stored | no effect in Nano: stored and read back |
| `sql_log_bin` | `OFF` | one value | only OFF: there is no binary log |
| `sql_mode` | `ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION` | restricted | this server is always strict; modes that change how a statement is read are refused |
| `sql_notes` | `ON` | stored | no effect in Nano: stored and read back |
| `sql_quote_show_create` | `ON` | stored | no effect in Nano: SHOW CREATE TABLE always quotes names |
| `sql_require_primary_key` | `OFF` | one value | only OFF |
| `sql_safe_updates` | `OFF` | effect | ON: an UPDATE or DELETE without WHERE and without LIMIT is refused (error 1175) |
| `sql_select_limit` | `18446744073709551615` | effect | a SELECT without its own LIMIT returns at most this many rows |
| `sql_warnings` | `OFF` | stored | no effect in Nano: stored and read back |
| `ssl_kx_group` | (empty) | read only | read only: the TLS key-exchange group of this connection |
| `system_time_zone` | `UTC` | read only | read only |
| `time_zone` | `SYSTEM` | restricted | UTC only (SYSTEM, UTC, +00:00): sessions run in UTC |
| `tmp_table_size` | `16777216` | stored | no effect in Nano: stored and read back |
| `transaction_isolation` | `READ-COMMITTED` | effect | the isolation level of this session's next transactions |
| `transaction_read_only` | `OFF` | effect | ON: this session's next transactions refuse writes (error 1792) |
| `unique_checks` | `ON` | stored | no effect in Nano: unique constraints are always checked |
| `version` | the server's | read only | read only |
| `version_comment` | `HeliosDB Nano` | read only | read only |
| `version_compile_os` | `Linux` | read only | read only |
| `wait_timeout` | `28800` | effect | seconds; this connection is closed after being idle that long |
| `innodb_lock_wait_timeout`, `lock_wait_timeout` | the server's `[session] lock_timeout_ms` | effect | seconds; how long a statement waits for a lock (the engine's `lock_timeout`) |

Old names are accepted: `tx_isolation`, `tx_read_only`, `storage_engine`.

`sql_mode`: the strict modes, `NO_AUTO_VALUE_ON_ZERO`, `NO_ENGINE_SUBSTITUTION`,
`PIPES_AS_CONCAT` and the modes that only concern options this server ignores
can be listed; the list is stored and read back. This server is always strict
(a value that does not fit is an error, never truncated), whatever the list
says. Modes that change how a statement is read or evaluated are refused:
`ANSI`, `ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES`, `HIGH_NOT_PRECEDENCE`,
`IGNORE_SPACE`, `REAL_AS_FLOAT`, `PAD_CHAR_TO_FULL_LENGTH`,
`NO_UNSIGNED_SUBTRACTION`, `ALLOW_INVALID_DATES`, `TIME_TRUNCATE_FRACTIONAL`
and the vendor modes.

### User variables

`SET @x = expr`, `SET @x := expr`, `SELECT @x`, `SELECT @x := expr` and `@x`
inside any statement. A variable never assigned is `NULL`. They belong to the
connection. Not supported, with error 1235: `SELECT … INTO @x`, and `:=`
inside a statement that reads a table.

### SHOW STATUS

`SHOW [SESSION | GLOBAL] STATUS [LIKE 'pattern']` reports `Aborted_connects`
(refused logins), `Questions` (statements of this connection), `Ssl_cipher`,
`Ssl_kx_group`, `Threads_connected`, `Threads_running` and `Uptime`.

## Several statements in one query

A client that asked for multiple statements when it connected
(`CLIENT_MULTI_STATEMENTS`: PDO by default, `mysqli::multi_query`, mysql2 and
Go's driver with `multiStatements`) may send `a; b; c`. Each statement is run
and answered by itself; the first error ends the batch. A client that did not
ask gets error 1064 for a second statement and nothing runs.

## SHOW, KILL

- `SHOW DATABASES [LIKE]` lists the databases that exist.
- `SHOW [FULL] PROCESSLIST` lists the live sessions of both wires; `Id` is the
  connection id (`CONNECTION_ID()`, the thread id of the handshake).
- `KILL QUERY id` interrupts the statement that session is running, when it is
  waiting for a lock (the statement ends with error 1317). `KILL [CONNECTION]
  id` also closes the connection, for sessions of the MySQL wire; for a
  session of the PostgreSQL wire it is refused with 1235. With authenticated
  logins a session may kill its own user's sessions; a superuser any.
- `SHOW BRANCHES`, `SHOW TOPOLOGY`, `SHOW <parameter>` and any other `SHOW`
  the listener has no answer of its own for are run by the engine. An unknown
  one is an error.
