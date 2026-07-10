//! Unity Catalog `routine_body=SQL` functions → DataFusion scalar UDFs.
//!
//! Ported from the `uc-query` sidecar (`ui/query-sidecar/src/functions.rs`).
//! A UC SQL function such as `add_tax(amount DOUBLE, rate DOUBLE)` with body
//! `amount * (1 + rate)` is registered as a DataFusion `ScalarUDF` on the
//! per-query `SessionContext` before the user SQL is planned, so the SQL can
//! call it — or so a function can be **tested** before it is saved to UC.
//!
//! The body is parsed once at registration time against a schema built from
//! the parameters (`parse_sql_expr`) and lowered to a `PhysicalExpr`
//! (`create_physical_expr`, which applies type coercion). At call time
//! `invoke_with_args` materializes the arguments into a single-batch
//! `RecordBatch` keyed by the parameter names, evaluates the precomputed
//! physical expression, and casts the result to the declared return type.

use std::any::Any;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::ArrayRef;
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use datafusion::common::{DFSchema, Result as DFResult};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::physical_expr::PhysicalExpr;
use datafusion::prelude::SessionContext;

/// A Unity Catalog `routine_body=SQL` scalar function lowered to a DataFusion UDF.
#[derive(Debug)]
struct SqlFunction {
    name: String,
    signature: Signature,
    arg_types: Vec<DataType>,
    return_type: DataType,
    input_schema: SchemaRef,
    physical_expr: Arc<dyn PhysicalExpr>,
}

impl PartialEq for SqlFunction {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.signature == other.signature
    }
}
impl Eq for SqlFunction {}
impl Hash for SqlFunction {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.signature.hash(state);
    }
}

impl ScalarUDFImpl for SqlFunction {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn signature(&self) -> &Signature {
        &self.signature
    }
    fn return_type(&self, _arg_types: &[DataType]) -> DFResult<DataType> {
        Ok(self.return_type.clone())
    }
    fn coerce_types(&self, arg_types: &[DataType]) -> DFResult<Vec<DataType>> {
        if arg_types.len() != self.arg_types.len() {
            return Err(DataFusionError::Plan(format!(
                "{} expects {} argument(s), got {}",
                self.name,
                self.arg_types.len(),
                arg_types.len()
            )));
        }
        Ok(self.arg_types.clone())
    }
    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DFResult<ColumnarValue> {
        let num_rows = args.number_rows;
        let columns: Vec<ArrayRef> = args
            .args
            .iter()
            .map(|v| v.to_array(num_rows))
            .collect::<DFResult<_>>()?;

        let batch = if columns.is_empty() {
            RecordBatch::try_new_with_options(
                Arc::clone(&self.input_schema),
                vec![],
                &RecordBatchOptions::new().with_row_count(Some(num_rows)),
            )
        } else {
            RecordBatch::try_new(Arc::clone(&self.input_schema), columns)
        }
        .map_err(|e| DataFusionError::Execution(format!("build args for {}: {e}", self.name)))?;

        let value = self.physical_expr.evaluate(&batch)?;
        let array = value.to_array(batch.num_rows())?;
        let casted = cast(&array, &self.return_type).map_err(|e| {
            DataFusionError::Execution(format!(
                "cast result of {} to {:?}: {e}",
                self.name, self.return_type
            ))
        })?;
        Ok(ColumnarValue::Array(casted))
    }
}

/// Register a `routine_body=SQL` function as a scalar UDF on `ctx`.
///
/// `name` is the (bare, lowercased) function name used in SQL. `input_params`
/// are `(parameter_name, type_text)` pairs in declaration order;
/// `return_type_text` and each `type_text` accept Unity Catalog
/// `ColumnTypeName` values (`INT`, `STRING`, `DOUBLE`, …) or SQL type text
/// (`DECIMAL(10,2)`). `routine_definition` is the SQL expression body.
/// Returns an error string when a type is unsupported or the body fails to
/// parse/plan — that error is the signal the function is not valid SQL.
pub fn register_sql_function(
    ctx: &SessionContext,
    name: &str,
    input_params: &[(String, String)],
    return_type_text: &str,
    routine_definition: &str,
) -> Result<(), String> {
    let fields: Vec<Field> = input_params
        .iter()
        .map(|(pname, ptext)| arrow_type_from_text(ptext).map(|dt| Field::new(pname, dt, true)))
        .collect::<Result<_, String>>()?;
    let arrow_schema: SchemaRef = Arc::new(Schema::new(fields));

    let df_schema = DFSchema::try_from(arrow_schema.as_ref().clone())
        .map_err(|e| format!("schema for function {name}: {e}"))?;

    let body_expr = ctx
        .parse_sql_expr(routine_definition, &df_schema)
        .map_err(|e| format!("parse body of function {name}: {e}"))?;
    let physical_expr = ctx
        .create_physical_expr(body_expr, &df_schema)
        .map_err(|e| format!("plan body of function {name}: {e}"))?;

    let arg_types: Vec<DataType> = arrow_schema
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect();
    let return_type = arrow_type_from_text(return_type_text)?;
    let signature = Signature::user_defined(Volatility::Immutable);

    let udf = SqlFunction {
        name: name.to_string(),
        signature,
        arg_types,
        return_type,
        input_schema: arrow_schema,
        physical_expr,
    };
    ctx.register_udf(ScalarUDF::from(udf));
    Ok(())
}

/// Map a Unity Catalog `ColumnTypeName` / SQL type text to an Arrow `DataType`.
fn arrow_type_from_text(text: &str) -> Result<DataType, String> {
    let upper = text.trim().to_uppercase();
    if upper.is_empty() {
        return Err("function type is required".to_string());
    }
    let base = upper.split(['(', '<']).next().unwrap_or(&upper).trim();
    let dt = match base {
        "INT" | "INTEGER" => DataType::Int32,
        "LONG" | "BIGINT" => DataType::Int64,
        "SHORT" | "SMALLINT" => DataType::Int16,
        "BYTE" | "TINYINT" => DataType::Int8,
        "FLOAT" | "REAL" => DataType::Float32,
        "DOUBLE" => DataType::Float64,
        "BOOLEAN" | "BOOL" => DataType::Boolean,
        "STRING" | "VARCHAR" | "CHAR" | "TEXT" => DataType::Utf8,
        "BINARY" => DataType::Binary,
        "DATE" => DataType::Date32,
        "TIMESTAMP" | "TIMESTAMP_NTZ" => DataType::Timestamp(TimeUnit::Microsecond, None),
        "DECIMAL" | "DEC" | "NUMERIC" => parse_decimal(&upper),
        other => return Err(format!("unsupported function type: {other}")),
    };
    Ok(dt)
}

/// Parse `DECIMAL(p,s)` text into `Decimal128`, defaulting to `(38, 0)`.
fn parse_decimal(upper: &str) -> DataType {
    if let (Some(open), Some(close)) = (upper.find('('), upper.find(')')) {
        if close > open {
            let inner = &upper[open + 1..close];
            let mut parts = inner.split(',');
            let precision = parts
                .next()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .unwrap_or(38);
            let scale = parts
                .next()
                .and_then(|s| s.trim().parse::<i8>().ok())
                .unwrap_or(0);
            return DataType::Decimal128(precision, scale);
        }
    }
    DataType::Decimal128(38, 18)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{Float64Array, Int32Array, StringArray};

    async fn run_scalar(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
        ctx.sql(sql).await.unwrap().collect().await.unwrap()
    }

    #[tokio::test]
    async fn registers_and_runs_int_function() {
        let ctx = SessionContext::new();
        register_sql_function(
            &ctx,
            "addone",
            &[("x".into(), "INT".into())],
            "INT",
            "x + 1",
        )
        .unwrap();
        let batches = run_scalar(&ctx, "SELECT addone(41) AS r").await;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(col.value(0), 42);
    }

    #[tokio::test]
    async fn registers_multi_param_double_function() {
        let ctx = SessionContext::new();
        register_sql_function(
            &ctx,
            "add_tax",
            &[
                ("amount".into(), "DOUBLE".into()),
                ("rate".into(), "DOUBLE".into()),
            ],
            "DOUBLE",
            "amount * (1 + rate)",
        )
        .unwrap();
        let batches = run_scalar(&ctx, "SELECT add_tax(100.0, 0.1) AS r").await;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((col.value(0) - 110.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn registers_string_function() {
        let ctx = SessionContext::new();
        register_sql_function(
            &ctx,
            "greet",
            &[("who".into(), "STRING".into())],
            "STRING",
            "concat('hello ', who)",
        )
        .unwrap();
        let batches = run_scalar(&ctx, "SELECT greet('world') AS r").await;
        let col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "hello world");
    }

    #[tokio::test]
    async fn invalid_body_is_rejected() {
        let ctx = SessionContext::new();
        let err = register_sql_function(&ctx, "bad", &[("x".into(), "INT".into())], "INT", "y + 1")
            .unwrap_err();
        assert!(
            err.contains("parse body") || err.contains("plan body"),
            "got: {err}"
        );
    }

    #[test]
    fn maps_common_types() {
        assert_eq!(arrow_type_from_text("INT").unwrap(), DataType::Int32);
        assert_eq!(arrow_type_from_text("string").unwrap(), DataType::Utf8);
        assert_eq!(arrow_type_from_text("DOUBLE").unwrap(), DataType::Float64);
        assert_eq!(
            arrow_type_from_text("DECIMAL(10,2)").unwrap(),
            DataType::Decimal128(10, 2)
        );
        assert!(arrow_type_from_text("STRUCT").is_err());
    }
}
