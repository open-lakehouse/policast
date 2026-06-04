//! A pluggable store for compiled policies.
//!
//! Before policast-cel started targeting Unity Catalog, engines read a
//! `PolicyManifest` from a JSON file on disk. With a catalog-native
//! store (see `research/unity-catalog-policy-store.md`) the resolution
//! path needs to answer more than "give me the manifest" — it needs to
//! know which policies apply to a given principal/table pair, and it
//! needs to do so via an async call that talks over the network.
//!
//! The `PolicyStore` trait is that abstraction. Two impls live here:
//!
//! - [`FileManifestStore`] — the current behavior, reading a JSON
//!   manifest file and applying every policy in it. Useful for offline
//!   development and as the default in unit tests.
//! - A separate `UnityCatalogPolicyStore` lives in the `policast-uc`
//!   crate; it implements the same trait against a REST resolver.
//!
//! Engines depend on the trait, not on a concrete impl, so swapping
//! stores is a one-line change.
//!
//! ```no_run
//! use policast_core::policy_store::{PolicyStore, FileManifestStore, PolicyQuery};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let store = FileManifestStore::from_path("examples/policies/manifest.json")?;
//! let query = PolicyQuery {
//!     table: "patients".into(),
//!     principal_id: "alice".into(),
//!     principal_role: "analyst".into(),
//! };
//! let resolved = store.resolve(&query).await?;
//! assert!(!resolved.manifest.policies.is_empty());
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::PolicastError;
use crate::model::CompiledPolicy;
use crate::policy_manifest::PolicyManifest;

/// Inputs to a policy resolution: which table is being queried and by
/// whom.
///
/// Kept intentionally small — attributes beyond role live on the
/// resolver side (UC), and the resolver returns them as
/// `ResolvedPolicies::identity_claims`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyQuery {
    /// The table being scanned. Either a short table name (for the
    /// file-based store) or a three-part UC name.
    pub table: String,
    /// The querying principal's identifier (e.g. email, username).
    pub principal_id: String,
    /// The principal's role, used by `applies_to` filtering and by CEL
    /// `principal.role` references.
    pub principal_role: String,
}

/// The result of a policy resolution: the compiled manifest that applies
/// to this query, plus any identity claims the resolver supplied
/// (region, groups, etc.) beyond what the caller already knew.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPolicies {
    /// The subset of the full manifest that applies to this
    /// `(table, principal)` pair.
    pub manifest: PolicyManifest,
    /// Extra principal attributes resolved by the store (e.g.
    /// `region`, `groups`) that the engine should feed into CEL
    /// evaluation.
    #[serde(default)]
    pub identity_claims: std::collections::BTreeMap<String, String>,
    /// The policy ids that were actually bound to this table for this
    /// principal. Useful for logging/auditing.
    #[serde(default)]
    pub bindings_applied: Vec<String>,
}

/// Async pluggable store for compiled policies.
///
/// See module docs for the two built-in impls.
#[async_trait]
pub trait PolicyStore: Send + Sync {
    /// Resolve the policies that apply to `query.table` for
    /// `query.principal_*`.
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError>;
}

/// File-backed `PolicyStore` that reads a full `PolicyManifest` from
/// disk once at construction and filters it per-query.
///
/// This preserves the pre-UC behavior exactly: every policy whose
/// `target_table` matches (including wildcard) or whose `applies_to`
/// role matches is returned.
pub struct FileManifestStore {
    manifest: PolicyManifest,
}

impl FileManifestStore {
    /// Read and parse a manifest JSON file.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, PolicastError> {
        let text = std::fs::read_to_string(path)?;
        let manifest = PolicyManifest::from_json(&text)?;
        Ok(Self { manifest })
    }

    /// Construct directly from an in-memory manifest (useful for
    /// tests).
    pub fn from_manifest(manifest: PolicyManifest) -> Self {
        Self { manifest }
    }

    /// Borrow the underlying manifest.
    pub fn manifest(&self) -> &PolicyManifest {
        &self.manifest
    }
}

#[async_trait]
impl PolicyStore for FileManifestStore {
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
        let matching: Vec<&CompiledPolicy> = self.manifest
            .policies
            .iter()
            .filter(|p| {
                table_matches(&p.target_table, &query.table)
                    && role_applies(p, &query.principal_role)
            })
            .collect();

        let bindings_applied: Vec<String> = matching.iter().map(|p| p.id.clone()).collect();

        let manifest = PolicyManifest {
            version: self.manifest.version.clone(),
            policies: matching.into_iter().cloned().collect(),
            principal_contract: self.manifest.principal_contract.clone(),
        };

        Ok(ResolvedPolicies {
            manifest,
            identity_claims: Default::default(),
            bindings_applied,
        })
    }
}

/// Match a policy's `target_table` against the query's table name.
///
/// Supports `*` (any table), `a.b.*` (any table in schema `a.b`), and
/// exact matches. Also accepts a bare table name matching the last
/// segment of a three-part name — this keeps the POC's short names
/// (`patients`) working alongside UC's `catalog.schema.table`.
pub fn table_matches(pattern: &str, table: &str) -> bool {
    if pattern == "*" || pattern == table {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        if let Some(dot) = table.rfind('.') {
            return &table[..dot] == prefix;
        }
    }
    // Tolerate three-part vs bare-name mismatches in either direction
    // so the existing file-manifest demos keep working when pointed at
    // UC-shaped target_table values.
    if let Some(dot) = pattern.rfind('.') {
        if &pattern[dot + 1..] == table {
            return true;
        }
    }
    if let Some(dot) = table.rfind('.') {
        if &table[dot + 1..] == pattern {
            return true;
        }
    }
    false
}

/// Returns true if the policy's `applies_to` (if any) includes the
/// principal's role. A missing `applies_to` means the policy applies to
/// everyone.
fn role_applies(policy: &CompiledPolicy, role: &str) -> bool {
    match &policy.applies_to {
        None => true,
        Some(a) => a.roles.is_empty() || a.roles.iter().any(|r| r == role),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppliesTo, Effect, FilterType};

    fn policy(id: &str, table: &str, roles: Option<Vec<&str>>) -> CompiledPolicy {
        CompiledPolicy {
            id: id.into(),
            effect: Effect::Permit,
            filter_type: FilterType::RowFilter,
            target_table: table.into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: "true".into(),
            applies_to: roles.map(|rs| AppliesTo {
                roles: rs.into_iter().map(String::from).collect(),
                principals: Vec::new(),
            }),
            description: None,
        }
    }

    fn store_with(policies: Vec<CompiledPolicy>) -> FileManifestStore {
        FileManifestStore::from_manifest(PolicyManifest {
            version: "1.0".into(),
            policies,
            principal_contract: None,
        })
    }

    #[test]
    fn test_table_matches_exact_and_wildcard() {
        assert!(table_matches("*", "patients"));
        assert!(table_matches("patients", "patients"));
        assert!(table_matches("hospital.clinical.*", "hospital.clinical.patients"));
        assert!(!table_matches("hospital.foo.*", "hospital.clinical.patients"));
        assert!(table_matches("hospital.clinical.patients", "patients"));
        assert!(table_matches("patients", "hospital.clinical.patients"));
        assert!(!table_matches("orders", "patients"));
    }

    #[tokio::test]
    async fn test_resolve_filters_by_table() {
        let store = store_with(vec![
            policy("p1", "patients", None),
            policy("p2", "orders", None),
        ]);
        let q = PolicyQuery {
            table: "patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        assert_eq!(resolved.manifest.policies.len(), 1);
        assert_eq!(resolved.manifest.policies[0].id, "p1");
        assert_eq!(resolved.bindings_applied, vec!["p1".to_string()]);
    }

    #[tokio::test]
    async fn test_resolve_filters_by_role() {
        let store = store_with(vec![
            policy("analyst_only", "patients", Some(vec!["analyst"])),
            policy("physician_only", "patients", Some(vec!["physician"])),
            policy("open_to_all", "patients", None),
        ]);
        let q = PolicyQuery {
            table: "patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        let ids: Vec<&str> = resolved
            .manifest
            .policies
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(ids, vec!["analyst_only", "open_to_all"]);
    }

    #[tokio::test]
    async fn test_resolve_wildcard_table() {
        let store = store_with(vec![policy("global", "*", None)]);
        let q = PolicyQuery {
            table: "anything".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        assert_eq!(resolved.manifest.policies.len(), 1);
    }

    #[tokio::test]
    async fn test_resolve_empty_applies_to_means_all_roles() {
        let store = store_with(vec![policy("p1", "patients", Some(vec![]))]);
        let q = PolicyQuery {
            table: "patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        assert_eq!(resolved.manifest.policies.len(), 1);
    }
}
