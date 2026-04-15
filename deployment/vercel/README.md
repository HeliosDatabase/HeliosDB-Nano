# Vercel is not supported

Vercel is a serverless platform and cannot run the HeliosDB Nano binary.
HeliosDB Nano requires a persistent process with disk access, which serverless
environments do not provide.

## Recommended alternatives

- **Fly.io** -- supports TCP services (PostgreSQL + MySQL wire protocols) and
  persistent volumes. See `../flyio/fly.toml`.
- **Railway** -- supports HTTP traffic with persistent storage. Note that only
  the REST API is accessible; PG/MySQL wire protocols require Fly.io. See
  `../railway/railway.toml`.
- **Render** -- supports Docker deployments with persistent disks. See
  `../render/render.yaml`.
- **Docker Compose** -- for self-hosted deployments. See
  `../docker/docker-compose.yml`.
