# AGENTS.md — policast-playground

## What this crate owns

`policast-playground` is the phase-two governed-query playground. It lets you load
Cedar policies, pick a Unity Catalog table and a test principal, optionally register
Unity Catalog `routine_body=SQL` functions as DataFusion UDFs, run SQL, and see the
**governed** result (the same row filters + column masks the engines enforce, via
`policast_datafusion::GovernedTable`). It is both a library (the engine) and a binary
(an Axum server that serves a self-contained SPA + `POST /api/query`).

## Why it is its own crate

It needs `policast_datafusion::GovernedTable` (enforcement) AND `policast_uc` (UC REST +
Cedar compile). `policast-datafusion` already depends on `policast-uc` (its `uc` feature),
so `policast-uc` cannot depend on `policast-datafusion`. A separate crate that depends on
both avoids the cycle.

## Key files

- `src/engine.rs`: the governed query path.
  - `execute(ExecuteParams)` — UC-free, fully testable: open Delta → compile inline
    Cedar → `GovernedTable` + register UDFs → run SQL → rows + applied policies/masks.
  - `PlaygroundService::run(PlaygroundRequest)` — resolves the table's storage URI +
    temp credentials over `policast_uc::UcRestClient`, then calls `execute`.
- `src/functions.rs`: UC SQL function → DataFusion `ScalarUDF` registration (ported from
  the `uc-query` sidecar's `functions.rs`).
- `src/server.rs`: Axum router (`GET /`, `GET /healthz`, `POST /api/query`); errors → JSON
  `{ "error": ... }`.
- `src/main.rs`: the `policast-playground` binary + CLI.
- `static/index.html`: the single-file SPA (vanilla JS, no build step), embedded via
  `include_str!`.

## Architecture notes

- **Enforcement is reused, not reimplemented.** Inline Cedar policies are compiled with
  `policast_uc::exchange::compile`; the resulting `PolicyManifest` + an `AttrIdentity`
  (built from the principal's role/attrs) feed `GovernedTable::new`. The masks shown come
  from `GovernedTable::masked_columns`; the applied policies come from
  `PolicyManifest::policies_for_table`.
- **Tag-scoped templates are not expanded here** — the playground compiles concrete
  policies (use `@target_table`, not `@target_tag`). Resolver-side tag expansion lives in
  `policast-uc`.
- **Result cells are formatted to strings** via Arrow's `ArrayFormatter` (NULL → `None`),
  so any column type renders; rows are capped at the request `limit`.
- **S3:** `deltalake` is built with the `s3` feature so hosted (S3-backed) UC tables work;
  local `file://` tables work for tests.

## Running

Turnkey local (Docker, no UC needed — seeds a demo `patients` Delta table):

```bash
just playground-up      # build + start on :8088, waits healthy, prints the URL
just playground-down    # stop
```

Or run the binary directly:

```bash
# Self-contained demo table (no UC):
cargo run -p policast-playground -- --demo-table file:///tmp/policast-playground/patients

# Hosted UC, auth on:
cargo run -p policast-playground -- \
  --uc-endpoint https://uc.openlakehousedemos.dev \
  --uc-auth enable --uc-bearer-token-env UC_TOKEN

# Local docker UC (auth off) over MinIO:
cargo run -p policast-playground -- \
  --uc-endpoint http://localhost:8080 --uc-auth disable \
  --uc-storage-option AWS_ENDPOINT_URL=http://localhost:9000 \
  --uc-storage-option AWS_ALLOW_HTTP=true \
  --uc-storage-option AWS_REGION=us-east-1
```

Then open the printed URL (default `http://127.0.0.1:8088`).

### Table resolution

`PlaygroundService::run` resolves `req.table` two ways:

- **Local override** (`--local-table NAME=URI`, or `--demo-table` which seeds +
  registers `hospital.clinical.patients`): open the URI directly, no UC.
- **UC** (default): `UcRestClient::resolve_table_access` → storage location +
  temp credentials.

`seed_demo_patients(uri)` writes the demo table idempotently (id, region,
treating_physician, ssn, diagnosis, legal_hold) so the prefilled SPA policies
have something to act on.

## Testing

`cargo test -p policast-playground` — UDF registration tests plus engine integration tests
that build a local Delta fixture and assert governed behavior (row filter drops rows,
column mask replaces cells with `***`, admin exemption, inline UDF over the table, compile
errors surface, row-limit truncation). No UC server or S3 needed.

## Editing guardrails

- Keep `execute` UC-free so it stays unit-testable against local Delta tables.
- Don't duplicate enforcement logic — extend `policast-datafusion` instead and call it here.
- Errors should be actionable in the UI: compile/SQL/function problems → `BAD_REQUEST`,
  UC/Delta problems → `BAD_GATEWAY` (see `server::ApiError`).
