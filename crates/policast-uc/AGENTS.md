# AGENTS.md — policast-uc

## What this crate owns

`policast-uc` is the Unity Catalog policy store + resolver layer. It turns UC's
`governance.policast.*` Delta tables into a Policy Decision Point: it reads the
governance state over the UC REST API, compiles/serves it, and exposes an
owner-aware policy edit API. It is the home for:

- A typed, authenticated **UC REST client** (`uc_rest`).
- The **resolver core** + wire types (`store`, `types`, `signature`) and the
  Axum **sidecar** (`sidecar`) implementing `/policies/resolve` and the policy
  edit API.
- Read **backends** (`backend::FileBackend`, `uc_bootstrap::UcBootstrapBackend`).
- A **materialized Redis PolicyStore** (`redis_store`) holding compiled
  manifests as binary protobuf.
- The **compile/exchange** flow (`exchange`) and the **owner-aware editor**
  (`editor`).

## Source of truth & data flow

Unity Catalog's `governance.policast.*` Delta tables are the **source of truth**.
Redis is the **materialized serving store** (compiled `PolicyManifest` per table,
encoded as `policast.v1` binary protobuf under the `policast:v2:manifest:` keyspace).
JSON stays the canonical, HMAC-signed wire format for `/policies/resolve`; protobuf
is only an at-rest encoding.

```
UC governance Delta tables ──(UcRestClient: get table + vend creds)──▶ UcBootstrapBackend snapshot
        ▲                                                                      │
        │ editor write-back (READ_WRITE vended creds, append new version)      │ materialize_into
   PolicyEditor / PUT /policies/:id                                           ▼
                                                              RedisManifestStore (protobuf)
                                                                               │ RedisFirstPolicyStore
                                                                               ▼  (in-memory fallback)
                                                                          engines
```

## Key files

- `src/uc_rest.rs`: `UcRestClient`, `AuthMode` (the `auth=true|false` toggle),
  `TableOperation`, credential vending.
- `src/store.rs`: `ResolverCore` (resolve a signed bundle; `materialize_tables`
  builds per-table manifests; latest-version-wins selection for appended edits).
- `src/backend.rs`: `ResolveBackend` trait, `PolicyRow` (now carries `owner`),
  `FileBackend`.
- `src/uc_bootstrap.rs`: Delta-backed snapshot + refresh; `materialize_into` and
  `run_materialize_loop` (the "flush UC updates into the PolicyStore" loop).
- `src/redis_store.rs`: `RedisManifestStore` (protobuf put/get + `materialize`),
  `RedisFirstPolicyStore` decorator (Redis first, inner fallback).
- `src/exchange.rs`: Cedar → `PolicyManifest` compile, `compile_single` (owner +
  source), `compile_and_publish`.
- `src/editor.rs`: `PolicyEditor` (get current / validate / edit), `authorize`
  (owner-or-admin), delta-rs append write-back.
- `src/sidecar.rs`: resolve route + `editor_router` (`GET`/`POST validate`/`PUT`
  on `/policies/:id`), reflush-to-Redis on edit.
- `src/bin/sidecar.rs`: the `policast-uc-sidecar` binary (CLI config surface).

## Cargo features

- `client` (default): `uc_rest` REST client + the resolve HTTP client.
- `sidecar`: the Axum service + binary.
- `redis`: the materialized protobuf PolicyStore (`redis_store`); enables
  `policast-core/{redis,proto}`.
- `uc-bootstrap`: Delta-backed backend + the `editor` (pulls in `deltalake` +
  `datafusion`; also enables `client`).

## Config surface (the sidecar binary)

`--uc-endpoint` (default `https://uc.openlakehousedemos.dev`), `--uc-auth
enable|disable` + `--uc-bearer-token-env` (static bearer; `disable` is the local
docker path), `--uc-storage-uri-template` / `--uc-storage-option`,
`--uc-refresh-interval-secs`, `--admin-role` (default `governance_admin`), and
`--redis-url` (materialize + edit reflush; requires the `redis` feature).

## Owner model

Policies carry an `owner` (from the Cedar `@owner` annotation, defaulting to
`created_by`). Everyone may read; only the owner or the configured admin role may
edit. Enforcement lives in `editor::authorize` (the edit API), not the engine hot
path. Edits **append** a new version row to `policies` + `manifest`; the resolver
treats the highest `version` per `policy_id` as authoritative.

## Protobuf regeneration

The `policast.v1` Rust types are checked in at
`../policast-core/src/proto/generated/policast.v1.rs` so normal builds need no
`protoc`. To regenerate after editing `proto/policast/v1/policast.proto`, restore
a `build.rs` calling `prost_build` and run
`POLICAST_PROTO_REGEN=1 cargo build -p policast-core --features proto` (see
`policast-core/src/proto.rs`).

## Testing guidance

- Fast: `cargo test -p policast-uc` (default `client`).
- Redis logic (materialize/proto round-trip, no server): `cargo test -p
  policast-uc --features redis`. Live Redis round-trip runs only when
  `POLICAST_TEST_REDIS_URL` is set.
- Editor + sidecar: `cargo test -p policast-uc --features sidecar,uc-bootstrap`.
