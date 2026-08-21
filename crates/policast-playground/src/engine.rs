//! The governed query engine.
//!
//! Given a table, a test principal, a set of Cedar policies, optional UC
//! SQL functions, and a SQL string, [`execute`] runs the query through a
//! [`GovernedTable`] so the result reflects the row filters and column
//! masks the policies imply for that principal.
//!
//! [`execute`] is UC-free (it takes an already-resolved table URI +
//! storage options) and is what the tests drive against a local Delta
//! table. [`PlaygroundService::run`] adds the UC step: resolve the
//! table's storage location + temporary credentials over the UC REST
//! API, then call [`execute`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::{SessionConfig, SessionContext};
use deltalake::{ensure_table_uri, open_table_with_storage_options};
use serde::{Deserialize, Serialize};

use policast_core::model::FilterType;
use policast_core::PolicyManifest;
use policast_datafusion::{AttrIdentity, GovernedTable};
use policast_uc::exchange;
use policast_uc::{AuthMode, TableOperation, UcRestClient};

use crate::functions::register_sql_function;
use crate::PlaygroundError;

/// Default + max row caps for a playground query.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

/// Row-major result cells; `None` is SQL NULL.
type CellRows = Vec<Vec<Option<String>>>;

/// The test principal: the role drives `principal.role`, and any extra
/// `attrs` populate `principal.<attr>` (e.g. `region`, `clearance`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrincipalSpec {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub attrs: BTreeMap<String, String>,
}

/// A Unity Catalog `routine_body=SQL` function to register before the
/// query is planned (inline test definition).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default)]
    pub params: Vec<FunctionParam>,
    /// Return type text (UC `ColumnTypeName` or SQL type text).
    pub data_type: String,
    /// The SQL expression body.
    pub routine_definition: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionParam {
    pub name: String,
    pub type_text: String,
}

/// A playground query request (the HTTP body shape).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlaygroundRequest {
    /// Fully-qualified UC table, e.g. `hospital.clinical.patients`.
    pub table: String,
    pub principal: PrincipalSpec,
    /// Cedar policy sources to compile + enforce. Empty = ungoverned.
    #[serde(default)]
    pub policies: Vec<String>,
    /// Inline UC SQL functions to register as UDFs.
    #[serde(default)]
    pub functions: Vec<FunctionDef>,
    pub sql: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Low-level execution parameters (no UC dependency).
pub struct ExecuteParams<'a> {
    /// Fully-qualified UC table name (drives policy matching).
    pub table_name: &'a str,
    /// Storage URI of the backing Delta table (e.g. `s3://…` or `file://…`).
    pub table_uri: &'a str,
    /// delta-rs storage options (vended creds, region, endpoint, …).
    pub storage_options: HashMap<String, String>,
    pub principal: &'a PrincipalSpec,
    pub policies: &'a [String],
    pub functions: &'a [FunctionDef],
    pub sql: &'a str,
    pub limit: usize,
}

/// A masked column in the governed result.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppliedMask {
    pub column: String,
    pub mask: String,
}

/// One compiled policy that targets the queried table (shown so the
/// user can see what governance is in play).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppliedPolicy {
    pub id: String,
    pub filter_type: String,
    pub target_table: String,
    pub cel_expression: String,
}

/// The governed query result.
#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// Row-major cells; `None` is SQL NULL.
    pub rows: CellRows,
    /// Rows produced before the display cap was applied.
    pub total_rows: usize,
    /// Whether `rows` was truncated to the limit.
    pub truncated: bool,
    pub applied_masks: Vec<AppliedMask>,
    pub applied_policies: Vec<AppliedPolicy>,
    pub elapsed_ms: u128,
}

fn filter_type_str(ft: &FilterType) -> &'static str {
    match ft {
        FilterType::RowFilter => "row_filter",
        FilterType::ColumnMask => "column_mask",
        FilterType::DenyOverride => "deny_override",
    }
}

/// Build the `AttrIdentity` for a principal: `role` + any extra attrs,
/// with `name`/`principal_id` defaulted from the id so policies that
/// reference `principal.name` resolve.
fn identity_for(principal: &PrincipalSpec) -> AttrIdentity {
    let mut id = AttrIdentity::new().with("role", principal.role.clone());
    for (k, v) in &principal.attrs {
        id = id.with(k.clone(), v.clone());
    }
    if !principal.attrs.contains_key("name") {
        id = id.with("name", principal.id.clone());
    }
    if !principal.attrs.contains_key("principal_id") {
        id = id.with("principal_id", principal.id.clone());
    }
    id
}

/// Compile the supplied Cedar policy sources into a single manifest.
fn compile_manifest(policies: &[String]) -> Result<PolicyManifest, PlaygroundError> {
    if policies.is_empty() {
        return Ok(PolicyManifest::new());
    }
    let joined = policies.join("\n\n");
    exchange::compile(&joined).map_err(|e| PlaygroundError::Compile(e.to_string()))
}

/// Last dotted segment of a table name (the alias used in SQL).
fn table_leaf(table_name: &str) -> &str {
    table_name.rsplit('.').next().unwrap_or(table_name)
}

/// Run a governed query against an already-resolved Delta table.
pub async fn execute(params: ExecuteParams<'_>) -> Result<QueryResult, PlaygroundError> {
    let started = Instant::now();
    let limit = params.limit.clamp(1, MAX_LIMIT);

    let manifest = compile_manifest(params.policies)?;

    // Surface the policies that target this table before the manifest is
    // moved into the GovernedTable.
    let applied_policies: Vec<AppliedPolicy> = manifest
        .policies_for_table(params.table_name)
        .into_iter()
        .map(|p| AppliedPolicy {
            id: p.id.clone(),
            filter_type: filter_type_str(&p.filter_type).to_string(),
            target_table: p.target_table.clone(),
            cel_expression: p.cel_expression.clone(),
        })
        .collect();

    // Open the backing Delta table.
    let table_url = ensure_table_uri(params.table_uri).map_err(|e| {
        PlaygroundError::Delta(format!("ensure_table_uri {}: {e}", params.table_uri))
    })?;
    let delta = open_table_with_storage_options(table_url, params.storage_options.clone())
        .await
        .map_err(|e| PlaygroundError::Delta(format!("open {}: {e}", params.table_uri)))?;
    let provider =
        delta.table_provider().build().await.map_err(|e| {
            PlaygroundError::Delta(format!("table_provider {}: {e}", params.table_uri))
        })?;

    let identity = identity_for(params.principal);
    let governed = GovernedTable::new(
        Arc::new(provider),
        manifest,
        params.table_name.to_string(),
        identity,
    );
    let applied_masks: Vec<AppliedMask> = governed
        .masked_columns()
        .into_iter()
        .map(|(column, mask)| AppliedMask { column, mask })
        .collect();

    // Per-query session: classic Utf8 layouts keep masks/filters simple.
    let mut config = SessionConfig::new();
    config
        .options_mut()
        .execution
        .parquet
        .schema_force_view_types = false;
    let ctx = SessionContext::new_with_config(config);

    for f in params.functions {
        let p: Vec<(String, String)> = f
            .params
            .iter()
            .map(|p| (p.name.clone(), p.type_text.clone()))
            .collect();
        let leaf = f.name.rsplit('.').next().unwrap_or(&f.name).to_lowercase();
        register_sql_function(&ctx, &leaf, &p, &f.data_type, &f.routine_definition).map_err(
            |message| PlaygroundError::Function {
                name: f.name.clone(),
                message,
            },
        )?;
    }

    let leaf = table_leaf(params.table_name).to_string();
    ctx.register_table(&leaf, Arc::new(governed))
        .map_err(|e| PlaygroundError::Query(format!("register table {leaf}: {e}")))?;

    let df = ctx
        .sql(params.sql)
        .await
        .map_err(|e| PlaygroundError::Query(e.to_string()))?;
    let arrow_schema = df.schema().as_arrow().clone();
    let columns: Vec<String> = arrow_schema
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    let batches = df
        .collect()
        .await
        .map_err(|e| PlaygroundError::Query(e.to_string()))?;

    let (rows, total_rows) = batches_to_rows(&batches, limit)?;

    Ok(QueryResult {
        columns,
        rows,
        total_rows,
        truncated: total_rows > limit,
        applied_masks,
        applied_policies,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Format Arrow batches into row-major `Option<String>` cells (NULL ->
/// `None`), capping the materialized rows at `limit` while still
/// counting the total produced.
fn batches_to_rows(
    batches: &[datafusion::arrow::record_batch::RecordBatch],
    limit: usize,
) -> Result<(CellRows, usize), PlaygroundError> {
    let opts = FormatOptions::default();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut total = 0usize;
    for batch in batches {
        let formatters: Vec<ArrayFormatter> = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c, &opts))
            .collect::<Result<_, _>>()
            .map_err(|e| PlaygroundError::Query(format!("format result: {e}")))?;
        for row in 0..batch.num_rows() {
            total += 1;
            if rows.len() >= limit {
                continue;
            }
            let mut r = Vec::with_capacity(batch.num_columns());
            for (ci, col) in batch.columns().iter().enumerate() {
                if col.is_null(row) {
                    r.push(None);
                } else {
                    r.push(Some(formatters[ci].value(row).to_string()));
                }
            }
            rows.push(r);
        }
    }
    Ok((rows, total))
}

/// The UC-aware playground service. It resolves a table's storage
/// location and temporary credentials over the UC REST API, then runs a
/// governed query against it.
///
/// For local development / offline use, a `local_tables` override maps a
/// table name straight to a storage URI, bypassing UC entirely — this is
/// what `--demo-table` / `--local-table` wire up so the playground works
/// with no catalog.
#[derive(Clone)]
pub struct PlaygroundService {
    client: UcRestClient,
    base_storage_options: HashMap<String, String>,
    default_limit: usize,
    local_tables: HashMap<String, String>,
}

impl PlaygroundService {
    /// Build a service against `uc_endpoint` with the given auth mode.
    pub fn new(uc_endpoint: impl Into<String>, auth: AuthMode) -> Self {
        Self {
            client: UcRestClient::new(uc_endpoint, auth),
            base_storage_options: HashMap::new(),
            default_limit: DEFAULT_LIMIT,
            local_tables: HashMap::new(),
        }
    }

    /// Static storage options merged under any vended credentials
    /// (region, endpoint, allow-http for RustFS, …).
    pub fn with_storage_options(mut self, opts: HashMap<String, String>) -> Self {
        self.base_storage_options = opts;
        self
    }

    pub fn with_default_limit(mut self, limit: usize) -> Self {
        self.default_limit = limit;
        self
    }

    /// Register a local table → storage-URI override. Tables resolved
    /// this way skip the UC REST round trip (used for the demo + offline
    /// dev). Returns `self` for chaining.
    pub fn with_local_table(mut self, name: impl Into<String>, uri: impl Into<String>) -> Self {
        self.local_tables.insert(name.into(), uri.into());
        self
    }

    /// Tables currently resolvable without UC (name → URI).
    pub fn local_tables(&self) -> &HashMap<String, String> {
        &self.local_tables
    }

    /// Run the governed query for `req`. If the table has a local
    /// override it is used directly; otherwise the table's storage
    /// location + temporary credentials are resolved over UC.
    pub async fn run(&self, req: &PlaygroundRequest) -> Result<QueryResult, PlaygroundError> {
        if req.sql.trim().is_empty() {
            return Err(PlaygroundError::Invalid("sql must not be empty".into()));
        }
        let limit = req.limit.unwrap_or(self.default_limit);

        // Local override: skip UC and open the URI directly.
        if let Some(uri) = self.local_tables.get(&req.table) {
            return execute(ExecuteParams {
                table_name: &req.table,
                table_uri: uri,
                storage_options: self.base_storage_options.clone(),
                principal: &req.principal,
                policies: &req.policies,
                functions: &req.functions,
                sql: &req.sql,
                limit,
            })
            .await;
        }

        let access = self
            .client
            .resolve_table_access(&req.table, TableOperation::Read, &self.base_storage_options)
            .await?;
        execute(ExecuteParams {
            table_name: &req.table,
            table_uri: &access.uri,
            storage_options: access.storage_options,
            principal: &req.principal,
            policies: &req.policies,
            functions: &req.functions,
            sql: &req.sql,
            limit,
        })
        .await
    }
}

/// The fully-qualified name the demo table is registered under.
pub const DEMO_TABLE_NAME: &str = "hospital.clinical.patients";

/// Seed a small demo `patients` Delta table at `uri` (idempotent — a
/// no-op if the table already exists). The schema/rows mirror the
/// shipped Cedar demo so the playground's prefilled policies (region row
/// filter, `ssn`/`diagnosis` masks, `legal_hold` deny, physician filter)
/// all have something to act on.
pub async fn seed_demo_patients(uri: &str) -> Result<(), PlaygroundError> {
    use datafusion::arrow::array::{
        builder::{BooleanBuilder, Int32Builder, StringBuilder},
        ArrayRef,
    };
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
    use deltalake::operations::create::CreateBuilder;
    use deltalake::operations::write::WriteBuilder;

    let table_url = ensure_table_uri(uri)
        .map_err(|e| PlaygroundError::Delta(format!("seed: bad uri {uri}: {e}")))?;

    // Idempotent: if it already opens, leave it alone.
    if open_table_with_storage_options(table_url.clone(), HashMap::new())
        .await
        .is_ok()
    {
        return Ok(());
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("treating_physician", DataType::Utf8, false),
        Field::new("ssn", DataType::Utf8, false),
        Field::new("diagnosis", DataType::Utf8, false),
        Field::new("legal_hold", DataType::Boolean, false),
    ]));

    let rows = [
        (1, "us-east", "Dr. Smith", "111-11-1111", "flu", false),
        (2, "us-west", "Dr. Jones", "222-22-2222", "diabetes", false),
        (3, "us-east", "Dr. Smith", "333-33-3333", "cancer", true),
        (4, "eu-west", "Dr. Brown", "444-44-4444", "asthma", false),
    ];
    let mut id = Int32Builder::new();
    let mut region = StringBuilder::new();
    let mut phys = StringBuilder::new();
    let mut ssn = StringBuilder::new();
    let mut diagnosis = StringBuilder::new();
    let mut legal = BooleanBuilder::new();
    for (i, r, p, s, d, l) in rows {
        id.append_value(i);
        region.append_value(r);
        phys.append_value(p);
        ssn.append_value(s);
        diagnosis.append_value(d);
        legal.append_value(l);
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(id.finish()) as ArrayRef,
            Arc::new(region.finish()) as ArrayRef,
            Arc::new(phys.finish()) as ArrayRef,
            Arc::new(ssn.finish()) as ArrayRef,
            Arc::new(diagnosis.finish()) as ArrayRef,
            Arc::new(legal.finish()) as ArrayRef,
        ],
    )
    .map_err(|e| PlaygroundError::Delta(format!("seed: build batch: {e}")))?;

    let delta_schema: deltalake::kernel::Schema = schema
        .as_ref()
        .try_into_kernel()
        .map_err(|e| PlaygroundError::Delta(format!("seed: schema conversion: {e}")))?;
    let table = CreateBuilder::new()
        .with_location(uri)
        .with_columns(delta_schema.fields().cloned())
        .await
        .map_err(|e| PlaygroundError::Delta(format!("seed: create {uri}: {e}")))?;
    WriteBuilder::new(
        table.log_store(),
        Some(
            table
                .snapshot()
                .map_err(|e| PlaygroundError::Delta(format!("seed: snapshot {uri}: {e}")))?
                .snapshot()
                .clone(),
        ),
    )
    .with_input_batches(vec![batch])
    .await
    .map_err(|e| PlaygroundError::Delta(format!("seed: write {uri}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{builder::Int32Builder, builder::StringBuilder, ArrayRef};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
    use deltalake::operations::create::CreateBuilder;
    use deltalake::operations::write::WriteBuilder;

    const TABLE: &str = "hospital.clinical.patients";

    const ROW_FILTER: &str = r#"
        @id("row_filter_region")
        @filter_type("row_filter")
        @target_table("hospital.clinical.patients")
        permit (principal, action == Action::"query", resource)
        when { resource.region == principal.region };
    "#;

    const MASK_SSN: &str = r#"
        @id("mask_ssn")
        @filter_type("column_mask")
        @target_table("hospital.clinical.patients")
        @column("ssn")
        forbid (principal, action, resource)
        unless { principal.role == "admin" };
    "#;

    /// Write a tiny patients Delta table (id, region, ssn) to `dir` and
    /// return its `file://`-able path.
    async fn seed_patients(dir: &std::path::Path) -> String {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("region", DataType::Utf8, false),
            Field::new("ssn", DataType::Utf8, false),
        ]));
        let mut id = Int32Builder::new();
        let mut region = StringBuilder::new();
        let mut ssn = StringBuilder::new();
        for (i, r, s) in [(1, "us-east", "111-11"), (2, "us-west", "222-22")] {
            id.append_value(i);
            region.append_value(r);
            ssn.append_value(s);
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(id.finish()) as ArrayRef,
                Arc::new(region.finish()) as ArrayRef,
                Arc::new(ssn.finish()) as ArrayRef,
            ],
        )
        .unwrap();
        let uri = dir.join("patients").to_str().unwrap().to_string();
        let delta_schema: deltalake::kernel::Schema = schema.as_ref().try_into_kernel().unwrap();
        let table = CreateBuilder::new()
            .with_location(&uri)
            .with_columns(delta_schema.fields().cloned())
            .await
            .unwrap();
        WriteBuilder::new(
            table.log_store(),
            Some(table.snapshot().unwrap().snapshot().clone()),
        )
        .with_input_batches(vec![batch])
        .await
        .unwrap();
        uri
    }

    fn principal(id: &str, role: &str, region: &str) -> PrincipalSpec {
        let mut attrs = BTreeMap::new();
        attrs.insert("region".to_string(), region.to_string());
        PrincipalSpec {
            id: id.into(),
            role: role.into(),
            attrs,
        }
    }

    /// Find the value in `result` for `column` on the row whose `id` equals
    /// `id_val`.
    fn cell<'a>(result: &'a QueryResult, id_val: &str, column: &str) -> Option<&'a str> {
        let id_col = result.columns.iter().position(|c| c == "id")?;
        let col = result.columns.iter().position(|c| c == column)?;
        result
            .rows
            .iter()
            .find(|r| r[id_col].as_deref() == Some(id_val))
            .and_then(|r| r[col].as_deref())
    }

    #[tokio::test]
    async fn test_governed_row_filter_and_mask_for_analyst() {
        let dir = tempfile::tempdir().unwrap();
        let uri = seed_patients(dir.path()).await;
        let p = principal("alice", "analyst", "us-east");
        let policies = vec![ROW_FILTER.to_string(), MASK_SSN.to_string()];
        let result = execute(ExecuteParams {
            table_name: TABLE,
            table_uri: &uri,
            storage_options: HashMap::new(),
            principal: &p,
            policies: &policies,
            functions: &[],
            sql: "SELECT * FROM patients ORDER BY id",
            limit: 100,
        })
        .await
        .unwrap();

        // Row filter: only the us-east row survives.
        assert_eq!(result.total_rows, 1, "row filter should drop us-west");
        assert_eq!(cell(&result, "1", "region"), Some("us-east"));
        // Column mask: analyst sees ssn replaced with the sentinel.
        assert_eq!(cell(&result, "1", "ssn"), Some("***"));
        // Surfaced governance.
        assert!(result.applied_masks.iter().any(|m| m.column == "ssn"));
        assert_eq!(result.applied_policies.len(), 2);
    }

    #[tokio::test]
    async fn test_admin_sees_unmasked_rows() {
        let dir = tempfile::tempdir().unwrap();
        let uri = seed_patients(dir.path()).await;
        let p = principal("zoe", "admin", "us-east");
        let policies = vec![ROW_FILTER.to_string(), MASK_SSN.to_string()];
        let result = execute(ExecuteParams {
            table_name: TABLE,
            table_uri: &uri,
            storage_options: HashMap::new(),
            principal: &p,
            policies: &policies,
            functions: &[],
            sql: "SELECT * FROM patients ORDER BY id",
            limit: 100,
        })
        .await
        .unwrap();

        // Row filter still applies (region match) → only us-east.
        assert_eq!(result.total_rows, 1);
        // Admin is exempt from the mask.
        assert_eq!(cell(&result, "1", "ssn"), Some("111-11"));
        assert!(result.applied_masks.is_empty(), "admin should see no masks");
    }

    #[tokio::test]
    async fn test_ungoverned_baseline_returns_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        let uri = seed_patients(dir.path()).await;
        let p = principal("alice", "analyst", "us-east");
        let result = execute(ExecuteParams {
            table_name: TABLE,
            table_uri: &uri,
            storage_options: HashMap::new(),
            principal: &p,
            policies: &[],
            functions: &[],
            sql: "SELECT * FROM patients ORDER BY id",
            limit: 100,
        })
        .await
        .unwrap();
        assert_eq!(result.total_rows, 2);
        assert!(result.applied_policies.is_empty());
        assert!(result.applied_masks.is_empty());
    }

    #[tokio::test]
    async fn test_inline_function_over_table() {
        let dir = tempfile::tempdir().unwrap();
        let uri = seed_patients(dir.path()).await;
        let p = principal("alice", "analyst", "us-east");
        let functions = vec![FunctionDef {
            name: "tag_region".into(),
            params: vec![FunctionParam {
                name: "r".into(),
                type_text: "STRING".into(),
            }],
            data_type: "STRING".into(),
            routine_definition: "concat('R-', r)".into(),
        }];
        let result = execute(ExecuteParams {
            table_name: TABLE,
            table_uri: &uri,
            storage_options: HashMap::new(),
            principal: &p,
            policies: &[], // ungoverned so both rows are visible
            functions: &functions,
            sql: "SELECT id, tag_region(region) AS tagged FROM patients ORDER BY id",
            limit: 100,
        })
        .await
        .unwrap();
        assert_eq!(result.total_rows, 2);
        assert_eq!(cell(&result, "1", "tagged"), Some("R-us-east"));
        assert_eq!(cell(&result, "2", "tagged"), Some("R-us-west"));
    }

    #[tokio::test]
    async fn test_bad_policy_surfaces_compile_error() {
        let dir = tempfile::tempdir().unwrap();
        let uri = seed_patients(dir.path()).await;
        let p = principal("alice", "analyst", "us-east");
        // @target_table and @target_tag are mutually exclusive → compile error.
        let bad = r#"
            @id("bad")
            @filter_type("row_filter")
            @target_table("hospital.clinical.patients")
            @target_tag("clinical")
            permit (principal, action, resource);
        "#;
        let err = execute(ExecuteParams {
            table_name: TABLE,
            table_uri: &uri,
            storage_options: HashMap::new(),
            principal: &p,
            policies: &[bad.to_string()],
            functions: &[],
            sql: "SELECT * FROM patients",
            limit: 100,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, PlaygroundError::Compile(_)));
    }

    #[tokio::test]
    async fn test_seed_demo_patients_and_service_local_table() {
        let dir = tempfile::tempdir().unwrap();
        let uri = format!("file://{}/patients", dir.path().to_str().unwrap());
        // Seed is idempotent: running twice must not error.
        seed_demo_patients(&uri).await.unwrap();
        seed_demo_patients(&uri).await.unwrap();

        let service = PlaygroundService::new("http://unused", policast_uc::AuthMode::Disabled)
            .with_local_table(DEMO_TABLE_NAME, &uri);
        assert!(service.local_tables().contains_key(DEMO_TABLE_NAME));

        // A governed run via the local override (no UC) applies the region
        // row filter + ssn mask for an analyst.
        let req = PlaygroundRequest {
            table: DEMO_TABLE_NAME.to_string(),
            principal: principal("alice", "analyst", "us-east"),
            policies: vec![ROW_FILTER.to_string(), MASK_SSN.to_string()],
            functions: vec![],
            sql: "SELECT id, region, ssn FROM patients ORDER BY id".to_string(),
            limit: Some(100),
        };
        let result = service.run(&req).await.unwrap();
        // Two us-east rows (ids 1 and 3) survive the region filter.
        assert_eq!(result.total_rows, 2);
        assert_eq!(cell(&result, "1", "ssn"), Some("***"));
        assert!(result.applied_masks.iter().any(|m| m.column == "ssn"));
    }

    #[tokio::test]
    async fn test_row_limit_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let uri = seed_patients(dir.path()).await;
        let p = principal("alice", "analyst", "us-east");
        let result = execute(ExecuteParams {
            table_name: TABLE,
            table_uri: &uri,
            storage_options: HashMap::new(),
            principal: &p,
            policies: &[],
            functions: &[],
            sql: "SELECT * FROM patients",
            limit: 1,
        })
        .await
        .unwrap();
        assert_eq!(result.total_rows, 2);
        assert_eq!(result.rows.len(), 1);
        assert!(result.truncated);
    }
}
