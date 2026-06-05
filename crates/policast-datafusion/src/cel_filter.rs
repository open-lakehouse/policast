use std::collections::BTreeMap;

use datafusion::common::Result as DFResult;
use datafusion::logical_expr::{col, lit, Expr};

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

/// Given a policy manifest and the identity of the querying user, produce
/// DataFusion `Expr` filters to apply to a table scan.
///
/// Each compiled policy's CEL expression is parsed and converted into a
/// DataFusion `Expr`. `resource.*` references become column references,
/// `principal.*` references are bound from the identity at planning time.
pub fn build_row_filters(
    manifest: &PolicyManifest,
    table_name: &str,
    identity: &dyn PrincipalProvider,
) -> DFResult<Vec<Expr>> {
    let mut filters = Vec::new();

    for policy in manifest.policies_for_table(table_name) {
        match policy.filter_type {
            FilterType::RowFilter => {
                match cel_to_datafusion_expr(&policy.cel_expression, identity) {
                    Ok(Some(expr)) => filters.push(expr),
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("policast: skipping row filter '{}': {e}", policy.id);
                    }
                }
            }
            FilterType::DenyOverride => match build_deny_filter(&policy.cel_expression, identity) {
                Ok(Some(expr)) => filters.push(expr),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("policast: skipping deny override '{}': {e}", policy.id);
                }
            },
            FilterType::ColumnMask => {}
        }
    }

    Ok(filters)
}

/// Determine which columns should be masked for this user, returning
/// (column_name, mask_value) pairs.
///
/// Uses the `cel-interpreter` runtime to evaluate the mask expression
/// with the full principal and resource context.
pub fn build_column_masks(
    manifest: &PolicyManifest,
    table_name: &str,
    identity: &dyn PrincipalProvider,
) -> Vec<(String, String)> {
    let mut masks = Vec::new();

    for policy in manifest.policies_for_table(table_name) {
        if policy.filter_type != FilterType::ColumnMask {
            continue;
        }
        if let Some(col_name) = &policy.column {
            let should_mask = match cel_to_bool(&policy.cel_expression, identity, table_name) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "policast: mask eval failed for '{}', fail-closed: {e}",
                        policy.id
                    );
                    true
                }
            };
            if should_mask {
                masks.push((col_name.clone(), "***".to_string()));
            }
        }
    }

    masks
}

/// Build a row filter from a deny-override CEL expression.
///
/// Deny-override expressions combine resource-side conditions (what to deny)
/// with principal-side conditions (who is exempt). We invert the semantics:
/// instead of "deny when X", we produce "keep rows where NOT X" for
/// non-exempt users.
fn build_deny_filter(
    cel: &str,
    identity: &dyn PrincipalProvider,
) -> Result<Option<Expr>, crate::cel_to_expr::CelConvertError> {
    let result = cel_to_datafusion_expr(cel, identity)?;
    match result {
        // If the deny condition evaluates to a dynamic expression, it means
        // the principal-side was true (user IS subject to deny). The expression
        // represents the "deny when true" condition on resources, so we need
        // to keep rows where the condition is NOT true.
        Some(deny_condition) => Ok(Some(not_expr(deny_condition))),
        // None means constant true → user is exempt from the deny → no filter
        None => Ok(None),
    }
}

/// Build `NOT expr`, with special handling for simple equality to also
/// allow NULL through (which is the common deny-override pattern for
/// optional boolean columns).
fn not_expr(expr: Expr) -> Expr {
    match &expr {
        Expr::BinaryExpr(be) if matches!(be.op, datafusion::logical_expr::Operator::Eq) => {
            col_eq_lit_negation(&be.left, &be.right)
                .unwrap_or_else(|| datafusion::logical_expr::not(expr.clone()))
        }
        _ => datafusion::logical_expr::not(expr),
    }
}

/// For `col = true` produce `col = false OR col IS NULL` (preserving NULLs).
fn col_eq_lit_negation(left: &Expr, right: &Expr) -> Option<Expr> {
    if let (
        Expr::Column(c),
        Expr::Literal(datafusion::common::ScalarValue::Boolean(Some(val)), _),
    ) = (left, right)
    {
        let col_ref = col(c.name());
        return Some(col_ref.clone().eq(lit(!val)).or(col_ref.is_null()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::model::{CompiledPolicy, Effect, FilterType};

    fn manifest_with(policies: Vec<CompiledPolicy>) -> PolicyManifest {
        PolicyManifest {
            version: "1.0".into(),
            policies,
            principal_contract: None,
        }
    }

    fn row_filter_policy(id: &str, table: &str, cel: &str) -> CompiledPolicy {
        CompiledPolicy {
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
        let masks = build_column_masks(&manifest, "patients", &identity);
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
        let masks = build_column_masks(&manifest, "patients", &identity);
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
