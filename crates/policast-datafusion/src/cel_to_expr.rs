use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use cel_parser::{ArithmeticOp, Atom, Expression, Member, RelationOp, UnaryOp};
use datafusion::common::ScalarValue;
use datafusion::logical_expr::{col, lit, not, Expr};

use crate::cel_filter::QueryIdentity;

/// Errors that can occur when converting CEL expressions.
#[derive(Debug, Clone)]
pub enum CelConvertError {
    ParseError(String),
    UnsupportedExpression(String),
    MissingIdentityField(String),
}

impl std::fmt::Display for CelConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError(msg) => write!(f, "CEL parse error: {msg}"),
            Self::UnsupportedExpression(msg) => write!(f, "unsupported CEL construct: {msg}"),
            Self::MissingIdentityField(field) => {
                write!(f, "identity missing field: {field}")
            }
        }
    }
}

impl std::error::Error for CelConvertError {}

/// Intermediate result while walking the CEL AST. A node is either a
/// planning-time constant (all inputs were known) or a runtime `Expr`
/// that references table columns.
#[derive(Debug, Clone)]
enum Resolved {
    Scalar(ScalarValue),
    DynExpr(Expr),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert a CEL expression string into a DataFusion `Expr`, binding
/// `principal.*` references from the identity and `resource.*` references
/// to table columns.
///
/// Returns:
/// - `Ok(Some(expr))` when the expression produces a column-dependent filter.
/// - `Ok(None)` when the expression evaluates to constant `true` at
///   planning time (meaning "no filter needed").
/// - `Err` if the expression cannot be translated.
pub fn cel_to_datafusion_expr(
    cel: &str,
    identity: &QueryIdentity,
) -> Result<Option<Expr>, CelConvertError> {
    let parsed =
        cel_parser::parse(cel).map_err(|e| CelConvertError::ParseError(e.to_string()))?;
    let resolved = convert_expr(&parsed, identity)?;
    match resolved {
        Resolved::Scalar(ScalarValue::Boolean(Some(true))) => Ok(None),
        Resolved::Scalar(ScalarValue::Boolean(Some(false))) => Ok(Some(lit(false))),
        Resolved::Scalar(_) => Ok(None),
        Resolved::DynExpr(expr) => Ok(Some(expr)),
    }
}

/// Evaluate a CEL expression to a boolean at planning time using the
/// `cel-interpreter` runtime. Used for column-mask decisions where all
/// inputs (`principal.*`, `resource.table_name`) are known.
///
/// Follows a *fail-closed* policy: if evaluation fails for any reason the
/// column is masked.
pub fn cel_to_bool(
    cel: &str,
    identity: &QueryIdentity,
    resource_table: &str,
) -> Result<bool, CelConvertError> {
    use cel_interpreter::{Context, Program, Value};

    let program =
        Program::compile(cel).map_err(|e| CelConvertError::ParseError(e.to_string()))?;

    let mut ctx = Context::default();

    let mut principal: HashMap<&str, Value> = HashMap::new();
    principal.insert("role", Value::String(Arc::new(identity.role.clone())));
    if let Some(ref region) = identity.region {
        principal.insert("region", Value::String(Arc::new(region.clone())));
    }
    if let Some(ref name) = identity.name {
        principal.insert("name", Value::String(Arc::new(name.clone())));
    }
    ctx.add_variable_from_value("principal", principal);

    let mut resource: HashMap<&str, Value> = HashMap::new();
    resource.insert(
        "table_name",
        Value::String(Arc::new(resource_table.to_string())),
    );
    ctx.add_variable_from_value("resource", resource);

    match program.execute(&ctx) {
        Ok(Value::Bool(b)) => Ok(b),
        Ok(_) => Ok(false),
        Err(_) => Ok(true),
    }
}

// ---------------------------------------------------------------------------
// Recursive AST walker
// ---------------------------------------------------------------------------

fn convert_expr(expr: &Expression, identity: &QueryIdentity) -> Result<Resolved, CelConvertError> {
    match expr {
        Expression::Atom(atom) => convert_atom(atom),

        Expression::Ident(name) => match name.as_str() {
            "true" => Ok(Resolved::Scalar(ScalarValue::Boolean(Some(true)))),
            "false" => Ok(Resolved::Scalar(ScalarValue::Boolean(Some(false)))),
            "null" => Ok(Resolved::Scalar(ScalarValue::Null)),
            _ => Err(CelConvertError::UnsupportedExpression(format!(
                "bare identifier '{name}' without member access"
            ))),
        },

        Expression::Member(left, member) => convert_member(left, member, identity),

        Expression::Relation(left, op, right) => {
            let l = convert_expr(left, identity)?;
            let r = convert_expr(right, identity)?;
            convert_relation(l, op, r)
        }

        Expression::And(left, right) => {
            let l = convert_expr(left, identity)?;
            let r = convert_expr(right, identity)?;
            convert_and(l, r)
        }

        Expression::Or(left, right) => {
            let l = convert_expr(left, identity)?;
            let r = convert_expr(right, identity)?;
            convert_or(l, r)
        }

        Expression::Unary(op, inner) => {
            let inner = convert_expr(inner, identity)?;
            convert_unary(op, inner)
        }

        Expression::Arithmetic(left, op, right) => {
            let l = convert_expr(left, identity)?;
            let r = convert_expr(right, identity)?;
            convert_arithmetic(l, op, r)
        }

        other => Err(CelConvertError::UnsupportedExpression(format!(
            "{other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Leaf converters
// ---------------------------------------------------------------------------

fn convert_atom(atom: &Atom) -> Result<Resolved, CelConvertError> {
    match atom {
        Atom::Int(v) => Ok(Resolved::Scalar(ScalarValue::Int64(Some(*v)))),
        Atom::UInt(v) => Ok(Resolved::Scalar(ScalarValue::UInt64(Some(*v)))),
        Atom::Float(v) => Ok(Resolved::Scalar(ScalarValue::Float64(Some(*v)))),
        Atom::String(v) => Ok(Resolved::Scalar(ScalarValue::Utf8(Some(v.to_string())))),
        Atom::Bool(v) => Ok(Resolved::Scalar(ScalarValue::Boolean(Some(*v)))),
        Atom::Null => Ok(Resolved::Scalar(ScalarValue::Null)),
        Atom::Bytes(_) => Err(CelConvertError::UnsupportedExpression(
            "byte literals".into(),
        )),
    }
}

fn convert_member(
    left: &Expression,
    member: &Member,
    identity: &QueryIdentity,
) -> Result<Resolved, CelConvertError> {
    match (left, member) {
        (Expression::Ident(obj), Member::Attribute(field)) if obj.as_str() == "resource" => {
            Ok(Resolved::DynExpr(col(field.as_str())))
        }

        (Expression::Ident(obj), Member::Attribute(field)) if obj.as_str() == "principal" => {
            resolve_principal_field(field, identity)
        }

        _ => Err(CelConvertError::UnsupportedExpression(
            "nested or complex member access".into(),
        )),
    }
}

fn resolve_principal_field(
    field: &str,
    identity: &QueryIdentity,
) -> Result<Resolved, CelConvertError> {
    match field {
        "role" => Ok(Resolved::Scalar(ScalarValue::Utf8(Some(
            identity.role.clone(),
        )))),
        "region" => identity
            .region
            .as_ref()
            .map(|r| Resolved::Scalar(ScalarValue::Utf8(Some(r.clone()))))
            .ok_or_else(|| CelConvertError::MissingIdentityField("region".into())),
        "name" => identity
            .name
            .as_ref()
            .map(|n| Resolved::Scalar(ScalarValue::Utf8(Some(n.clone()))))
            .ok_or_else(|| CelConvertError::MissingIdentityField("name".into())),
        other => Err(CelConvertError::MissingIdentityField(other.into())),
    }
}

// ---------------------------------------------------------------------------
// Operator converters
// ---------------------------------------------------------------------------

fn resolved_to_expr(r: Resolved) -> Expr {
    match r {
        Resolved::Scalar(sv) => lit(sv),
        Resolved::DynExpr(e) => e,
    }
}

fn convert_relation(
    left: Resolved,
    op: &RelationOp,
    right: Resolved,
) -> Result<Resolved, CelConvertError> {
    if let (Resolved::Scalar(ref l), Resolved::Scalar(ref r)) = (&left, &right) {
        let result = match op {
            RelationOp::Equals => scalar_eq(l, r),
            RelationOp::NotEquals => scalar_eq(l, r).map(|b| !b),
            RelationOp::LessThan => scalar_cmp(l, r).map(|o| o == Ordering::Less),
            RelationOp::LessThanEq => scalar_cmp(l, r).map(|o| o != Ordering::Greater),
            RelationOp::GreaterThan => scalar_cmp(l, r).map(|o| o == Ordering::Greater),
            RelationOp::GreaterThanEq => scalar_cmp(l, r).map(|o| o != Ordering::Less),
            RelationOp::In => None,
        };
        if let Some(b) = result {
            return Ok(Resolved::Scalar(ScalarValue::Boolean(Some(b))));
        }
    }

    let l = resolved_to_expr(left);
    let r = resolved_to_expr(right);
    let expr = match op {
        RelationOp::Equals => l.eq(r),
        RelationOp::NotEquals => l.not_eq(r),
        RelationOp::LessThan => l.lt(r),
        RelationOp::LessThanEq => l.lt_eq(r),
        RelationOp::GreaterThan => l.gt(r),
        RelationOp::GreaterThanEq => l.gt_eq(r),
        RelationOp::In => {
            return Err(CelConvertError::UnsupportedExpression(
                "'in' operator".into(),
            ));
        }
    };
    Ok(Resolved::DynExpr(expr))
}

fn convert_and(left: Resolved, right: Resolved) -> Result<Resolved, CelConvertError> {
    match (&left, &right) {
        (Resolved::Scalar(ScalarValue::Boolean(Some(false))), _)
        | (_, Resolved::Scalar(ScalarValue::Boolean(Some(false)))) => {
            Ok(Resolved::Scalar(ScalarValue::Boolean(Some(false))))
        }
        (Resolved::Scalar(ScalarValue::Boolean(Some(true))), _) => Ok(right),
        (_, Resolved::Scalar(ScalarValue::Boolean(Some(true)))) => Ok(left),
        _ => {
            let l = resolved_to_expr(left);
            let r = resolved_to_expr(right);
            Ok(Resolved::DynExpr(l.and(r)))
        }
    }
}

fn convert_or(left: Resolved, right: Resolved) -> Result<Resolved, CelConvertError> {
    match (&left, &right) {
        (Resolved::Scalar(ScalarValue::Boolean(Some(true))), _)
        | (_, Resolved::Scalar(ScalarValue::Boolean(Some(true)))) => {
            Ok(Resolved::Scalar(ScalarValue::Boolean(Some(true))))
        }
        (Resolved::Scalar(ScalarValue::Boolean(Some(false))), _) => Ok(right),
        (_, Resolved::Scalar(ScalarValue::Boolean(Some(false)))) => Ok(left),
        _ => {
            let l = resolved_to_expr(left);
            let r = resolved_to_expr(right);
            Ok(Resolved::DynExpr(l.or(r)))
        }
    }
}

fn convert_unary(op: &UnaryOp, inner: Resolved) -> Result<Resolved, CelConvertError> {
    match op {
        UnaryOp::Not => match inner {
            Resolved::Scalar(ScalarValue::Boolean(Some(b))) => {
                Ok(Resolved::Scalar(ScalarValue::Boolean(Some(!b))))
            }
            Resolved::DynExpr(e) => Ok(Resolved::DynExpr(not(e))),
            _ => Err(CelConvertError::UnsupportedExpression(
                "NOT on non-boolean".into(),
            )),
        },
        UnaryOp::DoubleNot => Ok(inner),
        UnaryOp::Minus => match inner {
            Resolved::Scalar(ScalarValue::Int64(Some(v))) => {
                Ok(Resolved::Scalar(ScalarValue::Int64(Some(-v))))
            }
            Resolved::Scalar(ScalarValue::Float64(Some(v))) => {
                Ok(Resolved::Scalar(ScalarValue::Float64(Some(-v))))
            }
            Resolved::DynExpr(e) => Ok(Resolved::DynExpr(Expr::Negative(Box::new(e)))),
            _ => Err(CelConvertError::UnsupportedExpression(
                "unary minus on unsupported type".into(),
            )),
        },
        UnaryOp::DoubleMinus => Ok(inner),
    }
}

fn convert_arithmetic(
    left: Resolved,
    op: &ArithmeticOp,
    right: Resolved,
) -> Result<Resolved, CelConvertError> {
    let l = resolved_to_expr(left);
    let r = resolved_to_expr(right);
    let expr = match op {
        ArithmeticOp::Add => l + r,
        ArithmeticOp::Subtract => l - r,
        ArithmeticOp::Multiply => l * r,
        ArithmeticOp::Divide => l / r,
        ArithmeticOp::Modulus => l % r,
    };
    Ok(Resolved::DynExpr(expr))
}

// ---------------------------------------------------------------------------
// Scalar helpers for constant folding
// ---------------------------------------------------------------------------

fn scalar_eq(a: &ScalarValue, b: &ScalarValue) -> Option<bool> {
    match (a, b) {
        (ScalarValue::Utf8(Some(a)), ScalarValue::Utf8(Some(b))) => Some(a == b),
        (ScalarValue::Boolean(Some(a)), ScalarValue::Boolean(Some(b))) => Some(a == b),
        (ScalarValue::Int64(Some(a)), ScalarValue::Int64(Some(b))) => Some(a == b),
        (ScalarValue::UInt64(Some(a)), ScalarValue::UInt64(Some(b))) => Some(a == b),
        (ScalarValue::Float64(Some(a)), ScalarValue::Float64(Some(b))) => Some(a == b),
        (ScalarValue::Null, ScalarValue::Null) => Some(true),
        _ => None,
    }
}

fn scalar_cmp(a: &ScalarValue, b: &ScalarValue) -> Option<Ordering> {
    match (a, b) {
        (ScalarValue::Int64(Some(a)), ScalarValue::Int64(Some(b))) => Some(a.cmp(b)),
        (ScalarValue::UInt64(Some(a)), ScalarValue::UInt64(Some(b))) => Some(a.cmp(b)),
        (ScalarValue::Float64(Some(a)), ScalarValue::Float64(Some(b))) => a.partial_cmp(b),
        (ScalarValue::Utf8(Some(a)), ScalarValue::Utf8(Some(b))) => Some(a.cmp(b)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cel_filter::QueryIdentity;

    fn analyst_us_east() -> QueryIdentity {
        QueryIdentity {
            role: "analyst".into(),
            region: Some("us-east".into()),
            name: None,
        }
    }

    fn physician_dr_smith() -> QueryIdentity {
        QueryIdentity {
            role: "physician".into(),
            region: None,
            name: Some("Dr. Smith".into()),
        }
    }

    fn legal_user() -> QueryIdentity {
        QueryIdentity {
            role: "legal".into(),
            region: Some("us-east".into()),
            name: None,
        }
    }

    fn admin_user() -> QueryIdentity {
        QueryIdentity {
            role: "admin".into(),
            region: None,
            name: None,
        }
    }

    // -- cel_to_datafusion_expr tests -----------------------------------------

    #[test]
    fn test_region_equality_filter() {
        let expr = cel_to_datafusion_expr(
            "(resource.region == principal.region)",
            &analyst_us_east(),
        )
        .unwrap();
        assert!(expr.is_some());
        let e = expr.unwrap();
        assert_eq!(
            format!("{e}"),
            format!("{}", col("region").eq(lit("us-east")))
        );
    }

    #[test]
    fn test_physician_name_filter() {
        let expr = cel_to_datafusion_expr(
            "(resource.treating_physician == principal.name)",
            &physician_dr_smith(),
        )
        .unwrap();
        assert!(expr.is_some());
        let e = expr.unwrap();
        assert_eq!(
            format!("{e}"),
            format!(
                "{}",
                col("treating_physician").eq(lit("Dr. Smith"))
            )
        );
    }

    #[test]
    fn test_missing_identity_field_returns_error() {
        let id = QueryIdentity {
            role: "analyst".into(),
            region: None,
            name: None,
        };
        let result =
            cel_to_datafusion_expr("(resource.region == principal.region)", &id);
        assert!(result.is_err());
    }

    #[test]
    fn test_deny_override_non_legal() {
        let cel = "(resource.legal_hold == true) && !(principal.role == \"legal\")";
        let expr = cel_to_datafusion_expr(cel, &analyst_us_east()).unwrap();
        assert!(expr.is_some(), "non-legal user should get a filter");
        let e = expr.unwrap();
        assert_eq!(
            format!("{e}"),
            format!("{}", col("legal_hold").eq(lit(true)))
        );
    }

    #[test]
    fn test_deny_override_legal_user() {
        let cel = "(resource.legal_hold == true) && !(principal.role == \"legal\")";
        let expr = cel_to_datafusion_expr(cel, &legal_user()).unwrap();
        assert!(
            expr.is_some(),
            "legal user: principal.role == 'legal' is true, so !(true) = false, whole AND = false"
        );
        assert_eq!(format!("{}", expr.unwrap()), format!("{}", lit(false)));
    }

    #[test]
    fn test_constant_true_returns_none() {
        let cel = "(principal.role == \"analyst\")";
        let expr =
            cel_to_datafusion_expr(cel, &analyst_us_east()).unwrap();
        assert!(expr.is_none(), "constant true should return None (no filter)");
    }

    #[test]
    fn test_constant_false_returns_lit_false() {
        let cel = "(principal.role == \"admin\")";
        let expr =
            cel_to_datafusion_expr(cel, &analyst_us_east()).unwrap();
        assert!(expr.is_some());
        assert_eq!(format!("{}", expr.unwrap()), format!("{}", lit(false)));
    }

    #[test]
    fn test_or_expression() {
        let cel = "(principal.role == \"admin\") || (principal.role == \"physician\")";
        let expr = cel_to_datafusion_expr(cel, &admin_user()).unwrap();
        assert!(expr.is_none(), "admin matches first branch → constant true → None");
    }

    #[test]
    fn test_not_expression() {
        let cel = "!(resource.active == false)";
        let id = analyst_us_east();
        let expr = cel_to_datafusion_expr(cel, &id).unwrap();
        assert!(expr.is_some());
    }

    #[test]
    fn test_comparison_operators() {
        let id = analyst_us_east();
        let gt = cel_to_datafusion_expr("resource.amount > 100", &id).unwrap();
        assert!(gt.is_some());

        let lt = cel_to_datafusion_expr("resource.count < 10", &id).unwrap();
        assert!(lt.is_some());

        let gte = cel_to_datafusion_expr("resource.score >= 50", &id).unwrap();
        assert!(gte.is_some());

        let lte = cel_to_datafusion_expr("resource.score <= 50", &id).unwrap();
        assert!(lte.is_some());

        let ne = cel_to_datafusion_expr("resource.status != \"deleted\"", &id).unwrap();
        assert!(ne.is_some());
    }

    #[test]
    fn test_arithmetic_in_filter() {
        let id = analyst_us_east();
        let expr =
            cel_to_datafusion_expr("resource.price + 10 > 100", &id).unwrap();
        assert!(expr.is_some());
    }

    #[test]
    fn test_parse_error() {
        let result = cel_to_datafusion_expr(")))invalid(((", &analyst_us_east());
        assert!(result.is_err());
    }

    // -- cel_to_bool tests (column-mask evaluation) ----------------------------

    #[test]
    fn test_mask_applies_to_analyst() {
        let cel = "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))";
        let result = cel_to_bool(cel, &analyst_us_east(), "patients").unwrap();
        assert!(result, "analyst should be masked");
    }

    #[test]
    fn test_mask_exempt_admin() {
        let cel = "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))";
        let result = cel_to_bool(cel, &admin_user(), "patients").unwrap();
        assert!(!result, "admin should NOT be masked");
    }

    #[test]
    fn test_mask_exempt_physician() {
        let cel = "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))";
        let result = cel_to_bool(cel, &physician_dr_smith(), "patients").unwrap();
        assert!(!result, "physician should NOT be masked");
    }

    #[test]
    fn test_mask_wrong_table() {
        let cel = "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))";
        let result = cel_to_bool(cel, &analyst_us_east(), "orders").unwrap();
        assert!(!result, "wrong table should not match");
    }

    #[test]
    fn test_mask_parse_error_is_fail_closed() {
        let result = cel_to_bool(")))bad(((", &analyst_us_east(), "patients");
        assert!(result.is_err());
    }
}
