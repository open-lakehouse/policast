# Continuous Integration

The CI pipeline lives in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).
It is structured as four jobs with deliberately different cost / trust profiles
so that fast feedback runs on every change while the expensive end-to-end smoke
is opt-in.

## Jobs

| Job | What it does | When it runs | Typical time |
|-----|--------------|--------------|--------------|
| `lint` | `cargo fmt --check` + `cargo clippy --all-features -D warnings` | every push / PR | ~1-3 min |
| `rust` | `cargo build` + `cargo test` (`--workspace --all-features`) + Cedar→`manifest.json` drift check, across a Rust toolchain matrix | every push / PR | ~10-20 min |
| `spark` | `sbt test` + `sbt assembly` of `policast-spark`, across a Spark version matrix; uploads the assembly jar | every push / PR | ~10-20 min |
| `smoke` | full `just ci-smoke` docker-compose end-to-end + the Spark governance demo | **opt-in** (see below) | ~30-90 min |

### `lint` — fast-failing fmt + clippy

`fmt` and `clippy` are split into their own job so a formatting or lint nit fails
in about a minute, in parallel with the slower build/test legs, instead of
blocking behind them.

`clippy` runs with `-D warnings`: **clippy warnings are hard errors.** This is a
deliberate, load-bearing policy — it keeps lint debt from accumulating on `main`.
If a specific lint is wrong for a specific call site, suppress it locally with a
justified `#[allow(...)]` at the narrowest possible scope rather than weakening
the global gate.

### `rust` — toolchain matrix

The `rust` job runs across a toolchain matrix:

- **`stable`** — catches upstream toolchain drift early.
- **`1.96`** — the floor pinned by `docker/Dockerfile` (`RUST_VERSION`). Keeping
  this leg green proves the toolchain baked into the runtime images still builds
  the tree.

Matrix legs are independently visible as separate check runs
(`rust stable (build + test)`, `rust 1.96 (build + test)`). The
`manifest.json` drift check runs once (on the `stable` leg) since the manifest
is toolchain-independent.

> Update the `1.96` entry whenever `docker/Dockerfile`'s `RUST_VERSION` changes,
> so the matrix keeps tracking the image floor.

### `spark` — Spark version matrix

The `spark` job runs across a Spark version matrix. Today only **`4.1.2`** is
supported — it matches `docker/Dockerfile`'s `SPARK_VERSION` and the default in
`policast-spark/build.sbt`. `build.sbt` reads `SPARK_VERSION` from the
environment, so adding a new row to the matrix is enough to build/test against
another (Scala 2.13 / Spark 4.x) line. Each leg uploads a version-suffixed
artifact (`policast-spark-assembly-<version>`).

### `smoke` — full docker end-to-end (advisory, opt-in)

`smoke` runs the whole compose stack (`just ci-smoke`): the resolver sidecar,
the DataFusion demos, Unity Catalog OSS, the `uc-full` MinIO-backed flow, and
the Spark governance demo. It is the most thorough check but also the most
expensive (~30-90 min) and the only one that depends on **external images**
(`newfrontdocker/unitycatalog`, `minio/minio`), which makes it more prone to
flakiness outside our control.

For those reasons it does **not** run on every PR. It triggers on:

| Trigger | Mechanism |
|---------|-----------|
| Push to `main` | `on.push.branches: [main]` |
| Nightly | `on.schedule` cron `0 7 * * *` (07:00 UTC) |
| Manual | `workflow_dispatch` |
| Opt-in on a PR | add the **`run-smoke`** label to the pull request |

#### Opting a PR into the smoke

Add the `run-smoke` label to a PR (create the label once in the repo's
*Issues → Labels* settings if it does not exist). The `pull_request` trigger
listens for the `labeled` event, so adding the label re-runs the workflow and
the `smoke` job's `if:` condition then evaluates true. Remove the label (or just
don't add it) to keep the smoke off for routine PRs.

Two efficiency features that reduce smoke cost:

- **Artifact reuse (no sbt-in-Docker):** the smoke downloads the `spark` job's
  `policast-spark-assembly-<version>` artifact, stages it into the build
  context, and builds the `spark-demo` image with
  `--build-arg SPARK_JAR_STAGE=spark-prebuilt`. That selects the
  `spark-prebuilt` Dockerfile stage and skips the sbt assembly inside Docker.
  The default `SPARK_JAR_STAGE=spark-build` still builds from source, so
  `just spark-demo` works locally without any CI artifact.
- **Docker layer caching:** images are pre-built with
  `docker/build-push-action` using the GitHub Actions cache backend
  (`cache-from`/`cache-to: type=gha`, one scope per image). Compose then reuses
  the loaded images, so `just ci-smoke` performs no uncached builds.

## Branch protection: required vs advisory checks

Recommended policy for the `main` branch:

| Check | Status | Why |
|-------|--------|-----|
| `lint (fmt + clippy)` | **REQUIRED** | Cheap, deterministic, no external deps. |
| `rust stable (build + test)` | **REQUIRED** | Core correctness on the primary toolchain. |
| `rust 1.96 (build + test)` | **REQUIRED** | Guarantees the image's pinned toolchain builds. |
| `spark 4.1.2 (build + test)` | **REQUIRED** | Core correctness of the Spark plugin. |
| `full smoke (docker compose)` | **ADVISORY** | Expensive + depends on external images; runs opt-in / nightly, not on every PR, so it cannot be a required check without blocking routine merges. |

The docker `smoke` is intentionally **advisory**: because it does not run on
every PR (and pulls external images that can rate-limit or change underneath
us), making it a required status check would block merges on a job that is
frequently absent. Treat a red nightly/`run-smoke` smoke as a signal to
investigate, not as a merge gate.

### Applying branch protection

Via the GitHub UI: **Settings → Branches → Branch protection rules → Add rule**,
pattern `main`, enable **Require status checks to pass before merging**, and
select exactly the four REQUIRED checks above. Do **not** add `full smoke
(docker compose)`.

Or with the GitHub CLI (a classic branch-protection rule):

```bash
gh api -X PUT repos/<org>/<repo>/branches/main/protection \
  -H "Accept: application/vnd.github+json" \
  -f 'required_status_checks[strict]=true' \
  -f 'required_status_checks[contexts][]=lint (fmt + clippy)' \
  -f 'required_status_checks[contexts][]=rust stable (build + test)' \
  -f 'required_status_checks[contexts][]=rust 1.96 (build + test)' \
  -f 'required_status_checks[contexts][]=spark 4.1.2 (build + test)' \
  -F 'enforce_admins=true' \
  -F 'required_pull_request_reviews[required_approving_review_count]=1' \
  -F 'restrictions=null'
```

> The context strings must match the job **`name:`** values exactly (including
> the matrix-expanded suffixes). If you add or rename matrix legs in
> `ci.yml`, update the required-checks list to match, or merges will hang
> waiting on a check name that never reports.
