# Localzet Deployer

[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPLv3-blue.svg)](./LICENSE)

Localzet Deployer is a self-hosted CI/CD control plane for teams that want to run their own delivery pipeline with
GitHub Webhooks, private container registries, and Portainer-based deployments.

## Overview

Localzet Deployer is designed for teams that want:

- GitHub Webhooks instead of GitHub-hosted build minutes
- a focused Rust control plane instead of a full CI platform
- private image publishing
- Portainer redeploy hooks
- deployment history, logs, health checks, and rollback

Core capabilities:

- GitHub push webhook ingestion
- HMAC webhook signature validation
- authenticated UI and API
- queued job execution with per-repository locking
- action-aware pipelines for `webhook`, `retry`, `manual`, and `rollback`
- immutable image references such as `image:sha-<shortsha>`
- deployment history and health checks
- manual redeploy and rollback
- PostgreSQL-backed history and logs

## Architecture

### Execution Flow

1. GitHub sends a `push` webhook to `POST /api/webhooks/github`.
2. The orchestrator validates `X-Hub-Signature-256`.
3. UI and API access are protected with a bearer token.
4. The service resolves a project by `repository + branch` from `config/app.toml`.
5. A job is created, queued, and locked by `repo:branch`.
6. The worker executes action-aware steps such as:
    - checkout
    - build and push
    - promote immutable tag to stable tag
    - redeploy
    - health check
7. Logs are stored in PostgreSQL and streamed to the UI with SSE.
8. Operators can inspect jobs and deployments, then trigger `retry`, `redeploy`, or `rollback`.

### Design Goals

- Keep the platform focused on deployment control instead of becoming a generic CI suite.
- Describe pipelines declaratively in TOML.
- Support redeploy and rollback without forcing a rebuild.
- Provide a lightweight built-in UI that can be replaced later.
- Persist operational history while keeping runtime queue state simple.

## Operator Notes

- The repository contains the Rust backend, built-in web UI, example configuration, and deployment assets.
- Operators typically only need the released image plus a mounted `app.toml`.
- `config/app.prod.toml.example` is the reference runtime template.
- `scripts/smoke.sh` is useful after first startup.

## API

- `GET /healthz`
- `GET /api/dashboard`
- `GET /api/projects`
- `GET /api/jobs`
- `GET /api/jobs/:id`
- `GET /api/jobs/:id/events`
- `GET /api/deployments`
- `POST /api/deployments/:id/rollback`
- `POST /api/projects/:id/redeploy`
- `POST /api/jobs/:id/cancel`
- `POST /api/jobs/:id/retry`
- `POST /api/webhooks/github`

## Configuration

The main runtime file is `config/app.toml`.

The configuration model supports:

- webhook secret validation
- bearer token authentication for the operator API
- PostgreSQL connectivity
- project-to-repository mapping
- action-aware pipeline steps
- environment variables for registry credentials and deployment commands

Step templates support variables such as:

- `$IMAGE_REFERENCE`
- `$STABLE_IMAGE_REFERENCE`
- `$JOB_ACTION`
- `$SOURCE_DEPLOYMENT_ID`

Steps can also be filtered with:

- `only_actions`
- `skip_actions`

## Quick Start

For operators who want to run the released container image:

1. Download the production config template from the repository into a local runtime directory:

```bash
mkdir -p /opt/deployer
curl -fsSL https://raw.githubusercontent.com/localzet/deployer/main/config/app.prod.toml.example \
  -o /opt/deployer/app.toml
```

2. Edit `/opt/deployer/app.toml` and set real values for:
   - `github_webhook_secret`
   - `auth.api_token`
   - PostgreSQL connection settings
   - registry credentials
   - Portainer webhook URL
   - health check URL

3. Use a Compose file that mounts the config into the container, for example:

```yaml
services:
  orchestrator:
    image: localzet/deployer:latest
    environment:
      CICD_CONFIG: /app/config/app.toml
      RUST_LOG: info,tower_http=info
    ports:
      - "8080:8080"
    volumes:
      - /opt/deployer:/app/config:ro
      - cicd-workspace:/workspace/repos
      - /var/run/docker.sock:/var/run/docker.sock
    depends_on:
      postgres:
        condition: service_healthy
    restart: unless-stopped

  postgres:
    image: postgres:16-bookworm
    environment:
      POSTGRES_DB: cicd
      POSTGRES_USER: cicd
      POSTGRES_PASSWORD: cicd
    volumes:
      - postgres-data:/var/lib/postgresql/data
    restart: unless-stopped

  registry:
    image: registry:2
    volumes:
      - registry-data:/var/lib/registry
    restart: unless-stopped

volumes:
  cicd-workspace:
  postgres-data:
  registry-data:
```

4. Start the stack:

```bash
docker compose up -d
```

5. Open `http://localhost:8080`.

6. Run the smoke check:

```bash
API_TOKEN='your-token' ./scripts/smoke.sh http://localhost:8080
```

## Operational Status

Implemented:

- GitHub push webhooks
- HMAC validation
- authenticated API and UI
- job queue and step execution
- live log streaming
- retry, redeploy, and rollback actions
- immutable image references
- deployment history
- health checks
- Docker Compose stack with service health checks
- PostgreSQL persistence for jobs and logs

Operationally important notes:

- manual redeploy and rollback can avoid rebuilding by promoting an immutable tag to the stable tag before redeploy
- image digest support is partially implemented and currently relies on registry/build tooling with a log-based fallback

## Roadmap

Planned hardening and product work:

1. Move rollback from SHA-oriented replay to strict artifact-based redeploy by digest
2. Improve digest capture so it does not rely on shell heuristics
3. Add RBAC on top of the current token-based authentication
4. Replace the built-in UI with a richer standalone frontend if needed

## License

This project is licensed under the GNU Affero General Public License v3.0 only. See [LICENSE](./LICENSE).
