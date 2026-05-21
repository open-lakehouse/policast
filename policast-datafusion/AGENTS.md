# AGENTS.md — policast-datafusion

## What this crate owns

`policast-datafusion` enforces compiled policies inside DataFusion query plans.
It wraps a `TableProvider` and injects governance behavior at scan time:

- Row filters from CEL (`FilterExec` wrappers).
- Deny overrides as fail-closed row predicates.
- Column masks via projection rewrites.
- Optional Delta Lake and Unity Catalog integrations via feature flags.

## Key files

- `src/governance_table.rs`: `GovernedTable` wrapper and plan rewrites.
- `src/cel_filter.rs`: resolves applicable policies and identity-aware filters.
- `src/cel_to_expr.rs`: CEL AST to DataFusion `Expr` conversion.
- `src/delta.rs`: Delta Lake table loading (`delta` feature).
- `src/uc.rs`: UC-backed policy resolution path (`uc` + `delta` features).
- `tests/integration_test.rs`: end-to-end enforcement behavior tests.

## Feature flags

- `delta`: enables delta-rs table provider support.
- `uc`: enables Unity Catalog resolver integration (requires `delta`).

## Typical workflows

- Run crate tests:
  - `cargo test -p policast-datafusion`
- Run only integration tests:
  - `cargo test -p policast-datafusion --test integration_test`
- Run with Delta support:
  - `cargo test -p policast-datafusion --features delta`

## Editing guardrails

- Keep enforcement fail-closed for parse/eval failures in governance paths.
- Preserve separation: user filters push down to inner table; governance filters are always enforced wrappers.
- When extending CEL support, update both constant-folding and runtime expression paths.
- Add integration coverage for any behavior changes in row filters, masks, or deny overrides.
