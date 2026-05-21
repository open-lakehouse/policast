# Unity Catalog + MinIO Compose walkthrough (`uc-full`)

This page documents the **full UC-backed local stack** added in Stage 3.

Use it when you want the sidecar to resolve from **Delta governance tables**
instead of the flat JSON store.

## What `uc-full` starts

The `uc-full` profile adds these services on top of the base compose stack:

1. `minio` — object storage for the `policast-demo` bucket.
2. `minio-init` — one-shot bucket bootstrap.
3. `unitycatalog` — pinned to `newfrontdocker/unitycatalog:v0.4.1`.
4. `uc-bootstrap` — one-shot init that:
   - discovers `examples/uc/ddl/*.sql` inputs,
   - seeds governance Delta tables (`policies`, `manifest`, `bindings`, `tags`)
     via `policast-uc-seed`.
5. `sidecar-uc-full` — runs `policast-uc-sidecar --backend uc-bootstrap`
   against MinIO-backed Delta tables.

The default `sidecar` service (flat-file backend) remains unchanged.

## Prerequisites

```bash
cp docker/.env.example .env
```

Optional knobs in `.env`:

- `POLICAST_UC_FULL_PORT` (default `8766`) — host port for `sidecar-uc-full`.
- `POLICAST_UC_REFRESH_SECS` (default `30`) — snapshot refresh interval.
- `MINIO_ROOT_USER`, `MINIO_ROOT_PASSWORD`, `MINIO_REGION`.

## Bring up uc-full

```bash
just uc-full-up
```

Equivalent raw compose command:

```bash
docker compose --profile uc-full up -d \
  minio minio-init unitycatalog uc-bootstrap sidecar-uc-full
```

Health check:

```bash
curl -fsS "http://127.0.0.1:${POLICAST_UC_FULL_PORT:-8766}/health"
```

## Run the DataFusion demo against uc-full

```bash
just uc-full-demo
```

Equivalent:

```bash
POLICAST_UC_ENDPOINT=http://sidecar-uc-full:8765 \
  docker compose --profile uc-full --profile demo run --rm datafusion-demo
```

## Tear down uc-full

```bash
just uc-full-down
```

This removes only uc-full services and leaves the default file-backed flow
(`just up` / `just demo`) untouched.

## Troubleshooting

### `uc-bootstrap` failed

Inspect logs:

```bash
docker compose logs uc-bootstrap
```

Common causes:

- Missing/invalid MinIO credentials in `.env`.
- Existing governance tables from a previous run with incompatible schema.
- Network reachability problems between compose services.

### sidecar is up but resolve is stale

`sidecar-uc-full` refreshes snapshots every `POLICAST_UC_REFRESH_SECS`.
Set it lower for faster local feedback while iterating.

### I only want the simple flow

Use the original commands:

```bash
just up
just demo
```

Those still run the flat-file backend on `POLICAST_UC_PORT` (default `8765`).
