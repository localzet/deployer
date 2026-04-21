# Localzet Deployer

[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPLv3-blue.svg)](./LICENSE)
[![Docker Publish](https://github.com/localzet/deployer/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/localzet/deployer/actions/workflows/docker-publish.yml)

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

## Repository Structure

- `apps/orchestrator` — Rust backend
- `web` — built-in web UI
- `config` — example runtime configuration
- `config/app.prod.toml.example` — production-oriented configuration template
- `infra` — Dockerfile and Docker Compose setup
- `db/migrations` — PostgreSQL schema
- `scripts/smoke.sh` — post-start smoke check

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

Toolchain baseline:

- Rust `1.88+` for local builds
- Docker Buildx for image publishing
- PostgreSQL 16 for the default stack

1. Copy [config/app.example.toml](./config/app.example.toml) to `config/app.toml`.
2. Set real values for:
    - `github_webhook_secret`
    - `auth.api_token`
    - PostgreSQL connection settings
    - registry credentials
    - Portainer webhook URL
    - health check URL
3. Start the stack:

```bash
docker compose -f infra/docker-compose.yml up --build -d
```

4. Open `http://localhost:8080`.
5. Run the smoke check:

```bash
API_TOKEN='your-token' ./scripts/smoke.sh http://localhost:8080
```

## Image Publishing

The workflow in [docker-publish.yml](./.github/workflows/docker-publish.yml) publishes images to:

- Docker Hub: `localzet/deployer`
- GHCR: `ghcr.io/localzet/deployer`

Required GitHub secret:

- `DOCKERHUB_TOKEN` — Docker Hub access token

The workflow uses the built-in `GITHUB_TOKEN` for GHCR publishing.

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
