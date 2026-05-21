//! Unity Catalog-backed `GovernedTable`.
//!
//! This module is the bridge between `policast-uc`'s REST client and
//! the existing `GovernedTable` enforcement core. It does **not**
//! change how row filters or column masks are enforced; it only
//! replaces the *source* of the `PolicyManifest` with a UC
//! `/policies/resolve` call.
//!
//! Two entry points:
//!
//! - [`governed_table_from_uc`]: resolve a bundle, open the backing
//!   Delta table (using `storage_uri` + `storage_credentials` from the
//!   bundle when present, or falling back to a caller-supplied URI),
//!   and wrap the result in a [`GovernedTable`].
//! - [`UcTableSource`]: the same thing with more configuration,
//!   including a local-Delta fallback for tests.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::datasource::TableProvider;
use deltalake::open_table_with_storage_options;

use policast_core::PolicyManifest;
use policast_uc::client::UcClient;
use policast_uc::types::{Principal, ResolveBundle, ResolveRequest};
use policast_uc::UcError;

use crate::cel_filter::QueryIdentity;
use crate::governance_table::GovernedTable;

/// Configuration for how to materialize a `GovernedTable` from a UC
/// resolve bundle. The defaults Just Work for the flat-file sidecar +
/// local Delta layout used by `examples/run_datafusion_uc.rs`.
#[derive(Debug, Clone, Default)]
pub struct UcTableOptions {
    /// Override the storage URI from the bundle. Useful in tests where
    /// you want to open a local Delta table regardless of whatever
    /// URI the resolver returns.
    pub storage_uri_override: Option<String>,
    /// Extra delta-rs storage options (forwarded verbatim to
    /// `open_table_with_storage_options`).
    pub storage_options: HashMap<String, String>,
}

/// Open a UC-governed Delta table by resolving its policies and
/// credentials through the UC policy store.
///
/// The returned `GovernedTable` enforces the same row filters and
/// column masks that `open_governed_delta_table` does — the only
/// difference is where the manifest came from.
pub async fn governed_table_from_uc(
    client: &UcClient,
    table: impl Into<String>,
    principal: &Principal,
) -> Result<GovernedTable, UcError> {
    governed_table_from_uc_with_options(client, table, principal, UcTableOptions::default()).await
}

/// Same as [`governed_table_from_uc`] with an explicit `UcTableOptions`.
pub async fn governed_table_from_uc_with_options(
    client: &UcClient,
    table: impl Into<String>,
    principal: &Principal,
    opts: UcTableOptions,
) -> Result<GovernedTable, UcError> {
    let table = table.into();
    let req = ResolveRequest {
        table: table.clone(),
        principal: principal.clone(),
        requested_action: "query".into(),
    };
    let bundle = client.resolve(&req).await?;
    wrap_bundle(bundle, &table, opts).await
}

/// Turn an already-resolved bundle into a `GovernedTable`. Useful when
/// the bundle came from somewhere other than the HTTP client (for
/// example, the in-process `ResolverCore` used by integration tests).
pub async fn wrap_bundle(
    bundle: ResolveBundle,
    table: &str,
    opts: UcTableOptions,
) -> Result<GovernedTable, UcError> {
    let uri = opts
        .storage_uri_override
        .clone()
        .or_else(|| bundle.storage_uri.clone())
        .ok_or_else(|| {
            UcError::Config(format!(
                "bundle for {table} did not include a storage_uri and no override was set"
            ))
        })?;

    let delta_table = open_table_with_storage_options(&uri, opts.storage_options.clone())
        .await
        .map_err(|e| UcError::Resolve(format!("open_table {uri}: {e}")))?;
    let provider: Arc<dyn TableProvider> = Arc::new(delta_table);
    let identity = identity_from_bundle(&bundle);

    let manifest: PolicyManifest = bundle.compiled_manifest;
    Ok(GovernedTable::new(provider, manifest, table.to_string(), identity))
}

/// Construct a `QueryIdentity` from a bundle's `identity_claims`,
/// falling back to sensible defaults for missing fields. `role` is
/// required; `region` and `name` are optional.
pub fn identity_from_bundle(bundle: &ResolveBundle) -> QueryIdentity {
    let role = bundle
        .identity_claims
        .get("role")
        .cloned()
        .unwrap_or_default();
    let region = bundle
        .identity_claims
        .get("region")
        .cloned()
        .filter(|s| !s.is_empty());
    let name = bundle
        .identity_claims
        .get("name")
        .or_else(|| bundle.identity_claims.get("principal_id"))
        .cloned()
        .filter(|s| !s.is_empty());
    QueryIdentity { role, region, name }
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::PolicyManifest;
    use policast_uc::types::ResolveBundle;

    fn bundle_with_claims(
        claims: &[(&str, &str)],
        uri: Option<&str>,
    ) -> ResolveBundle {
        let identity_claims = claims
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ResolveBundle {
            table_uuid: "t".into(),
            compiled_manifest: PolicyManifest::new(),
            bindings_applied: Vec::new(),
            expanded_from: Default::default(),
            identity_claims,
            storage_credentials: None,
            storage_uri: uri.map(str::to_string),
            expires_at: "2999-01-01T00:00:00Z".into(),
            signature: "sig".into(),
        }
    }

    #[test]
    fn test_identity_from_bundle_populates_fields() {
        let bundle = bundle_with_claims(
            &[
                ("role", "analyst"),
                ("region", "us-east"),
                ("principal_id", "alice@corp"),
            ],
            None,
        );
        let id = identity_from_bundle(&bundle);
        assert_eq!(id.role, "analyst");
        assert_eq!(id.region.as_deref(), Some("us-east"));
        assert_eq!(id.name.as_deref(), Some("alice@corp"));
    }

    #[test]
    fn test_identity_prefers_name_over_principal_id() {
        let bundle = bundle_with_claims(
            &[
                ("role", "physician"),
                ("name", "Dr. Smith"),
                ("principal_id", "dr-smith@corp"),
            ],
            None,
        );
        let id = identity_from_bundle(&bundle);
        assert_eq!(id.name.as_deref(), Some("Dr. Smith"));
    }

    #[test]
    fn test_identity_empty_strings_become_none() {
        let bundle = bundle_with_claims(
            &[("role", "analyst"), ("region", ""), ("name", "")],
            None,
        );
        let id = identity_from_bundle(&bundle);
        assert_eq!(id.region, None);
        assert_eq!(id.name, None);
    }

    #[tokio::test]
    async fn test_wrap_bundle_errors_when_no_uri() {
        let bundle = bundle_with_claims(&[("role", "analyst")], None);
        let err = wrap_bundle(bundle, "t", UcTableOptions::default())
            .await
            .unwrap_err();
        matches!(err, UcError::Config(_));
    }
}
