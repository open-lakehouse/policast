# Unity Catalog policy store — example layout

This directory contains the Delta DDL + seed rows for the three
governance tables described in
[`../../research/unity-catalog-policy-store.md`](../../research/unity-catalog-policy-store.md),
plus a flat-file "store root" that the `policast-uc` sidecar can serve
without a running Unity Catalog.

## Layout

```
examples/uc/
├── README.md                    (this file)
├── ddl/
│   ├── 01_create_catalog.sql    Create governance catalog + schema
│   ├── 02_policies.sql          DDL for governance.policast.policies
│   ├── 03_manifest.sql          DDL for governance.policast.manifest (CDF)
│   ├── 04_bindings.sql          DDL for governance.policast.bindings
│   ├── 05_seed.sql              Seed INSERTs for the healthcare POC
│   └── 06_tags.sql              DDL for governance.policast.tags (CDF)
├── properties/
│   └── patients.properties.json UC table properties overlay
└── store/                       Flat JSON store for the sidecar
    ├── policies.json
    ├── manifest.json
    ├── bindings.json
    └── tags.json                entity -> tag mappings for templates
```

## Using the DDL

Against an actual Unity Catalog + Spark SQL session:

```bash
spark-sql -f examples/uc/ddl/01_create_catalog.sql
spark-sql -f examples/uc/ddl/02_policies.sql
spark-sql -f examples/uc/ddl/03_manifest.sql
spark-sql -f examples/uc/ddl/04_bindings.sql
spark-sql -f examples/uc/ddl/06_tags.sql
spark-sql -f examples/uc/ddl/05_seed.sql
```

### Tag entity grammar

The `governance.policast.tags` table indexes tag assignments on two
kinds of entities:

| `entity_kind` | `entity` shape                                    | Example                                      |
|---------------|---------------------------------------------------|----------------------------------------------|
| `table`       | `catalog.schema.table`                            | `hospital.clinical.patients`                 |
| `column`      | `catalog.schema.table:column`                     | `hospital.clinical.patients:ssn`             |

A single entity may carry multiple tags; a single tag may appear on many
entities. Tags are treated as an unordered set by the resolver. Tag
expressions inside Cedar policies (`@target_tag("...")`,
`@applies_to_tag("...")`) are bare tag names today; future work may
extend the expression grammar without changing the storage shape.

### Templates in the shipped example

The seed data uses Cedar templates at both tag grains — table-level
(`@target_tag`) for a row filter, and column-level
(`@applies_to_tag`) for the column masks:

| Policy id                 | Scope                                          | Expands to (given current tags)                                  |
|---------------------------|------------------------------------------------|------------------------------------------------------------------|
| `row_filter_region`       | `@target_tag("clinical")`                      | `row_filter_region@hospital.clinical.patients`                   |
| `column_mask_by_pii_tag`  | `@applies_to_tag("pii")`                       | `column_mask_by_pii_tag@hospital.clinical.patients:ssn`          |
| `column_mask_by_phi_tag`  | `@applies_to_tag("phi")`                       | `column_mask_by_phi_tag@hospital.clinical.patients:diagnosis`    |

`row_filter_physician` is intentionally left concrete
(`@target_table("patients")`) because its CEL references
`resource.treating_physician`, a column specific to patient-shaped
tables — templating it across the `clinical` tag would evaluate the
predicate against tables that don't have that column. Mixed-style
authoring is the norm: tag-scope what generalizes, hand-author what
doesn't.

Adding a new clinical table or a new sensitive column is a one-row
INSERT into `governance.policast.tags` — no Cedar edit, no bindings
change. The resolver's `expanded_from` audit map preserves the template
lineage of every expanded id, so engine logs and governance dashboards
can trace a concrete rule back to the template + tag that produced it
(for example `row_filter_region@hospital.clinical.patients` maps to
`row_filter_region (target_tag=clinical)`).

## Using the flat-file store (sidecar dev mode)

The `policast-uc-sidecar` binary will serve the resolve endpoint from
the `store/` directory without any Delta or UC dependency. This is the
path exercised by the unit tests and by
`examples/run_datafusion_uc.rs`.

```bash
cargo run -p policast-uc --bin policast-uc-sidecar --features sidecar -- \
    --listen 127.0.0.1:8765 \
    --backend file \
    --store-root examples/uc/store \
    --signing-secret-env POLICAST_UC_SECRET
```

`--backend file` is the default, so the flag can be omitted for
dev-mode runs. The flat-file format is intentionally a 1:1
serialization of the Delta row shapes so that switching from flat
files to Delta later is a pure storage-backend swap.

## Using the Delta-backed store (production mode)

Once the four governance tables under `governance.policast.*` exist as
real Delta tables (whether managed by Unity Catalog OSS or sitting in
an S3-compatible bucket), the same binary can snapshot them on
startup and keep the in-memory view fresh via a periodic refresh
task. Build with the `uc-bootstrap` feature (the Compose image already
does this) and pass `--backend uc-bootstrap`:

```bash
cargo run -p policast-uc --bin policast-uc-sidecar \
    --features sidecar,uc-bootstrap -- \
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

The `{table}` placeholder in `--uc-storage-uri-template` is
substituted with each of `policies` / `manifest` / `bindings` /
`tags`. The `tags` table is treated as optional (matches
`FileBackend`'s treatment of `tags.json`); the other three are
required and the sidecar refuses to start if any is missing.

Every `--uc-refresh-interval-secs` the snapshot is rebuilt. Setting
`--uc-refresh-interval-secs 0` disables the refresh task entirely —
useful for one-shot tests and debugging, **not** for production where
an admin edit to a Cedar policy would stop propagating.

See
[`crates/policast-uc/src/bin/sidecar.rs`](../../crates/policast-uc/src/bin/sidecar.rs)
for the full flag list and
[`crates/policast-uc/src/uc_bootstrap.rs`](../../crates/policast-uc/src/uc_bootstrap.rs)
for the backend implementation.
