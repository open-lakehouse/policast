# AGENTS.md — policast-core

## What this crate owns

`policast-core` is the Cedar-to-CEL compiler and portable policy-manifest layer.
It is the source of truth for:

- Parsing Cedar policy text into structured policy objects.
- Translating Cedar EST conditions to CEL expressions.
- Producing and loading `PolicyManifest` JSON.
- Defining policy-store abstractions used by engines.
- Shipping the `policast` CLI for compile and UC flat-file workflows.

## Key files

- `src/cedar_parser.rs`: parses Cedar text into `ParsedPolicy`.
- `src/cel_emitter.rs`: converts Cedar EST JSON nodes to CEL strings.
- `src/policy_manifest.rs`: compiles parsed policies to `CompiledPolicy` rows.
- `src/policy_store.rs`: `PolicyStore` trait plus `FileManifestStore`.
- `src/model.rs`: manifest data model (`CompiledPolicy`, `FilterType`, etc.).
- `src/main.rs`: CLI entrypoint (`compile`, `uc publish`, `uc bind`, `uc diff`).

## Typical workflows

- Compile Cedar files to a manifest:
  - `cargo run -p policast-core -- compile --output manifest.json examples/policies/*.cedar`
- Use legacy positional mode:
  - `cargo run -p policast-core -- --output manifest.json examples/policies/*.cedar`
- Work with UC-style flat files:
  - `cargo run -p policast-core -- uc publish --store-root examples/uc/store examples/policies/*.cedar`

## Testing guidance

- Fast crate tests:
  - `cargo test -p policast-core`
- Optional focused run for compiler internals:
  - `cargo test -p policast-core policy_manifest`

## Editing guardrails

- Keep Cedar-to-CEL translation deterministic; avoid engine-specific behavior here.
- Preserve manifest backward compatibility where possible (`serde` field defaults/optionals).
- If adding policy annotations, update both parsing and compile-time validation paths.
- Prefer table-driven tests for parser/emitter edge cases.
