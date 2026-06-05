//! Opinionated governance "profiles".
//!
//! policast ships three first-class governance primitives, each mapping
//! onto a recognized Cedar idiom:
//!
//! | Profile        | Cedar shape                                   | Pattern |
//! |----------------|-----------------------------------------------|---------|
//! | `row_filter`   | `permit` + `when`                             | ABAC / ReBAC predicate on rows |
//! | `column_mask`  | `forbid` + `unless`                           | RBAC carve-out on a column/tag |
//! | `deny_override`| `forbid` + `when {resource…}` + `unless {…}`  | Service-wide guardrail (forbid-overrides-permit) |
//!
//! These are named *profiles* (not "templates") to avoid colliding with
//! Cedar's reserved term for scope-placeholder policies. The compiler
//! validates each policy against the shape of its profile and flags
//! `principal.*` attributes outside the canonical vocabulary.

use std::collections::BTreeSet;

use crate::error::PolicastError;
use crate::model::{Effect, FilterType};

/// The canonical `principal.*` attribute vocabulary the profiles are built
/// around. Referencing an attribute outside this set is allowed but
/// produces an advisory warning so authors notice typos or undeclared
/// ABAC attributes.
pub const CANONICAL_PRINCIPAL_ATTRS: &[&str] = &["role", "region", "name", "groups"];

/// One of the three opinionated governance primitives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyProfile {
    RowFilter,
    ColumnMask,
    DenyOverride,
}

impl PolicyProfile {
    /// The profile corresponding to a compiled policy's filter type.
    pub fn from_filter_type(ft: &FilterType) -> Self {
        match ft {
            FilterType::RowFilter => PolicyProfile::RowFilter,
            FilterType::ColumnMask => PolicyProfile::ColumnMask,
            FilterType::DenyOverride => PolicyProfile::DenyOverride,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PolicyProfile::RowFilter => "row_filter",
            PolicyProfile::ColumnMask => "column_mask",
            PolicyProfile::DenyOverride => "deny_override",
        }
    }
}

/// Validate that a policy conforms to the shape of its profile.
///
/// Returns a list of non-fatal advisory warnings on success. Structural
/// contradictions (wrong `effect`, a column mask that targets nothing)
/// are hard errors; missing-but-recommended clauses are warnings so that
/// degenerate-but-valid policies still compile.
pub fn validate_profile(
    policy_id: &str,
    profile: PolicyProfile,
    effect: Effect,
    when_count: usize,
    unless_count: usize,
    has_column: bool,
    has_applies_to_tag: bool,
) -> Result<Vec<String>, PolicastError> {
    let mut warnings = Vec::new();

    match profile {
        PolicyProfile::RowFilter => {
            if effect != Effect::Permit {
                return Err(profile_err(
                    policy_id,
                    "row_filter profile must use `permit`",
                ));
            }
            if when_count == 0 {
                warnings.push(format!(
                    "policy {policy_id:?}: row_filter has no `when` clause; it permits all rows and restricts nothing"
                ));
            }
        }
        PolicyProfile::ColumnMask => {
            if effect != Effect::Forbid {
                return Err(profile_err(
                    policy_id,
                    "column_mask profile must use `forbid`",
                ));
            }
            if !has_column && !has_applies_to_tag {
                return Err(profile_err(
                    policy_id,
                    "column_mask profile must target a @column or an @applies_to_tag",
                ));
            }
            if unless_count == 0 {
                warnings.push(format!(
                    "policy {policy_id:?}: column_mask has no `unless` carve-out; it will mask for every principal"
                ));
            }
        }
        PolicyProfile::DenyOverride => {
            if effect != Effect::Forbid {
                return Err(profile_err(
                    policy_id,
                    "deny_override profile must use `forbid`",
                ));
            }
            if when_count == 0 {
                warnings.push(format!(
                    "policy {policy_id:?}: deny_override has no `when` guard; it denies unconditionally"
                ));
            }
        }
    }

    Ok(warnings)
}

/// The subset of referenced principal attributes that fall outside the
/// canonical vocabulary.
pub fn non_canonical_principal_attrs(attrs: &BTreeSet<String>) -> Vec<String> {
    attrs
        .iter()
        .filter(|a| !CANONICAL_PRINCIPAL_ATTRS.contains(&a.as_str()))
        .cloned()
        .collect()
}

fn profile_err(policy_id: &str, msg: &str) -> PolicastError {
    PolicastError::CelEmit(format!("policy {policy_id:?}: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_filter_requires_permit() {
        let err = validate_profile(
            "p",
            PolicyProfile::RowFilter,
            Effect::Forbid,
            1,
            0,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must use `permit`"));
    }

    #[test]
    fn test_row_filter_no_when_warns() {
        let warnings = validate_profile(
            "p",
            PolicyProfile::RowFilter,
            Effect::Permit,
            0,
            0,
            false,
            false,
        )
        .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no `when`"));
    }

    #[test]
    fn test_column_mask_requires_forbid() {
        let err = validate_profile(
            "p",
            PolicyProfile::ColumnMask,
            Effect::Permit,
            0,
            1,
            true,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must use `forbid`"));
    }

    #[test]
    fn test_column_mask_requires_target() {
        let err = validate_profile(
            "p",
            PolicyProfile::ColumnMask,
            Effect::Forbid,
            0,
            1,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("@column or an @applies_to_tag"));
    }

    #[test]
    fn test_column_mask_no_unless_warns() {
        let warnings = validate_profile(
            "p",
            PolicyProfile::ColumnMask,
            Effect::Forbid,
            0,
            0,
            true,
            false,
        )
        .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no `unless`"));
    }

    #[test]
    fn test_column_mask_happy() {
        let warnings = validate_profile(
            "p",
            PolicyProfile::ColumnMask,
            Effect::Forbid,
            0,
            1,
            false,
            true,
        )
        .unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_deny_override_requires_forbid() {
        let err = validate_profile(
            "p",
            PolicyProfile::DenyOverride,
            Effect::Permit,
            1,
            1,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must use `forbid`"));
    }

    #[test]
    fn test_deny_override_no_when_warns() {
        let warnings = validate_profile(
            "p",
            PolicyProfile::DenyOverride,
            Effect::Forbid,
            0,
            1,
            false,
            false,
        )
        .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("denies unconditionally"));
    }

    #[test]
    fn test_non_canonical_attrs() {
        let attrs: BTreeSet<String> = ["role", "clearance", "region", "department"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut flagged = non_canonical_principal_attrs(&attrs);
        flagged.sort();
        assert_eq!(
            flagged,
            vec!["clearance".to_string(), "department".to_string()]
        );
    }

    #[test]
    fn test_profile_from_filter_type_roundtrip() {
        assert_eq!(
            PolicyProfile::from_filter_type(&FilterType::RowFilter).as_str(),
            "row_filter"
        );
        assert_eq!(
            PolicyProfile::from_filter_type(&FilterType::ColumnMask).as_str(),
            "column_mask"
        );
        assert_eq!(
            PolicyProfile::from_filter_type(&FilterType::DenyOverride).as_str(),
            "deny_override"
        );
    }
}
