# Nano adoption plan (issue 0.1 and README section 1): status

Source: `/home/gpc/HDB/heliosdb-adoption-plan.md`. Checked 2026-10-12 against v4.41.0.

## Already true (verified, not new)

| Plan item | Reality | Evidence |
|---|---|---|
| Prebuilt release binaries | Linux x86_64, Linux aarch64, macOS aarch64 (`.tar.gz`) and Windows x86_64 (`.zip`) with `SHA256SUMS` | `gh release view v4.41.0`; `.github/workflows/release.yml` build matrix |
| `curl \| sh` installer | `scripts/install.sh`, served from raw.githubusercontent.com (HTTP 200); downloads and verifies the checksum | run against v4.41.0 on 2026-10-12 |
| Docker image | `ghcr.io/heliosdatabase/heliosdb-nano`, public, multi-arch, tags `latest`, `4.41.0`, `4.41`, `v4.41.0` | anonymous GHCR tag listing; image started and queried over SCRAM + TLS as `helios` |

## Not true, so not claimed in the README

- `docker run -p 5432:5432 heliosdatabase/nano`: there is **no Docker Hub image** (hub API 404), and the real image refuses to start without `HELIOSDB_PASSWORD`. The README uses the GHCR name and the password.
- `https://heliosdb.com/install-nano.sh`: **404**. The README uses the raw GitHub URL. Hosting or redirecting the script on heliosdb.com is a website task.
- "compile it to WASM": `src/wasm/` is not compiled (no `mod wasm`, no cargo feature, no target in CI). Left out of the hero; the comparison table says "not shipped".
- Graph in the hero: graph traversal is a Rust/MCP API; there is no SQL surface (`src/graph/sql.rs` says integration is deferred). Described that way.
- Prebuilt Linux binaries need **glibc ≥ 2.39** (built on `ubuntu-24.04`). On a RHEL/Rocky 9 host the extracted binary fails with `GLIBC_2.39 not found`; the old installer installed it anyway.

## Changed in this branch

- `scripts/install.sh`: smoke-tests the downloaded binary (`--version`) before installing and stops with an explanation if it cannot run. Reproduced on an EL9 host (glibc 2.34): previously "installed", now `fatal` and nothing written.
- `README.md`: new badges that resolve without credentials (crates downloads, latest release, tests workflow, stars); hero and 60-second quickstart; "What you get", "Who it's for", comparison table (rows where others win included); Install reordered to binaries, Docker, then source; Ecosystem moved below Install; benchmark text leads with the wire-to-wire comparison and labels `pg35` as embedded-vs-networked; star CTA at the end of the quickstart and in the footer.

## Verification limits

- Docker flow: run with the v4.41.0 GHCR image using `--network host`, because `-p` publishing is broken on this host (missing iptables `DOCKER` chain, a host fault). Queried with `psycopg2`; `psql` is not installed here.
- Binary flow: the release binary cannot run on this host (glibc), so the same SQL was run against a locally built 4.41.0 binary with `start --memory`. The default loopback/no-password behaviour is from that run, not from the release download.
- The comparison-table cells for other projects come from general knowledge, not fresh research; the README says to check each project.

## Needs the owner (not done here)

1. Release workflow: build the Linux binaries on an older glibc (e.g. `ubuntu-22.04`, 2.35) or add musl so the binary runs on RHEL 9 / Ubuntu 22.04 / Debian 12. This changes the shipped artifact, so it needs its own gate.
2. macOS Intel binary (not built today).
3. Host `install-nano.sh` on heliosdb.com, or drop the plan's URL.
4. Docker Hub `heliosdatabase/nano`: create and publish, or keep GHCR only.
5. Demo GIF (asciinema/vhs), social preview image, Discord badge: assets that do not exist yet. The README has no placeholder for them.
6. GitHub settings: topics (`database, embedded-database, vector-database, postgresql, mysql, wasm, rust, graph-database, full-text-search`; drop `wasm` and think about `graph-database` per the above), Discussions, "good first issue" labels.
