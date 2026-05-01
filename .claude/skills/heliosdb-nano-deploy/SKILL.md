---
name: heliosdb-nano-deploy
description: Deploy HeliosDB-Nano in containers and managed-platform environments. Covers the in-repo Dockerfile (`Dockerfile.binary` and `deployment/docker/`), docker-compose, Fly.io (`deployment/flyio/fly.toml`), Railway (`deployment/railway/railway.toml`), Render (`deployment/render/render.yaml`), the bare-metal install script (`scripts/install-nano-pilot.sh`), and a documented systemd template. Use this when the user wants to ship Nano to a hosted environment or wrap it in a service.
allowed-tools: Bash(docker *), Bash(docker compose *), Bash(flyctl *), Bash(systemctl *), Read
---

# Deployment Recipes

## When to use
- Putting Nano behind a managed PaaS (Fly.io, Railway, Render).
- Building a Docker image for production.
- Wrapping the binary in a systemd unit.
- Orchestrating a multi-node cluster (paired with `heliosdb-nano-server` HA recipes).

> **Risk note**: deployment changes are visible to teammates and may affect production traffic. Confirm with the user before pushing image tags or running `flyctl deploy` / equivalent.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| docker build (binary image) | shell | `docker build -f Dockerfile.binary -t heliosdb-nano:local .` |
| docker compose up | shell | `cd deployment/docker && docker compose up -d` |
| fly deploy | shell | `cd deployment/flyio && flyctl deploy` |
| railway link | shell | `railway link` (uses `deployment/railway/railway.toml`) |
| render deploy | shell | wired by `deployment/render/render.yaml` (commit + push to deploy) |
| install (bare metal) | shell | `bash scripts/install-nano-pilot.sh` |
| systemd | shell | (template — see `heliosdb-nano-server` Recipe 7) |

## Recipes

### Recipe 1: Docker (binary-only image)
The repo ships `Dockerfile.binary` — a minimal runtime image around the release binary.

```bash
# from repo root
cargo build --release                      # produces target/release/heliosdb-nano
docker build -f Dockerfile.binary -t heliosdb-nano:dev .

docker run --rm -p 5432:5432 \
    -v $(pwd)/mydata:/var/lib/heliosdb \
    heliosdb-nano:dev start \
        --data-dir /var/lib/heliosdb \
        --listen 0.0.0.0 \
        --port 5432
```

### Recipe 2: docker-compose (multi-service development)
```bash
cd deployment/docker
docker compose up -d
docker compose logs -f heliosdb
```
Adjust `docker-compose.yml` for ports, volume paths, env-injected admin password.

### Recipe 3: Fly.io
```bash
cd deployment/flyio
flyctl auth login
flyctl launch --copy-config --no-deploy        # one-time setup
flyctl secrets set HELIOSDB_ADMIN_PASSWORD='change-me'
flyctl deploy
flyctl logs
```
Persistent storage via Fly volumes — see the `mounts` block in `fly.toml`.

### Recipe 4: Railway
```bash
cd deployment/railway
railway login
railway link              # connects this repo to your Railway project
railway up                # deploy
```
Configuration is read from `railway.toml`. Add a Railway volume for `/data`.

### Recipe 5: Render
`deployment/render/render.yaml` is a Blueprint — connect the repo from the Render dashboard and Render reads it on each push.

### Recipe 6: Bare-metal install script
```bash
bash scripts/install-nano-pilot.sh
# downloads/copies the binary, sets up data dir, optionally registers a systemd unit.
```
Tested for the "code-graph pilot" workflow — see the script for prompts.

### Recipe 7: Kubernetes (template — not pre-baked)
A minimal `Deployment` + `StatefulSet` + `Service` is left to the user. Key requirements:
- **Persistent volume** for `--data-dir`.
- **Init container** to `chown` the volume to the runtime user.
- **Liveness probe**: `GET /health` on `--http-port`.
- **Readiness probe**: same endpoint, allow longer initial delay for WAL replay.
- **Headless service** + **statefulset ordinality** for HA tier-1+.

A community-contributed Helm chart is on the roadmap (`FEATURE_REQUEST_*` markers in repo).

### Recipe 8: Backup volume mount
Whatever platform you use, always mount `/var/lib/heliosdb` (or your `--data-dir`) on a backed-up volume:
```yaml
# docker-compose
services:
  heliosdb:
    volumes:
      - heliosdb-data:/var/lib/heliosdb
volumes:
  heliosdb-data:
    driver: local
    driver_opts:
      type: none
      o: bind
      device: /backed-up/heliosdb
```
Pair with `--dump-schedule "0 */6 * * *"` (see `heliosdb-nano-backup`).

## Pitfalls
- **Default `--listen 127.0.0.1`** doesn't accept connections from outside the container. Use `--listen 0.0.0.0` in container deployments.
- **Container restarts wipe an `--memory` DB** unless `--dump-on-shutdown` + a mounted volume + a startup `restore` step are wired in. Generally use `--data-dir` in containers.
- **Image size**: the release binary is ~30 MB. `Dockerfile.binary` ships a thin runtime; don't accidentally rebuild from source in the runtime stage.
- **Fly volumes are per-region**: HA tier-1 across regions needs explicit `flyctl volumes` setup or external storage.
- **`fastembed_cache/`** (built with `--features code-embed`) is large and should not be COPYed into the production image — generate it on first run against a mounted writable volume, or pre-bake it into a separate cache image.
- **Don't bake admin secrets into images**. Use platform secret stores (`flyctl secrets`, Railway Variables, Render env groups, k8s Secrets).

## See also
- `heliosdb-nano-server` — daemon mode, TLS, auth, HA flags.
- `heliosdb-nano-backup` — backup volumes and `--dump-schedule`.
- `heliosdb-nano-observability` — `/health`, metrics, log shipping.
- `Dockerfile.binary`, `deployment/docker/`, `deployment/flyio/`, `deployment/railway/`, `deployment/render/` — concrete artifacts.
- `scripts/install-nano-pilot.sh` — bare-metal helper.
