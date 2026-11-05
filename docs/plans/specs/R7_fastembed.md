# R7 — `code-embed` without webpki-roots 0.26 (Sprinter b93af8c4a65f)

RUSTSEC-2026-0381 (webpki-roots < 1.0.10 trusts distrusted emSign intermediates) is waived in
`.cargo/audit.toml` because the optional `code-embed` feature pulls fastembed 4.9 → hf-hub
0.4 → ureq 2 → webpki-roots 0.26. fastembed 7.5.0 (latest stable) depends on hf-hub ^0.5
(optional) and ort =2.0.0-rc.15.

- Bump `fastembed` in Cargo.toml (`fastembed = { version = "4", optional = true }`, ~line 190)
  to the newest 7.x whose dependency tree has no webpki-roots < 1.0.10 and no other advisory;
  adapt every use of the fastembed API under the `code-embed` feature (grep `fastembed::`) to
  the new API — model enum names, `InitOptions`/`TextInitOptions`, embedding call signatures,
  error types; keep behaviour (same default model, same dimension, same cache directory
  semantics) and say where the new version forces a difference.
- Remove the RUSTSEC-2026-0381 ignore from `.cargo/audit.toml` (and any deny.toml exception)
  once the tree no longer contains the old crate.
- Check `ort` rc version pinning does not break the default build or the other features
  (`code-graph`, `graph-rag`).
- The dispatching session will run: `cargo update -p fastembed` (and whatever else the new
  version needs), `cargo tree --all-features -i webpki-roots`, `cargo audit`, `cargo deny
  check`, `cargo build --features code-embed,code-graph,graph-rag`, and the code-embed tests.
  Write down in your report the exact commands it must run and in which order.

Rules: do not run cargo, git or any build. Report the API changes made and anything that
could not be preserved.
