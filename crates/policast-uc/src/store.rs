//! The resolver core.
//!
//! Given a [`ResolveBackend`] and an HMAC signing secret, this module
//! implements the logic that turns a [`ResolveRequest`] into a signed
//! [`ResolveBundle`]. The logic is reused by:
//!
//! - the Axum sidecar (`src/sidecar.rs`) which exposes it over HTTP,
//! - the `policast-uc` client's in-process fallback when an endpoint
//!   URL is not configured, and
//! - unit tests.
//!
//! Resolution algorithm:
//!   1. Load all four governance tables from the backend (policies,
//!      manifest, bindings, tags).
//!   2. Filter bindings whose `target` matches the request's table
//!      (exact, schema-wildcard, or `*`).
//!   3. Filter bindings whose `principal_selector` matches the request's
//!      principal (`role:<r>`, `principal:<id>`, `*`).
//!   4. Join with the manifest rows to get the compiled CEL, and with
//!      the policies rows to get filter_type / target_table / column
//!      / target_tag / applies_to_tag.
//!   5. **Tag expansion**: for each [`CompiledPolicy`] that carries
//!      `target_tag` and/or `applies_to_tag`, fan it out over the tag
//!      index into one concrete policy per matching (table, column)
//!      tuple. Non-tag-scoped policies pass through unchanged.
//!      Expanded policies get deterministic ids of the form
//!      `{template_id}@{table}` or `{template_id}@{table}:{column}`.
//!   6. Assemble a [`PolicyManifest`] mirroring the schema the engines
//!      already consume.
//!   7. Sign the bundle and return.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use policast_core::model::{AppliesTo, CompiledPolicy, Effect, FilterType};
use policast_core::PolicyManifest;
use time::OffsetDateTime;

use crate::backend::{ManifestRow, PolicyRow, ResolveBackend, TagRow};
use crate::error::UcError;
use crate::signature::sign_bundle;
use crate::types::{Principal, ResolveBundle, ResolveRequest, StorageCredentials};

/// Default TTL for a freshly-issued bundle (15 minutes).
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// The resolver core wraps a backend and a signing secret.
pub struct ResolverCore {
    backend: Arc<dyn ResolveBackend>,
    secret: Vec<u8>,
    ttl: Duration,
    /// Optional storage URI template: `{table}` is substituted with the
    /// request's table before being placed on the bundle. Used by
    /// `examples/run_datafusion_uc.rs` to point engines at a local
    /// Delta table that was materialized side-by-side with the store.
    storage_uri_template: Option<String>,
    storage_credentials_template: Option<StorageCredentials>,
}

impl ResolverCore {
    pub fn new(backend: Arc<dyn ResolveBackend>, secret: impl Into<Vec<u8>>) -> Self {
        Self {
            backend,
            secret: secret.into(),
            ttl: DEFAULT_TTL,
            storage_uri_template: None,
            storage_credentials_template: None,
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn with_storage_uri_template(mut self, tpl: impl Into<String>) -> Self {
        self.storage_uri_template = Some(tpl.into());
        self
    }

    pub fn with_storage_credentials(mut self, creds: StorageCredentials) -> Self {
        self.storage_credentials_template = Some(creds);
        self
    }

    /// Resolve a request to a signed bundle.
    pub async fn resolve(&self, req: &ResolveRequest) -> Result<ResolveBundle, UcError> {
        let policies = self.backend.policies().await?;
        let manifest_rows = self.backend.manifest().await?;
        let bindings = self.backend.bindings().await?;
        let tags = self.backend.tags().await?;

        let mut applied_ids: Vec<(i32, String)> = Vec::new();
        for b in &bindings {
            if !target_matches(&b.target, &req.table) {
                continue;
            }
            if !selector_matches(&b.principal_selector, &req.principal) {
                continue;
            }
            applied_ids.push((b.precedence, b.policy_id.clone()));
        }
        applied_ids.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut seen = std::collections::HashSet::new();
        applied_ids.retain(|(_, id)| seen.insert(id.clone()));

        let policies_by_id: BTreeMap<&str, &PolicyRow> =
            policies.iter().map(|p| (p.policy_id.as_str(), p)).collect();
        let manifest_by_id: BTreeMap<&str, &ManifestRow> = manifest_rows
            .iter()
            .map(|m| (m.policy_id.as_str(), m))
            .collect();

        let mut compiled: Vec<CompiledPolicy> = Vec::new();
        let mut binding_ids: Vec<String> = Vec::new();
        for (_, policy_id) in &applied_ids {
            let Some(pol) = policies_by_id.get(policy_id.as_str()) else {
                continue;
            };
            let Some(man) = manifest_by_id.get(policy_id.as_str()) else {
                continue;
            };
            if !role_applies(pol, &req.principal.role) {
                continue;
            }
            compiled.push(row_to_compiled(pol, man)?);
            binding_ids.push(policy_id.clone());
        }

        let (expanded_policies, expanded_from) = expand_tag_scoped(compiled, &tags, &req.table)?;

        let manifest = PolicyManifest {
            version: "1.0".into(),
            policies: expanded_policies,
            // The flat-file store resolves from compiled CEL rows rather
            // than Cedar EST, so the principal footprint is not recomputed
            // on this path; it stays unset (optional, backwards-compatible).
            principal_contract: None,
        };

        let identity_claims = identity_claims_for(&req.principal);
        let now = OffsetDateTime::now_utc();
        let expires_at = (now + self.ttl)
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| UcError::Invalid(format!("rfc3339 format: {e}")))?;

        let storage_uri = self
            .storage_uri_template
            .as_ref()
            .map(|tpl| tpl.replace("{table}", &req.table));

        let unsigned = ResolveBundle {
            table_uuid: stable_table_uuid(&req.table),
            compiled_manifest: manifest,
            bindings_applied: binding_ids,
            expanded_from,
            identity_claims,
            storage_credentials: self.storage_credentials_template.clone(),
            storage_uri,
            expires_at,
            signature: String::new(),
        };

        sign_bundle(unsigned, &self.secret)
    }
}

fn target_matches(target: &str, table: &str) -> bool {
    if target == "*" || target == table {
        return true;
    }
    if let Some(prefix) = target.strip_suffix(".*") {
        if let Some(dot) = table.rfind('.') {
            return &table[..dot] == prefix;
        }
    }
    if let Some(dot) = target.rfind('.') {
        if &target[dot + 1..] == table {
            return true;
        }
    }
    if let Some(dot) = table.rfind('.') {
        if &table[dot + 1..] == target {
            return true;
        }
    }
    false
}

fn selector_matches(sel: &str, principal: &Principal) -> bool {
    if sel == "*" {
        return true;
    }
    if let Some(role) = sel.strip_prefix("role:") {
        return role == principal.role;
    }
    if let Some(id) = sel.strip_prefix("principal:") {
        return id == principal.id;
    }
    if let Some(group) = sel.strip_prefix("group:") {
        if let Some(groups) = principal.attrs.get("groups") {
            return groups.split(',').any(|g| g.trim() == group);
        }
        return false;
    }
    false
}

fn role_applies(pol: &PolicyRow, role: &str) -> bool {
    match &pol.applies_to_roles {
        None => true,
        Some(r) if r.is_empty() => true,
        Some(r) => r.iter().any(|x| x == role),
    }
}

fn identity_claims_for(principal: &Principal) -> BTreeMap<String, String> {
    let mut c: BTreeMap<String, String> = BTreeMap::new();
    c.insert("role".into(), principal.role.clone());
    c.insert("principal_id".into(), principal.id.clone());
    for (k, v) in &principal.attrs.0 {
        c.insert(k.clone(), v.clone());
    }
    c
}

fn row_to_compiled(pol: &PolicyRow, man: &ManifestRow) -> Result<CompiledPolicy, UcError> {
    let effect = match pol.effect.as_str() {
        "permit" => Effect::Permit,
        "forbid" => Effect::Forbid,
        other => return Err(UcError::Invalid(format!("unknown effect {other}"))),
    };
    let filter_type = match pol.filter_type.as_str() {
        "row_filter" => FilterType::RowFilter,
        "column_mask" => FilterType::ColumnMask,
        "deny_override" => FilterType::DenyOverride,
        other => return Err(UcError::Invalid(format!("unknown filter_type {other}"))),
    };
    let applies_to = pol.applies_to_roles.as_ref().map(|r| AppliesTo {
        roles: r.clone(),
        principals: Vec::new(),
    });
    Ok(CompiledPolicy {
        id: pol.policy_id.clone(),
        effect,
        filter_type,
        target_table: pol.target_table.clone(),
        column: pol.column.clone(),
        // Tag-scoped templates are passed through here unchanged;
        // [`expand_tag_scoped`] fans them out over the tag index
        // before the manifest is assembled.
        target_tag: pol.target_tag.clone(),
        applies_to_tag: pol.applies_to_tag.clone(),
        cel_expression: man.cel_expression.clone(),
        applies_to,
        description: pol.description.clone(),
    })
}

/// Expand every tag-scoped policy in `compiled` into one concrete
/// policy per matching (table, column) tuple in `tags`.
///
/// A [`CompiledPolicy`] is *tag-scoped* iff either `target_tag` or
/// `applies_to_tag` is set. The expansion rules:
///
/// * **Candidate tables**
///   - `target_tag = Some(t)` → every active table-tagged row with
///     tag `t`.
///   - otherwise → the literal `target_table` (or `req_table` if the
///     template says `"*"`).
/// * **Candidate columns** (per candidate table)
///   - `applies_to_tag = Some(c)` → every active column-tagged row
///     on that table with tag `c`. If none match for a particular
///     table, the template does not expand for that table.
///   - otherwise → the template's literal `column` (`None` allowed).
/// * Expansion is then filtered to `req_table` — the resolver only
///   ships policies that apply to the table the engine asked about.
/// * Expanded policy ids are deterministic:
///   - `{id}@{table}` for table-level expansion
///   - `{id}@{table}:{column}` for column-level expansion
///   (this means a tag-scoped template never shadows a concrete
///   policy with the same id).
/// * Non-tag-scoped policies pass through unchanged.
/// * Retired (tombstoned) tag rows are ignored.
///
/// Returns `(expanded_policies, expanded_from_audit)`.
fn expand_tag_scoped(
    compiled: Vec<CompiledPolicy>,
    tags: &[TagRow],
    req_table: &str,
) -> Result<(Vec<CompiledPolicy>, BTreeMap<String, String>), UcError> {
    let mut out: Vec<CompiledPolicy> = Vec::with_capacity(compiled.len());
    let mut audit: BTreeMap<String, String> = BTreeMap::new();

    for policy in compiled {
        if !policy.is_tag_scoped() {
            out.push(policy);
            continue;
        }

        let candidate_tables: Vec<String> = if let Some(ttag) = &policy.target_tag {
            tags.iter()
                .filter(|r| r.is_active() && r.is_table() && r.tag == *ttag)
                .map(|r| r.entity.clone())
                .collect()
        } else if policy.target_table == "*" {
            vec![req_table.to_string()]
        } else {
            vec![policy.target_table.clone()]
        };

        for table in candidate_tables {
            if table != req_table {
                continue;
            }

            if let Some(ctag) = &policy.applies_to_tag {
                let cols: Vec<String> = tags
                    .iter()
                    .filter(|r| r.is_active() && r.is_column() && r.tag == *ctag)
                    .filter_map(|r| r.as_table_column())
                    .filter(|(t, _)| *t == table)
                    .map(|(_, c)| c.to_string())
                    .collect();
                for col in cols {
                    let expanded_id = format!("{}@{}:{}", policy.id, table, col);
                    let provenance = tag_provenance(&policy);
                    audit.insert(
                        expanded_id.clone(),
                        format!("{} ({})", policy.id, provenance),
                    );
                    out.push(CompiledPolicy {
                        id: expanded_id,
                        effect: policy.effect.clone(),
                        filter_type: policy.filter_type.clone(),
                        target_table: table.clone(),
                        column: Some(col),
                        target_tag: None,
                        applies_to_tag: None,
                        cel_expression: policy.cel_expression.clone(),
                        applies_to: policy.applies_to.clone(),
                        description: policy.description.clone(),
                    });
                }
            } else {
                let expanded_id = format!("{}@{}", policy.id, table);
                let provenance = tag_provenance(&policy);
                audit.insert(
                    expanded_id.clone(),
                    format!("{} ({})", policy.id, provenance),
                );
                out.push(CompiledPolicy {
                    id: expanded_id,
                    effect: policy.effect.clone(),
                    filter_type: policy.filter_type.clone(),
                    target_table: table,
                    column: policy.column.clone(),
                    target_tag: None,
                    applies_to_tag: None,
                    cel_expression: policy.cel_expression.clone(),
                    applies_to: policy.applies_to.clone(),
                    description: policy.description.clone(),
                });
            }
        }
    }

    Ok((out, audit))
}

/// Render a human-readable provenance string for an audit entry, e.g.
/// `target_tag=clinical,applies_to_tag=pii`.
fn tag_provenance(policy: &CompiledPolicy) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = &policy.target_tag {
        parts.push(format!("target_tag={t}"));
    }
    if let Some(c) = &policy.applies_to_tag {
        parts.push(format!("applies_to_tag={c}"));
    }
    parts.join(",")
}

fn stable_table_uuid(table: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"policast-uc/table-uuid-v1/");
    h.update(table.as_bytes());
    let digest = h.finalize();
    let bytes = &digest[..16];
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        (u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ])) & 0x0000_ffff_ffff_ffff,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileBackend;
    use crate::signature::verify;
    use crate::types::{Principal, PrincipalAttrs};
    use std::sync::Arc;

    fn examples_store() -> FileBackend {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/uc/store");
        FileBackend::new(root)
    }

    #[test]
    fn test_target_matches_variants() {
        assert!(target_matches("*", "anything"));
        assert!(target_matches("a.b.c", "a.b.c"));
        assert!(target_matches("a.b.*", "a.b.c"));
        assert!(!target_matches("a.c.*", "a.b.c"));
        assert!(target_matches("a.b.c", "c"));
        assert!(target_matches("c", "a.b.c"));
        assert!(!target_matches("orders", "patients"));
    }

    #[test]
    fn test_selector_matches_variants() {
        let p = Principal {
            id: "alice".into(),
            role: "analyst".into(),
            attrs: PrincipalAttrs::new().with("groups", "clinical,finance"),
        };
        assert!(selector_matches("*", &p));
        assert!(selector_matches("role:analyst", &p));
        assert!(!selector_matches("role:physician", &p));
        assert!(selector_matches("principal:alice", &p));
        assert!(!selector_matches("principal:bob", &p));
        assert!(selector_matches("group:clinical", &p));
        assert!(!selector_matches("group:engineering", &p));
    }

    #[tokio::test]
    async fn test_resolve_analyst_us_east() {
        let core = ResolverCore::new(Arc::new(examples_store()), b"s".to_vec());
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new().with("region", "us-east"),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();
        verify(&bundle, b"s").unwrap();
        let ids: Vec<&str> = bundle
            .compiled_manifest
            .policies
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        // `row_filter_region` is now a table-level Cedar template
        // keyed off @target_tag("clinical"). The resolver expands it
        // to one concrete row filter per clinical-tagged table; on
        // the patients resolve the expanded id encodes the table.
        assert!(ids.contains(&"row_filter_region@hospital.clinical.patients"));
        // `row_filter_physician` is intentionally kept concrete (its
        // CEL references a patient-specific column), so the id is
        // unchanged — this catches regressions where a future edit
        // accidentally tag-scopes it.
        assert!(!ids.contains(&"row_filter_physician"));
        // column masks arrive as tag-expanded children of the two
        // templates bound to this table; the expanded id carries the
        // (table, column) coordinates so engine logs point back at
        // the concrete resource even though the author wrote a
        // tag-scoped rule.
        assert!(ids.contains(&"column_mask_by_pii_tag@hospital.clinical.patients:ssn"));
        assert!(ids.contains(&"column_mask_by_phi_tag@hospital.clinical.patients:diagnosis"));
        assert!(ids.contains(&"deny_legal_hold"));
        assert_eq!(bundle.identity_claims["role"], "analyst");
        assert_eq!(bundle.identity_claims["region"], "us-east");
        // The audit map preserves the template lineage of every
        // expanded policy so downstream consumers (engine logs,
        // governance dashboards) can trace a concrete rule back to
        // the template + tag that produced it — including the newly
        // tag-scoped row filter.
        assert_eq!(
            bundle
                .expanded_from
                .get("row_filter_region@hospital.clinical.patients")
                .map(String::as_str),
            Some("row_filter_region (target_tag=clinical)")
        );
        assert_eq!(
            bundle
                .expanded_from
                .get("column_mask_by_pii_tag@hospital.clinical.patients:ssn")
                .map(String::as_str),
            Some("column_mask_by_pii_tag (applies_to_tag=pii)")
        );
        assert_eq!(
            bundle
                .expanded_from
                .get("column_mask_by_phi_tag@hospital.clinical.patients:diagnosis")
                .map(String::as_str),
            Some("column_mask_by_phi_tag (applies_to_tag=phi)")
        );
    }

    #[tokio::test]
    async fn test_resolve_physician() {
        let core = ResolverCore::new(Arc::new(examples_store()), b"s".to_vec());
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "dr-smith".into(),
                role: "physician".into(),
                attrs: PrincipalAttrs::new(),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();
        let ids: Vec<&str> = bundle
            .compiled_manifest
            .policies
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        // row_filter_physician is bound via role:physician and stays
        // concrete (no tag expansion), so the id is unchanged.
        assert!(ids.contains(&"row_filter_physician"));
        // The analyst-only row filter is tag-scoped now, but role
        // filtering runs *before* expansion, so a physician never
        // even sees a candidate — expanded id absent.
        assert!(!ids.contains(&"row_filter_region@hospital.clinical.patients"));
        assert!(!ids.contains(&"row_filter_region"));
        // physicians are exempt from mask policies — but the mask is
        // bound to all principals, so it still ships in the manifest;
        // the engine's constant-fold makes it a no-op. Post-template
        // migration the id is the expanded form.
        assert!(ids.contains(&"column_mask_by_pii_tag@hospital.clinical.patients:ssn"));
    }

    /// Equivalence test: prove the template path in
    /// `examples/uc/store` produces the same concrete (target_table,
    /// column, filter_type, effect) set that the *hand-written*
    /// pre-template concrete policies did. CEL expressions are
    /// compared modulo the obsolete `resource.table_name` guard that
    /// the old concrete CEL carried — the tag expander supplies
    /// target_table so the guard is no longer needed.
    ///
    /// This is the test that justifies migrating the shipped example
    /// store from concrete rows to templates: if it ever flips red,
    /// the template path has diverged from what engines used to see.
    #[tokio::test]
    async fn test_template_and_concrete_paths_are_equivalent_for_masks() {
        let core = ResolverCore::new(Arc::new(examples_store()), b"s".to_vec());
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new().with("region", "us-east"),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();

        // Pull out just the column mask policies the engine will see.
        let masks: Vec<(&str, Option<&str>, &str)> = bundle
            .compiled_manifest
            .policies
            .iter()
            .filter(|p| p.filter_type == FilterType::ColumnMask)
            .map(|p| {
                (
                    p.target_table.as_str(),
                    p.column.as_deref(),
                    p.cel_expression.as_str(),
                )
            })
            .collect();

        // Pre-template, the analyst saw:
        //   column_mask_ssn on (patients, ssn)
        //   column_mask_diagnosis on (patients, diagnosis)
        // Both with the same CEL body (modulo the obsolete table_name
        // guard). Post-template the expansion must produce exactly
        // those two (table, column) tuples — no more, no less.
        let mut table_cols: Vec<(&str, &str)> = masks
            .iter()
            .map(|(t, c, _)| (*t, c.expect("column_mask must carry column")))
            .collect();
        table_cols.sort();
        assert_eq!(
            table_cols,
            vec![
                ("hospital.clinical.patients", "diagnosis"),
                ("hospital.clinical.patients", "ssn"),
            ]
        );

        // All masks share the same unless-based CEL predicate on the
        // principal role; the resource.table_name guard from the
        // pre-template era has been removed because target_table is
        // populated by expansion.
        let expected_cel =
            "!(((principal.role == \"admin\") || (principal.role == \"physician\")))";
        for (_, _, cel) in &masks {
            assert_eq!(*cel, expected_cel);
        }

        // Every mask is a forbid — same as the old concrete rules.
        assert!(bundle
            .compiled_manifest
            .policies
            .iter()
            .filter(|p| p.filter_type == FilterType::ColumnMask)
            .all(|p| p.effect == Effect::Forbid));
    }

    /// Companion to the masks equivalence test, pinning the behavior
    /// of `row_filter_region` across its template migration.
    ///
    /// Before: a single concrete row filter authored as
    /// `@target_table("patients")` with CEL
    /// `(resource.region == principal.region)`.
    /// After: a `@target_tag("clinical")` template that expands to
    /// one concrete row filter per clinical-tagged table. On the
    /// patients resolve that has to be observationally identical —
    /// same (target_table, cel_expression, effect, applies_to) — or
    /// running engines would see a behavior change on upgrade.
    ///
    /// If a future tag flip makes another table `clinical`, this
    /// test still passes (it filters to the patients entry), but the
    /// template will also fire on the new table, which is the
    /// intended generalization.
    #[tokio::test]
    async fn test_template_and_concrete_paths_are_equivalent_for_row_filter_region() {
        let core = ResolverCore::new(Arc::new(examples_store()), b"s".to_vec());
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new().with("region", "us-east"),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();

        let region_filter = bundle
            .compiled_manifest
            .policies
            .iter()
            .find(|p| {
                p.filter_type == FilterType::RowFilter
                    && p.id.starts_with("row_filter_region")
                    && p.target_table == "hospital.clinical.patients"
            })
            .expect("tag-expanded row_filter_region must be present for an analyst on patients");

        assert_eq!(
            region_filter.cel_expression,
            "(resource.region == principal.region)"
        );
        assert_eq!(region_filter.effect, Effect::Permit);
        assert_eq!(region_filter.column, None);
        let applies_to = region_filter
            .applies_to
            .as_ref()
            .expect("applies_to roles must be preserved across expansion");
        assert_eq!(applies_to.roles, vec!["analyst".to_string()]);

        // And the lineage survives as an audit entry — engines and
        // governance UIs can trace the concrete rule back to its
        // tag-scoped template.
        assert_eq!(
            bundle
                .expanded_from
                .get("row_filter_region@hospital.clinical.patients")
                .map(String::as_str),
            Some("row_filter_region (target_tag=clinical)")
        );
    }

    #[tokio::test]
    async fn test_resolve_unknown_table_is_empty() {
        let core = ResolverCore::new(Arc::new(examples_store()), b"s".to_vec());
        let req = ResolveRequest {
            table: "does.not.exist".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new(),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();
        assert!(bundle.compiled_manifest.policies.is_empty());
    }

    #[tokio::test]
    async fn test_signed_bundle_verifies() {
        let core = ResolverCore::new(Arc::new(examples_store()), b"sekret".to_vec());
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: Default::default(),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();
        verify(&bundle, b"sekret").unwrap();
        assert!(verify(&bundle, b"wrong-secret").is_err());
    }

    #[test]
    fn test_stable_table_uuid_is_deterministic() {
        let a = stable_table_uuid("hospital.clinical.patients");
        let b = stable_table_uuid("hospital.clinical.patients");
        assert_eq!(a, b);
        assert_ne!(a, stable_table_uuid("other.table"));
        // UUID-ish format check.
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
    }

    // -----------------------------------------------------------------
    // Tag-expansion unit tests. These exercise `expand_tag_scoped` in
    // isolation with handcrafted CompiledPolicy + TagRow inputs so the
    // examples/ fixture can evolve independently.
    // -----------------------------------------------------------------

    fn tag_table(entity: &str, tag: &str) -> TagRow {
        TagRow {
            entity: entity.into(),
            entity_kind: "table".into(),
            tag: tag.into(),
            set_by: None,
            set_at: None,
            retired_at: None,
        }
    }

    fn tag_column(entity: &str, tag: &str) -> TagRow {
        TagRow {
            entity: entity.into(),
            entity_kind: "column".into(),
            tag: tag.into(),
            set_by: None,
            set_at: None,
            retired_at: None,
        }
    }

    fn base_template(id: &str, ft: FilterType) -> CompiledPolicy {
        CompiledPolicy {
            id: id.into(),
            effect: Effect::Permit,
            filter_type: ft,
            target_table: "*".into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: "true".into(),
            applies_to: None,
            description: None,
        }
    }

    #[test]
    fn expand_passes_through_non_tag_scoped() {
        let p = CompiledPolicy {
            id: "concrete".into(),
            target_table: "db.s.t".into(),
            ..base_template("concrete", FilterType::RowFilter)
        };
        let (out, audit) = expand_tag_scoped(vec![p.clone()], &[], "db.s.t").unwrap();
        assert_eq!(out, vec![p]);
        assert!(audit.is_empty());
    }

    #[test]
    fn expand_single_tag_table_scoped_row_filter() {
        let template = CompiledPolicy {
            target_tag: Some("clinical".into()),
            ..base_template("rf_by_clinical", FilterType::RowFilter)
        };
        let tags = vec![
            tag_table("hospital.clinical.patients", "clinical"),
            tag_table("other.db.table", "finance"),
        ];
        let (out, audit) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "rf_by_clinical@hospital.clinical.patients");
        assert_eq!(out[0].target_table, "hospital.clinical.patients");
        assert!(out[0].target_tag.is_none());
        assert!(out[0].applies_to_tag.is_none());
        assert_eq!(
            audit
                .get("rf_by_clinical@hospital.clinical.patients")
                .unwrap(),
            "rf_by_clinical (target_tag=clinical)"
        );
    }

    #[test]
    fn expand_column_mask_via_applies_to_tag() {
        let template = CompiledPolicy {
            applies_to_tag: Some("pii".into()),
            ..base_template("mask_pii", FilterType::ColumnMask)
        };
        let tags = vec![
            tag_column("hospital.clinical.patients:ssn", "pii"),
            tag_column("hospital.clinical.patients:dob", "pii"),
            tag_column("hospital.clinical.patients:diagnosis", "phi"),
        ];
        let (out, audit) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        let mut ids: Vec<&str> = out.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "mask_pii@hospital.clinical.patients:dob",
                "mask_pii@hospital.clinical.patients:ssn",
            ]
        );
        for p in &out {
            assert_eq!(p.filter_type, FilterType::ColumnMask);
            assert!(p.column.is_some());
        }
        assert_eq!(audit.len(), 2);
    }

    #[test]
    fn expand_drops_template_when_no_tag_matches() {
        let template = CompiledPolicy {
            applies_to_tag: Some("pii".into()),
            ..base_template("mask_pii", FilterType::ColumnMask)
        };
        let tags = vec![tag_column("other.db.table:other_col", "pii")];
        let (out, audit) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        assert!(out.is_empty());
        assert!(audit.is_empty());
    }

    #[test]
    fn expand_ignores_retired_tags() {
        let template = CompiledPolicy {
            applies_to_tag: Some("pii".into()),
            ..base_template("mask_pii", FilterType::ColumnMask)
        };
        let tags = vec![
            TagRow {
                retired_at: Some("2025-01-01T00:00:00Z".into()),
                ..tag_column("hospital.clinical.patients:ssn", "pii")
            },
            tag_column("hospital.clinical.patients:dob", "pii"),
        ];
        let (out, _) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "mask_pii@hospital.clinical.patients:dob");
    }

    #[test]
    fn expand_combines_target_tag_and_applies_to_tag() {
        // Only expand (clinical table × pii column) pairs.
        let template = CompiledPolicy {
            target_tag: Some("clinical".into()),
            applies_to_tag: Some("pii".into()),
            ..base_template("mask_clinical_pii", FilterType::ColumnMask)
        };
        let tags = vec![
            tag_table("hospital.clinical.patients", "clinical"),
            tag_table("finance.payroll.employees", "finance"),
            tag_column("hospital.clinical.patients:ssn", "pii"),
            // finance.employees:ssn is pii but its table is not clinical
            tag_column("finance.payroll.employees:ssn", "pii"),
        ];
        let (out, audit) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].id,
            "mask_clinical_pii@hospital.clinical.patients:ssn"
        );
        assert_eq!(
            audit
                .get("mask_clinical_pii@hospital.clinical.patients:ssn")
                .unwrap(),
            "mask_clinical_pii (target_tag=clinical,applies_to_tag=pii)"
        );
    }

    #[test]
    fn expand_filters_to_requested_table_only() {
        // template tagged clinical matches two tables in the tag index,
        // but only the one the request is about should appear in the
        // bundle — the other would leak governance state to the engine.
        let template = CompiledPolicy {
            target_tag: Some("clinical".into()),
            ..base_template("rf_by_clinical", FilterType::RowFilter)
        };
        let tags = vec![
            tag_table("hospital.clinical.patients", "clinical"),
            tag_table("hospital.clinical.visits", "clinical"),
        ];
        let (out, _) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_table, "hospital.clinical.patients");
    }

    #[test]
    fn expand_preserves_applies_to_role_filter() {
        let template = CompiledPolicy {
            applies_to_tag: Some("pii".into()),
            applies_to: Some(AppliesTo {
                roles: vec!["analyst".into()],
                principals: Vec::new(),
            }),
            ..base_template("mask_pii_analysts", FilterType::ColumnMask)
        };
        let tags = vec![tag_column("hospital.clinical.patients:ssn", "pii")];
        let (out, _) =
            expand_tag_scoped(vec![template], &tags, "hospital.clinical.patients").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].applies_to.as_ref().unwrap().roles,
            vec!["analyst".to_string()]
        );
    }

    #[test]
    fn expand_multi_template_mixes_concrete_and_expanded() {
        let concrete = CompiledPolicy {
            target_table: "hospital.clinical.patients".into(),
            column: Some("ssn".into()),
            ..base_template("mask_ssn_concrete", FilterType::ColumnMask)
        };
        let template = CompiledPolicy {
            applies_to_tag: Some("pii".into()),
            ..base_template("mask_by_pii", FilterType::ColumnMask)
        };
        let tags = vec![tag_column("hospital.clinical.patients:dob", "pii")];
        let (out, audit) = expand_tag_scoped(
            vec![concrete, template],
            &tags,
            "hospital.clinical.patients",
        )
        .unwrap();
        let ids: Vec<&str> = out.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"mask_ssn_concrete"));
        assert!(ids.contains(&"mask_by_pii@hospital.clinical.patients:dob"));
        assert_eq!(audit.len(), 1);
        assert!(audit.contains_key("mask_by_pii@hospital.clinical.patients:dob"));
    }
}
