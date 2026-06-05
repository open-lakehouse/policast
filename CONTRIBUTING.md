# Contributing to policast

Thanks for your interest in contributing to **policast** — a portable data
governance compiler that turns [Cedar](https://www.cedarpolicy.com/)
authorization policies into [CEL](https://cel.dev/) expressions and enforces
them at query time across Apache DataFusion (Rust) and Apache Spark (Scala).

This document describes how to set up your environment, the conventions we
follow, and how to get a change merged. By participating you agree to abide by
our [Code of Conduct](CODE_OF_CONDUCT.md).

## Table of contents

- [Ways to contribute](#ways-to-contribute)
- [Project layout](#project-layout)
- [Development setup](#development-setup)
- [Branching workflow](#branching-workflow)
- [Testing discipline](#testing-discipline)
- [Authoring Cedar policies](#authoring-cedar-policies)
- [Pull request process](#pull-request-process)
- [Developer Certificate of Origin (sign-off)](#developer-certificate-of-origin-sign-off)
- [License](#license)

## Ways to contribute

- **Report bugs** and **request features** via
  [GitHub Issues](https://github.com/open-lakehouse/policast/issues) using the
  provided templates.
- **Improve documentation** under `docs/` and `research/`.
- **Author or refine governance policies** under `examples/policies/`.
- **Submit code** for the Rust crates or the Scala Spark plugin.

If you are planning a large change, please open an issue first so we can discuss
the design before you invest significant time.

## Project layout

| Path | Language | Purpose |
|------|----------|---------|
| `crates/policast-core/` | Rust | Cedar parser, CEL emitter, `policast` CLI, policy manifest |
| `crates/policast-datafusion/` | Rust | DataFusion `GovernedTable`, CEL→`Expr` compiler, row filters, column masks |
| `crates/policast-uc/` | Rust | Unity Catalog integration / resolver sidecar |
| `policast-spark/` | Scala | Spark plugin + Catalyst rules (separate sbt build, **not** a Cargo workspace member) |
| `examples/policies/` | Cedar/JSON | Sample policies and the compiled `manifest.json` |
| `scripts/` | Shell | `compile-policies.sh`, `ci-smoke.sh` |
| `docs/`, `research/` | Markdown | Documentation and design rationale |

See [`AGENTS.md`](AGENTS.md) for the full architecture overview.

## Development setup

### Prerequisites

- **Rust** (stable; the Docker builder pins `1.90`). Install via
  [rustup](https://rustup.rs/).
- **JDK 17** and **sbt** for the Scala `policast-spark` plugin.
- **Docker** + **Docker Compose** for the end-to-end demos and smoke tests.
- *(Optional)* [`just`](https://github.com/casey/just) to drive the Compose
  stack via the repo `Justfile` (`brew install just` or `cargo install just`).

### Building and running

The Rust workspace builds with cargo directly:

```bash
cargo build --workspace --all-features
cargo test  --workspace --all-features
```

The Scala plugin builds with sbt from `policast-spark/`:

```bash
cd policast-spark
sbt test
sbt assembly   # produces the fat plugin jar
```

The `Justfile` wraps the Dockerized demos and tooling. List everything with
`just`:

```bash
just              # list available recipes
just up           # start the resolver sidecar
just demo         # run the DataFusion demo against the sidecar
just demo-ssn     # contrast admin / physician / analyst masking
just spark-demo   # run the Spark governance demo
just test         # run `cargo test --workspace` inside the dev shell image
just ci-smoke     # full Compose smoke (also run in CI)
```

## Branching workflow

**Never commit directly to `main`.** All work happens on feature branches.

1. Create a branch off the latest `main`.
2. Name it with one of the conventional prefixes:

   | Prefix | Use when |
   |--------|----------|
   | `feat/` | Adding new functionality |
   | `fix/` | Fixing a bug |
   | `chore/` | Build, CI, dependency, or repo upkeep |
   | `refactor/` | Restructuring without behavior change |
   | `test/` | Adding or improving tests only |

   Keep names short, lowercase, and hyphen-separated, e.g.
   `feat/column-lineage-parser`.
3. Keep one logical unit of work per branch.

## Testing discipline

Every change ships with tests — production code is not considered complete
without corresponding coverage.

- **Aim for ~80% coverage** across methods and conditional branches (if/else,
  error paths).
- **Never delete a passing test.** If a test fails after a change, fix the code
  or update the test to match the new correct behavior.
- **Test the happy path *and* error/edge cases** for every exported function.

### Rust

- Use the standard test harness; prefer table-driven tests for functions with
  multiple input scenarios.
- Place tests alongside source (inline `#[cfg(test)]` modules or
  `tests/` integration tests).
- Before pushing, make sure the same checks CI runs pass locally:

  ```bash
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  ```

  Note: clippy is run with `-D warnings`, so warnings fail the build.

### Scala (Spark plugin)

```bash
cd policast-spark
sbt test
```

Do not assert on non-deterministic values (timestamps, random IDs) without
controlling the input, and do not skip flaky tests permanently — fix the root
cause.

## Authoring Cedar policies

Governance rules are authored in Cedar and compiled to a portable CEL manifest.

1. Add or edit `.cedar` files under `examples/policies/`. Use the Cedar
   annotations the compiler understands (`@id`, `@filter_type`,
   `@target_table`).
2. Recompile the manifest. Either run the script directly:

   ```bash
   ./scripts/compile-policies.sh
   ```

   or via Compose:

   ```bash
   just compile
   ```

3. Commit the regenerated `examples/policies/manifest.json` alongside the
   `.cedar` change. **CI fails if the manifest is out of date** (a drift check
   recompiles and diffs it), so never hand-edit the manifest.

## Pull request process

1. Push your feature branch and open a PR against `main`.
2. Fill out the PR template, including the testing checklist.
3. Reference any related issue (e.g. `Closes #123`).
4. Ensure the required CI checks pass. The `rust` and `spark` jobs gate every
   PR; the full Docker `smoke` job is heavier and may run on a label / schedule
   rather than every push (see the workflow and CI docs).
5. A maintainer will review. Address feedback by pushing follow-up commits to
   the same branch.

Keep PRs focused and reasonably small — they are easier to review and merge.

## Developer Certificate of Origin (sign-off)

We ask contributors to certify the origin of their work using the
[Developer Certificate of Origin (DCO)](https://developercertificate.org/).
Add a `Signed-off-by` line to each commit:

```bash
git commit -s -m "feat: add column-lineage parser"
```

This appends `Signed-off-by: Your Name <you@example.com>` using your
`git config user.name` / `user.email`.

> **Maintainer decision (TODO):** DCO sign-off is the proposed model. If the
> project adopts a CLA instead, update this section and add the relevant bot /
> check.

## License

By contributing, you agree that your contributions will be licensed under the
project's [Apache License 2.0](LICENSE), consistent with the rest of the
codebase (`license = "Apache-2.0"` in `Cargo.toml`).
