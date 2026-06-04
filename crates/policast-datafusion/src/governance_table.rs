use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::catalog::Session;
use datafusion::common::Result as DFResult;
use datafusion::datasource::TableProvider;
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown, TableType};
use datafusion::physical_expr::create_physical_expr;
use datafusion::physical_expr::expressions::{Column, Literal};
use datafusion::physical_plan::filter::FilterExec;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::physical_plan::{ExecutionPlan, PhysicalExpr};

use policast_core::PolicyManifest;

use crate::cel_filter::{build_column_masks, build_row_filters, QueryIdentity};

/// A governance-aware table wrapper that injects row-level filters
/// and column masks derived from compiled Cedar/CEL policies.
///
/// Wraps any existing `TableProvider` and transparently adds governance
/// predicates at scan time based on the querying user's identity.
pub struct GovernedTable {
    inner: Arc<dyn TableProvider>,
    manifest: PolicyManifest,
    table_name: String,
    identity: QueryIdentity,
}

impl GovernedTable {
    pub fn new(
        inner: Arc<dyn TableProvider>,
        manifest: PolicyManifest,
        table_name: impl Into<String>,
        identity: QueryIdentity,
    ) -> Self {
        Self {
            inner,
            manifest,
            table_name: table_name.into(),
            identity,
        }
    }

    /// Returns the list of (column, mask_value) pairs that should be applied.
    pub fn masked_columns(&self) -> Vec<(String, String)> {
        build_column_masks(&self.manifest, &self.table_name, &self.identity)
    }
}

impl std::fmt::Debug for GovernedTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GovernedTable")
            .field("table_name", &self.table_name)
            .field("identity_role", &self.identity.role)
            .finish()
    }
}

#[async_trait]
impl TableProvider for GovernedTable {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }
    fn table_type(&self) -> TableType {
        self.inner.table_type()
    }
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DFResult<Vec<TableProviderFilterPushDown>> {
        self.inner.supports_filters_pushdown(filters)
    }
    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        let governance_filters =
            build_row_filters(&self.manifest, &self.table_name, &self.identity)?;

        // Pass user-provided filters to the inner provider (it can push them
        // down if it supports them). Governance filters are applied as a
        // FilterExec wrapper so they are always enforced.
        let inner_plan = self.inner.scan(state, projection, filters, limit).await?;

        let plan = apply_governance_filters(inner_plan, &governance_filters)?;

        let masks = build_column_masks(&self.manifest, &self.table_name, &self.identity);
        if masks.is_empty() {
            return Ok(plan);
        }

        apply_column_masks(plan, &masks)
    }
}

/// Apply governance row filters by wrapping the plan in `FilterExec` nodes.
fn apply_governance_filters(
    plan: Arc<dyn ExecutionPlan>,
    filters: &[Expr],
) -> DFResult<Arc<dyn ExecutionPlan>> {
    if filters.is_empty() {
        return Ok(plan);
    }

    let schema = plan.schema();
    let df_schema = datafusion::common::DFSchema::try_from(schema.as_ref().clone())?;
    let props = datafusion::execution::context::ExecutionProps::new();

    let mut current = plan;
    for filter_expr in filters {
        let physical_expr = create_physical_expr(filter_expr, &df_schema, &props)?;
        current = Arc::new(FilterExec::try_new(physical_expr, current)?);
    }

    Ok(current)
}

/// Wrap an execution plan in a `ProjectionExec` that replaces masked
/// columns with literal string values while passing others through.
fn apply_column_masks(
    plan: Arc<dyn ExecutionPlan>,
    masks: &[(String, String)],
) -> DFResult<Arc<dyn ExecutionPlan>> {
    let schema = plan.schema();
    let masked_names: HashSet<&str> = masks.iter().map(|(col, _)| col.as_str()).collect();
    let mask_values: std::collections::HashMap<&str, &str> = masks
        .iter()
        .map(|(col, val)| (col.as_str(), val.as_str()))
        .collect();

    let mut projection_exprs: Vec<(Arc<dyn PhysicalExpr>, String)> = Vec::new();

    for (idx, field) in schema.fields().iter().enumerate() {
        let name = field.name().clone();
        if masked_names.contains(name.as_str()) {
            let mask_val = mask_values[name.as_str()];
            let lit_expr: Arc<dyn PhysicalExpr> = match field.data_type() {
                DataType::Utf8 | DataType::LargeUtf8 => Arc::new(Literal::new(
                    datafusion::common::ScalarValue::Utf8(Some(mask_val.to_string())),
                )),
                _ => Arc::new(Literal::new(datafusion::common::ScalarValue::Utf8(
                    Some(mask_val.to_string()),
                ))),
            };
            projection_exprs.push((lit_expr, name));
        } else {
            let col_expr: Arc<dyn PhysicalExpr> = Arc::new(Column::new(&name, idx));
            projection_exprs.push((col_expr, name));
        }
    }

    Ok(Arc::new(ProjectionExec::try_new(projection_exprs, plan)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::model::{CompiledPolicy, Effect, FilterType};
    use policast_core::PolicyManifest;

    fn _test_manifest() -> PolicyManifest {
        PolicyManifest {
            version: "1.0".into(),
            policies: vec![CompiledPolicy {
                id: "test_row_filter".into(),
                effect: Effect::Permit,
                filter_type: FilterType::RowFilter,
                target_table: "patients".into(),
                column: None,
                target_tag: None,
                applies_to_tag: None,
                cel_expression: "(resource.region == principal.region)".into(),
                applies_to: None,
                description: None,
            }],
        }
    }

    #[test]
    fn test_masked_columns_analyst() {
        let manifest = PolicyManifest {
            version: "1.0".into(),
            policies: vec![CompiledPolicy {
                id: "mask_ssn".into(),
                effect: Effect::Forbid,
                filter_type: FilterType::ColumnMask,
                target_table: "patients".into(),
                column: Some("ssn".into()),
                target_tag: None,
                applies_to_tag: None,
                cel_expression:
                    "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))"
                        .into(),
                applies_to: None,
                description: None,
            }],
        };
        let identity = QueryIdentity {
            role: "analyst".into(),
            region: None,
            name: None,
        };
        let governed = GovernedTable::new(
            Arc::new(DummyProvider),
            manifest,
            "patients",
            identity,
        );
        let masks = governed.masked_columns();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].0, "ssn");
    }

    struct DummyProvider;

    impl std::fmt::Debug for DummyProvider {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "DummyProvider")
        }
    }

    #[async_trait]
    impl TableProvider for DummyProvider {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn schema(&self) -> SchemaRef {
            Arc::new(datafusion::arrow::datatypes::Schema::empty())
        }
        fn table_type(&self) -> TableType {
            TableType::Base
        }
        async fn scan(
            &self,
            _state: &dyn Session,
            _projection: Option<&Vec<usize>>,
            _filters: &[Expr],
            _limit: Option<usize>,
        ) -> DFResult<Arc<dyn ExecutionPlan>> {
            unimplemented!("dummy provider")
        }
    }
}
