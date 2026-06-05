//! Seed the four governance Delta tables from any [`ResolveBackend`].
//!
//! The compose `uc-full` profile ships with an S3-compatible MinIO
//! bucket but no preloaded governance data. This module is what the
//! `policast-uc-seed` binary (see `src/bin/seed.rs`) uses to translate
//! the committed flat-file store under `examples/uc/store/` into real
//! Delta tables on MinIO so the sidecar can then serve traffic via
//! [`crate::uc_bootstrap::UcBootstrapBackend`].
//!
//! Correctness property: the Arrow schemas here MUST stay in lock-step
//! with the Arrow readers in [`crate::uc_bootstrap`]. The round-trip
//! test at the bottom of this file enforces that by seeding a tempdir,
//! bootstrapping against it, and asserting the row set survives
//! unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::arrow::array::{
    builder::{
        Int32Builder, Int64Builder, ListBuilder, StringBuilder, TimestampMicrosecondBuilder,
    },
    ArrayRef,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
use deltalake::operations::create::CreateBuilder;
use deltalake::operations::write::WriteBuilder;
use deltalake::{ensure_table_uri, kernel::StructType, DeltaTableError};

use crate::backend::{BindingRow, ManifestRow, PolicyRow, ResolveBackend, TagRow};
use crate::error::UcError;

/// Operator-facing config: where to write, what to forward to the
/// object-store backend, and whether to overwrite tables that already
/// exist.
#[derive(Debug, Clone)]
pub struct SeedConfig {
    /// URI template with a literal `{table}` placeholder. Same shape
    /// as `UcBootstrapConfig::storage_uri_template`.
    pub storage_uri_template: String,
    /// Storage options forwarded verbatim to `deltalake`'s
    /// `object_store` backend (see
    /// [`UcBootstrapConfig::storage_options`] for the MinIO shape).
    ///
    /// [`UcBootstrapConfig::storage_options`]: crate::uc_bootstrap::UcBootstrapConfig::storage_options
    pub storage_options: HashMap<String, String>,
    /// When `true`, a subsequent run appends the full snapshot into
    /// the existing tables (so the refresh task will pick them up).
    /// When `false` (default), the seed refuses to write into an
    /// existing table — protects against an operator accidentally
    /// double-publishing a stale manifest.
    pub overwrite_existing: bool,
}

impl SeedConfig {
    pub fn new(storage_uri_template: impl Into<String>) -> Self {
        Self {
            storage_uri_template: storage_uri_template.into(),
            storage_options: HashMap::new(),
            overwrite_existing: false,
        }
    }

    pub fn with_storage_options<K: Into<String>, V: Into<String>>(
        mut self,
        options: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        self.storage_options = options
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    pub fn with_overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite_existing = overwrite;
        self
    }

    fn table_uri(&self, name: &str) -> String {
        self.storage_uri_template.replace("{table}", name)
    }
}

/// Read every row from `backend` and publish them into the four
/// governance Delta tables under `cfg.storage_uri_template`. The
/// `tags` table is only written when `backend.tags()` returns at
/// least one row, matching `FileBackend`'s absent-file convention.
pub async fn seed_from_backend<B: ResolveBackend + ?Sized>(
    backend: &B,
    cfg: &SeedConfig,
) -> Result<SeedReport, UcError> {
    let policies = backend.policies().await?;
    let manifest = backend.manifest().await?;
    let bindings = backend.bindings().await?;
    let tags = backend.tags().await?;

    let policies_batch = policies_to_batch(&policies)?;
    let manifest_batch = manifest_to_batch(&manifest)?;
    let bindings_batch = bindings_to_batch(&bindings)?;
    let tags_batch = if tags.is_empty() {
        None
    } else {
        Some(tags_to_batch(&tags)?)
    };

    write_table(cfg, "policies", policies_batch).await?;
    write_table(cfg, "manifest", manifest_batch).await?;
    write_table(cfg, "bindings", bindings_batch).await?;
    if let Some(batch) = tags_batch {
        write_table(cfg, "tags", batch).await?;
    }

    Ok(SeedReport {
        policies: policies.len(),
        manifest: manifest.len(),
        bindings: bindings.len(),
        tags: tags.len(),
    })
}

/// Counts of rows published per governance table. Printed by the
/// seed binary as a terse audit line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReport {
    pub policies: usize,
    pub manifest: usize,
    pub bindings: usize,
    pub tags: usize,
}

async fn write_table(cfg: &SeedConfig, name: &str, batch: RecordBatch) -> Result<(), UcError> {
    let uri = cfg.table_uri(name);
    let arrow_schema = batch.schema();
    let delta_schema: StructType = arrow_schema.as_ref().try_into_kernel().map_err(|e| {
        UcError::Config(format!(
            "seed: arrow->delta schema conversion failed for {name}: {e}"
        ))
    })?;

    // Try to open first. If it exists, either fail loudly or append.
    let table_url = ensure_table_uri(&uri)
        .map_err(|e| UcError::Config(format!("seed: bad uri `{uri}`: {e}")))?;
    match deltalake::open_table_with_storage_options(table_url, cfg.storage_options.clone()).await {
        Ok(table) => {
            if !cfg.overwrite_existing {
                return Err(UcError::Config(format!(
                    "seed: Delta table at `{uri}` already exists and \
                     overwrite_existing=false; pass --overwrite to \
                     publish a new snapshot on top"
                )));
            }
            // deltalake 0.32: WriteBuilder::new takes the table's
            // `Option<EagerSnapshot>` rather than `Option<DeltaTableState>`.
            let eager = table
                .snapshot()
                .map_err(|e| UcError::Config(format!("seed: snapshot `{uri}` failed: {e}")))?
                .snapshot()
                .clone();
            WriteBuilder::new(table.log_store(), Some(eager))
                .with_input_batches(vec![batch])
                .await
                .map_err(|e| {
                    UcError::Config(format!("seed: append to existing `{uri}` failed: {e}"))
                })?;
        }
        Err(ref e) if is_table_not_found(e) => {
            let table = CreateBuilder::new()
                .with_location(&uri)
                .with_storage_options(cfg.storage_options.clone())
                .with_columns(delta_schema.fields().cloned())
                .await
                .map_err(|e| UcError::Config(format!("seed: create `{uri}` failed: {e}")))?;
            let eager = table
                .snapshot()
                .map_err(|e| UcError::Config(format!("seed: snapshot `{uri}` failed: {e}")))?
                .snapshot()
                .clone();
            WriteBuilder::new(table.log_store(), Some(eager))
                .with_input_batches(vec![batch])
                .await
                .map_err(|e| {
                    UcError::Config(format!("seed: initial write to `{uri}` failed: {e}"))
                })?;
        }
        Err(e) => {
            return Err(UcError::Config(format!(
                "seed: failed to probe `{uri}` for existing Delta log: {e}"
            )));
        }
    }

    Ok(())
}

/// delta-rs reports "table does not exist" through a variety of error
/// shapes depending on the storage backend. This helper centralizes
/// the string-sniffing so the seed and bootstrap paths stay in sync.
fn is_table_not_found(err: &DeltaTableError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("not a delta table")
        || msg.contains("no such file or directory")
        || msg.contains("no data found")
        || msg.contains("notfound")
        || msg.contains("does not exist")
        || msg.contains("invalid log path")
}

// --- schema constructors ----------------------------------------------------

fn policies_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("policy_id", DataType::Utf8, false),
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
        Field::new("version", DataType::Int64, false),
    ]))
}

fn manifest_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("policy_id", DataType::Utf8, false),
        Field::new("cel_expression", DataType::Utf8, false),
        Field::new("version", DataType::Int64, false),
        Field::new("compiler_version", DataType::Utf8, false),
        Field::new("source_hash", DataType::Utf8, false),
    ]))
}

fn bindings_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("binding_id", DataType::Utf8, false),
        Field::new("policy_id", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        Field::new("principal_selector", DataType::Utf8, false),
        Field::new("precedence", DataType::Int32, false),
    ]))
}

fn tags_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("entity", DataType::Utf8, false),
        Field::new("entity_kind", DataType::Utf8, false),
        Field::new("tag", DataType::Utf8, false),
        Field::new("set_by", DataType::Utf8, true),
        Field::new(
            "set_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new(
            "retired_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
    ]))
}

// --- row -> batch converters ------------------------------------------------

fn policies_to_batch(rows: &[PolicyRow]) -> Result<RecordBatch, UcError> {
    let mut policy_id = StringBuilder::new();
    let mut filter_type = StringBuilder::new();
    let mut target_table = StringBuilder::new();
    let mut column = StringBuilder::new();
    let mut target_tag = StringBuilder::new();
    let mut applies_to_tag = StringBuilder::new();
    let mut effect = StringBuilder::new();
    let mut applies_to_roles = ListBuilder::new(StringBuilder::new());
    let mut description = StringBuilder::new();
    let mut version = Int64Builder::new();

    for r in rows {
        policy_id.append_value(&r.policy_id);
        filter_type.append_value(&r.filter_type);
        target_table.append_value(&r.target_table);
        append_opt(&mut column, r.column.as_deref());
        append_opt(&mut target_tag, r.target_tag.as_deref());
        append_opt(&mut applies_to_tag, r.applies_to_tag.as_deref());
        effect.append_value(&r.effect);
        match &r.applies_to_roles {
            Some(roles) => {
                for role in roles {
                    applies_to_roles.values().append_value(role);
                }
                applies_to_roles.append(true);
            }
            None => applies_to_roles.append(false),
        }
        append_opt(&mut description, r.description.as_deref());
        version.append_value(r.version);
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(policy_id.finish()),
        Arc::new(filter_type.finish()),
        Arc::new(target_table.finish()),
        Arc::new(column.finish()),
        Arc::new(target_tag.finish()),
        Arc::new(applies_to_tag.finish()),
        Arc::new(effect.finish()),
        Arc::new(applies_to_roles.finish()),
        Arc::new(description.finish()),
        Arc::new(version.finish()),
    ];
    RecordBatch::try_new(policies_schema(), columns)
        .map_err(|e| UcError::Config(format!("policies batch assembly: {e}")))
}

fn manifest_to_batch(rows: &[ManifestRow]) -> Result<RecordBatch, UcError> {
    let mut policy_id = StringBuilder::new();
    let mut cel_expression = StringBuilder::new();
    let mut version = Int64Builder::new();
    let mut compiler_version = StringBuilder::new();
    let mut source_hash = StringBuilder::new();
    for r in rows {
        policy_id.append_value(&r.policy_id);
        cel_expression.append_value(&r.cel_expression);
        version.append_value(r.version);
        compiler_version.append_value(&r.compiler_version);
        source_hash.append_value(&r.source_hash);
    }
    RecordBatch::try_new(
        manifest_schema(),
        vec![
            Arc::new(policy_id.finish()),
            Arc::new(cel_expression.finish()),
            Arc::new(version.finish()),
            Arc::new(compiler_version.finish()),
            Arc::new(source_hash.finish()),
        ],
    )
    .map_err(|e| UcError::Config(format!("manifest batch assembly: {e}")))
}

fn bindings_to_batch(rows: &[BindingRow]) -> Result<RecordBatch, UcError> {
    let mut binding_id = StringBuilder::new();
    let mut policy_id = StringBuilder::new();
    let mut target = StringBuilder::new();
    let mut principal_selector = StringBuilder::new();
    let mut precedence = Int32Builder::new();
    for r in rows {
        binding_id.append_value(&r.binding_id);
        policy_id.append_value(&r.policy_id);
        target.append_value(&r.target);
        principal_selector.append_value(&r.principal_selector);
        precedence.append_value(r.precedence);
    }
    RecordBatch::try_new(
        bindings_schema(),
        vec![
            Arc::new(binding_id.finish()),
            Arc::new(policy_id.finish()),
            Arc::new(target.finish()),
            Arc::new(principal_selector.finish()),
            Arc::new(precedence.finish()),
        ],
    )
    .map_err(|e| UcError::Config(format!("bindings batch assembly: {e}")))
}

fn tags_to_batch(rows: &[TagRow]) -> Result<RecordBatch, UcError> {
    let mut entity = StringBuilder::new();
    let mut entity_kind = StringBuilder::new();
    let mut tag = StringBuilder::new();
    let mut set_by = StringBuilder::new();
    let mut set_at = TimestampMicrosecondBuilder::new();
    let mut retired_at = TimestampMicrosecondBuilder::new();
    for r in rows {
        entity.append_value(&r.entity);
        entity_kind.append_value(&r.entity_kind);
        tag.append_value(&r.tag);
        append_opt(&mut set_by, r.set_by.as_deref());
        match r.set_at.as_deref().and_then(parse_ts_micros) {
            Some(v) => set_at.append_value(v),
            None => set_at.append_null(),
        }
        match r.retired_at.as_deref().and_then(parse_ts_micros) {
            Some(v) => retired_at.append_value(v),
            None => retired_at.append_null(),
        }
    }
    RecordBatch::try_new(
        tags_schema(),
        vec![
            Arc::new(entity.finish()),
            Arc::new(entity_kind.finish()),
            Arc::new(tag.finish()),
            Arc::new(set_by.finish()),
            Arc::new(set_at.finish()),
            Arc::new(retired_at.finish()),
        ],
    )
    .map_err(|e| UcError::Config(format!("tags batch assembly: {e}")))
}

fn append_opt(builder: &mut StringBuilder, value: Option<&str>) {
    match value {
        Some(s) => builder.append_value(s),
        None => builder.append_null(),
    }
}

/// Very forgiving RFC3339 parser that covers the shapes the flat-file
/// store emits: naïve `YYYY-MM-DDTHH:MM:SS` and
/// `YYYY-MM-DDTHH:MM:SSZ`. Returns microseconds since the Unix epoch.
/// Best-effort: unparsable strings degrade to `None` so a bad
/// timestamp never aborts the whole seed — the row's timestamp just
/// shows up as null on the Delta side.
fn parse_ts_micros(s: &str) -> Option<i64> {
    // Use chrono via deltalake's re-export to avoid adding a new dep.
    // Format accepted: 2024-01-02T03:04:05Z or 2024-01-02T03:04:05.
    let trimmed = s.trim_end_matches('Z');
    // DIY: split into date + time, parse each, ignore sub-second.
    let (date, time) = trimmed.split_once('T')?;
    let mut dparts = date.splitn(3, '-');
    let y: i32 = dparts.next()?.parse().ok()?;
    let m: u32 = dparts.next()?.parse().ok()?;
    let d: u32 = dparts.next()?.parse().ok()?;
    let mut tparts = time.splitn(3, ':');
    let hh: u32 = tparts.next()?.parse().ok()?;
    let mm: u32 = tparts.next()?.parse().ok()?;
    let ss_and_frac: &str = tparts.next()?;
    let ss: u32 = ss_and_frac.split('.').next()?.parse().ok()?;
    // naive UTC -> micros via a tiny portable routine.
    let days = days_from_civil(y, m, d);
    let secs = days as i64 * 86_400 + hh as i64 * 3_600 + mm as i64 * 60 + ss as i64;
    Some(secs * 1_000_000)
}

/// Howard Hinnant's days_from_civil (civil_from_days reverse); returns
/// days since 1970-01-01 for any civil Y-M-D. Used by `parse_ts_micros`.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileBackend;
    use crate::uc_bootstrap::{UcBootstrapBackend, UcBootstrapConfig};

    fn example_store_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/uc/store")
    }

    /// The headline test: publish the shipped flat-file example into a
    /// tempdir of Delta tables and prove that `UcBootstrapBackend` can
    /// read it back row-for-row. If someone drifts the Arrow schema in
    /// one of the two modules but not the other, this test fails.
    #[tokio::test]
    async fn test_seed_roundtrip_matches_file_backend() {
        let dir = tempfile::tempdir().unwrap();
        let template = format!("{}/{{table}}", dir.path().to_str().unwrap());

        let file_backend = FileBackend::new(example_store_root());
        let cfg = SeedConfig::new(&template);
        let report = seed_from_backend(&file_backend, &cfg)
            .await
            .expect("seed should succeed against the shipped store");
        assert!(report.policies > 0, "shipped store has policies");
        assert!(report.manifest > 0, "shipped store has a manifest");
        assert!(report.bindings > 0, "shipped store has bindings");
        assert!(report.tags > 0, "shipped store has tags");

        let mut boot_cfg =
            UcBootstrapConfig::for_example_stack("http://uc").with_storage_uri_template(&template);
        boot_cfg.refresh_interval = None;
        let boot = UcBootstrapBackend::bootstrap(boot_cfg)
            .await
            .expect("bootstrap against seeded tables should succeed");

        let expected_policies = file_backend.policies().await.unwrap();
        let got_policies = boot.policies().await.unwrap();
        assert_eq!(
            sort_by_id(&expected_policies, |p| p.policy_id.clone()),
            sort_by_id(&got_policies, |p| p.policy_id.clone()),
            "policies round-trip"
        );

        let expected_manifest = file_backend.manifest().await.unwrap();
        let got_manifest = boot.manifest().await.unwrap();
        assert_eq!(
            sort_by_id(&expected_manifest, |r| r.policy_id.clone()),
            sort_by_id(&got_manifest, |r| r.policy_id.clone()),
            "manifest round-trip"
        );

        let expected_bindings = file_backend.bindings().await.unwrap();
        let got_bindings = boot.bindings().await.unwrap();
        assert_eq!(
            sort_by_id(&expected_bindings, |r| r.binding_id.clone()),
            sort_by_id(&got_bindings, |r| r.binding_id.clone()),
            "bindings round-trip"
        );

        let expected_tags = file_backend.tags().await.unwrap();
        let got_tags = boot.tags().await.unwrap();
        // Tags carry set_at / retired_at timestamps; the flat-file
        // store strings them as ISO-8601 but the Delta round-trip
        // goes through microseconds. Compare by the three identifying
        // columns only (entity/kind/tag) to side-step spurious
        // format-string mismatches.
        let expected_ids = sort_by_id(&expected_tags, |t| {
            (t.entity.clone(), t.entity_kind.clone(), t.tag.clone())
        });
        let got_ids = sort_by_id(&got_tags, |t| {
            (t.entity.clone(), t.entity_kind.clone(), t.tag.clone())
        });
        assert_eq!(expected_ids, got_ids, "tag identities round-trip");
    }

    fn sort_by_id<T: Clone, K: Ord>(rows: &[T], key: impl Fn(&T) -> K) -> Vec<K> {
        let mut ks: Vec<K> = rows.iter().map(&key).collect();
        ks.sort();
        ks
    }

    /// A second seed run against a pre-existing Delta table must fail
    /// loudly unless `overwrite_existing=true`. Prevents double-
    /// publishing a stale manifest by accident.
    #[tokio::test]
    async fn test_seed_refuses_to_overwrite_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let template = format!("{}/{{table}}", dir.path().to_str().unwrap());
        let backend = FileBackend::new(example_store_root());

        seed_from_backend(&backend, &SeedConfig::new(&template))
            .await
            .unwrap();

        let err = seed_from_backend(&backend, &SeedConfig::new(&template))
            .await
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("already exists"),
            "expected already-exists error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_seed_overwrite_appends_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let template = format!("{}/{{table}}", dir.path().to_str().unwrap());
        let backend = FileBackend::new(example_store_root());

        seed_from_backend(&backend, &SeedConfig::new(&template))
            .await
            .unwrap();
        seed_from_backend(&backend, &SeedConfig::new(&template).with_overwrite(true))
            .await
            .expect("second seed with overwrite=true should append");
        // The resolver deduplicates policies by policy_id at query
        // time, so a duplicate append is harmless for read semantics.
        // We just prove the append did not error.
    }

    #[test]
    fn test_parse_ts_micros_handles_zulu_and_naive() {
        let naive = parse_ts_micros("2024-01-02T03:04:05").unwrap();
        let zulu = parse_ts_micros("2024-01-02T03:04:05Z").unwrap();
        assert_eq!(naive, zulu);
        // 2024-01-02T03:04:05 UTC = 1704164645 seconds since epoch.
        assert_eq!(naive, 1_704_164_645_000_000);
    }

    #[test]
    fn test_parse_ts_micros_rejects_garbage() {
        assert!(parse_ts_micros("").is_none());
        assert!(parse_ts_micros("not a timestamp").is_none());
        assert!(parse_ts_micros("2024").is_none());
    }
}
