# GovernedTable with Unity Catalog

## What it does

`GovernedTable::from_uc` turns Unity Catalog (OSS) into a Policy Decision
Point (PDP) for Cedar/CEL governance. Instead of reading a local JSON
manifest, the engine asks UC — or a sidecar that speaks UC's contract —
to resolve `(principal, table)` into a signed bundle containing the
compiled CEL manifest, the identity claims, and the storage credentials
needed to open the underlying Delta table.

Policies remain authored in Cedar and compiled by `policast-core`. The
only thing that changes versus `open_governed_delta_table` is the
*source* of the manifest: UC now holds policies, bindings, and identity
claims inside its own Delta metastore.

For the local end-to-end Compose flow (MinIO + UC OSS + Delta-backed
sidecar), see [compose.md](./compose.md).

## Architecture

```
                  Cedar policies
                       │
                       ▼
               ┌───────────────┐
               │ policast-core │  Cedar → CEL compilation
               └───────┬───────┘
                       │ PolicyManifest (JSON)
                       │
                       ▼          policast uc publish
            ┌────────────────────────────────┐
            │ Unity Catalog (control plane)  │
            │                                │
            │  governance.policast.policies  │  Delta, versioned
            │  governance.policast.manifest  │  Delta + CDF
            │  governance.policast.bindings  │  Delta
            │  volume: governance.policast.raw│ Cedar sources
            │                                │
            │      /policies/resolve         │  REST endpoint
            └───────────────┬────────────────┘
                            │ ResolveBundle
                            │ (manifest + creds + signature)
                            ▼
               ┌──────────────────────────┐
               │ policast-uc              │  Rust client
               │ UnityCatalogPolicyStore  │  LRU + TTL cache
               └───────────┬──────────────┘
                           │
                           ▼
               ┌──────────────────────────┐
               │ policast-datafusion      │
               │ GovernedTable            │  Same FilterExec +
               │                          │  ProjectionExec path
               └──────────────────────────┘
```

The enforcement core (`GovernedTable`, `FilterExec` wrappers,
`ProjectionExec` masks) is unchanged. Only the *source* of the
`PolicyManifest` is different.

## Governance enforcement

Row filters, column masks, and deny overrides are enforced exactly as
documented in [../delta/overview.md](../delta/overview.md). Everything in
that document still applies; this page only covers how the manifest gets
from UC to the engine.

## Using Unity Catalog

### Enable the feature

The UC integration is behind the `uc` Cargo feature on
`policast-datafusion`:

```toml
[dependencies]
policast-datafusion = { path = "policast-datafusion", features = ["uc", "delta"] }
```

This pulls in the `policast-uc` crate (REST client + cache + signature
verification) and re-exports `GovernedTable::from_uc`.

### Publish Cedar policies into UC

```bash
policast uc publish \
    --endpoint http://localhost:8765 \
    --signing-secret-env POLICAST_UC_SECRET \
    examples/policies/row_filter.cedar \
    examples/policies/column_mask.cedar \
    examples/policies/deny_legal_hold.cedar
```

This compiles the Cedar sources, produces a `PolicyManifest`, and posts
it to the resolver's `/admin/publish` endpoint which MERGEs into
`governance.policast.policies` and `governance.policast.manifest`.

### Bind a policy to a table

```bash
policast uc bind \
    --endpoint http://localhost:8765 \
    --policy row_filter_region \
    --target hospital.clinical.patients \
    --principal-selector 'role:analyst'
```

This inserts a row into `governance.policast.bindings` and denormalizes
`policast.applied_policies` onto the target table's property dictionary.

### Open a governed table from UC

```rust
use policast_datafusion::uc::{UcClientConfig, governed_table_from_uc};
use policast_uc::{Principal, PrincipalAttrs};

let client = UcClientConfig::new("http://localhost:8765")
    .with_signing_secret_env("POLICAST_UC_SECRET")
    .build()?;

let principal = Principal {
    id: "alice@hospital.com".into(),
    role: "analyst".into(),
    attrs: PrincipalAttrs::new().with("region", "us-east"),
};

let governed = governed_table_from_uc(
    &client,
    "hospital.clinical.patients",
    &principal,
).await?;

ctx.register_table("patients", std::sync::Arc::new(governed))?;
```

Internally this:
1. POSTs a `ResolveRequest` to `/policies/resolve`.
2. Verifies the HMAC signature on the returned `ResolveBundle`.
3. Caches the bundle under `(table_uuid, principal_hash)` with TTL =
   `expires_at`.
4. Opens the Delta table at the URL from the bundle's
   `storage_credentials`.
5. Wraps the `DeltaTable` in a `GovernedTable` with the compiled
   `PolicyManifest` and the identity claims from the bundle.

### Running the sidecar

Until an upstream UC patch lands, the `policast-uc` crate ships an Axum
sidecar with the same contract as the planned UC endpoint:

```bash
cargo run -p policast-uc --bin policast-uc-sidecar --features sidecar -- \
    --listen 127.0.0.1:8765 \
    --store-root ./examples/uc/store \
    --signing-secret-env POLICAST_UC_SECRET
```

The sidecar reads the three governance tables (as Delta or, for tests,
as a flat JSON file layout) and issues signed `ResolveBundle`s. Engines
cannot tell whether they are talking to UC or the sidecar — the wire
contract is identical.

## Query identity

Identity fields are unchanged from the Delta path:

```rust
pub struct QueryIdentity {
    pub role: String,
    pub region: Option<String>,
    pub name: Option<String>,
}
```

When using UC as the PDP, the `QueryIdentity` is derived from the
`identity_claims` field of the `ResolveBundle`. The resolver may enrich
the identity (e.g. attach `region` from a user directory) so the engine
does not have to know every principal's attribute schema.

## Caching and invalidation

- `UnityCatalogPolicyStore` wraps an `lru::LruCache` with TTL equal to
  `expires_at` on the bundle.
- The cache key is `(table_uuid, sha256(principal_id + role + attrs))`.
- An optional CDF listener on `governance.policast.manifest` can
  push-invalidate entries when policies are republished. Enable it via
  `UcClientConfig::with_cdf_invalidation(true)`.

## Trust and fail-closed behavior

- If `/policies/resolve` returns non-2xx, `from_uc` returns
  `Err(UcError::Resolve(...))` and the table is never registered.
- If the HMAC signature does not verify, the client returns
  `Err(UcError::BadSignature)` and drops the bundle.
- If `expires_at` has passed at use time, the cache entry is evicted
  and the next call re-resolves.
- There is no "fail-open" path: a user with no resolvable bundle
  cannot scan the table.

## Physical plan structure

Unchanged from the Delta path. For a query like
`SELECT patient_id, ssn, region FROM patients`, the plan is:

```
ProjectionExec [patient_id (pass-through), ssn → Literal("***"), region (pass-through)]
  └─ FilterExec [legal_hold = false OR legal_hold IS NULL]
       └─ FilterExec [region = 'us-east']
            └─ DeltaScan (inner provider, opened with UC-vended creds)
```

The only difference is that the `DeltaScan` below uses credentials
vended by UC alongside the manifest, rather than credentials supplied
independently.
