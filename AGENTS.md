# AGENTS.md — policast-cel

## What this project is

**policast-cel** is a portable data governance compiler and enforcement layer for the open lakehouse. It compiles [Cedar](https://www.cedarpolicy.com/) authorization policies into [CEL](https://cel.dev/) (Common Expression Language) expressions, then enforces those expressions at query time across multiple engines — Apache DataFusion (Rust) and Apache Spark (Scala/JVM).

The core thesis: author governance rules once in Cedar, compile to a portable manifest of CEL expressions, and push enforcement into query engine optimizer rules and physical plan wrappers. No policy logic leaks into application SQL.

## Why Cedar

Cedar is an open-source policy language (Rust-native, CNCF Sandbox track) purpose-built for authorization. It was chosen for this project because of properties that matter specifically in a multi-catalog lakehouse:

- **Default deny, forbid overrides permit, order independence** — composable safety guarantees across teams and catalogs.
- **Bounded evaluation, no side effects** — sub-millisecond decisions on the catalog hot path.
- **SMT-based policy analysis** — provable invariants about who can access what, not just unit tests.
- **Reads like SQL** — governance reviewers can audit policies without learning a programming language.
- **Schema validation at authoring time** — typos and type errors caught before deployment.

The full rationale, including a comparison with OPA/Rego and OpenFGA, and a portable `Lakehouse` Cedar schema covering Polaris, Lakekeeper, and Unity Catalog, is in `research/cedar-open-lakehouse.md`.

## Architecture

```
Cedar Policy (.cedar)
        │
        ▼
   policast-core (Rust)
   ├── Cedar Parser (cedar-policy crate, EST)
   ├── Cedar EST → CEL Emitter
   └── Policy Manifest (JSON)
        │
        ├──────────────────────┐
        ▼                      ▼
   policast-datafusion     policast-spark
   (Rust)                  (Scala/JVM)
   ├── GovernedTable       ├── PolicastPlugin
   ├── CEL→Expr compiler   ├── Catalyst Rules
   ├── Row filters         └── CelEvaluator (cel-java)
   ├── Column masks
   └── Delta Lake (delta-rs, feature-gated)
```

### Crate / module layout

| Path | Language | Purpose |
|------|----------|---------|
| `crates/policast-core/` | Rust | Cedar parser, CEL emitter, CLI binary (`policast`), policy manifest |
| `crates/policast-datafusion/` | Rust | DataFusion `GovernedTable` wrapper, CEL→DataFusion `Expr` compiler, row filters, column masks, optional Delta Lake via `delta` feature |
| `policast-spark/` | Scala | Spark 4.1 plugin, Catalyst optimizer rules, CEL evaluator (cel-java), not a Cargo workspace member |
| `examples/` | Mixed | Cedar policies, sample data (`patients.csv`), DataFusion and Spark demo runners |
| `scripts/` | Shell | `compile-policies.sh` convenience script |
| `docs/` | Markdown | Technical documentation (Delta integration, etc.) |
| `research/` | Markdown | Background research and design rationale documents |

### Module-specific agent guides

Use these focused guides when working inside a specific module:

- [`crates/policast-core/AGENTS.md`](crates/policast-core/AGENTS.md)
- [`crates/policast-datafusion/AGENTS.md`](crates/policast-datafusion/AGENTS.md)
- [`policast-spark/AGENTS.md`](policast-spark/AGENTS.md)

### Key dependencies

- **policast-core**: `cedar-policy` (4.x), `serde`, `serde_json`, `clap`, `thiserror`
- **policast-datafusion**: `datafusion` (46), `cel-interpreter`, `cel-parser`, optional `deltalake` (0.25)
- **policast-spark**: Spark 4.1, `cel-java` 0.12, Gson

## Governance model

Three policy types are supported, all authored in Cedar and compiled to CEL:

| Type | Cedar effect | Enforcement |
|------|-------------|-------------|
| **Row filter** | `permit` with `when` clause referencing `resource.*` and `principal.*` | `FilterExec` wrapping the inner scan |
| **Column mask** | `permit` with condition on `principal.role` | `ProjectionExec` replacing column values with `"***"` |
| **Deny override** | `forbid` with `when`/`unless` | Inverted row filter — keeps rows where deny condition is false; exempt users get the filter constant-folded away |

Cedar policy annotations (`@id`, `@filter_type`, `@target_table`) drive the manifest structure. The `PolicyManifest` JSON is the handoff format between the compiler and the engines.

## How the pieces connect

1. **Author** governance in `.cedar` files under `examples/policies/`.
2. **Compile** with `cargo run -p policast-core -- --output manifest.json *.cedar`.
3. **Enforce in DataFusion** — `GovernedTable::new(inner_table, manifest, table_name, identity)` wraps any `TableProvider` and injects filters/masks into the physical plan.
4. **Enforce in Spark** — `PolicastPlugin` + `PolicastExtensions` + Spark conf properties; Catalyst optimizer rule `PolicastRowFilterRule` rewrites logical plans.

## Research and reference material

| Document | Path | Summary |
|----------|------|---------|
| Cedar for the Open Lakehouse | `research/cedar-open-lakehouse.md` | Deep dive on why Cedar fits the lakehouse ecosystem. Covers the PARC model, design decisions (bounded eval, forbid-overrides-permit, SMT analysis), a portable `Lakehouse` Cedar schema spanning Polaris/Lakekeeper/Unity Catalog, integration patterns (embedded PDP, sidecar PDP, compile-to-grants), and an honest trade-off matrix vs OPA and OpenFGA. |
| Delta Lake integration | `docs/delta/overview.md` | How `GovernedTable` wraps Delta Lake tables via delta-rs, physical plan structure, query identity binding. |

## Catalog integration targets

The project is designed to produce governance that works across three catalog systems. Current status:

| Catalog | Native auth model | Cedar on-ramp | Status |
|---------|-------------------|---------------|--------|
| **Apache Polaris** | 2-tier RBAC, OPA hook (1.3+) | Replace OPA PDP with Cedar at the same interface | Designed, not yet implemented |
| **Lakekeeper** | OpenFGA + OPA bridge, pluggable `Authorizer` trait (Rust) | Implement `Authorizer` with `cedar-policy` crate | Natural fit — same language (Rust) |
| **Unity Catalog** | SQL GRANT model, no PDP hook yet | Compile Cedar → GRANT statements, or wrap the REST API | Compile path designed |

## Conventions

- **Branching**: work on feature branches (`feat/`, `fix/`, `chore/`, `refactor/`, `test/`), never commit directly to `main`.
- **Testing**: every code change must be accompanied by tests; target ~80% coverage.
- **Policy files**: Cedar policies live in `examples/policies/`; compiled manifests are JSON.
- **Feature gates**: Delta Lake support is behind the `delta` Cargo feature in `policast-datafusion`.

## Current state

Both completed plan files (`.cursor/plans/`) document the work done so far:

1. **Cedar-to-CEL compiler POC** — Rust core, parser, CEL emitter, manifest, DataFusion and Spark integrations, healthcare demo. All tasks completed.
2. **DataFusion governance + Delta-rs** — real CEL→Expr compilation (replacing string pattern-matching POC), column masks in the physical plan, Delta Lake feature gate. All tasks completed.

## Next likely directions

Based on the research in `research/cedar-open-lakehouse.md` and the current codebase:

- **Catalog PDP integration** — implement Cedar as an embedded PDP for Lakekeeper (`Authorizer` trait) or as a sidecar PDP for Polaris (replacing OPA).
- **Portable Lakehouse schema** — adopt the `Lakehouse` Cedar namespace from the research doc as the canonical schema, mapping actions across Polaris/Lakekeeper/Unity Catalog.
- **ABAC and context-aware policies** — extend the `QueryIdentity` / request context to carry MFA, network, purpose attributes for richer policy evaluation.
- **Compile-to-grants** — build a Cedar→SQL GRANT compiler for Unity Catalog integration.
- **SMT-based policy analysis** — integrate `cedar-policy-symcc` to prove invariants about the policy set before deployment.
- **Batch authorization** — leverage `is_authorized_batch` and targeted partial evaluation for list operations.
