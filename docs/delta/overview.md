# GovernedTable with Delta Lake

## What it does

`GovernedTable` is a DataFusion `TableProvider` wrapper that enforces
governance policies — row-level filters and column-level masks — on any
table it wraps. When combined with [delta-rs](https://github.com/delta-io/delta-rs),
it brings Cedar/CEL-based access control to Delta Lake tables queried
through DataFusion.

Policies are authored in Cedar, compiled into portable CEL expressions by
`policast-core`, and enforced at query time inside the DataFusion physical
plan. No policy logic leaks into application SQL; the governance layer is
invisible to the end user.

## Architecture

```
                  Cedar policies
                       │
                       ▼
               ┌───────────────┐
               │ policast-core │  Cedar → CEL compilation
               └───────┬───────┘
                       │ PolicyManifest (JSON)
                       ▼
              ┌─────────────────┐
              │ policast-       │  CEL → DataFusion Expr
              │ datafusion      │  + PhysicalPlan wrappers
              └────────┬────────┘
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
     DeltaTable    MemTable    Parquet/CSV
     (delta-rs)                (any TableProvider)
```

`GovernedTable` sits between DataFusion's query engine and the underlying
storage provider. At scan time it:

1. Delegates the base scan to the inner `TableProvider` (e.g. `DeltaTable`).
2. Wraps the resulting `ExecutionPlan` in `FilterExec` nodes for each
   governance row filter.
3. Wraps the plan again in a `ProjectionExec` that replaces masked column
   values with literals (`"***"`).

Because governance predicates are applied as physical-plan wrappers rather
than passed as hints to the inner provider, they are **always enforced** —
even when the inner provider does not support filter pushdown.

## Governance enforcement

### Row filters

Row filter policies restrict which rows a user can see. A CEL expression
like `(resource.region == principal.region)` is compiled into a DataFusion
`Expr` at planning time:

- `resource.*` references become column references (`col("region")`).
- `principal.*` references are bound from the querying user's
  `QueryIdentity` as literal values (`lit("us-east")`).
- Sub-expressions that depend only on `principal.*` are constant-folded
  at planning time, so a deny-override like
  `(resource.legal_hold == true) && !(principal.role == "legal")` produces
  no filter at all for a user with the `legal` role.

The resulting `Expr` is converted to a `PhysicalExpr` and wrapped around
the inner scan as a `FilterExec`.

### Column masks

Column mask policies replace sensitive column values with a mask string.
The CEL expression for a mask is evaluated entirely at planning time using
the `cel-interpreter` runtime (all inputs — `principal.*` and
`resource.table_name` — are known). If the expression evaluates to `true`,
the column is masked; otherwise the real values pass through.

Masking is enforced by a `ProjectionExec` that replaces the column's
physical expression with a `Literal("***")` while keeping the column name
and position intact. This means masked values never appear in query
results regardless of what SQL the caller writes.

### Deny overrides

Deny-override policies (Cedar `forbid` rules) express "deny access when
condition X, unless the user is exempt." The governance layer inverts this:
it produces a row filter that **keeps** rows where the deny condition is
false. For exempt users, constant folding eliminates the filter entirely.

## Using Delta Lake

### Enable the feature

The Delta Lake integration is behind the `delta` Cargo feature:

```toml
[dependencies]
policast-datafusion = { path = "policast-datafusion", features = ["delta"] }
```

This pulls in `deltalake = "0.25"` which uses the same DataFusion 46
dependency as `policast-datafusion`, so there are no version conflicts.

### Open a governed Delta table

Two convenience functions are provided in the `policast_datafusion::delta`
module:

**`open_governed_delta_table`** opens a Delta table from a URI and wraps
it in one step:

```rust
use policast_datafusion::delta::open_governed_delta_table;

let governed = open_governed_delta_table(
    "s3://my-bucket/patients_delta",
    manifest,
    "patients",
    identity,
).await?;

ctx.register_table("patients", Arc::new(governed))?;
```

**`wrap_delta_table`** wraps an already-opened `DeltaTable`, useful when
you need custom storage configuration:

```rust
use policast_datafusion::delta::wrap_delta_table;

let delta_table = deltalake::open_table_with_storage_options(uri, opts).await?;
let governed = wrap_delta_table(delta_table, manifest, "patients", identity);
```

Both return a `GovernedTable` that can be registered with any DataFusion
`SessionContext`.

### Without Delta Lake

`GovernedTable` wraps `Arc<dyn TableProvider>`, so it works with any
DataFusion table — `MemTable`, Parquet files, CSV, or any custom provider:

```rust
let mem_table: Arc<dyn TableProvider> = Arc::new(MemTable::try_new(schema, batches)?);
let governed = GovernedTable::new(mem_table, manifest, "patients", identity);
```

## Query identity

Every `GovernedTable` is bound to a `QueryIdentity` that represents the
user executing the query:

```rust
pub struct QueryIdentity {
    pub role: String,
    pub region: Option<String>,
    pub name: Option<String>,
}
```

Policy expressions reference these fields as `principal.role`,
`principal.region`, and `principal.name`. When a field is `None` and a
policy references it, that policy is skipped with a warning rather than
causing a query failure.

## Physical plan structure

For a governed Delta table query like
`SELECT patient_id, ssn, region FROM patients`, the physical plan looks
like:

```
ProjectionExec [patient_id (pass-through), ssn → Literal("***"), region (pass-through)]
  └─ FilterExec [legal_hold = false OR legal_hold IS NULL]
       └─ FilterExec [region = 'us-east']
            └─ DeltaScan (inner provider)
```

The inner `DeltaScan` handles Parquet file pruning and row-group skipping.
Governance filters sit on top as `FilterExec` wrappers, and column masks
as a final `ProjectionExec`. This layering means the inner provider's
optimizations (predicate pushdown, file skipping) still apply to user
filters, while governance predicates are enforced unconditionally.
