**MySQL clients log in with a password: `caching_sha2_password` against the same accounts and roles as PostgreSQL clients.**

The MySQL listener accepted every login, so the server refused to start it
(`--mysql`, `--mysql-socket`) beside a method that asks for a password.

Now it starts, and under `password`, `md5` or `scram-sha-256` a MySQL client
must present the password of the account it names: the server's own accounts,
and every role that may log in (`CREATE ROLE … LOGIN PASSWORD`). The plugin is
`caching_sha2_password`, the default of MySQL 8 clients. Over TLS and over the
Unix socket the client sends the password as it is; over plain TCP it asks for
the server's RSA public key and sends the password encrypted with it (the key
pair is generated at start, or loaded with `--mysql-rsa-private-key`). After
one such login the server accepts the plugin's scramble for that account from
memory, until its password changes or the account is dropped. A password in
the clear on a connection without TLS is refused and never checked.
`mysql_native_password` is not offered: a client that asks for it is switched
to `caching_sha2_password`. `mysql_clear_password` is accepted on TLS and the
Unix socket only. A wrong password and an unknown user get the same answer
(error 1045) after the same work, and an empty password never logs in.
`COM_CHANGE_USER` authenticates again and starts a new session.

`CREATE USER 'u'@'%' IDENTIFIED BY 'p'`, `ALTER USER … IDENTIFIED BY`,
`DROP USER` and `SET PASSWORD` act on the same roles, so an account made on
one wire logs in on both. The host must be `%` or `localhost`; any other host,
and any option that would be dropped (`REQUIRE`, `PASSWORD EXPIRE`, `ACCOUNT
LOCK`, a stored hash), is refused with error 1396.

Under `--auth trust` the listener asks for nothing, as before. The connection
id of the handshake is now the session's id, the number `SHOW PROCESSLIST`
lists and `KILL` takes.

Tests: `tests/mysql_auth_tests.rs`, `tests/daemon_http_auth_subprocess.rs`.
