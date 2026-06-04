//! Scaffolding for the opinionated governance [profiles](crate::profile).
//!
//! Turns a small set of options into a correct, ready-to-edit Cedar policy
//! for one of the three primitives. The output is guaranteed to pass the
//! compiler's profile validation, so `policast new` followed by
//! `policast compile` always round-trips.

use crate::error::PolicastError;
use crate::profile::PolicyProfile;

/// Inputs for rendering a profile scaffold.
#[derive(Debug, Clone, Default)]
pub struct ScaffoldOptions {
    pub profile_kind: Option<PolicyProfile>,
    pub id: Option<String>,
    pub target_table: Option<String>,
    pub target_tag: Option<String>,
    pub column: Option<String>,
    pub applies_to_tag: Option<String>,
    pub roles: Vec<String>,
    pub description: Option<String>,
}

/// Parse the CLI profile name (`row-filter`, `column-mask`, `deny-override`)
/// into a [`PolicyProfile`].
pub fn parse_profile_kind(name: &str) -> Result<PolicyProfile, PolicastError> {
    match name {
        "row-filter" | "row_filter" => Ok(PolicyProfile::RowFilter),
        "column-mask" | "column_mask" => Ok(PolicyProfile::ColumnMask),
        "deny-override" | "deny_override" => Ok(PolicyProfile::DenyOverride),
        other => Err(PolicastError::CelEmit(format!(
            "unknown profile {other:?}; expected one of: row-filter, column-mask, deny-override"
        ))),
    }
}

/// Render a Cedar policy scaffold for the given profile and options.
pub fn render_scaffold(opts: &ScaffoldOptions) -> Result<String, PolicastError> {
    let profile = opts
        .profile_kind
        .ok_or_else(|| PolicastError::CelEmit("a profile kind is required".into()))?;

    if opts.target_table.is_some() && opts.target_tag.is_some() {
        return Err(PolicastError::CelEmit(
            "--table and --target-tag are mutually exclusive".into(),
        ));
    }
    if opts.column.is_some() && opts.applies_to_tag.is_some() {
        return Err(PolicastError::CelEmit(
            "--column and --tag are mutually exclusive".into(),
        ));
    }

    match profile {
        PolicyProfile::RowFilter => Ok(render_row_filter(opts)),
        PolicyProfile::ColumnMask => render_column_mask(opts),
        PolicyProfile::DenyOverride => Ok(render_deny_override(opts)),
    }
}

fn render_row_filter(opts: &ScaffoldOptions) -> String {
    let id = opts.id.clone().unwrap_or_else(|| "row_filter_new".to_string());
    let mut out = String::new();
    push_header(&mut out, &id, "row_filter", opts);
    if !opts.roles.is_empty() {
        out.push_str(&format!("@roles(\"{}\")\n", opts.roles.join(",")));
    }
    out.push_str("permit (\n    principal,\n    action == Action::\"query\",\n    resource\n)\n");
    out.push_str("when {\n    // TODO: edit this predicate to match your row-visibility rule.\n");
    out.push_str("    resource.region == principal.region\n};\n");
    out
}

fn render_column_mask(opts: &ScaffoldOptions) -> Result<String, PolicastError> {
    if opts.column.is_none() && opts.applies_to_tag.is_none() {
        return Err(PolicastError::CelEmit(
            "column_mask requires --column <name> or --tag <tag>".into(),
        ));
    }
    let id = opts
        .id
        .clone()
        .unwrap_or_else(|| "column_mask_new".to_string());
    let mut out = String::new();
    push_header(&mut out, &id, "column_mask", opts);
    if let Some(col) = &opts.column {
        out.push_str(&format!("@column(\"{col}\")\n"));
    }
    if let Some(tag) = &opts.applies_to_tag {
        out.push_str(&format!("@applies_to_tag(\"{tag}\")\n"));
    }
    out.push_str("forbid (\n    principal,\n    action == Action::\"query\",\n    resource\n)\n");
    out.push_str("unless {\n");
    out.push_str(&format!("    {}\n", exempt_clause(&opts.roles, "admin")));
    out.push_str("};\n");
    Ok(out)
}

fn render_deny_override(opts: &ScaffoldOptions) -> String {
    let id = opts
        .id
        .clone()
        .unwrap_or_else(|| "deny_override_new".to_string());
    let mut out = String::new();
    push_header(&mut out, &id, "deny_override", opts);
    out.push_str("forbid (\n    principal,\n    action == Action::\"query\",\n    resource\n)\n");
    out.push_str("when {\n    // TODO: edit this guard to name the rows to deny.\n");
    out.push_str("    resource.legal_hold == true\n}\n");
    out.push_str("unless {\n");
    out.push_str(&format!("    {}\n", exempt_clause(&opts.roles, "legal")));
    out.push_str("};\n");
    out
}

/// Emit the shared annotation header (`@id`, `@filter_type`, scope, description).
fn push_header(out: &mut String, id: &str, filter_type: &str, opts: &ScaffoldOptions) {
    out.push_str(&format!("@id(\"{id}\")\n"));
    out.push_str(&format!("@filter_type(\"{filter_type}\")\n"));
    if let Some(table) = &opts.target_table {
        out.push_str(&format!("@target_table(\"{table}\")\n"));
    }
    if let Some(tag) = &opts.target_tag {
        out.push_str(&format!("@target_tag(\"{tag}\")\n"));
    }
    if let Some(desc) = &opts.description {
        out.push_str(&format!("@description(\"{desc}\")\n"));
    }
}

/// Build a `principal.role == "x" || principal.role == "y"` exemption clause
/// from a role list, falling back to `default_role` when none are given.
fn exempt_clause(roles: &[String], default_role: &str) -> String {
    let effective: Vec<String> = if roles.is_empty() {
        vec![default_role.to_string()]
    } else {
        roles.to_vec()
    };
    effective
        .iter()
        .map(|r| format!("principal.role == \"{r}\""))
        .collect::<Vec<_>>()
        .join(" || ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cedar_parser::parse_policies;
    use crate::policy_manifest::PolicyManifest;

    fn compile_ok(cedar: &str) -> PolicyManifest {
        let parsed = parse_policies(cedar).expect("scaffold should parse as Cedar");
        let mut m = PolicyManifest::new();
        m.compile_policies(&parsed)
            .expect("scaffold should pass profile validation");
        m
    }

    #[test]
    fn test_parse_profile_kind() {
        assert_eq!(parse_profile_kind("row-filter").unwrap(), PolicyProfile::RowFilter);
        assert_eq!(parse_profile_kind("column_mask").unwrap(), PolicyProfile::ColumnMask);
        assert_eq!(parse_profile_kind("deny-override").unwrap(), PolicyProfile::DenyOverride);
        assert!(parse_profile_kind("bogus").is_err());
    }

    #[test]
    fn test_row_filter_scaffold_compiles() {
        let opts = ScaffoldOptions {
            profile_kind: Some(PolicyProfile::RowFilter),
            id: Some("rf".into()),
            target_table: Some("patients".into()),
            roles: vec!["analyst".into()],
            ..Default::default()
        };
        let cedar = render_scaffold(&opts).unwrap();
        assert!(cedar.contains("@filter_type(\"row_filter\")"));
        assert!(cedar.contains("@roles(\"analyst\")"));
        assert!(cedar.contains("permit"));
        let m = compile_ok(&cedar);
        assert_eq!(m.policies[0].id, "rf");
    }

    #[test]
    fn test_column_mask_scaffold_with_tag_compiles() {
        let opts = ScaffoldOptions {
            profile_kind: Some(PolicyProfile::ColumnMask),
            id: Some("cm".into()),
            applies_to_tag: Some("pii".into()),
            roles: vec!["admin".into(), "physician".into()],
            ..Default::default()
        };
        let cedar = render_scaffold(&opts).unwrap();
        assert!(cedar.contains("@applies_to_tag(\"pii\")"));
        assert!(cedar.contains("principal.role == \"admin\" || principal.role == \"physician\""));
        let m = compile_ok(&cedar);
        assert_eq!(m.policies[0].applies_to_tag.as_deref(), Some("pii"));
    }

    #[test]
    fn test_column_mask_scaffold_requires_target() {
        let opts = ScaffoldOptions {
            profile_kind: Some(PolicyProfile::ColumnMask),
            id: Some("cm".into()),
            ..Default::default()
        };
        assert!(render_scaffold(&opts).is_err());
    }

    #[test]
    fn test_deny_override_scaffold_compiles() {
        let opts = ScaffoldOptions {
            profile_kind: Some(PolicyProfile::DenyOverride),
            id: Some("legal".into()),
            target_table: Some("patients".into()),
            ..Default::default()
        };
        let cedar = render_scaffold(&opts).unwrap();
        assert!(cedar.contains("@filter_type(\"deny_override\")"));
        assert!(cedar.contains("principal.role == \"legal\""));
        let m = compile_ok(&cedar);
        assert_eq!(m.policies[0].id, "legal");
    }

    #[test]
    fn test_mutually_exclusive_scope() {
        let opts = ScaffoldOptions {
            profile_kind: Some(PolicyProfile::RowFilter),
            target_table: Some("patients".into()),
            target_tag: Some("clinical".into()),
            ..Default::default()
        };
        assert!(render_scaffold(&opts).is_err());
    }
}
