use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::cedar_parser::{ConditionKind, ParsedPolicy};
use crate::cel_emitter::{cedar_expr_to_cel, collect_principal_attrs};
use crate::error::PolicastError;
use crate::model::{AppliesTo, CompiledPolicy, Effect, FilterType, PrincipalContract};
use crate::profile::{
    non_canonical_principal_attrs, validate_profile, PolicyProfile, CANONICAL_PRINCIPAL_ATTRS,
};

/// A versioned manifest of compiled policies, portable as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyManifest {
    pub version: String,
    pub policies: Vec<CompiledPolicy>,
    /// Compile-time footprint of the `principal.*` attributes referenced
    /// across all policies. `None` when no policy references the principal
    /// (keeps the JSON identical to pre-footprint manifests for older
    /// consumers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_contract: Option<PrincipalContract>,
}

impl PolicyManifest {
    pub fn new() -> Self {
        Self {
            version: "1.0".to_string(),
            policies: Vec::new(),
            principal_contract: None,
        }
    }

    /// Compile a set of parsed Cedar policies into the manifest.
    ///
    /// Each policy's `when`/`unless` conditions are translated to CEL
    /// expressions. The `filter_type` and `target_table` are inferred from
    /// annotations on the Cedar policy (e.g. `@filter_type("row_filter")`
    /// and `@target_table("patients")`). The `principal_contract` footprint
    /// is recomputed across all policies after each batch.
    pub fn compile_policies(&mut self, parsed: &[ParsedPolicy]) -> Result<(), PolicastError> {
        let mut principal_attrs: BTreeSet<String> = self.collected_principal_attrs();

        for policy in parsed {
            let compiled = compile_single_policy(policy)?;
            for cond in &policy.conditions {
                principal_attrs.extend(collect_principal_attrs(&cond.body));
            }
            self.policies.push(compiled);
        }

        self.principal_contract = if principal_attrs.is_empty() {
            None
        } else {
            Some(PrincipalContract {
                required_attributes: principal_attrs.into_iter().collect(),
            })
        };
        Ok(())
    }

    /// The principal attributes already recorded on this manifest, used as
    /// the accumulator seed when compiling further batches of policies.
    fn collected_principal_attrs(&self) -> BTreeSet<String> {
        self.principal_contract
            .as_ref()
            .map(|c| c.required_attributes.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> Result<String, PolicastError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(json: &str) -> Result<Self, PolicastError> {
        Ok(serde_json::from_str(json)?)
    }

    /// Return all policies whose `target_table` matches `table`.
    ///
    /// Supports three shapes on `target_table`:
    ///   - `*`               — matches any table;
    ///   - `a.b.*`           — matches any table in schema `a.b`;
    ///   - exact match.
    ///
    /// To bridge the short-name tables used in the local Delta demos
    /// (`patients`) with UC's three-part names
    /// (`hospital.clinical.patients`), the last segment of either side
    /// is also accepted. This keeps existing tests working when the
    /// manifest rows come from UC.
    pub fn policies_for_table(&self, table: &str) -> Vec<&CompiledPolicy> {
        self.policies
            .iter()
            .filter(|p| crate::policy_store::table_matches(&p.target_table, table))
            .collect()
    }
}

impl Default for PolicyManifest {
    fn default() -> Self {
        Self::new()
    }
}

fn compile_single_policy(policy: &ParsedPolicy) -> Result<CompiledPolicy, PolicastError> {
    let effect = match policy.effect.as_str() {
        "permit" => Effect::Permit,
        "forbid" => Effect::Forbid,
        other => return Err(PolicastError::CelEmit(format!("Unknown effect: {other}"))),
    };

    let filter_type = policy
        .annotations
        .get("filter_type")
        .map(|ft| match ft.as_str() {
            "row_filter" => FilterType::RowFilter,
            "column_mask" => FilterType::ColumnMask,
            "deny_override" => FilterType::DenyOverride,
            _ => FilterType::RowFilter,
        })
        .unwrap_or_else(|| {
            if effect == Effect::Forbid {
                FilterType::DenyOverride
            } else {
                FilterType::RowFilter
            }
        });

    let target_table_annot = policy.annotations.get("target_table").cloned();
    let target_tag = policy
        .annotations
        .get("target_tag")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let column_annot = policy.annotations.get("column").cloned();
    let applies_to_tag = policy
        .annotations
        .get("applies_to_tag")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Validate: at most one of (target_table, target_tag) set, and at
    // most one of (column, applies_to_tag) set. Either may be omitted
    // (target_table defaults to "*"; column/tag left unset on row
    // filters and deny overrides).
    if target_table_annot.is_some() && target_tag.is_some() {
        return Err(PolicastError::CelEmit(format!(
            "policy {:?}: @target_table and @target_tag are mutually exclusive",
            policy.id
        )));
    }
    if column_annot.is_some() && applies_to_tag.is_some() {
        return Err(PolicastError::CelEmit(format!(
            "policy {:?}: @column and @applies_to_tag are mutually exclusive",
            policy.id
        )));
    }

    let target_table = target_table_annot.unwrap_or_else(|| "*".to_string());
    let column = column_annot;

    let description = policy.annotations.get("description").cloned();

    let mut when_parts: Vec<String> = Vec::new();
    let mut unless_parts: Vec<String> = Vec::new();

    for cond in &policy.conditions {
        let cel = cedar_expr_to_cel(&cond.body)?;
        match cond.kind {
            ConditionKind::When => when_parts.push(cel),
            ConditionKind::Unless => unless_parts.push(cel),
        }
    }

    // Validate the policy against the shape of its profile. Structural
    // contradictions are hard errors; advisory warnings (e.g. a missing
    // `when` guard or a non-canonical principal attribute) go to stderr.
    let profile = PolicyProfile::from_filter_type(&filter_type);
    let warnings = validate_profile(
        &policy.id,
        profile,
        effect,
        when_parts.len(),
        unless_parts.len(),
        column.is_some(),
        applies_to_tag.is_some(),
    )?;
    for warning in warnings {
        eprintln!("policast: {warning}");
    }

    let mut referenced_principal_attrs = std::collections::BTreeSet::new();
    for cond in &policy.conditions {
        referenced_principal_attrs.extend(collect_principal_attrs(&cond.body));
    }
    for attr in non_canonical_principal_attrs(&referenced_principal_attrs) {
        eprintln!(
            "policast: policy {:?} references non-canonical principal attribute `{attr}` (canonical: {})",
            policy.id,
            CANONICAL_PRINCIPAL_ATTRS.join(", ")
        );
    }

    // Build the final CEL expression:
    //   when-clauses are ANDed together,
    //   unless-clauses are negated and ANDed.
    let mut all_parts: Vec<String> = Vec::new();
    for w in &when_parts {
        all_parts.push(w.clone());
    }
    for u in &unless_parts {
        all_parts.push(format!("!({u})"));
    }

    let cel_expression = if all_parts.is_empty() {
        "true".to_string()
    } else if all_parts.len() == 1 {
        all_parts.into_iter().next().unwrap()
    } else {
        all_parts.join(" && ")
    };

    let applies_to = policy.annotations.get("roles").map(|roles_str| {
        let roles: Vec<String> = roles_str.split(',').map(|s| s.trim().to_string()).collect();
        AppliesTo {
            roles,
            principals: Vec::new(),
        }
    });

    Ok(CompiledPolicy {
        id: policy.id.clone(),
        effect,
        filter_type,
        target_table,
        column,
        target_tag,
        applies_to_tag,
        cel_expression,
        applies_to,
        description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cedar_parser::parse_policies;

    #[test]
    fn test_compile_simple_policy() {
        let cedar = r#"
            @id("test_row_filter")
            @filter_type("row_filter")
            @target_table("patients")
            permit (
                principal,
                action == Action::"query",
                resource
            )
            when {
                resource.region == principal.region
            };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        assert_eq!(manifest.policies.len(), 1);
        let p = &manifest.policies[0];
        assert_eq!(p.id, "test_row_filter");
        assert_eq!(p.effect, Effect::Permit);
        assert_eq!(p.filter_type, FilterType::RowFilter);
        assert_eq!(p.target_table, "patients");
        assert!(p.cel_expression.contains("resource.region"));
        assert!(p.cel_expression.contains("principal.region"));
        assert!(p.target_tag.is_none());
        assert!(p.applies_to_tag.is_none());
        assert!(!p.is_tag_scoped());
    }

    /// A policy scoped by `@target_tag` should record the tag, default
    /// `target_table` to `"*"`, and report itself as tag-scoped.
    #[test]
    fn test_target_tag_annotation() {
        let cedar = r#"
            @id("row_filter_clinical")
            @filter_type("row_filter")
            @target_tag("clinical")
            permit (
                principal,
                action == Action::"query",
                resource
            )
            when {
                resource.region == principal.region
            };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let p = &manifest.policies[0];
        assert_eq!(p.target_tag.as_deref(), Some("clinical"));
        assert_eq!(p.target_table, "*");
        assert!(p.column.is_none());
        assert!(p.applies_to_tag.is_none());
        assert!(p.is_tag_scoped());
    }

    /// A column-mask policy may use `@applies_to_tag` instead of
    /// `@column`; the column slot stays empty until the resolver
    /// expands the policy.
    #[test]
    fn test_applies_to_tag_annotation() {
        let cedar = r#"
            @id("mask_pii_non_clinical")
            @filter_type("column_mask")
            @target_table("patients")
            @applies_to_tag("pii")
            forbid (
                principal,
                action == Action::"query",
                resource
            )
            when {
                resource.table_name == "patients"
            }
            unless {
                principal.role == "physician"
            };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let p = &manifest.policies[0];
        assert_eq!(p.filter_type, FilterType::ColumnMask);
        assert_eq!(p.applies_to_tag.as_deref(), Some("pii"));
        assert!(p.column.is_none());
        assert_eq!(p.target_table, "patients");
        assert!(p.is_tag_scoped());
    }

    /// Both tag annotations may coexist when a template scopes by
    /// table-tag AND column-tag (e.g. "mask PII on any clinical table").
    #[test]
    fn test_both_tag_annotations() {
        let cedar = r#"
            @id("mask_pii_on_clinical")
            @filter_type("column_mask")
            @target_tag("clinical")
            @applies_to_tag("pii")
            forbid (
                principal,
                action,
                resource
            )
            unless {
                principal.role == "physician"
            };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let p = &manifest.policies[0];
        assert_eq!(p.target_tag.as_deref(), Some("clinical"));
        assert_eq!(p.applies_to_tag.as_deref(), Some("pii"));
        assert_eq!(p.target_table, "*");
        assert!(p.column.is_none());
    }

    /// @target_table and @target_tag are mutually exclusive.
    #[test]
    fn test_target_table_and_target_tag_rejected() {
        let cedar = r#"
            @id("broken")
            @filter_type("row_filter")
            @target_table("patients")
            @target_tag("clinical")
            permit (principal, action, resource);
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        let err = manifest
            .compile_policies(&parsed)
            .expect_err("should reject mutually exclusive annotations");
        assert!(err.to_string().contains("mutually exclusive"));
    }

    /// @column and @applies_to_tag are mutually exclusive.
    #[test]
    fn test_column_and_applies_to_tag_rejected() {
        let cedar = r#"
            @id("broken_column")
            @filter_type("column_mask")
            @target_table("patients")
            @column("ssn")
            @applies_to_tag("pii")
            forbid (principal, action, resource);
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        let err = manifest
            .compile_policies(&parsed)
            .expect_err("should reject mutually exclusive annotations");
        assert!(err.to_string().contains("mutually exclusive"));
    }

    /// Empty tag strings are treated as absent so authors cannot
    /// accidentally publish `@target_tag("")`.
    #[test]
    fn test_empty_tag_treated_as_absent() {
        let cedar = r#"
            @id("empty_tag")
            @filter_type("row_filter")
            @target_table("patients")
            @target_tag("")
            permit (principal, action, resource);
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let p = &manifest.policies[0];
        assert!(p.target_tag.is_none());
        assert!(!p.is_tag_scoped());
    }

    /// Tag fields must survive the JSON roundtrip and remain optional
    /// (absent when unset) so that older manifest consumers keep
    /// parsing.
    #[test]
    fn test_tag_fields_json_roundtrip() {
        let cedar = r#"
            @id("tagged_roundtrip")
            @filter_type("column_mask")
            @applies_to_tag("phi")
            forbid (principal, action, resource);
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let json = manifest.to_json().unwrap();
        // When the tag is set, it appears in the JSON exactly once.
        assert!(
            json.contains("\"applies_to_tag\": \"phi\""),
            "json should carry the tag: {json}"
        );
        // When target_tag is unset, it is skipped (no noisy nulls).
        assert!(!json.contains("\"target_tag\""));

        let reloaded = PolicyManifest::from_json(&json).unwrap();
        assert_eq!(reloaded.policies[0].applies_to_tag.as_deref(), Some("phi"));
        assert!(reloaded.policies[0].target_tag.is_none());
    }

    #[test]
    fn test_compile_forbid_with_unless() {
        let cedar = r#"
            @id("deny_legal_hold")
            @filter_type("deny_override")
            @target_table("patients")
            forbid (
                principal,
                action,
                resource
            )
            when {
                resource.legal_hold == true
            }
            unless {
                principal.role == "legal"
            };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let p = &manifest.policies[0];
        assert_eq!(p.effect, Effect::Forbid);
        assert_eq!(p.filter_type, FilterType::DenyOverride);
        assert!(p.cel_expression.contains("resource.legal_hold"));
        assert!(p.cel_expression.contains("!("));
    }

    /// Compiling policies records the union of referenced principal
    /// attributes into `principal_contract`, sorted and de-duplicated.
    #[test]
    fn test_principal_contract_footprint() {
        let cedar = r#"
            @id("region")
            @filter_type("row_filter")
            @target_table("patients")
            permit (principal, action, resource)
            when { resource.region == principal.region };

            @id("physician")
            @filter_type("row_filter")
            @target_table("patients")
            permit (principal, action, resource)
            when { resource.treating_physician == principal.name };

            @id("mask")
            @filter_type("column_mask")
            @applies_to_tag("pii")
            forbid (principal, action, resource)
            unless { principal.role == "admin" };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let contract = manifest
            .principal_contract
            .expect("contract should be populated");
        assert_eq!(
            contract.required_attributes,
            vec!["name".to_string(), "region".to_string(), "role".to_string()]
        );
    }

    /// A policy set that never references the principal leaves the
    /// contract unset, so the JSON matches pre-footprint manifests.
    #[test]
    fn test_principal_contract_absent_when_unreferenced() {
        let cedar = r#"
            @id("amount")
            @target_table("orders")
            permit (principal, action, resource)
            when { resource.amount > 0 };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        assert!(manifest.principal_contract.is_none());
        let json = manifest.to_json().unwrap();
        assert!(!json.contains("principal_contract"));
    }

    /// The contract accumulates across successive `compile_policies` calls.
    #[test]
    fn test_principal_contract_accumulates_across_batches() {
        let mut manifest = PolicyManifest::new();
        manifest
            .compile_policies(
                &parse_policies(
                    r#"@id("a") @target_table("t")
                       permit (principal, action, resource)
                       when { resource.region == principal.region };"#,
                )
                .unwrap(),
            )
            .unwrap();
        manifest
            .compile_policies(
                &parse_policies(
                    r#"@id("b") @target_table("t")
                       permit (principal, action, resource)
                       when { resource.x == principal.clearance };"#,
                )
                .unwrap(),
            )
            .unwrap();

        let contract = manifest.principal_contract.unwrap();
        assert_eq!(
            contract.required_attributes,
            vec!["clearance".to_string(), "region".to_string()]
        );
    }

    /// The contract survives the JSON roundtrip.
    #[test]
    fn test_principal_contract_json_roundtrip() {
        let cedar = r#"
            @id("region")
            @target_table("patients")
            permit (principal, action, resource)
            when { resource.region == principal.region };
        "#;
        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let json = manifest.to_json().unwrap();
        assert!(json.contains("\"principal_contract\""));
        assert!(json.contains("\"region\""));

        let reloaded = PolicyManifest::from_json(&json).unwrap();
        assert_eq!(
            reloaded.principal_contract.unwrap().required_attributes,
            vec!["region".to_string()]
        );
    }

    #[test]
    fn test_manifest_json_roundtrip() {
        let cedar = r#"
            @id("roundtrip_test")
            @target_table("orders")
            permit (
                principal,
                action,
                resource
            )
            when {
                resource.amount > 0
            };
        "#;

        let parsed = parse_policies(cedar).unwrap();
        let mut manifest = PolicyManifest::new();
        manifest.compile_policies(&parsed).unwrap();

        let json = manifest.to_json().unwrap();
        let reloaded = PolicyManifest::from_json(&json).unwrap();
        assert_eq!(reloaded.policies.len(), 1);
        assert_eq!(reloaded.policies[0].id, "roundtrip_test");
    }
}
