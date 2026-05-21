use serde_json::Value;

use crate::error::PolicastError;

/// Translate a Cedar EST condition body (JSON) into a CEL expression string.
///
/// The Cedar JSON/EST format uses tagged objects where the key is the operator
/// and the value contains the operands. This walks the tree recursively,
/// emitting the equivalent CEL syntax at each node.
pub fn cedar_expr_to_cel(expr: &Value) -> Result<String, PolicastError> {
    match expr {
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))),

        Value::Object(map) if map.len() == 1 => {
            let (key, val) = map.iter().next().unwrap();
            translate_node(key, val)
        }

        Value::Object(map) if map.contains_key("Value") => {
            translate_value_literal(map.get("Value").unwrap())
        }

        Value::Array(arr) => {
            let items: Result<Vec<String>, _> = arr.iter().map(cedar_expr_to_cel).collect();
            Ok(format!("[{}]", items?.join(", ")))
        }

        other => Err(PolicastError::CelEmit(format!(
            "Unsupported Cedar EST node: {other}"
        ))),
    }
}

fn translate_node(key: &str, val: &Value) -> Result<String, PolicastError> {
    match key {
        // -- Literal values --
        "Value" => translate_value_literal(val),
        "Var" => translate_var(val),

        // -- Binary comparison operators --
        "==" | "!=" | "<" | "<=" | ">" | ">=" => translate_binary_op(key, val),

        // -- Logical operators --
        "&&" => translate_binary_op("&&", val),
        "||" => translate_binary_op("||", val),
        "!" => {
            let inner = cedar_expr_to_cel(val.get("arg").unwrap_or(val))?;
            Ok(format!("!({inner})"))
        }

        // -- Attribute access (dot operator) --
        "." => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in dot access".into())
            })?)?;
            let attr = val
                .get("attr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PolicastError::CelEmit("Missing 'attr' in dot access".into()))?;
            Ok(format!("{left}.{attr}"))
        }

        // -- `has` operator (attribute existence) --
        "has" => {
            let inner = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in has".into())
            })?)?;
            let attr = val
                .get("attr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PolicastError::CelEmit("Missing 'attr' in has".into()))?;
            Ok(format!("has({inner}.{attr})"))
        }

        // -- `like` operator (wildcard string match) --
        "like" => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in like".into())
            })?)?;
            let pattern = val
                .get("pattern")
                .ok_or_else(|| PolicastError::CelEmit("Missing 'pattern' in like".into()))?;
            let regex = cedar_like_to_regex(pattern)?;
            Ok(format!("{left}.matches(\"{regex}\")"))
        }

        // -- `if-then-else` --
        "if-then-else" => {
            let cond = cedar_expr_to_cel(val.get("if").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'if' in if-then-else".into())
            })?)?;
            let then_expr = cedar_expr_to_cel(val.get("then").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'then' in if-then-else".into())
            })?)?;
            let else_expr = cedar_expr_to_cel(val.get("else").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'else' in if-then-else".into())
            })?)?;
            Ok(format!("({cond}) ? ({then_expr}) : ({else_expr})"))
        }

        // -- `in` (set membership / hierarchy) --
        "in" => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in 'in'".into())
            })?)?;
            let right = cedar_expr_to_cel(val.get("right").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'right' in 'in'".into())
            })?)?;
            Ok(format!("{left} in {right}"))
        }

        // -- `is` (entity type check) --
        "is" => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in 'is'".into())
            })?)?;
            let entity_type = val
                .get("entity_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    PolicastError::CelEmit("Missing 'entity_type' in 'is'".into())
                })?;
            Ok(format!("{left}.type == \"{entity_type}\""))
        }

        // -- `contains` (set contains element) --
        "contains" => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in contains".into())
            })?)?;
            let right = cedar_expr_to_cel(val.get("right").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'right' in contains".into())
            })?)?;
            Ok(format!("{right} in {left}"))
        }

        // -- `containsAll` / `containsAny` --
        "containsAll" => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in containsAll".into())
            })?)?;
            let right = cedar_expr_to_cel(val.get("right").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'right' in containsAll".into())
            })?)?;
            Ok(format!("{right}.all(e, e in {left})"))
        }

        "containsAny" => {
            let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'left' in containsAny".into())
            })?)?;
            let right = cedar_expr_to_cel(val.get("right").ok_or_else(|| {
                PolicastError::CelEmit("Missing 'right' in containsAny".into())
            })?)?;
            Ok(format!("{right}.exists(e, e in {left})"))
        }

        // -- Arithmetic --
        "+" | "-" | "*" => translate_binary_op(key, val),

        "negate" => {
            let inner = cedar_expr_to_cel(val.get("arg").unwrap_or(val))?;
            Ok(format!("-({inner})"))
        }

        // -- Set literal --
        "Set" => {
            if let Some(arr) = val.as_array() {
                let items: Result<Vec<String>, _> = arr.iter().map(cedar_expr_to_cel).collect();
                Ok(format!("[{}]", items?.join(", ")))
            } else {
                Err(PolicastError::CelEmit("Set node is not an array".into()))
            }
        }

        // -- Record literal --
        "Record" => {
            if let Some(obj) = val.as_object() {
                let entries: Result<Vec<String>, PolicastError> = obj
                    .iter()
                    .map(|(k, v)| {
                        let cel_val = cedar_expr_to_cel(v)?;
                        Ok(format!("\"{k}\": {cel_val}"))
                    })
                    .collect();
                Ok(format!("{{{}}}", entries?.join(", ")))
            } else {
                Err(PolicastError::CelEmit("Record node is not an object".into()))
            }
        }

        // -- Entity reference --
        "__entity" | "Ref" => translate_entity_ref(val),

        other => Err(PolicastError::CelEmit(format!(
            "Unsupported Cedar EST operator: {other}"
        ))),
    }
}

fn translate_binary_op(op: &str, val: &Value) -> Result<String, PolicastError> {
    let left = cedar_expr_to_cel(val.get("left").ok_or_else(|| {
        PolicastError::CelEmit(format!("Missing 'left' in binary op '{op}'"))
    })?)?;
    let right = cedar_expr_to_cel(val.get("right").ok_or_else(|| {
        PolicastError::CelEmit(format!("Missing 'right' in binary op '{op}'"))
    })?)?;
    Ok(format!("({left} {op} {right})"))
}

fn translate_value_literal(val: &Value) -> Result<String, PolicastError> {
    match val {
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(format!(
            "\"{}\"",
            s.replace('\\', "\\\\").replace('"', "\\\"")
        )),
        Value::Null => Ok("null".to_string()),
        Value::Array(arr) => {
            let items: Result<Vec<String>, _> =
                arr.iter().map(translate_value_literal).collect();
            Ok(format!("[{}]", items?.join(", ")))
        }
        Value::Object(map) => {
            if let (Some(entity_type), Some(id)) =
                (map.get("type").and_then(|v| v.as_str()), map.get("id"))
            {
                let id_owned = id.to_string();
                let id_str = id.as_str().unwrap_or(&id_owned);
                Ok(format!("\"{entity_type}::{id_str}\""))
            } else if map.contains_key("__entity") || map.contains_key("Ref") {
                let entity = map
                    .get("__entity")
                    .or_else(|| map.get("Ref"))
                    .unwrap();
                translate_entity_ref(entity)
            } else {
                let entries: Result<Vec<String>, PolicastError> = map
                    .iter()
                    .map(|(k, v)| {
                        let cel_val = translate_value_literal(v)?;
                        Ok(format!("\"{k}\": {cel_val}"))
                    })
                    .collect();
                Ok(format!("{{{}}}", entries?.join(", ")))
            }
        }
    }
}

fn translate_entity_ref(val: &Value) -> Result<String, PolicastError> {
    let entity_type = val
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let id = val
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    Ok(format!("\"{entity_type}::{id}\""))
}

fn translate_var(val: &Value) -> Result<String, PolicastError> {
    match val.as_str() {
        Some("principal") => Ok("principal".to_string()),
        Some("resource") => Ok("resource".to_string()),
        Some("action") => Ok("action".to_string()),
        Some("context") => Ok("context".to_string()),
        Some(other) => Ok(other.to_string()),
        None => Err(PolicastError::CelEmit("Var node is not a string".into())),
    }
}

/// Convert a Cedar `like` wildcard pattern to a regex string.
/// Cedar `like` uses `*` as the only wildcard (matches any sequence of chars).
fn cedar_like_to_regex(pattern: &Value) -> Result<String, PolicastError> {
    match pattern {
        Value::String(s) => {
            let mut regex = String::from("^");
            for ch in s.chars() {
                match ch {
                    '*' => regex.push_str(".*"),
                    '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|'
                    | '\\' => {
                        regex.push('\\');
                        regex.push(ch);
                    }
                    _ => regex.push(ch),
                }
            }
            regex.push('$');
            Ok(regex)
        }
        Value::Array(parts) => {
            let mut regex = String::from("^");
            for part in parts {
                if let Some(obj) = part.as_object() {
                    if obj.contains_key("Wildcard") {
                        regex.push_str(".*");
                    } else if let Some(lit) = obj.get("Literal").and_then(|v| v.as_str()) {
                        for ch in lit.chars() {
                            match ch {
                                '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^'
                                | '$' | '|' | '\\' => {
                                    regex.push('\\');
                                    regex.push(ch);
                                }
                                _ => regex.push(ch),
                            }
                        }
                    }
                }
            }
            regex.push('$');
            Ok(regex)
        }
        _ => Err(PolicastError::CelEmit(format!(
            "Unsupported like pattern format: {pattern}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_equality() {
        let expr = json!({"==": {"left": {"Var": "resource"}, "right": {"Value": "test"}}});
        let result = cedar_expr_to_cel(&expr).unwrap();
        // The Var "resource" -> resource, Value "test" -> "test"
        assert!(result.contains("=="));
        assert!(result.contains("resource"));
    }

    #[test]
    fn test_dot_access() {
        let expr = json!({".": {"left": {"Var": "resource"}, "attr": "region"}});
        let result = cedar_expr_to_cel(&expr).unwrap();
        assert_eq!(result, "resource.region");
    }

    #[test]
    fn test_logical_and() {
        let expr = json!({"&&": {
            "left": {"Value": true},
            "right": {"Value": false}
        }});
        let result = cedar_expr_to_cel(&expr).unwrap();
        assert_eq!(result, "(true && false)");
    }

    #[test]
    fn test_has_attribute() {
        let expr = json!({"has": {"left": {"Var": "resource"}, "attr": "ssn"}});
        let result = cedar_expr_to_cel(&expr).unwrap();
        assert_eq!(result, "has(resource.ssn)");
    }

    #[test]
    fn test_if_then_else() {
        let expr = json!({"if-then-else": {
            "if": {"Value": true},
            "then": {"Value": "yes"},
            "else": {"Value": "no"}
        }});
        let result = cedar_expr_to_cel(&expr).unwrap();
        assert_eq!(result, "(true) ? (\"yes\") : (\"no\")");
    }

    #[test]
    fn test_nested_dot_comparison() {
        let expr = json!({"==": {
            "left": {".": {"left": {"Var": "resource"}, "attr": "region"}},
            "right": {".": {"left": {"Var": "principal"}, "attr": "region"}}
        }});
        let result = cedar_expr_to_cel(&expr).unwrap();
        assert_eq!(result, "(resource.region == principal.region)");
    }
}
