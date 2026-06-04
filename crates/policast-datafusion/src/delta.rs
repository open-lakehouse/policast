use std::sync::Arc;

use datafusion::datasource::TableProvider;
use deltalake::open_table;
use deltalake::DeltaTable;

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
    let delta_table = open_table(table_uri).await?;
    Ok(wrap_delta_table(delta_table, manifest, table_name, identity))
}

/// Wrap an already-opened `DeltaTable` with governance policies.
///
/// Use this when you have an existing `DeltaTable` instance (e.g.,
/// created with custom storage options) and want to add governance.
pub fn wrap_delta_table(
    delta_table: DeltaTable,
    manifest: PolicyManifest,
    table_name: impl Into<String>,
    identity: QueryIdentity,
) -> GovernedTable {
    let provider: Arc<dyn TableProvider> = Arc::new(delta_table);
    GovernedTable::new(provider, manifest, table_name, identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_delta_table_types_compile() {
        // Verify that DeltaTable implements TableProvider at compile time.
        // We can't open a real table in a unit test without a fixture,
        // but this ensures the type constraints are met.
        fn _assert_table_provider<T: TableProvider>() {}
        _assert_table_provider::<DeltaTable>();
    }
}
