use std::sync::Arc;

use datafusion::datasource::TableProvider;
use deltalake::DeltaTable;
use deltalake::{ensure_table_uri, open_table};

use policast_core::PolicyManifest;

use crate::cel_filter::QueryIdentity;
use crate::governance_table::GovernedTable;

/// Open a Delta Lake table from a URI and wrap it with governance policies.
///
/// The returned `GovernedTable` enforces row-level filters and column masks
/// derived from the policy manifest against the querying user's identity.
///
/// # Arguments
///
/// * `table_uri` - Path or URI to the Delta table (local path, S3, GCS, ADLS).
/// * `manifest` - Compiled policy manifest (from Cedar policies).
/// * `table_name` - Logical table name used to match policies.
/// * `identity` - The identity of the querying user.
///
/// # Example
///
/// ```rust,no_run
/// # use policast_datafusion::delta::open_governed_delta_table;
/// # use policast_core::PolicyManifest;
/// # use policast_datafusion::QueryIdentity;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = PolicyManifest::new();
/// let identity = QueryIdentity {
///     role: "analyst".into(),
///     region: Some("us-east".into()),
///     name: None,
/// };
/// let table = open_governed_delta_table(
///     "/path/to/delta/table",
///     manifest,
///     "my_table",
///     identity,
/// ).await?;
/// # Ok(())
/// # }
/// ```
pub async fn open_governed_delta_table(
    table_uri: impl AsRef<str>,
    manifest: PolicyManifest,
    table_name: impl Into<String>,
    identity: QueryIdentity,
) -> Result<GovernedTable, Box<dyn std::error::Error>> {
    // deltalake 0.32's `open_table` takes a parsed `Url`; `ensure_table_uri`
    // normalizes local paths into `file://` URLs (and validates remote ones).
    let table_url = ensure_table_uri(table_uri)?;
    let delta_table = open_table(table_url).await?;
    wrap_delta_table(delta_table, manifest, table_name, identity).await
}

/// Wrap an already-opened `DeltaTable` with governance policies.
///
/// Use this when you have an existing `DeltaTable` instance (e.g.,
/// created with custom storage options) and want to add governance.
///
/// As of `deltalake` 0.32, `DeltaTable` no longer implements DataFusion's
/// `TableProvider` directly; we materialize a provider through the
/// [`TableProviderBuilder`](deltalake::delta_datafusion::TableProviderBuilder)
/// exposed by [`DeltaTable::table_provider`], which is async and fallible.
pub async fn wrap_delta_table(
    delta_table: DeltaTable,
    manifest: PolicyManifest,
    table_name: impl Into<String>,
    identity: QueryIdentity,
) -> Result<GovernedTable, Box<dyn std::error::Error>> {
    let provider: Arc<dyn TableProvider> = Arc::new(delta_table.table_provider().build().await?);
    Ok(GovernedTable::new(provider, manifest, table_name, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_delta_table_signature_compiles() {
        // As of deltalake 0.32, `DeltaTable` no longer implements
        // `TableProvider`; a provider is built asynchronously via
        // `DeltaTable::table_provider().build()`. We can't open a real table
        // in a unit test without a fixture, so this just pins the public
        // `wrap_delta_table` signature (async, fallible) at compile time.
        fn _assert_signature<F, Fut>(_f: F)
        where
            F: Fn(DeltaTable, PolicyManifest, String, QueryIdentity) -> Fut,
            Fut: std::future::Future<Output = Result<GovernedTable, Box<dyn std::error::Error>>>,
        {
        }
        _assert_signature(wrap_delta_table);
    }
}
