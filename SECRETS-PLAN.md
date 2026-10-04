# P0-2 bounded implementation plan (2026-10-04)

Base: `123b42fa44021c5348ea8f507c6b3af1bee4ceb0`.

Source inspection shows `start` already accepts `--password-file` and `HELIOSDB_PASSWORD`, but daemon re-exec in `src/main.rs:start_server_daemon` resolves the secret and appends it as child `--password`. The entrypoint has a compatibility fallback that still puts secrets on argv for old binaries, and compose currently uses `HELIOSDB_PASSWORD` directly. The scoped implementation will:

1. Preserve password-file/env precedence and audit every command/config secret input reachable from the CLI; add bounded file/env support only where existing code has a password secret source and tests/docs can verify it.
2. Change daemon transfer so the child receives a secret through an inherited private descriptor or file-backed handoff with cleanup and a bounded lifetime, never child argv; ensure the parent does not log the value and preserve daemon startup behavior.
3. Warn on literal legacy `--password`, scrub argv where a safe platform API is available, and document that launch-time argv exposure cannot be retroactively prevented on unsupported platforms.
4. Update Docker Compose to use a mounted secret file and document operator usage. Add focused unit/static tests for precedence, daemon argument construction, and redaction.

Out of scope: HTTP authentication implementation, release/tag/push, GHSA, production changes, and heavy builds until the root scheduler grants the build lock.
