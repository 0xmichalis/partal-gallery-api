# partal-gallery-api

A small, self-hosted HTTP API that stores [Partal](https://partal.xyz) galleries
in Postgres: a thin, authenticated, per-address JSON document store. Built with
Axum + SQLx and shipped as a distroless image, in the same style as
[nftbk](https://github.com/0xmichalis/nftbk).

## What it does

One row per (lowercased) wallet address, with that address's galleries held in a
`data` JSON array. The server treats `data` as opaque JSON — it only persists and
returns it. User authentication and any metadata enrichment stay in the calling
backend; this service is reached server-to-server with a shared bearer token.

## API

All `/v1` endpoints require `Authorization: Bearer <GALLERY_AUTH_TOKEN>`.

| Method | Path                     | Description                                   |
| ------ | ------------------------ | --------------------------------------------- |
| GET    | `/health`                | Liveness probe (no auth). Returns `ok`.       |
| GET    | `/v1/galleries/{address}`| Fetch the stored document, or `404`.          |
| PUT    | `/v1/galleries/{address}`| Upsert the document. Body: `{ "data": [...] }`. `data` must be a JSON array. |
| DELETE | `/v1/galleries/{address}`| Delete the document (idempotent, returns `204`). |

`{address}` is lowercased server-side before use, so reads and writes stay
symmetric regardless of caller casing. Errors are returned as RFC 7807
`application/problem+json`.

### OpenAPI

Interactive docs (Swagger UI) are served at `/v1/docs`, and the raw OpenAPI 3
spec at `/v1/openapi.json`. Both are public (no token); the documented
`/v1/galleries` operations still require the bearer token.

### Examples

```bash
TOKEN=local-dev-token
BASE=http://localhost:8091

# Upsert
curl -X PUT "$BASE/v1/galleries/0xabc" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"data":[{"id":"g1","title":"My gallery","description":"","sections":[]}]}'

# Read
curl "$BASE/v1/galleries/0xabc" -H "Authorization: Bearer $TOKEN"

# Delete
curl -X DELETE "$BASE/v1/galleries/0xabc" -H "Authorization: Bearer $TOKEN"
```

## Configuration

Set via environment (a `.env` file is loaded automatically; see `.env.example`):

| Variable             | Required | Description                                            |
| -------------------- | -------- | ------------------------------------------------------ |
| `DATABASE_URL`       | yes      | Postgres connection string.                            |
| `GALLERY_AUTH_TOKEN` | yes      | Shared bearer token clients must present.               |

CLI flags: `--listen-address` (default `127.0.0.1:8091`), `--log-level`,
`--max-db-connections` (default `5`), `--no-color`.

Migrations in `migrations/` are embedded into the binary and applied on startup,
so a fresh database self-provisions its schema.

## Run locally

```bash
# Full stack (Postgres + API) via Docker:
docker compose up --build

# Or run the binary against your own Postgres:
cp .env.example .env   # edit as needed
cargo run
```

## Deploy

The container is published to `ghcr.io/0xmichalis/partal-gallery-api:latest` by
CI on pushes to `main`. Run it alongside a Postgres database (ideally its own
database and role); the schema is applied automatically on startup.
