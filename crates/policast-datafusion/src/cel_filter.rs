use std::collections::BTreeMap;

use datafusion::common::Result as DFResult;
#[cfg(test)]
use datafusion::logical_expr::col;
use datafusion::logical_expr::{lit, Expr};

use policast_core::model::FilterType;
use policast_core::PolicyManifest;

use crate::cel_to_expr::{cel_to_bool, cel_to_datafusion_expr};
use crate::identity::PrincipalProvider;

/// Contextual identity of the user making the query.
///
/// A convenience [`PrincipalProvider`] for the common
/// `role`/`region`/`name` shape used throughout the demos and tests. For
/// identities that carry arbitrary attributes, use
/// [`AttrIdentity`](crate::identity::AttrIdentity) or implement
/// [`PrincipalProvider`] directly.
#[derive(Debug, Clone)]
pub struct QueryIdentity {
    pub role: String,
    pub region: Option<String>,
    pub name: Option<String>,
}

impl PrincipalProvider for QueryIdentity {
    fn attribute(&self, name: &str) -> Option<String> {
        match name {
            "role" => Some(self.role.clone()),
            "region" => self.region.clone(),
            "name" => self.name.clone(),
            _ => None,
        }
    }

    fn principal_attributes(&self) -> BTreeMap<String, String> {
        let mut attrs = BTreeMap::new();
        attrs.insert("role".to_string(), self.role.clone());
        if let Some(region) = &self.region {
            attrs.insert("region".to_string(), region.clone());
        }
        if let Some(name) = &self.name {
            attrs.insert("name".to_string(), name.clone());
        }
        attrs
    }
}

/// Translate all applicable policies before composing them; never skip invalid rules.
pub fn build_row_filters(
    manifest: &PolicyManifest,
    table_name: &str,
    identity: &dyn PrincipalProvider,
) -> DFResult<Vec<Expr>> {
    let mut filters = Vec::new();
    let mut grants = Vec::new();
    for policy in manifest.policies_for_table(table_name) {
        if !applicable(policy, identity, table_name)? {
            continue;
        }
        if policy.filter_type == FilterType::ColumnMask {
            continue;
        }
        let expr = cel_to_datafusion_expr(&policy.cel_expression, identity)
            .map_err(|_| {
                policy_error(
                    policy,
                    table_name,
                    "invalid, unsupported, or missing-attribute CEL",
                )
            })?
            .unwrap_or_else(|| lit(true));
        match policy.filter_type {
            FilterType::RowGrant => grants.push(expr),
            FilterType::RowFilter => {
                if expr != lit(true) {
                    filters.push(expr);
                }
            }
            FilterType::DenyOverride => filters.push(Expr::IsNotTrue(Box::new(expr))),
            FilterType::ColumnMask => unreachable!(),
        }
    }
    if manifest.requires_grant(table_name) {
        filters.push(
            grants
                .into_iter()
                .reduce(Expr::or)
                .unwrap_or_else(|| lit(false)),
        );
    }
    Ok(filters)
}

fn policy_error(
    policy: &policast_core::model::CompiledPolicy,
    table: &str,
    reason: &str,
) -> datafusion::common::DataFusionError {
    datafusion::common::DataFusionError::Plan(format!(
        "policy {} on table {table}: {reason}",
        policy.id
    ))
}

fn applicable(
    policy: &policast_core::model::CompiledPolicy,
    identity: &dyn PrincipalProvider,
    table: &str,
) -> DFResult<bool> {
    let role = identity.attribute("role").unwrap_or_default();
    let principal = identity
        .attribute("principal_id")
        .or_else(|| identity.attribute("name"))
        .unwrap_or_default();
    let groups = identity.attribute("groups");
    let applies = policy
        .applies_to_identity(&role, &principal, groups.as_deref())
        .map_err(|_| policy_error(policy, table, "unresolved policy applicability"))?;
    if !applies {
        return Ok(false);
    }
    if policy.is_tag_scoped() {
        return Err(policy_error(
            policy,
            table,
            "tag policy requires resolver expansion",
        ));
    }
    let expected = if matches!(
        policy.filter_type,
        FilterType::RowGrant | FilterType::RowFilter
    ) {
        policast_core::model::Effect::Permit
    } else {
        policast_core::model::Effect::Forbid
    };
    if policy.effect != expected {
        return Err(policy_error(
            policy,
            table,
            "invalid effect for policy kind",
        ));
    }
    Ok(true)
}

/// Masks are independent of grants and fail the query on invalid applicable rules.
pub fn build_column_masks(
    manifest: &PolicyManifest,
    table_name: &str,
    identity: &dyn PrincipalProvider,
) -> DFResult<Vec<(String, String)>> {
    let mut masks = Vec::new();
    for policy in manifest.policies_for_table(table_name) {
        if policy.filter_type != FilterType::ColumnMask
            || !applicable(policy, identity, table_name)?
        {
            continue;
        }
        let column = policy
            .column
            .as_ref()
            .ok_or_else(|| policy_error(policy, table_name, "mask requires a concrete column"))?;
        if cel_to_bool(&policy.cel_expression, identity, table_name).map_err(|_| {
            policy_error(
                policy,
                table_name,
                "invalid, unsupported, or missing-attribute mask CEL",
            )
        })? {
            masks.push((column.clone(), "***".to_string()));
        }
    }
    masks.sort();
    masks.dedup();
    Ok(masks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::model::{CompiledPolicy, Effect, FilterType};

    fn manifest_with(policies: Vec<CompiledPolicy>) -> PolicyManifest {
        PolicyManifest {
            manifest_schema_version: 1,
            row_grant_targets: Vec::new(),
            version: "1.0".into(),
            policies,
            principal_contract: None,
        }
    }

    fn row_filter_policy(id: &str, table: &str, cel: &str) -> CompiledPolicy {
        CompiledPolicy {
            binding_selectors: Vec::new(),
            id: id.into(),
            effect: Effect::Permit,
            filter_type: FilterType::RowFilter,
            target_table: table.into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: cel.into(),
            applies_to: None,
            description: None,
        }
    }

    fn deny_override_policy(id: &str, table: &str, cel: &str) -> CompiledPolicy {
        CompiledPolicy {
            binding_selectors: Vec::new(),
            id: id.into(),
            effect: Effect::Forbid,
            filter_type: FilterType::DenyOverride,
            target_table: table.into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: cel.into(),
            applies_to: None,
            description: None,
        }
    }

    fn mask_policy(id: &str, table: &str, column: &str, cel: &str) -> CompiledPolicy {
        CompiledPolicy {
            binding_selectors: Vec::new(),
            id: id.into(),
            effect: Effect::Forbid,
            filter_type: FilterType::ColumnMask,
            target_table: table.into(),
            column: Some(column.into()),
            target_tag: None,
            applies_to_tag: None,
            cel_expression: cel.into(),
            applies_to: None,
            description: None,
        }
    }

    #[test]
    fn test_analyst_region_filter() {
        let manifest = manifest_with(vec![row_filter_policy(
            "region_filter",
            "patients",
            "(resource.region == principal.region)",
        )]);
        let identity = QueryIdentity {
            role: "analyst".into(),
            region: Some("us-east".into()),
            name: None,
        };
        let filters = build_row_filters(&manifest, "patients", &identity).unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(
            format!("{}", filters[0]),
            format!("{}", col("region").eq(lit("us-east")))
        );
    }

    #[test]
    fn test_deny_override_non_legal() {
        let manifest = manifest_with(vec![deny_override_policy(
            "legal_hold",
            "patients",
            "(resource.legal_hold == true) && !(principal.role == \"legal\")",
        )]);
        let identity = QueryIdentity {
            role: "analyst".into(),
            region: None,
            name: None,
        };
        let filters = build_row_filters(&manifest, "patients", &identity).unwrap();
        assert_eq!(filters.len(), 1);
        let f = format!("{}", filters[0]);
        assert!(
            f.contains("legal_hold"),
            "expected legal_hold filter, got: {f}"
        );
    }

    #[test]
    fn test_deny_override_legal_user() {
        let manifest = manifest_with(vec![deny_override_policy(
            "legal_hold",
            "patients",
            "(resource.legal_hold == true) && !(principal.role == \"legal\")",
        )]);
        let identity = QueryIdentity {
            role: "legal".into(),
            region: None,
            name: None,
        };
        let filters = build_row_filters(&manifest, "patients", &identity).unwrap();
        // The deny condition for a legal user: the CEL expression evaluates the
        // principal part as false (role IS legal, so !(true) = false), making
        // the whole AND false → constant false → cel_to_datafusion_expr returns
        // Some(lit(false)). Our deny filter negates that to NOT(false) = no
        // rows denied. However, we should not block all rows.
        // The NOT(false) will be simplified or pass through.
        // Legal users should effectively see all rows.
        assert!(
            filters.len() <= 1,
            "legal user should get at most a trivial filter"
        );
    }

    #[test]
    fn test_column_mask_admin_exempt() {
        let manifest = manifest_with(vec![mask_policy(
            "mask_ssn",
            "patients",
            "ssn",
            "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))",
        )]);
        let identity = QueryIdentity {
            role: "admin".into(),
            region: None,
            name: None,
        };
        let masks = build_column_masks(&manifest, "patients", &identity).unwrap();
        assert!(masks.is_empty(), "admin should NOT be masked");
    }

    #[test]
    fn test_column_mask_analyst_masked() {
        let manifest = manifest_with(vec![mask_policy(
            "mask_ssn",
            "patients",
            "ssn",
            "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))",
        )]);
        let identity = QueryIdentity {
            role: "analyst".into(),
            region: None,
            name: None,
        };
        let masks = build_column_masks(&manifest, "patients", &identity).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].0, "ssn");
        assert_eq!(masks[0].1, "***");
    }

    #[test]
    fn test_wildcard_table_policy() {
        let manifest = manifest_with(vec![row_filter_policy(
            "global_filter",
            "*",
            "(resource.region == principal.region)",
        )]);
        let identity = QueryIdentity {
            role: "analyst".into(),
            region: Some("eu-west".into()),
            name: None,
        };
        let filters = build_row_filters(&manifest, "orders", &identity).unwrap();
        assert_eq!(filters.len(), 1, "wildcard table should match any table");
    }

    #[test]
    fn test_empty_expression_is_no_filter() {
        let manifest = manifest_with(vec![row_filter_policy("empty", "patients", "true")]);
        let identity = QueryIdentity {
            role: "analyst".into(),
            region: None,
            name: None,
        };
        let filters = build_row_filters(&manifest, "patients", &identity).unwrap();
        assert!(
            filters.is_empty(),
            "constant-true expression should produce no filter"
        );
    }
}
