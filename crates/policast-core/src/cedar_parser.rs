use cedar_policy::PolicySet;
use serde_json::Value;
use std::collections::HashMap;

use crate::error::PolicastError;

/// A parsed Cedar policy in its JSON/EST form, ready for CEL translation.
#[derive(Debug, Clone)]
pub struct ParsedPolicy {
    pub id: String,
    pub effect: String,
    pub principal_constraint: Value,
    pub action_constraint: Value,
    pub resource_constraint: Value,
    pub conditions: Vec<ParsedCondition>,
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ParsedCondition {
    pub kind: ConditionKind,
    pub body: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKind {
    When,
    Unless,
}

/// Parse a Cedar policy text into a vec of `ParsedPolicy`, each carrying
/// the JSON/EST representation of its conditions for downstream translation.
pub fn parse_policies(cedar_text: &str) -> Result<Vec<ParsedPolicy>, PolicastError> {
    let policy_set: PolicySet = cedar_text
        .parse()
        .map_err(|e| PolicastError::CedarParse(format!("{e}")))?;

    let mut results = Vec::new();

    for policy in policy_set.policies() {
        let json_val: Value = serde_json::to_value(policy.to_json().map_err(|e| {
            PolicastError::CedarParse(format!("Failed to convert policy to JSON: {e}"))
        })?)
        .map_err(|e| PolicastError::CedarParse(format!("JSON serialization error: {e}")))?;

        let effect = json_val
            .get("effect")
            .and_then(|v| v.as_str())
            .unwrap_or("permit")
            .to_string();

        let principal_constraint = json_val.get("principal").cloned().unwrap_or(Value::Null);
        let action_constraint = json_val.get("action").cloned().unwrap_or(Value::Null);
        let resource_constraint = json_val.get("resource").cloned().unwrap_or(Value::Null);

        let conditions = parse_conditions(&json_val)?;

        let mut annotations = HashMap::new();
        if let Some(ann_obj) = json_val.get("annotations").and_then(|v| v.as_object()) {
            for (k, v) in ann_obj {
                if let Some(s) = v.as_str() {
                    annotations.insert(k.clone(), s.to_string());
                }
            }
        }

        let id = annotations
            .get("id")
            .cloned()
            .unwrap_or_else(|| policy.id().to_string());

        results.push(ParsedPolicy {
            id,
            effect,
            principal_constraint,
            action_constraint,
            resource_constraint,
            conditions,
            annotations,
        });
    }

    // Cedar's `PolicySet` is HashMap-backed, so `policies()` yields a
    // non-deterministic order. Sort by policy id for a stable, reproducible
    // manifest (Cedar evaluation is order-independent, so this is purely
    // cosmetic for enforcement but essential for diff/drift checks).
    results.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(results)
}

fn parse_conditions(json_val: &Value) -> Result<Vec<ParsedCondition>, PolicastError> {
    let mut conditions = Vec::new();
    if let Some(conds) = json_val.get("conditions").and_then(|v| v.as_array()) {
        for cond in conds {
            let kind = match cond.get("kind").and_then(|v| v.as_str()) {
                Some("when") => ConditionKind::When,
                Some("unless") => ConditionKind::Unless,
                other => {
                    return Err(PolicastError::CedarParse(format!(
                        "Unknown condition kind: {other:?}"
                    )))
                }
            };
            let body = cond.get("body").cloned().unwrap_or(Value::Bool(true));
            conditions.push(ParsedCondition { kind, body });
        }
    }
    Ok(conditions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_permit() {
        let cedar = r#"
            permit (
                principal,
                action == Action::"query",
                resource
            )
            when {
                resource.region == "us-east"
            };
        "#;

        let policies = parse_policies(cedar).expect("should parse");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].effect, "permit");
        assert_eq!(policies[0].conditions.len(), 1);
        assert_eq!(policies[0].conditions[0].kind, ConditionKind::When);
    }

    #[test]
    fn test_parse_forbid_with_unless() {
        let cedar = r#"
            @id("deny_test")
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

        let policies = parse_policies(cedar).expect("should parse");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].id, "deny_test");
        assert_eq!(policies[0].effect, "forbid");
        assert_eq!(policies[0].conditions.len(), 2);
    }

    #[test]
    fn test_parse_policies_sorted_by_id_deterministically() {
        // Authored out of id order and parsed repeatedly: Cedar's PolicySet is
        // HashMap-backed, so without an explicit sort the output order would be
        // non-deterministic. We always emit policies sorted by id.
        let cedar = r#"
            @id("zebra")
            permit (principal, action == Action::"query", resource)
            when { resource.region == "us-east" };

            @id("alpha")
            permit (principal, action == Action::"query", resource)
            when { resource.region == "us-west" };

            @id("mike")
            permit (principal, action == Action::"query", resource)
            when { resource.region == "eu" };
        "#;

        let ids: Vec<String> = parse_policies(cedar)
            .expect("should parse")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec!["alpha", "mike", "zebra"]);

        // Re-parsing yields the identical order every time.
        for _ in 0..5 {
            let again: Vec<String> = parse_policies(cedar)
                .expect("should parse")
                .into_iter()
                .map(|p| p.id)
                .collect();
            assert_eq!(again, ids);
        }
    }
}
