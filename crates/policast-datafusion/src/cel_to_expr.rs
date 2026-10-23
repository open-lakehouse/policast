use std::cmp::Ordering;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use cel_parser::ast::{operators, CallExpr, Expr as CelExpr, IdedExpr, SelectExpr};
use cel_parser::reference::Val;
use cel_parser::Parser;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::{col, lit, not, Expr};

use crate::identity::PrincipalProvider;

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
    identity: &dyn PrincipalProvider,
) -> Result<Option<Expr>, CelConvertError> {
    let parsed = parse_cel(cel)?;
    let resolved = convert_expr(&parsed, identity)?;
    match resolved {
        Resolved::Scalar(ScalarValue::Boolean(Some(true))) => Ok(None),
        Resolved::Scalar(ScalarValue::Boolean(Some(false))) => Ok(Some(lit(false))),
        Resolved::Scalar(_) => Err(CelConvertError::UnsupportedExpression(
            "policy predicate must be boolean".into(),
        )),
        Resolved::DynExpr(expr) => Ok(Some(expr)),
    }
}

/// Parse a CEL expression into its AST, fail-safe against parser panics.
///
/// cel-parser 0.10 uses an ANTLR-generated backend that can *panic* on some
/// malformed inputs instead of returning a parse error. Governance enforcement
/// runs on the query hot path and must never crash the engine, so we trap any
/// unwind and surface it as an ordinary [`CelConvertError::ParseError`]. The
/// closure returns `Result<_, String>` (not the parser's error type) to keep
/// the caught value `UnwindSafe`.
fn parse_cel(cel: &str) -> Result<IdedExpr, CelConvertError> {
    match catch_unwind(AssertUnwindSafe(|| {
        Parser::new().parse(cel).map_err(|e| e.to_string())
    })) {
        Ok(Ok(expr)) => Ok(expr),
        Ok(Err(msg)) => Err(CelConvertError::ParseError(msg)),
        Err(_) => Err(CelConvertError::ParseError(format!(
            "CEL parser panicked on expression {cel:?}"
        ))),
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
    identity: &dyn PrincipalProvider,
    resource_table: &str,
) -> Result<bool, CelConvertError> {
    use cel_interpreter::{Context, Program, Value};
    // Validate every branch against the same supported subset as row policies.
    convert_expr(&parse_cel(cel)?, identity)?;

    // Same fail-safe rationale as `parse_cel`: `Program::compile` runs the same
    // ANTLR-based parser, which can panic on malformed input. Trap it so a bad
    // mask expression fails closed (masks) instead of crashing the engine.
    let program = match catch_unwind(AssertUnwindSafe(|| {
        Program::compile(cel).map_err(|e| e.to_string())
    })) {
        Ok(Ok(program)) => program,
        Ok(Err(msg)) => return Err(CelConvertError::ParseError(msg)),
        Err(_) => {
            return Err(CelConvertError::ParseError(format!(
                "CEL parser panicked on expression {cel:?}"
            )))
        }
    };

    let mut ctx = Context::default();

    let mut principal: HashMap<String, Value> = HashMap::new();
    for (key, value) in identity.principal_attributes() {
        principal.insert(key, Value::String(Arc::new(value)));
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
        Ok(_) => Err(CelConvertError::UnsupportedExpression(
            "mask predicate must be boolean".into(),
        )),
        Err(_) => Err(CelConvertError::UnsupportedExpression(
            "mask evaluation failed".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Recursive AST walker
// ---------------------------------------------------------------------------

fn convert_expr(
    node: &IdedExpr,
    identity: &dyn PrincipalProvider,
) -> Result<Resolved, CelConvertError> {
    match &node.expr {
        // Scalar literals (`1`, `"x"`, `true`, `null`, ...).
        CelExpr::Literal(val) => convert_val(val),

        // Bare identifiers. `true`/`false`/`null` normally parse as literals,
        // but keep this arm as a defensive fallback for any that reach here.
        CelExpr::Ident(name) => match name.as_str() {
            "true" => Ok(Resolved::Scalar(ScalarValue::Boolean(Some(true)))),
            "false" => Ok(Resolved::Scalar(ScalarValue::Boolean(Some(false)))),
            "null" => Ok(Resolved::Scalar(ScalarValue::Null)),
            _ => Err(CelConvertError::UnsupportedExpression(format!(
                "bare identifier '{name}' without member access"
            ))),
        },

        // Field selection: `resource.<col>` / `principal.<attr>`.
        CelExpr::Select(select) => convert_select(select, identity),

        // In CEL's canonical AST every operator is a function call
        // (`_==_`, `_&&_`, `!_`, `_+_`, ...); see `cel_parser::ast::operators`.
        CelExpr::Call(call) => convert_call(call, identity),

        other => Err(CelConvertError::UnsupportedExpression(format!("{other:?}"))),
    }
}

// ---------------------------------------------------------------------------
// Leaf converters
// ---------------------------------------------------------------------------

fn convert_val(val: &Val) -> Result<Resolved, CelConvertError> {
    let scalar = match val {
        Val::Int(v) => ScalarValue::Int64(Some(*v)),
        Val::UInt(v) => ScalarValue::UInt64(Some(*v)),
        Val::Double(v) => ScalarValue::Float64(Some(*v)),
        Val::String(v) => ScalarValue::Utf8(Some(v.clone())),
        Val::Boolean(v) => ScalarValue::Boolean(Some(*v)),
        Val::Null => ScalarValue::Null,
        Val::Bytes(_) => {
            return Err(CelConvertError::UnsupportedExpression(
                "byte literals".into(),
            ))
        }
    };
    Ok(Resolved::Scalar(scalar))
}

fn convert_select(
    select: &SelectExpr,
    identity: &dyn PrincipalProvider,
) -> Result<Resolved, CelConvertError> {
    // Presence tests (`has(x.y)`) cannot be translated to a column predicate.
    if select.test {
        return Err(CelConvertError::UnsupportedExpression(
            "presence test (has)".into(),
        ));
    }

    match &select.operand.expr {
        CelExpr::Ident(obj) if obj == "resource" => {
            Ok(Resolved::DynExpr(col(select.field.as_str())))
        }

        CelExpr::Ident(obj) if obj == "principal" => {
            resolve_principal_field(&select.field, identity)
        }

        _ => Err(CelConvertError::UnsupportedExpression(
            "nested or complex member access".into(),
        )),
    }
}

fn resolve_principal_field(
    field: &str,
    identity: &dyn PrincipalProvider,
) -> Result<Resolved, CelConvertError> {
    identity
        .attribute(field)
        .map(|v| Resolved::Scalar(ScalarValue::Utf8(Some(v))))
        .ok_or_else(|| CelConvertError::MissingIdentityField(field.into()))
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

/// Dispatch an operator call (`func_name` is one of `cel_parser::ast::operators`)
/// to the matching converter. Only pure boolean / comparison / arithmetic
/// operators are supported; method calls and macros are rejected.
fn convert_call(
    call: &CallExpr,
    identity: &dyn PrincipalProvider,
) -> Result<Resolved, CelConvertError> {
    // A `target` means a receiver-style call such as `x.startsWith(y)`; only
    // free-function operator calls are translatable to a column predicate.
    if call.target.is_some() {
        return Err(CelConvertError::UnsupportedExpression(format!(
            "method call '{}'",
            call.func_name
        )));
    }

    match call.func_name.as_str() {
        operators::LOGICAL_NOT => convert_not(convert_unary_arg(call, identity)?),
        operators::NEGATE => convert_negate(convert_unary_arg(call, identity)?),
        operators::LOGICAL_AND => {
            let (l, r) = convert_binary_args(call, identity)?;
            convert_and(l, r)
        }
        operators::LOGICAL_OR => {
            let (l, r) = convert_binary_args(call, identity)?;
            convert_or(l, r)
        }
        operators::EQUALS
        | operators::NOT_EQUALS
        | operators::LESS
        | operators::LESS_EQUALS
        | operators::GREATER
        | operators::GREATER_EQUALS => {
            let (l, r) = convert_binary_args(call, identity)?;
            convert_relation(call.func_name.as_str(), l, r)
        }
        operators::ADD
        | operators::SUBSTRACT
        | operators::MULTIPLY
        | operators::DIVIDE
        | operators::MODULO => {
            let (l, r) = convert_binary_args(call, identity)?;
            convert_arithmetic(call.func_name.as_str(), l, r)
        }
        operators::IN => Err(CelConvertError::UnsupportedExpression(
            "'in' operator".into(),
        )),
        other => Err(CelConvertError::UnsupportedExpression(format!(
            "function/operator '{other}'"
        ))),
    }
}

fn convert_unary_arg(
    call: &CallExpr,
    identity: &dyn PrincipalProvider,
) -> Result<Resolved, CelConvertError> {
    match call.args.as_slice() {
        [arg] => convert_expr(arg, identity),
        _ => Err(CelConvertError::UnsupportedExpression(format!(
            "operator '{}' expects 1 argument",
            call.func_name
        ))),
    }
}

fn convert_binary_args(
    call: &CallExpr,
    identity: &dyn PrincipalProvider,
) -> Result<(Resolved, Resolved), CelConvertError> {
    match call.args.as_slice() {
        [left, right] => Ok((
            convert_expr(left, identity)?,
            convert_expr(right, identity)?,
        )),
        _ => Err(CelConvertError::UnsupportedExpression(format!(
            "operator '{}' expects 2 arguments",
            call.func_name
        ))),
    }
}

fn convert_relation(
    op: &str,
    left: Resolved,
    right: Resolved,
) -> Result<Resolved, CelConvertError> {
    if let (Resolved::Scalar(ref l), Resolved::Scalar(ref r)) = (&left, &right) {
        let result = match op {
            operators::EQUALS => scalar_eq(l, r),
            operators::NOT_EQUALS => scalar_eq(l, r).map(|b| !b),
            operators::LESS => scalar_cmp(l, r).map(|o| o == Ordering::Less),
            operators::LESS_EQUALS => scalar_cmp(l, r).map(|o| o != Ordering::Greater),
            operators::GREATER => scalar_cmp(l, r).map(|o| o == Ordering::Greater),
            operators::GREATER_EQUALS => scalar_cmp(l, r).map(|o| o != Ordering::Less),
            _ => None,
        };
        if let Some(b) = result {
            return Ok(Resolved::Scalar(ScalarValue::Boolean(Some(b))));
        }
    }

    let l = resolved_to_expr(left);
    let r = resolved_to_expr(right);
    let expr = match op {
        operators::EQUALS => l.eq(r),
        operators::NOT_EQUALS => l.not_eq(r),
        operators::LESS => l.lt(r),
        operators::LESS_EQUALS => l.lt_eq(r),
        operators::GREATER => l.gt(r),
        operators::GREATER_EQUALS => l.gt_eq(r),
        _ => {
            return Err(CelConvertError::UnsupportedExpression(format!(
                "relation '{op}'"
            )))
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

fn convert_not(inner: Resolved) -> Result<Resolved, CelConvertError> {
    match inner {
        Resolved::Scalar(ScalarValue::Boolean(Some(b))) => {
            Ok(Resolved::Scalar(ScalarValue::Boolean(Some(!b))))
        }
        Resolved::DynExpr(e) => Ok(Resolved::DynExpr(not(e))),
        _ => Err(CelConvertError::UnsupportedExpression(
            "NOT on non-boolean".into(),
        )),
    }
}

fn convert_negate(inner: Resolved) -> Result<Resolved, CelConvertError> {
    match inner {
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
    }
}

fn convert_arithmetic(
    op: &str,
    left: Resolved,
    right: Resolved,
) -> Result<Resolved, CelConvertError> {
    let l = resolved_to_expr(left);
    let r = resolved_to_expr(right);
    let expr = match op {
        operators::ADD => l + r,
        operators::SUBSTRACT => l - r,
        operators::MULTIPLY => l * r,
        operators::DIVIDE => l / r,
        operators::MODULO => l % r,
        _ => {
            return Err(CelConvertError::UnsupportedExpression(format!(
                "arithmetic '{op}'"
            )))
        }
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
        let expr =
            cel_to_datafusion_expr("(resource.region == principal.region)", &analyst_us_east())
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
            format!("{}", col("treating_physician").eq(lit("Dr. Smith")))
        );
    }

    #[test]
    fn test_missing_identity_field_returns_error() {
        let id = QueryIdentity {
            role: "analyst".into(),
            region: None,
            name: None,
        };
        let result = cel_to_datafusion_expr("(resource.region == principal.region)", &id);
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
        let expr = cel_to_datafusion_expr(cel, &analyst_us_east()).unwrap();
        assert!(
            expr.is_none(),
            "constant true should return None (no filter)"
        );
    }

    #[test]
    fn test_constant_false_returns_lit_false() {
        let cel = "(principal.role == \"admin\")";
        let expr = cel_to_datafusion_expr(cel, &analyst_us_east()).unwrap();
        assert!(expr.is_some());
        assert_eq!(format!("{}", expr.unwrap()), format!("{}", lit(false)));
    }

    #[test]
    fn test_or_expression() {
        let cel = "(principal.role == \"admin\") || (principal.role == \"physician\")";
        let expr = cel_to_datafusion_expr(cel, &admin_user()).unwrap();
        assert!(
            expr.is_none(),
            "admin matches first branch → constant true → None"
        );
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
        let expr = cel_to_datafusion_expr("resource.price + 10 > 100", &id).unwrap();
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
