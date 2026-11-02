//! The policy exchange flow: Cedar source -> compiled `PolicyManifest`.
//!
//! This is the compile half of the round trip. A `.cedar` policy (with
//! its `@id` / `@filter_type` / `@target_table` / tag / `@owner`
//! annotations) is parsed and compiled by `policast-core` into the
//! portable [`PolicyManifest`] (CEL expressions). The manifest can then
//! be encoded as binary protobuf ([`PolicyManifest::to_protobuf`]) and
//! published into the materialized Redis store.
//!
//! Two publish granularities exist:
//!
//! * **Direct publish** ([`compile_and_publish`], `redis` feature) —
//!   compile raw Cedar and write the resulting per-table manifests
//!   straight to Redis. Useful for tests/demos and seeding. It writes
//!   exactly the policies in the supplied Cedar; it does **not** merge
//!   with policies already bound to those tables.
//! * **Materialize-from-backend**
//!   ([`RedisManifestStore::materialize`](crate::redis_store::RedisManifestStore::materialize))
//!   — the production flush: recompiles the full governance state into
//!   per-table manifests. The owner-aware edit path writes the new
//!   version to the governance Delta tables and then re-materializes.

use std::collections::BTreeMap;

use policast_core::{parse_policies, CompiledPolicy, PolicyManifest};

use crate::error::UcError;

/// A single compiled policy plus the governance metadata the edit path
/// needs (owner from `@owner`, the original Cedar source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPolicyDoc {
    pub policy: CompiledPolicy,
    /// Owner principal from `@owner`, or `None` to default to the
    /// author (`created_by`) at write time.
    pub owner: Option<String>,
    /// The exact Cedar source that compiled to `policy`.
    pub cedar_source: String,
}

/// Compile a (possibly multi-policy) Cedar document into a manifest.
pub fn compile(cedar_text: &str) -> Result<PolicyManifest, UcError> {
    let parsed = parse_policies(cedar_text)?;
    let mut manifest = PolicyManifest::new();
    manifest.compile_policies(&parsed)?;
    Ok(manifest)
}

/// Compile a Cedar document that must contain **exactly one** policy,
/// returning the compiled policy together with its `@owner` and source.
///
/// Used by the owner-aware edit API, which operates on a single
/// `policy_id` at a time.
pub fn compile_single(cedar_text: &str) -> Result<CompiledPolicyDoc, UcError> {
    let parsed = parse_policies(cedar_text)?;
    if parsed.len() != 1 {
        return Err(UcError::Invalid(format!(
            "expected exactly one policy in the Cedar source, found {}",
            parsed.len()
        )));
    }
    let owner = parsed[0].owner().map(str::to_string);
    let mut manifest = PolicyManifest::new();
    manifest.compile_policies(&parsed)?;
    let policy = manifest.policies.into_iter().next().ok_or_else(|| {
        UcError::Invalid("compile produced no policy for a single-policy source".into())
    })?;
    Ok(CompiledPolicyDoc {
        policy,
        owner,
        cedar_source: cedar_text.to_string(),
    })
}

/// Group a manifest's policies by their concrete `target_table`.
///
/// Policies whose `target_table` is a wildcard (`*` or `a.b.*`) cannot
/// be keyed by a single table and are skipped — use the
/// materialize-from-backend path for tag/wildcard expansion. Returns a
/// per-table manifest map suitable for direct Redis publishing.
pub fn group_by_table(manifest: &PolicyManifest) -> BTreeMap<String, PolicyManifest> {
    let mut by_table: BTreeMap<String, Vec<CompiledPolicy>> = BTreeMap::new();
    for policy in &manifest.policies {
        let table = &policy.target_table;
        if table == "*" || table.ends_with(".*") {
            continue;
        }
        by_table
            .entry(table.clone())
            .or_default()
            .push(policy.clone());
    }
    by_table
        .into_iter()
        .map(|(table, policies)| {
            (
                table,
                PolicyManifest {
                    manifest_schema_version: manifest.manifest_schema_version,
                    row_grant_targets: manifest.row_grant_targets.clone(),
                    version: manifest.version.clone(),
                    policies,
                    principal_contract: manifest.principal_contract.clone(),
                },
            )
        })
        .collect()
}

/// Compile raw Cedar and publish the resulting per-table manifests to
/// Redis as binary protobuf. Returns the tables written.
///
/// See the module docs: this overwrites each table key with exactly the
/// compiled policies and does not merge with existing bindings. For the
/// production flush, use
/// [`RedisManifestStore::materialize`](crate::redis_store::RedisManifestStore::materialize).
#[cfg(feature = "redis")]
pub async fn compile_and_publish(
    cedar_text: &str,
    store: &crate::redis_store::RedisManifestStore,
) -> Result<Vec<String>, UcError> {
    let manifest = compile(cedar_text)?;
    let by_table = group_by_table(&manifest);
    let mut written = Vec::with_capacity(by_table.len());
    for (table, manifest) in by_table {
        store.put_manifest(&table, &manifest).await?;
        written.push(table);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::{Effect, FilterType};

    const ROW_FILTER: &str = r#"
        @id("row_filter_region")
        @filter_type("row_filter")
        @target_table("hospital.clinical.patients")
        @owner("alice@hospital.com")
        @roles("analyst")
        permit (principal, action == Action::"query", resource)
        when { resource.region == principal.region };
    "#;

    #[test]
    fn test_compile_single_extracts_owner_and_cel() {
        let doc = compile_single(ROW_FILTER).unwrap();
        assert_eq!(doc.policy.id, "row_filter_region");
        assert_eq!(doc.policy.effect, Effect::Permit);
        assert_eq!(doc.policy.filter_type, FilterType::RowFilter);
        assert_eq!(doc.policy.target_table, "hospital.clinical.patients");
        assert!(doc.policy.cel_expression.contains("resource.region"));
        assert_eq!(doc.owner.as_deref(), Some("alice@hospital.com"));
        assert_eq!(doc.cedar_source, ROW_FILTER);
    }

    #[test]
    fn test_compile_single_rejects_multi_policy() {
        let two = format!(
            "{ROW_FILTER}\n{}",
            ROW_FILTER.replace("region", "physician")
        );
        let err = compile_single(&two).unwrap_err();
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn test_compile_single_rejects_empty() {
        let err = compile_single("   ").unwrap_err();
        // An empty document parses to zero policies.
        assert!(err.to_string().contains("exactly one") || err.to_string().contains("found 0"));
    }

    #[test]
    fn test_compile_single_surfaces_compile_errors() {
        let broken = r#"
            @id("broken")
            @filter_type("row_filter")
            @target_table("t")
            @target_tag("clinical")
            permit (principal, action, resource);
        "#;
        let err = compile_single(broken).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn test_group_by_table_skips_wildcards() {
        let cedar = r#"
            @id("concrete")
            @filter_type("row_filter")
            @target_table("a.b.c")
            permit (principal, action, resource) when { resource.x == 1 };

            @id("wild")
            @filter_type("row_filter")
            @target_table("*")
            permit (principal, action, resource) when { resource.y == 2 };
        "#;
        let manifest = compile(cedar).unwrap();
        let grouped = group_by_table(&manifest);
        assert_eq!(grouped.len(), 1);
        assert!(grouped.contains_key("a.b.c"));
        assert!(!grouped.contains_key("*"));
    }
}
