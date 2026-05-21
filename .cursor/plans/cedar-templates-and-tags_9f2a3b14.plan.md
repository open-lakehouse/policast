---
name: cedar-templates-and-tags
overview: Make tags the primary governance authoring surface for policast-cel. Teach policast-core to recognize `@target_tag`/`@applies_to_tag` Cedar annotations, teach the Unity Catalog resolver to expand tag-scoped policies into concrete (table, column) bindings via a new `governance.policast.tags` table, wire an end-to-end `docker-compose` stack with UC 0.4.1, ship a Cursor skill that drives column/table tagging from a live UC schema, and capstone with SMT-backed invariant proofs via cedar-policy-symcc.
todos:
  - id: core-tag-model
    content: "Extend CompiledPolicy in policast-core/src/model.rs with optional target_tag (table-level) and applies_to_tag (column-level) fields; serialize as tag_expression strings so the shape is forward-compatible with AND/OR/NOT tag algebras"
    status: completed
  - id: core-tag-annotations
    content: "Parse @target_tag and @applies_to_tag annotations in policast-core/src/policy_manifest.rs::compile_single_policy; validate that at least one of (target_table, target_tag) is set and (column, applies_to_tag) are mutually exclusive"
    status: completed
  - id: core-tag-tests
    content: "Table-driven tests for tag annotations: target_tag only, applies_to_tag only, both, neither (backward compat), mixed with existing target_table/column, JSON roundtrip"
    status: completed
  - id: uc-tag-schema
    content: "Add ddl/06_tags.sql creating governance.policast.tags (entity STRING, entity_kind STRING, tag STRING, set_by STRING, set_at TIMESTAMP) with Delta + CDF; document the grammar in examples/uc/README.md"
    status: completed
  - id: uc-tag-backend
    content: "Add TagRow to policast-uc::backend, teach FileBackend to read tags.json; seed examples/uc/store/tags.json and examples/uc/ddl/05_seed.sql. (DeltaBackend split into stage-3 production-backend TODO; see uc-production-backend.)"
    status: completed
  - id: uc-tag-expansion
    content: "In policast-uc::store::ResolverCore::resolve, after binding selection, expand tag-scoped policies: for each CompiledPolicy with target_tag/applies_to_tag, look up matching entities in the tag index and emit one concrete CompiledPolicy per match with target_table/column filled in; preserve the tag expression in a new ResolveBundle.expanded_from audit map for tracebility. Also extend PolicyRow with target_tag/applies_to_tag columns and update ddl/02_policies.sql."
    status: completed
  - id: uc-tag-expansion-tests
    content: "Unit tests: single-tag expansion, multi-match expansion, no-match (policy should be dropped not errored), interaction with role filtering, binding precedence still honored, bundle signature still verifies, retired tags ignored, mixed concrete+template batches"
    status: completed
  - id: examples-templates
    content: "Rewrite examples/policies/column_mask.cedar as Cedar templates using @applies_to_tag(\"pii\") and @applies_to_tag(\"phi\") so the two per-column forbid rules become one template each; examples/run_datafusion_uc.rs now shows the templates expanding to column_mask_by_pii_tag@hospital.clinical.patients:ssn and column_mask_by_phi_tag@hospital.clinical.patients:diagnosis end to end. Row-filter-by-tag templates deferred to a follow-up (they don't reduce policy count in the current shipped example — row filters are already role-specific, not column-specific)."
    status: completed
  - id: examples-templates-row-filter
    content: "Follow-up to examples-templates: tag-scope row_filter_region via @target_tag(\"clinical\") so the analyst regional-isolation rule applies to every clinical-tagged table, not just patients. Keep row_filter_physician concrete as a mixed-style example (its CEL references a patient-specific column). Add a second equivalence test (test_template_and_concrete_paths_are_equivalent_for_row_filter_region) pinning that the template path preserves (target_table, cel, effect, applies_to.roles) on the patients resolve, and update the Spark bundled manifest and docs to match."
    status: completed
  - id: examples-seed-tags
    content: "Seed ssn->pii, diagnosis->phi, patients->clinical in both flat (tags.json) and Delta (05_seed.sql) stores; update examples/uc/store/{policies,manifest,bindings}.json to use the two templates, update patients.properties.json applied_policies list, update Spark's bundled resources manifest to show the expanded-id form, add an equivalence test proving the template path emits the same (target_table, column, filter_type, cel, effect) set that the pre-template concrete policies did."
    status: completed
  - id: uc-production-backend-decision
    content: "Lock in UcBootstrapBackend as the Stage 3 target (snapshot via UC REST at startup, tail manifest/tags CDF for freshness). See plan Design section for the trade-off analysis versus DeltaBackend (single-tenant fallback) and UcRestBackend (deferred to a later phase for multi-tenant credential vending)."
    status: completed
  - id: uc-bootstrap-backend-scaffold
    content: "Add policast-uc/src/uc_bootstrap.rs with a UcBootstrapConfig (UC endpoint, governance catalog/schema names, refresh interval) and a UcBootstrapBackend struct implementing ResolveBackend. This stage ships the scaffold only: every method returns UcError::Config(\"not yet wired\"); snapshot and CDF tail land as follow-up commits. Gate any transport deps behind a new `uc-bootstrap` feature so `cargo test --workspace` stays green on the default feature set. (Shipped in 3d3179a.)"
    status: completed
  - id: uc-bootstrap-snapshot
    content: "Implement startup snapshot: UcBootstrapBackend::bootstrap(cfg) opens the four governance Delta tables (policies, manifest, bindings, tags) via deltalake + a static storage_uri_template + storage_options, converts each Arrow RecordBatch into the PolicyRow/ManifestRow/BindingRow/TagRow types, and stashes them in four RwLock<Vec> caches. The uc-bootstrap feature gates the heavy dep fan-out (deltalake + datafusion); default cargo test --workspace stays lightweight. tags remains optional so legacy deployments without the tag index still bootstrap. Tests seed local Delta fixtures with deltalake::CreateBuilder + WriteBuilder and round-trip all four tables (happy path + missing-required + missing-optional + refresh-appends). UC-REST credential vending is a follow-up (uc-bootstrap-credentials) that swaps the static config for per-table credentials without changing the ResolveBackend impl."
    status: completed
  - id: uc-bootstrap-credentials
    content: "UcBootstrapBackend now resolves governance-table access in dual mode: if `storage_uri_template` is set it keeps the static MinIO/local path, otherwise it uses UC REST per table (`GET /api/2.1/unity-catalog/tables/{full_name}` for storage location plus `POST /api/2.1/unity-catalog/temporary-table-credentials` for short-lived creds) and maps vended values into delta-rs storage options (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, plus arbitrary credential key/value passthrough). Sidecar wiring now exposes `--uc-bearer-token-env` and no longer requires `--uc-storage-uri-template` in uc-bootstrap mode. Includes a fake-UC integration test (`test_bootstrap_can_resolve_table_access_via_uc_rest`) plus option-injection unit coverage, while preserving static-template compatibility for uc-full compose demos."
    status: completed
  - id: uc-bootstrap-cdf-tail
    content: "Periodic refresh task: when cfg.refresh_interval.is_some(), bootstrap() spawns a tokio task that sleeps for the interval and calls refresh_snapshot() on each tick; failures log + keep the previous snapshot so a transient UC/MinIO hiccup does not take governance offline. bootstrap_with_invalidation() wires an InvalidationSender so every successful refresh fans InvalidateAll out to the resolver's bundle cache, keeping the LRU in lock-step with governance mutations. AbortHandle-on-Drop guarantees no leaked workers when the last Backend clone goes away. Ships 4 new tests: no task when interval is None, task picks up appended rows within a few intervals, bundle-cache invalidation fanout, drop-aborts-task. Scope call: implementation is a full re-scan, not a CDF diff — the Arrow column set is tiny (tens of rows per table in a typical deployment) so the scan cost is negligible, and a full re-scan is simpler to reason about than a per-row diff. Upgrading to a true CDF diff is a future optimization if latency becomes a bottleneck."
    status: completed
  - id: uc-bootstrap-sidecar-wiring
    content: "Sidecar binary (policast-uc/src/bin/sidecar.rs) now accepts --backend {file,uc-bootstrap}; defaults to file so existing invocations keep working. When --backend uc-bootstrap is selected it requires --uc-storage-uri-template (with a literal {table} placeholder) and accepts repeated --uc-storage-option K=V pairs that flow through delta-rs's object_store backend (MinIO, S3, ADLS, GCS). --uc-refresh-interval-secs (default 30) drives the refresh task; 0 disables it. Also exposes a uc_bootstrap_sidecar() Router constructor in src/sidecar.rs for in-process wiring, covered by a new roundtrip test (test_uc_bootstrap_sidecar_roundtrip) that builds hand-rolled Delta fixtures in a tempdir and verifies the resolve contract end-to-end. Dockerfile's sidecar-build stage now builds with --features sidecar,uc-bootstrap so the shipped image supports both backends. examples/uc/README.md and docker/README.md have new sections covering the production-mode invocation."
    status: completed
  - id: compose-sidecar-dockerfile
    content: "Multi-stage docker/Dockerfile producing policast-sidecar:local (runtime), policast-demo:local (run_datafusion_uc_http example), and policast-shell:local (interactive dev). Pinned Rust 1.90 builder, optional .cargo/config.toml for corporate mirrors, healthcheck on /health. (Shipped in 447a8ab via the docker-compose-stack merge.)"
    status: completed
  - id: compose-stack-base
    content: "Base docker-compose.yml with sidecar (FileBackend against the mounted ./examples/uc/store), datafusion-demo (profile=demo), compile (profile=tools), df-shell (profile=shell), and a staged unitycatalog service (profile=uc-oss). Shipped in 447a8ab via merge and verified end-to-end against the tag-expanded policies in 7f84d9b."
    status: completed
  - id: compose-stack-uc-oss
    content: "compose-stack now includes a full UC profile behind `--profile uc-full`: MinIO (`policast-demo` bucket), idempotent `minio-init`, Unity Catalog pinned to `newfrontdocker/unitycatalog:v0.4.1` (also still available under profile=uc-oss), a one-shot `uc-bootstrap` init container, and a `sidecar-uc-full` service running `--backend uc-bootstrap`. Bootstrap validates/discovers `examples/uc/ddl/*.sql` inputs and publishes the shipped template manifest + bindings + tag rows into MinIO-backed governance Delta tables (`policies`, `manifest`, `bindings`, `tags`) via the new `policast-uc-seed` binary. The pre-existing flat-file sidecar flow remains unchanged (`sidecar` service still defaults to FileBackend on `:8765`), while uc-full publishes on a separate host port (`POLICAST_UC_FULL_PORT`, default 8766) to avoid collisions."
    status: completed
  - id: compose-datafusion-demo
    content: "examples/run_datafusion_uc_http.rs talks to the sidecar over HTTP, resolves via UC-style three-part names, and registers a GovernedTable under the short name the Cedar column_mask policies reference. Compose runs it as the datafusion-demo service. (Shipped in 447a8ab.)"
    status: completed
  - id: compose-just-targets
    content: "Justfile recipes for the DX loop: init, up, down, clean, logs, ps, health, demo, demo-analyst, demo-physician, demo-admin, demo-ssn, compile, shell, run, test, uc-oss-up/-down, build (+rebuild). (Shipped in 447a8ab; demo-ssn comment updated for the tag-template world in 7f84d9b.)"
    status: completed
  - id: compose-just-uc-full
    content: "`Justfile` now includes `just uc-full-up` (bring up `minio`, `minio-init`, `unitycatalog`, `uc-bootstrap`, and `sidecar-uc-full`; then wait for sidecar-uc-full health), `just uc-full-demo` (run the existing DataFusion demo against `http://sidecar-uc-full:8765`), and `just uc-full-down` (remove uc-full services without touching the base file-backed sidecar flow). Existing file-backed recipes are unchanged."
    status: completed
  - id: compose-docs
    content: "docker/README.md walks the default sidecar-only flow, the `demo-ssn` contrast across admin/physician/analyst, the compile-tools service, the interactive shell, the staged UC OSS service, and known limitations. Now also points at the UcBootstrapBackend scaffold + Stage-3 TODOs. (Shipped in 447a8ab; refreshed in 7f84d9b.)"
    status: completed
  - id: compose-docs-uc-full
    content: "Added docs/unity-catalog/compose.md with a copy-paste uc-full walkthrough: MinIO boot, UC 0.4.1 boot, bootstrap container, sidecar on `--backend uc-bootstrap`, `just uc-full-demo`, teardown, and troubleshooting. Cross-linked from docs/unity-catalog/overview.md and docker/README.md."
    status: completed
  - id: skill-tag-columns
    content: "Add .cursor/skills/tag-columns/SKILL.md: given a UC table name (or a flat-file schema), walk the data owner column-by-column through the canonical tag vocabulary (pii, phi, financial, clinical, public), emit either a tags.json edit (flat-file mode) or a single MERGE against governance.policast.tags (UC mode), and run a coverage check to point at — or draft — a matching Cedar template. Ship a companion tag-vocabulary.md reference that pins ownership + retirement semantics for each tag."
    status: completed
  - id: skill-templates
    content: "Ship starter Cedar templates in .cursor/skills/tag-columns/templates/ (column_mask_by_pii.cedar, column_mask_by_phi.cedar, column_mask_by_financial.cedar, row_filter_by_clinical.cedar) so the skill's Step 5 coverage check can offer a one-click 'author a template that covers this tag' action without regenerating Cedar from first principles each time."
    status: completed
  - id: skill-fixture
    content: "Golden-file test that the skill produces the expected tag diff + template set when fed the hospital.clinical.patients schema; run it from CI as a regression guard"
    status: pending
  - id: symcc-feature
    content: "Add an optional 'symcc' Cargo feature to policast-core that pulls cedar-policy-symcc; scaffold policast analyze --invariants <file>.cedar subcommand"
    status: pending
  - id: symcc-invariants
    content: "Define the first 2-3 invariants in Cedar: (a) no non-physician principal observes a pii-tagged column, (b) legal_hold forbid always dominates row_filter_region, (c) tag expansion is monotonic (adding a tag only adds masks); prove them against the compiled + expanded manifest"
    status: pending
  - id: symcc-ci
    content: "Wire cargo run -p policast-core --features symcc -- analyze into CI as a gate; on failure, surface a minimal counter-example principal"
    status: pending
  - id: symcc-research-note
    content: "Add research/tag-driven-cedar-templates.md documenting the template+tag model, the expansion algorithm, the SMT invariants, and the comparison with Snowflake Horizon / UC on Databricks / BigQuery policy tags"
    status: pending
isProject: false
---

# Tag-Driven Cedar Templates for the Open Lakehouse

## Why now

`policast-cel` already proves that Cedar authored policies can be compiled to CEL and enforced portably across DataFusion and Spark. The remaining gap between "works on the healthcare demo" and "works on an enterprise catalog with thousands of tables" is the *authoring surface*: today every column mask and row filter names its target table and column literally, which means N policies per catalog and a linear maintenance cost as tables are added.

Every major commercial catalog (Snowflake Horizon, Unity Catalog on Databricks, BigQuery) has converged on the same primitive to solve this — **tags** — driving masking and row-filter decisions through a small, reviewable set of templates that attach to tag expressions rather than to individual securables. Cedar has a matching primitive (policy templates with slot variables), but no open-source lakehouse project currently drives it from catalog tags.

This plan closes that gap. When it is complete, policast-cel will:

- Accept Cedar *templates* as the primary authoring artifact.
- Use UC-stored tags as the binding surface between templates and concrete `(table, column)` pairs.
- Ship as a runnable `docker compose` stack so the story is reproducible in one command.
- Offer a Cursor skill that turns "what is this column?" into a tag edit, collapsing the learning curve to policy authoring.
- Prove invariants about the whole setup with SMT, so a governance reviewer can rely on machine-checked guarantees instead of code review vibes.

## Current state

| Layer | What exists | What is missing |
|-------|-------------|-----------------|
| `policast-core` | Cedar parser, CEL emitter, `CompiledPolicy { target_table, column, target_tag, applies_to_tag, cel_expression, ... }` model, `@target_tag` / `@applies_to_tag` annotation parsing, JSON manifest | SMT analysis hooks (Stage 5) |
| `policast-uc` | Resolver core with pluggable `ResolveBackend`; `FileBackend` (flat JSON); tag expansion; signed `ResolveBundle`; HTTP sidecar; `UcBootstrapBackend` with Delta-backed snapshot loader, dual access resolution (static template or UC REST table+temp-creds vending), RwLock row caches, periodic refresh task with optional bundle-cache invalidation fanout, and drop-aborts-task lifecycle (feature = `uc-bootstrap`) | Optional hardening: credential expiration-aware cache + proactive re-vend before expiry |
| `examples/` | Healthcare demo (template-based), `run_datafusion_uc.rs`, `run_datafusion_uc_http.rs` (verified against the live sidecar), UC DDL + flat store, tag seeds | A UC-backed twin of `run_datafusion_uc_http` that reads the patients table from MinIO |
| Compose stack | `docker/Dockerfile` (sidecar + demo + shell targets), `docker-compose.yml` includes `--profile uc-full` (MinIO + minio-init + UC v0.4.1 + uc-bootstrap init + sidecar-uc-full on `--backend uc-bootstrap`) while preserving the default flat-file flow, `docker/bootstrap/uc-full-init.sh` (DDL discovery + governance Delta seed), `Justfile` includes `uc-full-up / uc-full-demo / uc-full-down`, docs at `docs/unity-catalog/compose.md` + links from overview and docker/README | Optional hardening: run DDL against UC REST/SQL directly during bootstrap and add a live tag-edit smoke test in CI |
| `docs/` | `delta/overview.md`, `unity-catalog/overview.md` + `unity-catalog/compose.md`, `docker/README.md`, and `research/smt-invariants.md` (formalized invariant set) | Tag authoring guide polish + future `policast analyze` implementation notes |
| `.cursor/skills` | `tag-columns` skill + 4 starter templates (pii, phi, financial, clinical) + `policy-qa-generator` skill for interview-driven Cedar authoring | Skill fixture tests (optional hardening) |

```mermaid
flowchart LR
  subgraph today [Per-table authoring]
    TmplOld["column_mask_ssn (patients.ssn)\ncolumn_mask_diagnosis (patients.diagnosis)\nrow_filter_region (patients)"]
    TmplOld --> Man1["PolicyManifest"]
    Man1 --> Eng1["DataFusion / Spark"]
  end

  subgraph target [Tag-driven authoring]
    TmplNew["template: mask_by_tag(PII)\ntemplate: mask_by_tag(PHI)\ntemplate: row_filter_by_tag(clinical)"]
    Tags["governance.policast.tags\nssn->pii, diagnosis->phi, patients->clinical"]
    TmplNew --> Resolver["ResolverCore.resolve\n(tag expansion)"]
    Tags --> Resolver
    Resolver --> Man2["PolicyManifest (concrete)"]
    Man2 --> Eng2["DataFusion / Spark (unchanged)"]
    Man2 -.-> Smt["cedar-policy-symcc\ninvariants"]
  end
```

## Design

### Data model (item 1)

`CompiledPolicy` gains two optional string fields:

```rust
pub struct CompiledPolicy {
    pub id: String,
    pub effect: Effect,
    pub filter_type: FilterType,
    pub target_table: String,          // may be "*" when target_tag is set
    pub column: Option<String>,        // None when applies_to_tag is set
    pub target_tag: Option<String>,    // NEW: table-level tag expression
    pub applies_to_tag: Option<String>,// NEW: column-level tag expression
    pub cel_expression: String,
    pub applies_to: Option<AppliesTo>,
    pub description: Option<String>,
}
```

The tag expression is a plain string for v1 (a single tag name). The plan explicitly *does not* ship a tag algebra in this phase — `"pii"`, not `"pii AND NOT public"` — because the grammar interacts with SMT analysis and we want to lock the storage shape first. Future work can extend the string into an expression without breaking the manifest contract.

Cedar annotations:

```cedar
@id("mask_pii_non_clinical")
@filter_type("column_mask")
@applies_to_tag("pii")              // NEW: expand per column tagged pii
@applies_to_roles(["analyst","intern"])
forbid (principal, action, resource)
when { principal.role != "physician" };
```

A Cedar policy must specify exactly one of `(target_table, target_tag)` and at most one of `(column, applies_to_tag)`. `compile_single_policy` enforces that.

### Tag index (item 2)

New Delta table under the `governance.policast` schema:

```sql
CREATE TABLE governance.policast.tags (
    entity       STRING NOT NULL,  -- 'catalog.schema.table' or 'catalog.schema.table:column'
    entity_kind  STRING NOT NULL,  -- 'table' | 'column'
    tag          STRING NOT NULL,
    set_by       STRING NOT NULL,
    set_at       TIMESTAMP NOT NULL,
    retired_at   TIMESTAMP
) USING DELTA
  PARTITIONED BY (tag)
  TBLPROPERTIES ('delta.enableChangeDataFeed' = 'true');
```

Matching `TagRow` in `policast-uc::backend`, plus `tags.json` in the flat store for tests and examples.

The resolver's `ResolveBackend` trait gains:

```rust
async fn tags(&self) -> Result<Vec<TagRow>, UcError>;
```

with a default `Ok(Vec::new())` implementation so older backends don't break.

### Tag expansion (item 3)

In `ResolverCore::resolve`, after the existing binding filter produces the candidate `CompiledPolicy` list, a new expansion step:

```text
for each policy in candidates:
    if policy.target_tag is Some or policy.applies_to_tag is Some:
        entities = tag_index.lookup(policy.target_tag or policy.applies_to_tag)
        for each entity in entities:
            emit a concrete policy with target_table/column filled in
            (and target_tag/applies_to_tag cleared so engines never see them)
    else:
        emit policy as-is
```

The expansion runs server-side, inside the sidecar, before the bundle is signed. Engines therefore see exactly the same `PolicyManifest` shape they see today — *no changes to `policast-datafusion` or `policast-spark` in this plan at all*. That is the key architectural property: tags are a publisher-side concept.

The `ResolveBundle` gains an `expanded_from` map (policy_id → tag_expression) for audit/debug purposes; it is signed alongside the manifest.

### Production backend for the sidecar (Stage 3 — decision locked)

`FileBackend` is the only `ResolveBackend` that exists today as a
fully-wired implementation. The compose demo in stage 3 cannot ship
on `FileBackend` forever — running UC 0.4.1 next to the sidecar only
tells a half-story if the governance state still lives in a JSON blob
mounted into the sidecar image.

Three shapes were evaluated:

| Shape | What reads the four governance Delta tables | Storage creds | When to pick it |
|-------|---------------------------------------------|---------------|-----------------|
| **`DeltaBackend` (direct)** | `deltalake` crate inside the sidecar process | held by the sidecar | single-tenant deployment where sidecar and UC share a trust boundary |
| **`UcRestBackend`** | UC REST API resolves the four tables, vends credentials, then `deltalake` opens them | vended per-call by UC | multi-tenant UC deployments; keeps UC on the credential vending path (the project's thesis) |
| **`UcBootstrapBackend`** | UC REST at startup snapshots rows; then a CDF listener on `manifest` and `tags` invalidates cached rows | vended once at startup, refreshed on CDF events | simplest operationally; best fit for the compose demo |

**Decision: ship `UcBootstrapBackend`.** Rationale:
1. *Respects the UC-as-PDP thesis.* Credentials for the four
   governance tables are vended by UC at startup — the sidecar never
   has an out-of-band path to the storage bucket. This is the same
   guarantee that `GovernedTable` already relies on for user data.
2. *Performance story is tractable.* Resolve-path reads hit an
   in-memory snapshot, not UC, so resolve latency stays at flat-file
   speed. UC is only in the slow path at startup and on CDF deltas.
3. *Freshness is explicit.* CDF on `manifest` and `tags` gives a
   clear, auditable invalidation signal. No TTL guessing, no
   full-table refetch cadence — the snapshot moves forward only when
   a governance admin commits a Delta change.
4. *Smallest surface to ship for the compose demo.* Exactly two
   things to build: the UC REST snapshot loader and the CDF tail
   loop. `UcRestBackend` adds a credential-vending interceptor on
   every resolve; `DeltaBackend` adds bucket-credentials management
   to the sidecar. Both are larger than what we need.

`UcRestBackend` is the next backend to land once the compose demo
validates the freshness model — it is the right long-term shape for
multi-tenant UC deployments where per-principal credential scoping
matters. `DeltaBackend` is documented as a single-tenant fallback
but deliberately not featured.

All three implement the `ResolveBackend` trait and therefore benefit
from the `tags()` default-impl contract already in place: adding any
of them does not break the others, and the tag-expansion algorithm
below is agnostic to which one is wired in.

### Docker-compose stack (items 4–7)

One command to stand up:

```
compose/
├── docker-compose.yaml
├── minio/   (bucket = policast-demo)
├── unitycatalog/  (v0.4.1 config + conf dir)
├── bootstrap/     (SQL + policy publish init container)
└── sidecar/       (policast-uc Dockerfile)
```

Bringup order: `minio` → `unitycatalog` → `bootstrap` (one-shot, exits 0) → `policast-uc-sidecar`. The DataFusion demo runs as a separate `just uc-compose-demo` command against the running stack.

Bootstrap installs the healthcare demo end-to-end:
1. Creates catalog/schema/volume via UC.
2. Materializes `hospital.clinical.patients` as a Delta table on MinIO.
3. Runs all `examples/uc/ddl/*.sql`.
4. Runs `policast uc publish` with the *template* policies.
5. Seeds the tags table with `ssn→pii, diagnosis→phi, patients→clinical`.

### Cursor skill (items 8–10)

`.cursor/skills/tag-columns/SKILL.md` activates on prompts like "help me tag columns in `hospital.clinical.patients`" or "what policies apply to this table?". The skill:

1. Reads column metadata via UC REST (or the flat store when `UC_ENDPOINT` is unset).
2. Walks the user column-by-column, asking a narrow classification question (`pii | phi | financial | public`).
3. Emits a unified diff for `tags.json` (flat mode) or a `MERGE` against `governance.policast.tags` (Delta mode).
4. Offers to link a canonical template from `skills/tag-columns/templates/` if no policy yet covers the tag.

A golden-file test under `tests/skill_tag_columns/` pins the expected output for the `patients` schema.

### SMT invariants (items 11–14)

The `policast-core` crate gains an optional `symcc` feature pulling `cedar-policy-symcc`. A new `policast analyze` subcommand reads the *expanded* manifest (the sidecar can dump it with `/admin/dump`) plus an invariants file and reports `holds` / `counterexample`.

Three invariants ship with the plan:

| Invariant | Statement |
|-----------|-----------|
| `pii_requires_physician` | ∀ principal, resource: if a column is tagged `pii` and principal.role ≠ `physician`, then the column_mask fires |
| `legal_hold_dominates_region` | ∀ principal, resource: the `deny_legal_hold` forbid overrides every `row_filter_region` permit |
| `tag_expansion_monotonic` | Adding a tag to a column only restricts visibility, never widens it |

CI runs the analyzer on every push; a failing invariant fails the build with the counter-example attached as an annotation.

## Execution order

The plan is deliberately layered so each stage leaves `main` in a shippable state:

1. **Core model + annotations** (items in sub-plan #1) — merges alone, with no behavior change because no resolver yet expands tags; existing manifests still compile; new fields are optional.
2. **UC tag backend + expansion** (items in sub-plan #2) — adds the tag table, the backend, the expansion; the healthcare demo starts using templates.
3. **Docker-compose stack** (items in sub-plan #3) — reuses `feat/docker-compose-stack`'s in-flight work; merges on top of #1+#2 so the demo shows tag expansion working end-to-end.
4. **Cursor tagging skill** (items in sub-plan #4) — depends on the tag index being real; can land in parallel with #3.
5. **SMT invariants** (items in sub-plan #5) — capstone; depends on the expanded manifest shape being stable.

Each stage will get its own `.cursor/plans/*.plan.md` with detailed TODOs. This document is the epic tracker; it links to the sub-plans as they are created.

## Non-goals for this epic

- **No tag algebra.** `target_tag` is a bare tag name for v1. Expressions like `pii AND NOT public` are explicitly deferred.
- **No engine-side tag awareness.** `policast-datafusion` and `policast-spark` do not learn about tags. Expansion happens in the publisher/resolver only.
- **No automatic tag classification.** The Cursor skill asks the user; it does not run an ML model over column contents.
- **No tag lifecycle / governance workflow.** Retiring a tag, approvals on a tag change, and tag access control itself are out of scope (they reuse UC's existing permission model).

## Success criteria

- `cargo test --workspace` is green after each stage.
- `just uc-compose-demo` prints the same governed output it does today, but the underlying policies are 3 templates instead of 6 hand-written rules.
- Adding a new clinical table to the compose stack (plus a tag assignment) automatically inherits `pii`/`phi` masks with zero policy edits.
- `policast analyze` passes the three shipped invariants and rejects a deliberately-broken policy that violates one of them.
- A data owner can go from "here is a new table" to "it is governed" using only the Cursor skill plus a template.
