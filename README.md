# policast-cel

Compile [Cedar](https://www.cedarpolicy.com/) policies into [CEL](https://cel.dev/) (Common Expression Language) expressions for portable data governance across Apache Spark and Apache DataFusion.

## Overview

**policast-cel** bridges two complementary technologies:

- **Cedar** — a policy language designed for authorization, with clear semantics for permit/forbid decisions, ABAC (attribute-based access control), and hierarchical entity models.
- **CEL** — a fast, portable expression language used across Google Cloud, Kubernetes, and other systems for evaluating predicates at runtime.

By authoring governance rules in Cedar and compiling them to CEL, you get:

1. **Single policy source of truth** — write once in Cedar, enforce everywhere.
2. **Portable execution** — CEL expressions run natively on JVM (via [cel-java](https://github.com/google/cel-java)) and Rust (via [cel-rust](https://github.com/cel-rust/cel-rust)).
3. **Engine-agnostic governance** — the same policies apply in Spark (Catalyst optimizer rules) and DataFusion (filter pushdown).

## Architecture

```
Cedar Policy (.cedar)
        │
        ▼
   policast-core (Rust)
   ├── Cedar Parser (cedar-policy crate)
   ├── Cedar EST → CEL Emitter
   └── Policy Manifest (JSON)
        │
        ├──────────────────────┐
        ▼                      ▼
   policast-datafusion     policast-spark
   (Rust)                  (Scala/JVM)
   ├── GovernedTable       ├── PolicastPlugin
   ├── Row filters         ├── Catalyst Rules
   └── Column masks        └── CelEvaluator
```

## Project Structure

```
policast-cel/
├── policast-core/           # Rust: Cedar parser + CEL compiler + PolicyStore trait + CLI
├── policast-datafusion/     # Rust: DataFusion governance integration (+ uc feature)
├── policast-uc/             # Rust: Unity Catalog policy store client + Axum resolver sidecar
├── policast-spark/          # Scala: Spark governance integration
├── examples/
│   ├── policies/            # Cedar policy files
│   │   ├── row_filter.cedar
│   │   ├── column_mask.cedar
│   │   └── deny_legal_hold.cedar
│   ├── uc/                  # Unity Catalog policy store (DDL + flat-file seed)
│   ├── data/
│   │   └── patients.csv     # Sample healthcare data
│   ├── run_datafusion.rs    # DataFusion demo (file manifest)
│   ├── run_datafusion_uc.rs # DataFusion demo (UC resolver → GovernedTable)
│   └── run_spark.scala      # Spark demo
├── docs/
│   ├── delta/overview.md
│   └── unity-catalog/overview.md
├── research/
│   └── unity-catalog-policy-store.md
└── scripts/
    └── compile-policies.sh  # Compile Cedar → manifest.json
```

## Unity Catalog as a Policy Decision Point

`policast-cel` can use Unity Catalog (OSS) itself as the backing store
for Cedar policies, compiled CEL manifests, and principal→policy
bindings. The `policast-uc` crate ships:

- A typed REST client for a `/policies/resolve` endpoint that returns a
  signed `ResolveBundle` containing a compiled `PolicyManifest`,
  identity claims, and short-lived storage credentials.
- An Axum sidecar (`policast-uc-sidecar`) that implements the same
  contract against Delta-backed governance tables (or a flat-file
  store for local dev / tests).
- A `GovernedTable::from_uc` constructor in `policast-datafusion` that
  swaps the manifest source without changing the enforcement core.

See [`research/unity-catalog-policy-store.md`](research/unity-catalog-policy-store.md)
for the full design and [`docs/unity-catalog/overview.md`](docs/unity-catalog/overview.md)
for usage.

## POC: Healthcare Data Governance

The proof-of-concept demonstrates three governance patterns on a `patients` table:

| Policy | Type | Rule |
|--------|------|------|
| `row_filter_region` | Row-filter **template** (`@target_tag("clinical")`) | Analysts see only rows whose `region` matches theirs, on every table tagged `clinical` |
| `row_filter_physician` | Row filter (concrete) | Physicians see only their own patients — kept concrete because the CEL references a patient-specific column |
| `column_mask_by_pii_tag` | Column-mask **template** (`@applies_to_tag("pii")`) | Columns tagged `pii` (e.g. `patients.ssn`) hidden unless role is admin/physician |
| `column_mask_by_phi_tag` | Column-mask **template** (`@applies_to_tag("phi")`) | Columns tagged `phi` (e.g. `patients.diagnosis`) hidden unless role is admin/physician |
| `deny_legal_hold` | Deny override | Records under legal hold blocked for non-legal roles |

Templates are the primary authoring surface at both grains: a table-level
template (`@target_tag`) fans out across every table carrying the named
tag, and a column-level template (`@applies_to_tag`) fans out across
every column carrying the named tag. The Unity Catalog resolver expands
templates at query time into concrete, per-table (or per-column) policies
— adding a new clinical table or a new sensitive column becomes a
one-row INSERT into `governance.policast.tags` rather than a Cedar edit.
Mixed-style authoring is the norm: tag-scope what generalizes
(`row_filter_region`), keep concrete what doesn't (`row_filter_physician`).
See [`examples/uc/README.md`](examples/uc/README.md#templates-in-the-shipped-example)
for the expansion mechanics.

### Sample Cedar Policy

```cedar
@id("row_filter_region")
@filter_type("row_filter")
@target_tag("clinical")
@roles("analyst")
permit (
    principal,
    action == Action::"query",
    resource
)
when {
    resource.region == principal.region
};
```

### Compiled CEL Output

```json
{
  "id": "row_filter_region",
  "effect": "permit",
  "filter_type": "row_filter",
  "target_table": "*",
  "target_tag": "clinical",
  "cel_expression": "(resource.region == principal.region)"
}
```

At resolve time the sidecar expands this to
`row_filter_region@hospital.clinical.patients` (and any other clinical-tagged
table) with the same CEL body, emitting an `expanded_from` audit entry of
`row_filter_region (target_tag=clinical)`.

## Quick Start

### Option A: Docker Compose (recommended)

The repo ships a [docker-compose stack](docker/README.md) that runs the
`policast-uc-sidecar` resolver and a one-shot DataFusion demo that
talks to it over HTTP:

```bash
cp docker/.env.example .env
docker compose up -d sidecar
docker compose --profile demo run --rm datafusion-demo
```

Other profiles: `tools` (recompile Cedar manifest), `shell`
(interactive Rust dev shell), `uc-oss` (staged Unity Catalog OSS
server). See [docker/README.md](docker/README.md) for the full map.

### Option B: Local toolchain

#### Prerequisites

- Rust 1.75+ (for policast-core and policast-datafusion)
- JDK 11+ and sbt 1.10+ (for policast-spark)
- Apache Spark 3.5+ (for running the Spark demo)

### 1. Compile Cedar Policies

```bash
# Build and run the compiler
cargo build --release -p policast-core

# Compile all example policies into a manifest
cargo run --release -p policast-core -- \
    --output examples/policies/manifest.json \
    --verbose \
    examples/policies/row_filter.cedar \
    examples/policies/column_mask.cedar \
    examples/policies/deny_legal_hold.cedar

# Or use the convenience script
./scripts/compile-policies.sh
```

### 2. Run the DataFusion Demo

```bash
cargo run --example run_datafusion -p policast-datafusion
```

### 3. Run the Spark Demo

```bash
# Build the Spark jar
cd policast-spark && sbt assembly

# Run with spark-submit
spark-submit \
    --class com.policast.spark.examples.RunSpark \
    --conf spark.plugins=com.policast.spark.PolicastPlugin \
    --conf spark.sql.extensions=com.policast.spark.PolicastExtensions \
    --conf spark.policast.manifest.path=examples/policies/manifest.json \
    --conf spark.policast.user.role=analyst \
    --conf spark.policast.user.region=us-east \
    target/scala-2.13/policast-spark-assembly-0.1.0.jar
```

## Cedar → CEL Translation

The compiler translates Cedar EST (External Syntax Tree) nodes to CEL:

| Cedar | CEL | Notes |
|-------|-----|-------|
| `==`, `!=`, `<`, `>`, `<=`, `>=` | Same operators | Direct mapping |
| `&&`, `\|\|`, `!` | Same operators | Direct mapping |
| `resource.field` | `resource.field` | Dot access preserved |
| `has resource.field` | `has(resource.field)` | Attribute existence |
| `x like "foo*"` | `x.matches("^foo.*$")` | Wildcard → regex |
| `if c then a else b` | `(c) ? (a) : (b)` | Ternary conditional |
| `x in [a, b]` | `x in [a, b]` | Set membership |
| `when { ... }` | CEL predicate | ANDed together |
| `unless { ... }` | `!(CEL predicate)` | Negated, then ANDed |

## Spark Configuration

| Property | Description | Default |
|----------|-------------|---------|
| `spark.plugins` | Set to `com.policast.spark.PolicastPlugin` | — |
| `spark.sql.extensions` | Set to `com.policast.spark.PolicastExtensions` | — |
| `spark.policast.manifest.path` | Path to compiled policy manifest JSON | `policies/manifest.json` |
| `spark.policast.user.role` | Current user's role | `analyst` |
| `spark.policast.user.region` | Current user's region | — |
| `spark.policast.user.name` | Current user's name | — |

## License

Apache License 2.0
