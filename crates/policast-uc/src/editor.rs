//! Owner-aware policy edit + versioning against the governance Delta
//! tables.
//!
//! This is the write side of the policy lifecycle:
//!
//! 1. **Get** the current version of a policy (`cedar_source`,
//!    `version`, `owner`, compiled CEL).
//! 2. **Validate** a proposed Cedar edit (compile-only dry run).
//! 3. **Edit** — authorize the requester (owner or admin), compile the
//!    new Cedar, and **append** a new version row to
//!    `governance.policast.policies` and `governance.policast.manifest`
//!    via delta-rs (`READ_WRITE` vended credentials). The resolver
//!    treats the highest `version` per `policy_id` as authoritative, so
//!    an append is a publish.
//!
//! Re-materializing the serving store (Redis) after an edit is the
//! caller's responsibility (the sidecar handler does it): write here,
//! then `refresh_snapshot` + `materialize_into`.
//!
//! Gated behind the `uc-bootstrap` feature (it needs delta-rs).

use std::sync::Arc;

use datafusion::arrow::array::{
    builder::{Int64Builder, ListBuilder, StringBuilder, TimestampMicrosecondBuilder},
    Array, ArrayRef, AsArray,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
use deltalake::operations::create::CreateBuilder;
use deltalake::operations::write::WriteBuilder;
use deltalake::{ensure_table_uri, open_table_with_storage_options};
use sha2::{Digest, Sha256};

use crate::error::UcError;
use crate::exchange::{self, CompiledPolicyDoc};
use crate::types::Principal;
use crate::uc_bootstrap::{resolve_table_access, scan_delta_table, UcBootstrapConfig};
use crate::uc_rest::TableOperation;

use policast_core::model::{Effect, FilterType};

/// Default role that may edit any policy regardless of ownership.
pub const DEFAULT_ADMIN_ROLE: &str = "governance_admin";

/// The current (latest active) version of a policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentPolicy {
    pub policy_id: String,
    pub cedar_source: String,
    pub version: i64,
    pub owner: Option<String>,
    pub created_by: String,
    /// Compiled CEL for this version (from the manifest table), if present.
    pub cel_expression: Option<String>,
}

/// Result of a successful edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    pub policy_id: String,
    pub new_version: i64,
    pub owner: String,
}

/// Edits policies against the governance Delta tables.
#[derive(Debug, Clone)]
pub struct PolicyEditor {
    cfg: UcBootstrapConfig,
    admin_role: String,
}

impl PolicyEditor {
    /// Build an editor over the governance tables described by `cfg`,
    /// with `admin_role` as the role that may edit any policy.
    pub fn new(cfg: UcBootstrapConfig, admin_role: impl Into<String>) -> Self {
        Self {
            cfg,
            admin_role: admin_role.into(),
        }
    }

    /// Compile-only dry run: returns the compiled policy (and owner) for
    /// `cedar`, or a compile error. No writes.
    pub fn validate(&self, cedar: &str) -> Result<CompiledPolicyDoc, UcError> {
        exchange::compile_single(cedar)
    }

    /// Read the latest active version of `policy_id`, or `None` if the
    /// policy does not exist.
    pub async fn get_current(&self, policy_id: &str) -> Result<Option<CurrentPolicy>, UcError> {
        let rows = self.read_policy_rows().await?;
        let Some(current) = latest_active(&rows, policy_id) else {
            return Ok(None);
        };
        let cel = self.latest_cel(policy_id).await?;
        Ok(Some(CurrentPolicy {
            policy_id: current.policy_id.clone(),
            cedar_source: current.cedar_source.clone(),
            version: current.version,
            owner: current.owner.clone(),
            created_by: current.created_by.clone(),
            cel_expression: cel,
        }))
    }

    /// Edit `policy_id`: authorize `requester`, compile `new_cedar`, and
    /// append a new version to the policies + manifest tables.
    pub async fn edit(
        &self,
        policy_id: &str,
        new_cedar: &str,
        requester: &Principal,
    ) -> Result<EditOutcome, UcError> {
        let current = self
            .get_current(policy_id)
            .await?
            .ok_or_else(|| UcError::NotFound(format!("policy `{policy_id}`")))?;

        authorize(
            current.owner.as_deref(),
            &current.created_by,
            requester,
            &self.admin_role,
        )?;

        let doc = exchange::compile_single(new_cedar)?;
        if doc.policy.id != policy_id {
            return Err(UcError::Invalid(format!(
                "edited Cedar declares @id(\"{}\") but the edit targets `{policy_id}`",
                doc.policy.id
            )));
        }

        let new_version = current.version + 1;
        let owner = doc
            .owner
            .clone()
            .or_else(|| current.owner.clone())
            .unwrap_or_else(|| current.created_by.clone());

        let now = now_micros();
        let policy_batch = build_policy_batch(&doc, &owner, new_version, &requester.id, now)?;
        let manifest_batch = build_manifest_batch(&doc, new_version, now)?;

        self.append_batch("policies", policy_batch).await?;
        self.append_batch("manifest", manifest_batch).await?;

        Ok(EditOutcome {
            policy_id: policy_id.to_string(),
            new_version,
            owner,
        })
    }

    async fn read_policy_rows(&self) -> Result<Vec<FullPolicyRow>, UcError> {
        let access = resolve_table_access(&self.cfg, "policies", TableOperation::Read).await?;
        let batches = scan_delta_table(&access.uri, &access.storage_options).await?;
        let mut rows = Vec::new();
        for batch in &batches {
            rows.extend(extract_policy_rows(batch)?);
        }
        Ok(rows)
    }

    async fn latest_cel(&self, policy_id: &str) -> Result<Option<String>, UcError> {
        let access = resolve_table_access(&self.cfg, "manifest", TableOperation::Read).await?;
        let batches = scan_delta_table(&access.uri, &access.storage_options).await?;
        let mut best: Option<(i64, String)> = None;
        for batch in &batches {
            let pid = string_col(batch, "policy_id")?;
            let cel = string_col(batch, "cel_expression")?;
            let ver = int64_col(batch, "version")?;
            for i in 0..batch.num_rows() {
                if pid.value(i) != policy_id {
                    continue;
                }
                let v = ver.value(i);
                if best.as_ref().map(|(bv, _)| v > *bv).unwrap_or(true) {
                    best = Some((v, cel.value(i).to_string()));
                }
            }
        }
        Ok(best.map(|(_, cel)| cel))
    }

    async fn append_batch(&self, short_name: &str, batch: RecordBatch) -> Result<(), UcError> {
        let access =
            resolve_table_access(&self.cfg, short_name, TableOperation::ReadWrite).await?;
        let url = ensure_table_uri(&access.uri)
            .map_err(|e| UcError::Config(format!("edit: bad uri for {short_name}: {e}")))?;
        let table = open_table_with_storage_options(url, access.storage_options.clone())
            .await
            .map_err(|e| UcError::Config(format!("edit: open {short_name}: {e}")))?;
        let eager = table
            .snapshot()
            .map_err(|e| UcError::Config(format!("edit: snapshot {short_name}: {e}")))?
            .snapshot()
            .clone();
        WriteBuilder::new(table.log_store(), Some(eager))
            .with_input_batches(vec![batch])
            .await
            .map_err(|e| UcError::Config(format!("edit: append {short_name}: {e}")))?;
        Ok(())
    }
}

/// Authorize an edit: the requester must be the effective owner (the
/// `@owner`, defaulting to `created_by`) or hold the admin role.
pub fn authorize(
    owner: Option<&str>,
    created_by: &str,
    requester: &Principal,
    admin_role: &str,
) -> Result<(), UcError> {
    let effective_owner = owner.unwrap_or(created_by);
    if requester.id == effective_owner || requester.role == admin_role {
        Ok(())
    } else {
        Err(UcError::Forbidden(format!(
            "principal `{}` (role `{}`) is not the owner `{}` or an admin (`{}`)",
            requester.id, requester.role, effective_owner, admin_role
        )))
    }
}

/// Full row of `governance.policast.policies` (the editor needs the
/// columns the resolver's `PolicyRow` omits — `cedar_source`,
/// `created_by`, `retired_at`).
#[derive(Debug, Clone)]
struct FullPolicyRow {
    policy_id: String,
    cedar_source: String,
    owner: Option<String>,
    created_by: String,
    version: i64,
    retired: bool,
}

/// Latest non-retired row for `policy_id` by version.
fn latest_active<'a>(rows: &'a [FullPolicyRow], policy_id: &str) -> Option<&'a FullPolicyRow> {
    rows.iter()
        .filter(|r| r.policy_id == policy_id && !r.retired)
        .max_by_key(|r| r.version)
}

fn now_micros() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000) as i64
}

// --- schema (must match examples/uc/ddl/02_policies.sql + 03_manifest.sql) ---

/// Canonical Arrow schema for `governance.policast.policies`.
pub fn policies_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("policy_id", DataType::Utf8, false),
        Field::new("cedar_source", DataType::Utf8, false),
        Field::new("filter_type", DataType::Utf8, false),
        Field::new("target_table", DataType::Utf8, false),
        Field::new("column", DataType::Utf8, true),
        Field::new("target_tag", DataType::Utf8, true),
        Field::new("applies_to_tag", DataType::Utf8, true),
        Field::new("effect", DataType::Utf8, false),
        Field::new(
            "applies_to_roles",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
        Field::new("description", DataType::Utf8, true),
        Field::new("owner", DataType::Utf8, true),
        Field::new("version", DataType::Int64, false),
        Field::new("created_by", DataType::Utf8, false),
        Field::new(
            "created_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ),
        Field::new(
            "retired_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
    ]))
}

/// Canonical Arrow schema for `governance.policast.manifest`.
pub fn manifest_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("policy_id", DataType::Utf8, false),
        Field::new("cel_expression", DataType::Utf8, false),
        Field::new("version", DataType::Int64, false),
        Field::new(
            "compiled_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ),
        Field::new("compiler_version", DataType::Utf8, false),
        Field::new("source_hash", DataType::Utf8, false),
    ]))
}

fn filter_type_str(ft: &FilterType) -> &'static str {
    match ft {
        FilterType::RowFilter => "row_filter",
        FilterType::ColumnMask => "column_mask",
        FilterType::DenyOverride => "deny_override",
    }
}

fn effect_str(e: &Effect) -> &'static str {
    match e {
        Effect::Permit => "permit",
        Effect::Forbid => "forbid",
    }
}

fn build_policy_batch(
    doc: &CompiledPolicyDoc,
    owner: &str,
    version: i64,
    created_by: &str,
    created_at_micros: i64,
) -> Result<RecordBatch, UcError> {
    let p = &doc.policy;
    let mut roles = ListBuilder::new(StringBuilder::new());
    match &p.applies_to {
        Some(a) if !a.roles.is_empty() => {
            for r in &a.roles {
                roles.values().append_value(r);
            }
            roles.append(true);
        }
        _ => roles.append(false),
    }
    let mut created_at = TimestampMicrosecondBuilder::new();
    created_at.append_value(created_at_micros);
    let mut retired_at = TimestampMicrosecondBuilder::new();
    retired_at.append_null();

    let columns: Vec<ArrayRef> = vec![
        str_arr(&[&p.id]),
        str_arr(&[&doc.cedar_source]),
        str_arr(&[filter_type_str(&p.filter_type)]),
        str_arr(&[&p.target_table]),
        opt_str_arr(&[p.column.as_deref()]),
        opt_str_arr(&[p.target_tag.as_deref()]),
        opt_str_arr(&[p.applies_to_tag.as_deref()]),
        str_arr(&[effect_str(&p.effect)]),
        Arc::new(roles.finish()),
        opt_str_arr(&[p.description.as_deref()]),
        opt_str_arr(&[Some(owner)]),
        Arc::new({
            let mut b = Int64Builder::new();
            b.append_value(version);
            b.finish()
        }),
        str_arr(&[created_by]),
        Arc::new(created_at.finish()),
        Arc::new(retired_at.finish()),
    ];
    RecordBatch::try_new(policies_schema(), columns)
        .map_err(|e| UcError::Config(format!("edit: policy batch assembly: {e}")))
}

fn build_manifest_batch(
    doc: &CompiledPolicyDoc,
    version: i64,
    compiled_at_micros: i64,
) -> Result<RecordBatch, UcError> {
    let source_hash = hex::encode(Sha256::digest(doc.cedar_source.as_bytes()));
    let mut compiled_at = TimestampMicrosecondBuilder::new();
    compiled_at.append_value(compiled_at_micros);
    let columns: Vec<ArrayRef> = vec![
        str_arr(&[&doc.policy.id]),
        str_arr(&[&doc.policy.cel_expression]),
        Arc::new({
            let mut b = Int64Builder::new();
            b.append_value(version);
            b.finish()
        }),
        Arc::new(compiled_at.finish()),
        str_arr(&[policast_core_version()]),
        str_arr(&[&source_hash]),
    ];
    RecordBatch::try_new(manifest_schema(), columns)
        .map_err(|e| UcError::Config(format!("edit: manifest batch assembly: {e}")))
}

fn policast_core_version() -> &'static str {
    // The compiler version recorded in the manifest. policast-uc and
    // policast-core share the workspace version.
    env!("CARGO_PKG_VERSION")
}

fn str_arr(values: &[&str]) -> ArrayRef {
    let mut b = StringBuilder::new();
    for v in values {
        b.append_value(v);
    }
    Arc::new(b.finish())
}

fn opt_str_arr(values: &[Option<&str>]) -> ArrayRef {
    let mut b = StringBuilder::new();
    for v in values {
        match v {
            Some(s) => b.append_value(s),
            None => b.append_null(),
        }
    }
    Arc::new(b.finish())
}

// --- read helpers -----------------------------------------------------------

fn string_col<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a datafusion::arrow::array::StringArray, UcError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| UcError::Config(format!("policies table missing column `{name}`")))?;
    arr.as_string_opt::<i32>()
        .ok_or_else(|| UcError::Config(format!("policies column `{name}` is not Utf8")))
}

fn int64_col<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a datafusion::arrow::array::Int64Array, UcError> {
    let arr = batch
        .column_by_name(name)
        .ok_or_else(|| UcError::Config(format!("table missing column `{name}`")))?;
    arr.as_any()
        .downcast_ref::<datafusion::arrow::array::Int64Array>()
        .ok_or_else(|| UcError::Config(format!("column `{name}` is not Int64")))
}

fn extract_policy_rows(batch: &RecordBatch) -> Result<Vec<FullPolicyRow>, UcError> {
    let policy_id = string_col(batch, "policy_id")?;
    let cedar_source = string_col(batch, "cedar_source")?;
    let version = int64_col(batch, "version")?;
    let created_by = string_col(batch, "created_by")?;
    let owner = batch
        .column_by_name("owner")
        .and_then(|a| a.as_string_opt::<i32>());
    let retired = batch.column_by_name("retired_at");

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(FullPolicyRow {
            policy_id: policy_id.value(i).to_string(),
            cedar_source: cedar_source.value(i).to_string(),
            owner: owner.and_then(|a| {
                if a.is_null(i) || a.value(i).is_empty() {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            }),
            created_by: created_by.value(i).to_string(),
            version: version.value(i),
            retired: retired.map(|a| a.is_valid(i)).unwrap_or(false),
        });
    }
    Ok(out)
}

/// Test helper: create the policies + manifest Delta tables under
/// `template` (a `{table}` URI template) seeded with one initial
/// version of a single policy. Exposed for the sidecar's tests too.
#[doc(hidden)]
pub async fn seed_editable_tables(
    template: &str,
    policy_id: &str,
    cedar_source: &str,
    owner: Option<&str>,
    created_by: &str,
) -> Result<(), UcError> {
    use crate::exchange;
    let doc = exchange::compile_single(cedar_source)?;
    let now = now_micros();
    let owner = owner.map(str::to_string);
    let policy_batch = build_policy_batch(
        &CompiledPolicyDoc {
            policy: doc.policy.clone(),
            owner: owner.clone(),
            cedar_source: cedar_source.to_string(),
        },
        owner.as_deref().unwrap_or(created_by),
        1,
        created_by,
        now,
    )?;
    let manifest_batch = build_manifest_batch(&doc, 1, now)?;
    let _ = policy_id;

    create_and_write(&template.replace("{table}", "policies"), policy_batch).await?;
    create_and_write(&template.replace("{table}", "manifest"), manifest_batch).await?;
    Ok(())
}

#[doc(hidden)]
async fn create_and_write(uri: &str, batch: RecordBatch) -> Result<(), UcError> {
    let schema = batch.schema();
    let delta_schema: deltalake::kernel::StructType = schema
        .as_ref()
        .try_into_kernel()
        .map_err(|e| UcError::Config(format!("seed editable: schema conv: {e}")))?;
    let table = CreateBuilder::new()
        .with_location(uri)
        .with_columns(delta_schema.fields().cloned())
        .await
        .map_err(|e| UcError::Config(format!("seed editable: create {uri}: {e}")))?;
    let eager = table
        .snapshot()
        .map_err(|e| UcError::Config(format!("seed editable: snapshot {uri}: {e}")))?
        .snapshot()
        .clone();
    WriteBuilder::new(table.log_store(), Some(eager))
        .with_input_batches(vec![batch])
        .await
        .map_err(|e| UcError::Config(format!("seed editable: write {uri}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PrincipalAttrs;

    fn principal(id: &str, role: &str) -> Principal {
        Principal {
            id: id.into(),
            role: role.into(),
            attrs: PrincipalAttrs::new(),
        }
    }

    #[test]
    fn test_authorize_owner_allowed() {
        let p = principal("alice", "analyst");
        assert!(authorize(Some("alice"), "creator", &p, "governance_admin").is_ok());
    }

    #[test]
    fn test_authorize_admin_allowed() {
        let p = principal("zoe", "governance_admin");
        assert!(authorize(Some("alice"), "creator", &p, "governance_admin").is_ok());
    }

    #[test]
    fn test_authorize_non_owner_denied() {
        let p = principal("mallory", "analyst");
        let err = authorize(Some("alice"), "creator", &p, "governance_admin").unwrap_err();
        assert!(matches!(err, UcError::Forbidden(_)));
    }

    #[test]
    fn test_authorize_defaults_owner_to_created_by() {
        // owner is None -> falls back to created_by; the creator may edit.
        let creator = principal("creator", "analyst");
        assert!(authorize(None, "creator", &creator, "governance_admin").is_ok());
        let other = principal("bob", "analyst");
        assert!(authorize(None, "creator", &other, "governance_admin").is_err());
    }

    const CEDAR_V1: &str = r#"
        @id("row_filter_region")
        @filter_type("row_filter")
        @target_table("hospital.clinical.patients")
        @owner("alice@hospital.com")
        @roles("analyst")
        permit (principal, action == Action::"query", resource)
        when { resource.region == principal.region };
    "#;

    const CEDAR_V2: &str = r#"
        @id("row_filter_region")
        @filter_type("row_filter")
        @target_table("hospital.clinical.patients")
        @owner("alice@hospital.com")
        @roles("analyst")
        permit (principal, action == Action::"query", resource)
        when { resource.region == principal.region }
        unless { principal.role == "auditor" };
    "#;

    fn editor_for(dir: &std::path::Path) -> (PolicyEditor, String) {
        let template = format!("{}/{{table}}", dir.to_str().unwrap());
        let cfg = UcBootstrapConfig::for_example_stack("http://uc")
            .with_storage_uri_template(&template);
        (PolicyEditor::new(cfg, DEFAULT_ADMIN_ROLE), template)
    }

    #[tokio::test]
    async fn test_get_current_reads_seeded_policy() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, template) = editor_for(dir.path());
        seed_editable_tables(
            &template,
            "row_filter_region",
            CEDAR_V1,
            Some("alice@hospital.com"),
            "alice@hospital.com",
        )
        .await
        .unwrap();

        let current = editor
            .get_current("row_filter_region")
            .await
            .unwrap()
            .expect("policy should exist");
        assert_eq!(current.version, 1);
        assert_eq!(current.owner.as_deref(), Some("alice@hospital.com"));
        assert_eq!(current.cedar_source, CEDAR_V1);
        assert!(current.cel_expression.is_some());

        assert!(editor.get_current("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_edit_appends_new_version_for_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, template) = editor_for(dir.path());
        seed_editable_tables(
            &template,
            "row_filter_region",
            CEDAR_V1,
            Some("alice@hospital.com"),
            "alice@hospital.com",
        )
        .await
        .unwrap();

        let owner = principal("alice@hospital.com", "analyst");
        let outcome = editor
            .edit("row_filter_region", CEDAR_V2, &owner)
            .await
            .expect("owner edit should succeed");
        assert_eq!(outcome.new_version, 2);
        assert_eq!(outcome.owner, "alice@hospital.com");

        // The latest version is now 2 with the new CEL.
        let current = editor
            .get_current("row_filter_region")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.version, 2);
        assert_eq!(current.cedar_source, CEDAR_V2);
        assert!(current.cel_expression.unwrap().contains("auditor"));
    }

    #[tokio::test]
    async fn test_edit_denied_for_non_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, template) = editor_for(dir.path());
        seed_editable_tables(
            &template,
            "row_filter_region",
            CEDAR_V1,
            Some("alice@hospital.com"),
            "alice@hospital.com",
        )
        .await
        .unwrap();

        let mallory = principal("mallory@evil.com", "analyst");
        let err = editor
            .edit("row_filter_region", CEDAR_V2, &mallory)
            .await
            .unwrap_err();
        assert!(matches!(err, UcError::Forbidden(_)));
        // No new version was written.
        let current = editor
            .get_current("row_filter_region")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.version, 1);
    }

    #[tokio::test]
    async fn test_edit_allowed_for_admin() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, template) = editor_for(dir.path());
        seed_editable_tables(
            &template,
            "row_filter_region",
            CEDAR_V1,
            Some("alice@hospital.com"),
            "alice@hospital.com",
        )
        .await
        .unwrap();

        let admin = principal("zoe@hospital.com", DEFAULT_ADMIN_ROLE);
        let outcome = editor
            .edit("row_filter_region", CEDAR_V2, &admin)
            .await
            .expect("admin edit should succeed");
        assert_eq!(outcome.new_version, 2);
    }

    #[tokio::test]
    async fn test_edit_rejects_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, template) = editor_for(dir.path());
        seed_editable_tables(
            &template,
            "row_filter_region",
            CEDAR_V1,
            Some("alice@hospital.com"),
            "alice@hospital.com",
        )
        .await
        .unwrap();

        let renamed = CEDAR_V2.replace("row_filter_region", "different_id");
        let owner = principal("alice@hospital.com", "analyst");
        let err = editor
            .edit("row_filter_region", &renamed, &owner)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("different_id"));
    }

    #[tokio::test]
    async fn test_edit_missing_policy_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, template) = editor_for(dir.path());
        seed_editable_tables(
            &template,
            "row_filter_region",
            CEDAR_V1,
            Some("alice@hospital.com"),
            "alice@hospital.com",
        )
        .await
        .unwrap();

        let owner = principal("alice@hospital.com", "analyst");
        let err = editor.edit("ghost", CEDAR_V2, &owner).await.unwrap_err();
        assert!(matches!(err, UcError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_validate_is_compile_only() {
        let dir = tempfile::tempdir().unwrap();
        let (editor, _template) = editor_for(dir.path());
        let doc = editor.validate(CEDAR_V1).unwrap();
        assert_eq!(doc.policy.id, "row_filter_region");
        assert!(editor.validate("not cedar at all").is_err());
    }
}
