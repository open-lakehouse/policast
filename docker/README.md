# policast Compose stack

Local docker-compose environment that runs the core runnable units of
policast-cel end-to-end: the Cedar→CEL compiler, the UC-style resolver
sidecar, and the DataFusion enforcement path.

## Services

| Service | Profile | Lifetime | Purpose |
|---------|---------|----------|---------|
| `sidecar` | (default) | long-running | `policast-uc-sidecar` on `:8765`, serving `/policies/resolve` from `examples/uc/store` (defaults to `--backend file`; also supports `--backend uc-bootstrap` — see below) |
| `datafusion-demo` | `demo` | one-shot | Runs the `run_datafusion_uc_http` example against the sidecar over HTTP |
| `compile` | `tools` | one-shot | Runs `scripts/compile-policies.sh` to (re)build `examples/policies/manifest.json` |
| `df-shell` | `shell` | interactive | Rust toolchain + workspace mounted at `/workspace` for ad-hoc `cargo run --example ...` |
| `unitycatalog` | `uc-oss` | long-running | Upstream Unity Catalog OSS image — staged only, not yet wired to the sidecar |

## Quickstart

```bash
cp docker/.env.example .env
docker compose up -d sidecar
docker compose --profile demo run --rm datafusion-demo
```

Or with `just` (install via `brew install just` on macOS):

```bash
just init          # creates .env if missing
just up            # start + wait for healthy
just demo          # run the DataFusion demo (analyst)
just demo-admin    # admin: no row filter, no column masks
just demo-physician# physician: row-filtered, no column masks
just demo-ssn      # run all three back-to-back (SSN masking contrast)
just clean         # tear down + drop named volumes
```

See the [Justfile](../Justfile) for the full recipe list (`just`
with no args lists them).

For the full Unity Catalog + MinIO profile walkthrough (`just uc-full-up`,
`just uc-full-demo`, `just uc-full-down`), see
[`docs/unity-catalog/compose.md`](../docs/unity-catalog/compose.md).

The demo will:

1. Wait for `sidecar:8765/health` to return `200 OK`.
2. Create a local Delta table of patients inside the container.
3. Build a `UcClient` pointed at `http://sidecar:8765` using `POLICAST_UC_SECRET`.
4. `POST /policies/resolve` for the configured principal (default: analyst in `us-east`).
5. Verify the HMAC signature, register the `GovernedTable` in DataFusion, and print the row-filtered + column-masked result.

Switch roles to see different enforcement:

```bash
POLICAST_PRINCIPAL_ROLE=physician \
POLICAST_PRINCIPAL_NAME="Dr. Smith" \
POLICAST_PRINCIPAL_REGION= \
  docker compose --profile demo run --rm datafusion-demo
```

### SSN + diagnosis column masks

Column masking is authored as **tag-scoped Cedar templates**
([`examples/policies/column_mask.cedar`](../examples/policies/column_mask.cedar)):

- `column_mask_by_pii_tag` redacts every column carrying the `pii` tag.
- `column_mask_by_phi_tag` redacts every column carrying the `phi` tag.

Column → tag assignments live in the governance tag index
([`examples/uc/ddl/06_tags.sql`](../examples/uc/ddl/06_tags.sql),
mirrored by `examples/uc/store/tags.json`). For the shipped demo the
index has `patients:ssn → pii` and `patients:diagnosis → phi`, so the
sidecar expands the two templates at resolve time into
`column_mask_by_pii_tag@hospital.clinical.patients:ssn` and
`column_mask_by_phi_tag@hospital.clinical.patients:diagnosis` — with
the expansion recorded in `ResolveBundle.expanded_from` for audit.
Tag a new column `pii` and column masking picks it up automatically
with no Cedar edit.

Both templates' `unless` clauses still whitelist `admin` and
`physician`, so effective enforcement is unchanged from the
pre-template incarnation. The easiest way to see the contrast is:

```bash
just demo-ssn
```

which runs the same `SELECT` three times, as admin → physician →
analyst. Expected behavior:

| role      | rows returned | `ssn`         | `diagnosis`   |
|-----------|---------------|---------------|---------------|
| admin     | all non-legal-hold rows | real value | real value |
| physician | only rows where `treating_physician == principal.name` | real value | real value |
| analyst   | only rows where `region == principal.region` | `***` | `***` |

The masking is applied server-side in DataFusion by wrapping the scan
in a `ProjectionExec` (see
[`crates/policast-datafusion/src/governance_table.rs`](../crates/policast-datafusion/src/governance_table.rs)),
so the raw SSN never leaves the table provider for non-privileged
roles.

> Note: the Cedar policy references `resource.table_name == "patients"`
> (the short name) while the UC policy store keys on the three-part
> `hospital.clinical.patients`. The HTTP demo honors both by resolving
> against the UC name and registering the `GovernedTable` under the
> short name (derived from `POLICAST_TABLE` or overridden via
> `POLICAST_GOVERNED_NAME`).

## Rebuilding the policy manifest

`examples/uc/store/` ships a pre-compiled manifest. To re-compile from
the Cedar sources under `examples/policies/`:

```bash
docker compose --profile tools run --rm compile
```

The workspace is bind-mounted so the new `manifest.json` is written
straight back to the host. The next `docker compose up sidecar` picks
it up from the read-only mount at `/data/store`.

## Interactive dev shell

```bash
docker compose --profile shell run --rm df-shell
# inside the container:
cargo test -p policast-uc
cargo run --example run_datafusion_uc_http \
  -p policast-datafusion --features "uc delta"
```

The `cargo-registry` and `cargo-target` named volumes persist build
caches across shell invocations.

## Unity Catalog OSS (staged)

```bash
docker compose --profile uc-oss up -d unitycatalog
```

This starts the upstream UC OSS image on `:8081`. It is **not** yet
consulted by the sidecar by default. Today the `sidecar` service runs
the flat-file `FileBackend`
([`crates/policast-uc/src/backend.rs`](../crates/policast-uc/src/backend.rs)) against
the mounted `examples/uc/store/` directory. The production-backend
replacement, `UcBootstrapBackend`
([`crates/policast-uc/src/uc_bootstrap.rs`](../crates/policast-uc/src/uc_bootstrap.rs)),
is fully wired: it snapshots the four governance Delta tables
(`policies`, `manifest`, `bindings`, `tags`) on startup and refreshes
them every `--uc-refresh-interval-secs`, optionally fanning
`InvalidateAll` out to the resolver's bundle cache via
[`crates/policast-uc/src/cdc.rs`](../crates/policast-uc/src/cdc.rs).

The sidecar binary is built with `--features sidecar,uc-bootstrap`
(see the `sidecar-build` stage in
[`docker/Dockerfile`](./Dockerfile)) so both backends are available in
the shipped image. Select the production backend with:

```bash
docker compose run --rm sidecar \
  --listen 0.0.0.0:8765 \
  --backend uc-bootstrap \
  --uc-storage-uri-template s3://policast-demo/governance/policast/{table} \
  --uc-storage-option AWS_ENDPOINT_URL=http://minio:9000 \
  --uc-storage-option AWS_ACCESS_KEY_ID=... \
  --uc-storage-option AWS_SECRET_ACCESS_KEY=... \
  --uc-storage-option AWS_REGION=us-east-1 \
  --uc-storage-option AWS_ALLOW_HTTP=true \
  --uc-refresh-interval-secs 30 \
  --signing-secret-env POLICAST_UC_SECRET
```

What is still pending: a `uc-full` compose profile that adds MinIO
(+ the `policast-demo` bucket), pins
`newfrontdocker/unitycatalog:v0.4.1`, bootstraps the catalog via an
init container that runs `examples/uc/ddl/*.sql` + seeds `tags.json`,
and flips the default `sidecar` service to `--backend uc-bootstrap`.
See
[`.cursor/plans/cedar-templates-and-tags_9f2a3b14.plan.md`](../.cursor/plans/cedar-templates-and-tags_9f2a3b14.plan.md)
tasks `compose-stack-uc-oss` / `compose-just-uc-full` /
`compose-docs-uc-full` / `uc-bootstrap-credentials`. Until that lands,
this service is present so you can validate UC connectivity and iterate
on the integration without rewriting the compose stack.

## Corporate crates.io mirror

If you build behind a corporate firewall that blocks `index.crates.io`
and instead exposes a crates.io mirror (e.g. a sparse-index proxy),
this stack picks it up via an **uncommitted** `.cargo/config.toml`
at the repo root. The committed tree only ships `.cargo/.gitkeep` so
the directory exists for `docker/Dockerfile`'s `COPY .cargo ./.cargo`
to resolve on a fresh clone — no proxy URLs are ever committed.

How it flows:

- **Image build:** the `builder` stage in
  [`docker/Dockerfile`](./Dockerfile) runs `COPY .cargo ./.cargo`, so
  if you drop a local `.cargo/config.toml` it gets baked into the
  `sidecar-build` and `demo-build` stages and `cargo build` resolves
  through the mirror.
- **Container runtime:** the `compile`, `df-shell`, and `uc-bootstrap`
  services bind-mount the repo at `/workspace`, so cargo running
  inside them walks the same `/workspace/.cargo/config.toml`.
- **`.dockerignore`** excludes `.cargo/registry/` and `.cargo/git/`,
  so cache archives never ship into images regardless.

If `.cargo/config.toml` is absent (the OSS default), cargo falls back
to `index.crates.io` transparently. You don't need to do anything.

### Setting up a local mirror

Create `.cargo/config.toml` at the repo root with your mirror's URL:

```toml
[net]
git-fetch-with-cli = true
retry = 5

[http]
timeout = 120

[source.crates-io]
replace-with = "crates-proxy"

[source.crates-proxy]
registry = "sparse+https://crates-proxy.{your-crates-proxy}.com/"
```

Replace `crates-proxy.{your-crates-proxy}.com` with your mirror's
host. The `sparse+https://` scheme uses cargo's sparse registry
protocol (the post-1.68 default) — drop the `sparse+` prefix only if
your mirror exposes the legacy git-index protocol instead.

`.gitignore` covers `.cargo/*` except `.cargo/.gitkeep`, so this
file stays local. Double-check with `git status` after creating it —
it should not appear as a new file.

If you already maintain a host-wide `~/.cargo/config.toml` with the
same mirror entry, a one-liner keeps the in-container path in sync:

```bash
cp ~/.cargo/config.toml .cargo/config.toml
```

## Rust toolchain version

The Dockerfile pins Rust via the `RUST_VERSION` build arg (default
`1.90`). Bumping is a one-liner on the build command:

```bash
docker build --build-arg RUST_VERSION=1.91 --target sidecar -f docker/Dockerfile .
```

1.88+ is required by transitive deps (`time`, `serde_with`,
`smol_str`). Stick to 1.90 or newer unless you have a reason.

## Known limitations

- **No TLS.** Auth is HMAC signatures on the resolve bundle; suitable
  for local dev only.
- **`unitycatalog` is staged**, not wired. See above.
- **`policast-spark` is not included.** It needs a JVM toolchain and
  is run out-of-band with `sbt` / `spark-submit`.
- **The sidecar binary has no `--storage-uri-template` flag** yet, so
  the demo creates its own local Delta table and passes it via
  `UcTableOptions::storage_uri_override`. Real deployments will have
  UC vend `storage_uri` + `storage_credentials` on the bundle.

## Cleaning up

```bash
docker compose down -v        # drop containers and the named caches
docker image rm \
  policast-sidecar:local \
  policast-demo:local \
  policast-shell:local
```
