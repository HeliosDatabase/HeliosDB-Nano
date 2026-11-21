**Role membership, `SET ROLE` and `ALTER ROLE … RENAME TO` work.**

`GRANT role TO role` and `ALTER ROLE … RENAME TO` were refused. `SET ROLE` and
`SET SESSION AUTHORIZATION` were refused on the PostgreSQL wire (or, with
`legacy_acl_noop`, acknowledged without doing anything). Role attributes were
recorded and decided nothing.

Now `GRANT role TO member [WITH ADMIN OPTION]` and `REVOKE [ADMIN OPTION FOR]
role FROM member` record membership; a member holds what was granted to the
role, through every level, unless it is declared `NOINHERIT`. A grant that would
make a role a member of itself is refused. `SET ROLE` switches the session to a
role it is a member of (a superuser to any role); `RESET ROLE` and `SET ROLE
NONE` return; `SET SESSION AUTHORIZATION` is for superusers. After a switch
`current_user` answers the new role, `session_user` the login, and statements
are checked as the new role. `SET ROLE` also works on the embedded API. A switch
the session may not make is SQLSTATE `42501`; `SET LOCAL ROLE` is refused with
`0A000`. `legacy_acl_noop` no longer affects `SET ROLE`.

`SUPERUSER` bypasses the checks. `CREATE ROLE`, `ALTER ROLE` and `DROP ROLE`
need `CREATEROLE`; only a superuser gives `SUPERUSER`, `REPLICATION` or
`BYPASSRLS`, or alters or drops a superuser; a role may change its own password.
`CREATE DATABASE` needs `CREATEDB`. `DROP ROLE` is refused while the role owns
objects or holds privileges, and removes its memberships. `ALTER ROLE … RENAME
TO` keeps the role's grants, ownerships and memberships.

Tests: `tests/privilege_roles.rs`.
