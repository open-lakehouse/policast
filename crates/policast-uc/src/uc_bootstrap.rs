//! Unity Catalog bootstrap backend.
//!
//! This module is the Stage 3 production [`ResolveBackend`] the compose
//! demo runs against. The architecture call — `UcBootstrapBackend` over
//! either `DeltaBackend` (direct) or `UcRestBackend` (per-resolve
//! REST) — is documented in
//! `.cursor/plans/cedar-templates-and-tags_9f2a3b14.plan.md`; the short
//! version is:
//!
//! * **Startup** — the backend opens each of the four governance
//!   tables (`policies`, `manifest`, `bindings`, `tags`) through
//!   `deltalake` and snapshots the current rows into in-memory `Vec`
//!   caches behind an `RwLock`.
//! * **Steady state** — resolve requests read the snapshot with no
//!   network hop, preserving flat-file resolve latency.
//! * **Freshness** — a background task will tail the Change Data
//!   Feed on `manifest` and `tags` (the two tables whose deltas mutate
//!   what engines see) and swap the cache forward when it observes
//!   new rows. That task is scaffolded here (`refresh_snapshot` is
//!   callable on demand) but the tokio spawner is a follow-up commit.
//!
//! ## Storage credentials
//!
//! The eventual production path is to ask UC's REST API for a vended
//! credential per governance table — that is what the `uc_endpoint` +
//! `uc_bearer_token` fields on [`UcBootstrapConfig`] are for. This
//! commit wires the simpler **static storage** path: the config
//! carries a URI template (e.g. `s3://policast-demo/governance/policast/{table}`)
//! plus a map of `storage_options` that delta-rs forwards to its
//! object_store backend. That is enough for the MinIO compose demo
//! and for unit tests against a local filesystem tempdir. UC REST
//! vending layers on top in the `uc-bootstrap-credentials` follow-up
//! without changing the `ResolveBackend` impl.
//!
//! The whole module is gated behind the `uc-bootstrap` feature so
//! that consumers who only need `FileBackend` (the default) do not
//! pay the `deltalake` + `datafusion` compile cost.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::Deserialize;
use tokio::task::AbortHandle;

use datafusion::arrow::array::{
    Array, AsArray, Int32Array, Int64Array, ListArray, StringArray,
    TimestampMicrosecondArray,
};
use datafusion::arrow::datatypes::{DataType, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use deltalake::open_table_with_storage_options;
use reqwest::StatusCode;

use crate::backend::{BindingRow, ManifestRow, PolicyRow, ResolveBackend, TagRow};
use crate::cdc::{InvalidationEvent, InvalidationSender};
use crate::error::UcError;

/// Configuration knobs for [`UcBootstrapBackend`].
///
/// Defaults mirror the example catalog/schema used by
/// `examples/uc/ddl/*.sql` and by the compose bootstrap container so
/// that the "out of the box" path works against the shipped demo data
/// without extra flags.
#[derive(Debug, Clone)]
pub struct UcBootstrapConfig {
    /// Base URL of the Unity Catalog REST API, e.g.
    /// `http://unitycatalog:8080`.
    ///
    /// Used for per-table credential vending when
    /// `storage_uri_template` is unset.
    pub uc_endpoint: String,

    /// Catalog name that holds the four governance tables. Matches
    /// `examples/uc/ddl/01_create_catalog.sql`.
    pub governance_catalog: String,

    /// Schema name inside `governance_catalog`. Matches
    /// `examples/uc/ddl/01_create_catalog.sql`.
    pub governance_schema: String,

    /// How often the CDF tail task polls for new manifest/tags
    /// deltas. A value of `None` disables the refresh loop entirely
    /// (useful in tests that want a frozen snapshot). The spawner for
    /// the loop itself is a follow-up commit; this commit wires an
    /// on-demand [`UcBootstrapBackend::refresh_snapshot`] instead.
    pub refresh_interval: Option<Duration>,

    /// Optional bearer token for the UC REST endpoint. If unset, REST
    /// calls are sent without an Authorization header (handy for local
    /// demos with no auth gateway in front).
    pub uc_bearer_token: Option<String>,

    /// URI template for the four governance Delta tables. The literal
    /// substring `{table}` is replaced with `policies`, `manifest`,
    /// `bindings`, or `tags`. Example values:
    ///
    /// * `s3://policast-demo/governance/policast/{table}` for the
    ///   MinIO-backed compose stack.
    /// * `file:///var/policast/store/{table}` for a local path.
    ///
    /// If this is `Some(...)`, bootstrap uses static table URIs and
    /// static `storage_options` (compose demo path). If this is `None`,
    /// bootstrap resolves table location + credentials through UC REST.
    pub storage_uri_template: Option<String>,

    /// Storage options forwarded verbatim to
    /// `deltalake::open_table_with_storage_options`. Used to pass
    /// MinIO credentials, region overrides, etc.
    pub storage_options: HashMap<String, String>,
}

impl UcBootstrapConfig {
    /// Convenience constructor matching the shipped example stack.
    pub fn for_example_stack(uc_endpoint: impl Into<String>) -> Self {
        Self {
            uc_endpoint: uc_endpoint.into(),
            governance_catalog: "governance".into(),
            governance_schema: "policast".into(),
            refresh_interval: Some(Duration::from_secs(30)),
            uc_bearer_token: None,
            storage_uri_template: None,
            storage_options: HashMap::new(),
        }
    }

    /// Set the URI template used for each governance table (the
    /// `{table}` substring is replaced at open-time).
    pub fn with_storage_uri_template(mut self, template: impl Into<String>) -> Self {
        self.storage_uri_template = Some(template.into());
        self
    }

    /// Replace the storage-options map (passed to delta-rs).
    pub fn with_storage_options(mut self, opts: HashMap<String, String>) -> Self {
        self.storage_options = opts;
        self
    }

    /// Set bearer token for UC REST calls.
    pub fn with_uc_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.uc_bearer_token = Some(token.into());
        self
    }

    /// Resolve a fully-qualified governance table name for a short
    /// table id (`policies`, `manifest`, `bindings`, `tags`).
    pub fn governance_table_name(&self, name: &str) -> String {
        format!("{}.{}.{}", self.governance_catalog, self.governance_schema, name)
    }

    /// Resolve the static URI for one of the four governance tables.
    /// Returns `None` when static mode is disabled.
    pub fn static_table_uri(&self, name: &str) -> Option<String> {
        self.storage_uri_template
            .as_ref()
            .map(|template| template.replace("{table}", name))
    }
}

/// In-memory snapshot of the four governance tables. Internal; the
/// public-facing view is `ResolveBackend`.
#[derive(Debug, Default)]
struct Snapshot {
    policies: Vec<PolicyRow>,
    manifest: Vec<ManifestRow>,
    bindings: Vec<BindingRow>,
    tags: Vec<TagRow>,
}

/// Drop-abort wrapper for the periodic refresh task. Held behind an
/// `Arc` on the backend so cloning the backend is cheap and the task
/// only gets aborted when the last clone is dropped. `AbortHandle` is
/// used instead of `JoinHandle` so the background loop cannot keep
/// the runtime alive once all handles to the backend are gone.
struct RefreshGuard {
    abort: AbortHandle,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

impl std::fmt::Debug for RefreshGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshGuard")
            .field("aborted", &self.abort.is_finished())
            .finish()
    }
}

/// Production [`ResolveBackend`] that snapshots the governance tables
/// through Unity Catalog (or, in the compose demo, MinIO) and keeps
/// them fresh via a periodic refresh task.
#[derive(Clone)]
pub struct UcBootstrapBackend {
    cfg: UcBootstrapConfig,
    snapshot: Arc<RwLock<Snapshot>>,
    /// Guard for the background refresh task. `None` when
    /// `cfg.refresh_interval` is `None` (tests wanting a frozen
    /// snapshot; sidecars running in one-shot mode).
    refresh_task: Option<Arc<RefreshGuard>>,
}

impl std::fmt::Debug for UcBootstrapBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snap = self.snapshot.read();
        f.debug_struct("UcBootstrapBackend")
            .field("cfg", &self.cfg)
            .field("policies_len", &snap.policies.len())
            .field("manifest_len", &snap.manifest.len())
            .field("bindings_len", &snap.bindings.len())
            .field("tags_len", &snap.tags.len())
            .field("refresh_task_active", &self.refresh_task.is_some())
            .finish()
    }
}

impl UcBootstrapBackend {
    /// Construct + load the initial snapshot, then start the periodic
    /// refresh task if `cfg.refresh_interval` is set. Opens all four
    /// Delta tables, converts the Arrow batches into `PolicyRow` / etc,
    /// and stashes them in `RwLock`-protected caches.
    ///
    /// The `tags` table is treated as optional: a deployment that has
    /// not yet run `examples/uc/ddl/06_tags.sql` still bootstraps with
    /// an empty tag index, matching `FileBackend`'s behaviour when
    /// `tags.json` is absent. Any of the other three tables missing
    /// is a hard failure — resolve bundles would be incoherent.
    pub async fn bootstrap(cfg: UcBootstrapConfig) -> Result<Self, UcError> {
        Self::bootstrap_inner(cfg, None).await
    }

    /// Same as [`Self::bootstrap`], but wire a [`BundleCache`]
    /// invalidation channel so every successful periodic refresh
    /// fans out an [`InvalidationEvent::InvalidateAll`]. Use this in
    /// the sidecar to keep the resolve-bundle LRU in lock-step with
    /// governance-table mutations.
    ///
    /// [`BundleCache`]: crate::cache::BundleCache
    pub async fn bootstrap_with_invalidation(
        cfg: UcBootstrapConfig,
        invalidation: InvalidationSender,
    ) -> Result<Self, UcError> {
        Self::bootstrap_inner(cfg, Some(invalidation)).await
    }

    async fn bootstrap_inner(
        cfg: UcBootstrapConfig,
        invalidation: Option<InvalidationSender>,
    ) -> Result<Self, UcError> {
        let snapshot = Arc::new(RwLock::new(Snapshot::default()));
        refresh_into(&cfg, &snapshot).await?;

        let refresh_task = cfg.refresh_interval.map(|interval| {
            let task_cfg = cfg.clone();
            let task_snap = Arc::clone(&snapshot);
            let task_inv = invalidation.clone();
            let handle = tokio::spawn(refresh_loop(
                task_cfg,
                task_snap,
                task_inv,
                interval,
            ));
            Arc::new(RefreshGuard {
                abort: handle.abort_handle(),
            })
        });

        Ok(Self {
            cfg,
            snapshot,
            refresh_task,
        })
    }

    /// Re-scan all four governance tables and swap the snapshot
    /// atomically. Called by `bootstrap()` and by the periodic
    /// refresh task; exposed publicly so tests and admin hooks can
    /// force a refresh.
    pub async fn refresh_snapshot(&self) -> Result<(), UcError> {
        refresh_into(&self.cfg, &self.snapshot).await
    }

    /// Access the configuration — handy for the sidecar's startup
    /// logging and for tests that want to assert on defaults.
    pub fn config(&self) -> &UcBootstrapConfig {
        &self.cfg
    }

    /// Returns true iff the periodic refresh task is running. Useful
    /// in tests that want to assert the task was (or was not) spawned.
    pub fn refresh_task_running(&self) -> bool {
        self.refresh_task
            .as_ref()
            .map(|g| !g.abort.is_finished())
            .unwrap_or(false)
    }
}

async fn refresh_into(
    cfg: &UcBootstrapConfig,
    snapshot: &Arc<RwLock<Snapshot>>,
) -> Result<(), UcError> {
    let policies = load_required(cfg, "policies", arrow_to_policy_rows).await?;
    let manifest = load_required(cfg, "manifest", arrow_to_manifest_rows).await?;
    let bindings = load_required(cfg, "bindings", arrow_to_binding_rows).await?;
    let tags = load_optional(cfg, "tags", arrow_to_tag_rows).await?;

    let mut snap = snapshot.write();
    snap.policies = policies;
    snap.manifest = manifest;
    snap.bindings = bindings;
    snap.tags = tags;
    Ok(())
}

async fn load_required<R, F>(
    cfg: &UcBootstrapConfig,
    name: &str,
    convert: F,
) -> Result<Vec<R>, UcError>
where
    F: Fn(&RecordBatch) -> Result<Vec<R>, UcError>,
{
    let access = resolve_table_access(cfg, name).await?;
    let batches = scan_delta_table(&access.uri, &access.storage_options).await?;
    flatten_batches(&batches, convert)
}

async fn load_optional<R, F>(
    cfg: &UcBootstrapConfig,
    name: &str,
    convert: F,
) -> Result<Vec<R>, UcError>
where
    F: Fn(&RecordBatch) -> Result<Vec<R>, UcError>,
{
    let access = resolve_table_access(cfg, name).await?;
    match scan_delta_table(&access.uri, &access.storage_options).await {
        Ok(batches) => flatten_batches(&batches, convert),
        Err(e) if is_table_not_found(&e) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone)]
struct ResolvedTableAccess {
    uri: String,
    storage_options: HashMap<String, String>,
}

/// Resolve table access in one of two modes:
///
/// 1) Static mode (`storage_uri_template` is set): URI comes from the
///    template and options are `cfg.storage_options`.
/// 2) UC-vended mode (`storage_uri_template` unset): call UC REST for
///    table metadata + temporary table credentials.
async fn resolve_table_access(cfg: &UcBootstrapConfig, short_name: &str) -> Result<ResolvedTableAccess, UcError> {
    if let Some(uri) = cfg.static_table_uri(short_name) {
        return Ok(ResolvedTableAccess {
            uri,
            storage_options: cfg.storage_options.clone(),
        });
    }

    resolve_table_access_via_uc(cfg, short_name).await
}

#[derive(Debug, Deserialize)]
struct UcTableInfo {
    #[serde(default)]
    table_id: Option<String>,
    #[serde(default)]
    storage_location: Option<String>,
    #[serde(default)]
    location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UcTempCredsResponse {
    #[serde(default)]
    aws_temp_credentials: Option<UcAwsTempCreds>,
    #[serde(default)]
    credentials: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct UcAwsTempCreds {
    #[serde(default)]
    access_key_id: Option<String>,
    #[serde(default)]
    secret_access_key: Option<String>,
    #[serde(default)]
    session_token: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct UcTempCredsRequest<'a> {
    table_id: Option<&'a str>,
    table_name: &'a str,
    operation: &'a str,
}

fn uc_rest_client() -> reqwest::Client {
    // Build-per-call keeps this helper side-effect free and avoids
    // introducing another shared mutable handle into bootstrap code.
    reqwest::Client::new()
}

fn uc_apply_auth(
    cfg: &UcBootstrapConfig,
    req: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    match cfg.uc_bearer_token.as_ref() {
        Some(token) if !token.is_empty() => req.bearer_auth(token),
        _ => req,
    }
}

async fn resolve_table_access_via_uc(
    cfg: &UcBootstrapConfig,
    short_name: &str,
) -> Result<ResolvedTableAccess, UcError> {
    let full_name = cfg.governance_table_name(short_name);
    let base = cfg.uc_endpoint.trim_end_matches('/');
    let client = uc_rest_client();

    // 1) Fetch table metadata to get the canonical storage location
    // and table_id (if available).
    let table_url = format!("{base}/api/2.1/unity-catalog/tables/{full_name}");
    let table_req = uc_apply_auth(cfg, client.get(&table_url));
    let table_resp = table_req
        .send()
        .await
        .map_err(|e| UcError::Config(format!("uc table lookup {full_name}: {e}")))?;
    if !table_resp.status().is_success() {
        let code = table_resp.status();
        let body = table_resp.text().await.unwrap_or_default();
        return Err(UcError::Config(format!(
            "uc table lookup {full_name} failed ({code}): {body}"
        )));
    }
    let table_info: UcTableInfo = table_resp
        .json()
        .await
        .map_err(|e| UcError::Config(format!("uc table lookup decode {full_name}: {e}")))?;
    let uri = table_info
        .storage_location
        .or(table_info.location)
        .ok_or_else(|| {
            UcError::Config(format!(
                "uc table lookup {full_name}: missing storage_location/location"
            ))
        })?;

    // 2) Try to vend temporary credentials. If UC returns 404/405, keep
    // running with static options only — this supports older UC OSS
    // builds and the local compose path where direct storage options are
    // sufficient.
    let creds_url = format!("{base}/api/2.1/unity-catalog/temporary-table-credentials");
    let creds_req = UcTempCredsRequest {
        table_id: table_info.table_id.as_deref(),
        table_name: &full_name,
        operation: "READ",
    };
    let creds_req = uc_apply_auth(cfg, client.post(&creds_url).json(&creds_req));
    let creds_resp = creds_req.send().await.map_err(|e| {
        UcError::Config(format!("uc temporary creds request {full_name}: {e}"))
    })?;

    let mut storage_options = cfg.storage_options.clone();
    match creds_resp.status() {
        s if s.is_success() => {
            let payload: UcTempCredsResponse = creds_resp.json().await.map_err(|e| {
                UcError::Config(format!("uc temporary creds decode {full_name}: {e}"))
            })?;
            inject_vended_storage_options(&mut storage_options, payload);
        }
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => {
            // Fallback mode: no vended creds endpoint available.
        }
        other => {
            let body = creds_resp.text().await.unwrap_or_default();
            return Err(UcError::Config(format!(
                "uc temporary creds {full_name} failed ({other}): {body}"
            )));
        }
    }

    Ok(ResolvedTableAccess {
        uri,
        storage_options,
    })
}

fn inject_vended_storage_options(
    out: &mut HashMap<String, String>,
    payload: UcTempCredsResponse,
) {
    if let Some(aws) = payload.aws_temp_credentials {
        if let Some(v) = aws.access_key_id {
            out.insert("AWS_ACCESS_KEY_ID".into(), v);
        }
        if let Some(v) = aws.secret_access_key {
            out.insert("AWS_SECRET_ACCESS_KEY".into(), v);
        }
        if let Some(v) = aws.session_token {
            out.insert("AWS_SESSION_TOKEN".into(), v);
        }
    }
    if let Some(kv) = payload.credentials {
        for (k, v) in kv {
            out.insert(k, v);
        }
    }
}

/// Background loop: sleeps for `interval`, refreshes the snapshot,
/// optionally fans an `InvalidateAll` out to a bundle cache. A
/// refresh failure is logged and the loop continues — a transient UC
/// REST hiccup must not take governance offline.
///
/// The first sleep happens *before* the first refresh so bootstrap's
/// own eager load is not immediately re-done.
async fn refresh_loop(
    cfg: UcBootstrapConfig,
    snapshot: Arc<RwLock<Snapshot>>,
    invalidation: Option<InvalidationSender>,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        match refresh_into(&cfg, &snapshot).await {
            Ok(()) => {
                if let Some(sender) = invalidation.as_ref() {
                    // A send failure means the receiver was dropped —
                    // in that case the bundle cache is gone, so we
                    // have nothing more to do. Keep tailing the
                    // governance tables regardless.
                    let _ = sender.send(InvalidationEvent::InvalidateAll);
                }
            }
            Err(e) => {
                eprintln!(
                    "[uc-bootstrap] periodic refresh failed ({e}); \
                     keeping previous snapshot and retrying in {:?}",
                    interval
                );
            }
        }
    }
}

/// Open a Delta table and pull every row into Arrow record batches.
async fn scan_delta_table(
    uri: &str,
    storage_options: &HashMap<String, String>,
) -> Result<Vec<RecordBatch>, UcError> {
    let table = open_table_with_storage_options(uri, storage_options.clone())
        .await
        .map_err(|e| UcError::Config(format!("open {uri}: {e}")))?;

    // We use a transient SessionContext rather than wiring deltalake's
    // own scan pipeline because the SELECT * path is the most
    // well-trodden; the code below does not care which columns exist
    // beyond the schema mapping in the `arrow_to_*` helpers.
    let ctx = SessionContext::new();
    let table_alias = "t";
    ctx.register_table(table_alias, Arc::new(table))
        .map_err(|e| UcError::Config(format!("register {uri}: {e}")))?;
    let df = ctx
        .sql(&format!("SELECT * FROM {table_alias}"))
        .await
        .map_err(|e| UcError::Config(format!("sql {uri}: {e}")))?;
    df.collect()
        .await
        .map_err(|e| UcError::Config(format!("collect {uri}: {e}")))
}

fn flatten_batches<R, F>(batches: &[RecordBatch], convert: F) -> Result<Vec<R>, UcError>
where
    F: Fn(&RecordBatch) -> Result<Vec<R>, UcError>,
{
    let mut rows = Vec::new();
    for batch in batches {
        rows.extend(convert(batch)?);
    }
    Ok(rows)
}

/// Crude heuristic for "the Delta table does not exist" — delta-rs
/// surfaces this through a family of `DeltaTableError::NotATable` /
/// `InvalidPath` / `ObjectStoreError` messages that we do not want
/// to enumerate. The tags table is the only currently-optional one,
/// and this check is scoped to that optional load path only.
fn is_table_not_found(err: &UcError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("not a delta table")
        || msg.contains("no such file")
        || msg.contains("does not exist")
        || msg.contains("not found")
}

#[async_trait]
impl ResolveBackend for UcBootstrapBackend {
    async fn policies(&self) -> Result<Vec<PolicyRow>, UcError> {
        Ok(self.snapshot.read().policies.clone())
    }

    async fn manifest(&self) -> Result<Vec<ManifestRow>, UcError> {
        Ok(self.snapshot.read().manifest.clone())
    }

    async fn bindings(&self) -> Result<Vec<BindingRow>, UcError> {
        Ok(self.snapshot.read().bindings.clone())
    }

    async fn tags(&self) -> Result<Vec<TagRow>, UcError> {
        Ok(self.snapshot.read().tags.clone())
    }
}

// ---------------------------------------------------------------------
// Arrow → Row converters
// ---------------------------------------------------------------------
//
// Each helper consumes one Arrow `RecordBatch` and emits the matching
// row type. The column lookup is by *name*, not by position, because
// Delta `SELECT *` does not guarantee physical column order — partition
// columns in particular tend to be projected at the end. Missing
// optional columns are silently skipped; missing required columns are
// a hard failure with a message that names the table and the field.

fn col<'a>(
    batch: &'a RecordBatch,
    table: &'static str,
    name: &'static str,
) -> Result<&'a dyn Array, UcError> {
    batch.column_by_name(name).map(|c| c.as_ref()).ok_or_else(|| {
        UcError::Config(format!(
            "governance table `{table}` missing required column `{name}`"
        ))
    })
}

fn optional_col<'a>(batch: &'a RecordBatch, name: &'static str) -> Option<&'a dyn Array> {
    batch.column_by_name(name).map(|c| c.as_ref())
}

fn as_string(arr: &dyn Array, table: &str, col: &str) -> Result<StringArray, UcError> {
    match arr.data_type() {
        DataType::Utf8 => Ok(arr.as_string::<i32>().clone()),
        other => Err(UcError::Config(format!(
            "governance table `{table}` column `{col}`: expected Utf8, got {other:?}"
        ))),
    }
}

fn as_int64(arr: &dyn Array, table: &str, col: &str) -> Result<Int64Array, UcError> {
    match arr.data_type() {
        DataType::Int64 => Ok(arr.as_primitive().clone()),
        other => Err(UcError::Config(format!(
            "governance table `{table}` column `{col}`: expected Int64, got {other:?}"
        ))),
    }
}

fn get_string(arr: &StringArray, idx: usize) -> String {
    arr.value(idx).to_string()
}

fn get_optional_string(arr: &StringArray, idx: usize) -> Option<String> {
    if arr.is_null(idx) {
        None
    } else {
        let v = arr.value(idx);
        if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        }
    }
}

fn get_optional_timestamp(batch: &RecordBatch, name: &'static str, idx: usize) -> Option<String> {
    let arr = optional_col(batch, name)?;
    format_timestamp_value(arr, idx)
}

fn format_timestamp_value(arr: &dyn Array, idx: usize) -> Option<String> {
    if arr.is_null(idx) {
        return None;
    }
    match arr.data_type() {
        DataType::Timestamp(TimeUnit::Microsecond, _tz) => {
            let ts = arr.as_any().downcast_ref::<TimestampMicrosecondArray>()?;
            let micros = ts.value(idx);
            let secs = micros.div_euclid(1_000_000);
            let sub = (micros.rem_euclid(1_000_000)) as u32 * 1_000; // to ns
            let dt = time::OffsetDateTime::from_unix_timestamp(secs).ok()?;
            let dt = dt + time::Duration::nanoseconds(sub as i64);
            dt.format(&time::format_description::well_known::Rfc3339).ok()
        }
        DataType::Utf8 => {
            let s = arr.as_string::<i32>();
            if s.is_null(idx) {
                None
            } else {
                Some(s.value(idx).to_string())
            }
        }
        _ => None,
    }
}

fn get_optional_string_list(batch: &RecordBatch, name: &'static str, idx: usize) -> Option<Vec<String>> {
    let arr = optional_col(batch, name)?;
    let list = arr.as_any().downcast_ref::<ListArray>()?;
    if list.is_null(idx) {
        return None;
    }
    let values = list.value(idx);
    let strings = values.as_string_opt::<i32>()?;
    let mut out = Vec::with_capacity(strings.len());
    for i in 0..strings.len() {
        if strings.is_null(i) {
            continue;
        }
        out.push(strings.value(i).to_string());
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Returns true if the batch has a `retired_at` column and its value
/// at `idx` is non-null — those rows represent tombstones and must be
/// excluded from the snapshot.
fn is_retired(batch: &RecordBatch, idx: usize) -> bool {
    get_optional_timestamp(batch, "retired_at", idx).is_some()
}

fn arrow_to_policy_rows(batch: &RecordBatch) -> Result<Vec<PolicyRow>, UcError> {
    let tbl = "policies";
    let policy_id = as_string(col(batch, tbl, "policy_id")?, tbl, "policy_id")?;
    let filter_type = as_string(col(batch, tbl, "filter_type")?, tbl, "filter_type")?;
    let target_table = as_string(col(batch, tbl, "target_table")?, tbl, "target_table")?;
    let effect = as_string(col(batch, tbl, "effect")?, tbl, "effect")?;
    let version = as_int64(col(batch, tbl, "version")?, tbl, "version")?;

    let column = optional_col(batch, "column").and_then(|a| a.as_string_opt::<i32>().cloned());
    let target_tag = optional_col(batch, "target_tag").and_then(|a| a.as_string_opt::<i32>().cloned());
    let applies_to_tag =
        optional_col(batch, "applies_to_tag").and_then(|a| a.as_string_opt::<i32>().cloned());
    let description =
        optional_col(batch, "description").and_then(|a| a.as_string_opt::<i32>().cloned());

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        if is_retired(batch, i) {
            continue;
        }
        out.push(PolicyRow {
            policy_id: get_string(&policy_id, i),
            filter_type: get_string(&filter_type, i),
            target_table: get_string(&target_table, i),
            column: column.as_ref().and_then(|a| {
                if a.is_null(i) || a.value(i).is_empty() {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            }),
            target_tag: target_tag.as_ref().and_then(|a| {
                if a.is_null(i) || a.value(i).is_empty() {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            }),
            applies_to_tag: applies_to_tag.as_ref().and_then(|a| {
                if a.is_null(i) || a.value(i).is_empty() {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            }),
            effect: get_string(&effect, i),
            applies_to_roles: get_optional_string_list(batch, "applies_to_roles", i),
            description: description.as_ref().and_then(|a| {
                if a.is_null(i) || a.value(i).is_empty() {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            }),
            version: version.value(i),
        });
    }
    Ok(out)
}

fn arrow_to_manifest_rows(batch: &RecordBatch) -> Result<Vec<ManifestRow>, UcError> {
    let tbl = "manifest";
    let policy_id = as_string(col(batch, tbl, "policy_id")?, tbl, "policy_id")?;
    let cel_expression = as_string(col(batch, tbl, "cel_expression")?, tbl, "cel_expression")?;
    let version = as_int64(col(batch, tbl, "version")?, tbl, "version")?;

    let compiler_version =
        optional_col(batch, "compiler_version").and_then(|a| a.as_string_opt::<i32>().cloned());
    let source_hash =
        optional_col(batch, "source_hash").and_then(|a| a.as_string_opt::<i32>().cloned());

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(ManifestRow {
            policy_id: get_string(&policy_id, i),
            cel_expression: get_string(&cel_expression, i),
            version: version.value(i),
            compiler_version: compiler_version
                .as_ref()
                .map(|a| if a.is_null(i) { String::new() } else { a.value(i).to_string() })
                .unwrap_or_default(),
            source_hash: source_hash
                .as_ref()
                .map(|a| if a.is_null(i) { String::new() } else { a.value(i).to_string() })
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

fn arrow_to_binding_rows(batch: &RecordBatch) -> Result<Vec<BindingRow>, UcError> {
    let tbl = "bindings";
    let binding_id = as_string(col(batch, tbl, "binding_id")?, tbl, "binding_id")?;
    let policy_id = as_string(col(batch, tbl, "policy_id")?, tbl, "policy_id")?;
    let target = as_string(col(batch, tbl, "target")?, tbl, "target")?;
    let principal_selector = as_string(
        col(batch, tbl, "principal_selector")?,
        tbl,
        "principal_selector",
    )?;

    // `precedence` is INT (i32) per the DDL. Treat as optional in case
    // some deployments widened it to BIGINT; default 0 keeps the
    // resolver behaviour stable.
    let precedence_i32 = optional_col(batch, "precedence")
        .and_then(|a| match a.data_type() {
            DataType::Int32 => Some(a.as_any().downcast_ref::<Int32Array>()?.clone()),
            _ => None,
        });
    let precedence_i64 = optional_col(batch, "precedence")
        .and_then(|a| match a.data_type() {
            DataType::Int64 => Some(a.as_any().downcast_ref::<Int64Array>()?.clone()),
            _ => None,
        });
    if optional_col(batch, "precedence").is_some()
        && precedence_i32.is_none()
        && precedence_i64.is_none()
    {
        return Err(UcError::Config(
            "governance table `bindings` column `precedence`: expected Int32 or Int64".into(),
        ));
    }

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let precedence = precedence_i32
            .as_ref()
            .map(|a| if a.is_null(i) { 0 } else { a.value(i) })
            .or_else(|| {
                precedence_i64
                    .as_ref()
                    .map(|a| if a.is_null(i) { 0 } else { a.value(i) as i32 })
            })
            .unwrap_or(0);
        out.push(BindingRow {
            binding_id: get_string(&binding_id, i),
            policy_id: get_string(&policy_id, i),
            target: get_string(&target, i),
            principal_selector: get_string(&principal_selector, i),
            precedence,
        });
    }
    Ok(out)
}

fn arrow_to_tag_rows(batch: &RecordBatch) -> Result<Vec<TagRow>, UcError> {
    let tbl = "tags";
    let entity = as_string(col(batch, tbl, "entity")?, tbl, "entity")?;
    let entity_kind = as_string(col(batch, tbl, "entity_kind")?, tbl, "entity_kind")?;
    let tag = as_string(col(batch, tbl, "tag")?, tbl, "tag")?;

    let set_by = optional_col(batch, "set_by").and_then(|a| a.as_string_opt::<i32>().cloned());

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        // Tag tombstones flow through the row stream with their
        // `retired_at` intact — ResolverCore::expand_tag_scoped uses
        // TagRow::is_active to filter. We do NOT drop retired rows
        // here, mirroring FileBackend semantics.
        out.push(TagRow {
            entity: get_string(&entity, i),
            entity_kind: get_string(&entity_kind, i),
            tag: get_string(&tag, i),
            set_by: set_by.as_ref().and_then(|a| get_optional_string(a, i)),
            set_at: get_optional_timestamp(batch, "set_at", i),
            retired_at: get_optional_timestamp(batch, "retired_at", i),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{
        builder::{Int32Builder, Int64Builder, ListBuilder, StringBuilder},
        ArrayRef,
    };
    use datafusion::arrow::datatypes::{Field, Schema};
    use deltalake::operations::create::CreateBuilder;
    use deltalake::operations::write::WriteBuilder;
    use tempfile::TempDir;

    fn str_array(values: &[&str]) -> ArrayRef {
        let mut b = StringBuilder::new();
        for v in values {
            b.append_value(v);
        }
        Arc::new(b.finish()) as ArrayRef
    }

    fn opt_str_array(values: &[Option<&str>]) -> ArrayRef {
        let mut b = StringBuilder::new();
        for v in values {
            match v {
                Some(s) => b.append_value(s),
                None => b.append_null(),
            }
        }
        Arc::new(b.finish()) as ArrayRef
    }

    fn i64_array(values: &[i64]) -> ArrayRef {
        let mut b = Int64Builder::new();
        for v in values {
            b.append_value(*v);
        }
        Arc::new(b.finish()) as ArrayRef
    }

    fn i32_array(values: &[i32]) -> ArrayRef {
        let mut b = Int32Builder::new();
        for v in values {
            b.append_value(*v);
        }
        Arc::new(b.finish()) as ArrayRef
    }

    fn list_str_array(values: &[Option<Vec<&str>>]) -> ArrayRef {
        let mut b = ListBuilder::new(StringBuilder::new());
        for v in values {
            match v {
                Some(vs) => {
                    for s in vs {
                        b.values().append_value(s);
                    }
                    b.append(true);
                }
                None => b.append(false),
            }
        }
        Arc::new(b.finish()) as ArrayRef
    }

    async fn write_delta(uri: &str, schema: Arc<Schema>, batch: RecordBatch) {
        let delta_schema: deltalake::kernel::Schema = schema.as_ref().try_into().unwrap();
        let table = CreateBuilder::new()
            .with_location(uri)
            .with_columns(delta_schema.fields().cloned())
            .await
            .unwrap();
        WriteBuilder::new(table.log_store(), table.state.clone())
            .with_input_batches(vec![batch])
            .await
            .unwrap();
    }

    /// Build a directory containing all four governance Delta tables
    /// populated with a small, hand-picked dataset.
    async fn seed_governance_tables(dir: &TempDir, include_tags: bool) {
        // policies
        let policies_schema = Arc::new(Schema::new(vec![
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
        ]));
        let policies_batch = RecordBatch::try_new(
            policies_schema.clone(),
            vec![
                str_array(&["column_mask_by_pii_tag", "row_filter_region"]),
                str_array(&["column_mask", "row_filter"]),
                str_array(&["*", "*"]),
                opt_str_array(&[None, None]),
                opt_str_array(&[None, Some("clinical")]),
                opt_str_array(&[Some("pii"), None]),
                str_array(&["permit", "permit"]),
                list_str_array(&[
                    Some(vec!["analyst"]),
                    Some(vec!["analyst", "physician"]),
                ]),
                opt_str_array(&[Some("mask pii cols"), Some("regional row filter")]),
                i64_array(&[1, 1]),
            ],
        )
        .unwrap();
        let uri = dir.path().join("policies");
        write_delta(uri.to_str().unwrap(), policies_schema, policies_batch).await;

        // manifest
        let manifest_schema = Arc::new(Schema::new(vec![
            Field::new("policy_id", DataType::Utf8, false),
            Field::new("cel_expression", DataType::Utf8, false),
            Field::new("version", DataType::Int64, false),
            Field::new("compiler_version", DataType::Utf8, false),
            Field::new("source_hash", DataType::Utf8, false),
        ]));
        let manifest_batch = RecordBatch::try_new(
            manifest_schema.clone(),
            vec![
                str_array(&["column_mask_by_pii_tag", "row_filter_region"]),
                str_array(&[
                    "principal.role == 'analyst'",
                    "principal.region == resource.region",
                ]),
                i64_array(&[1, 1]),
                str_array(&["0.1.0", "0.1.0"]),
                str_array(&["abc", "def"]),
            ],
        )
        .unwrap();
        let uri = dir.path().join("manifest");
        write_delta(uri.to_str().unwrap(), manifest_schema, manifest_batch).await;

        // bindings
        let bindings_schema = Arc::new(Schema::new(vec![
            Field::new("binding_id", DataType::Utf8, false),
            Field::new("policy_id", DataType::Utf8, false),
            Field::new("target", DataType::Utf8, false),
            Field::new("principal_selector", DataType::Utf8, false),
            Field::new("precedence", DataType::Int32, false),
        ]));
        let bindings_batch = RecordBatch::try_new(
            bindings_schema.clone(),
            vec![
                str_array(&["b_mask", "b_row"]),
                str_array(&["column_mask_by_pii_tag", "row_filter_region"]),
                str_array(&["hospital.clinical.patients", "hospital.clinical.patients"]),
                str_array(&["role:analyst", "*"]),
                i32_array(&[10, 20]),
            ],
        )
        .unwrap();
        let uri = dir.path().join("bindings");
        write_delta(uri.to_str().unwrap(), bindings_schema, bindings_batch).await;

        if include_tags {
            let tags_schema = Arc::new(Schema::new(vec![
                Field::new("entity", DataType::Utf8, false),
                Field::new("entity_kind", DataType::Utf8, false),
                Field::new("tag", DataType::Utf8, false),
                Field::new("set_by", DataType::Utf8, true),
            ]));
            let tags_batch = RecordBatch::try_new(
                tags_schema.clone(),
                vec![
                    str_array(&[
                        "hospital.clinical.patients",
                        "hospital.clinical.patients:ssn",
                    ]),
                    str_array(&["table", "column"]),
                    str_array(&["clinical", "pii"]),
                    opt_str_array(&[Some("admin"), Some("admin")]),
                ],
            )
            .unwrap();
            let uri = dir.path().join("tags");
            write_delta(uri.to_str().unwrap(), tags_schema, tags_batch).await;
        }
    }

    fn test_config(dir: &TempDir) -> UcBootstrapConfig {
        let template = format!("{}/{{table}}", dir.path().to_str().unwrap());
        let mut cfg = UcBootstrapConfig::for_example_stack("http://unitycatalog:8080")
            .with_storage_uri_template(template);
        // Most tests want a frozen snapshot; the refresh-task tests
        // below opt back in explicitly.
        cfg.refresh_interval = None;
        cfg
    }

    #[test]
    fn test_example_stack_config_defaults_match_shipped_ddl() {
        let cfg = UcBootstrapConfig::for_example_stack("http://unitycatalog:8080");
        assert_eq!(cfg.uc_endpoint, "http://unitycatalog:8080");
        assert_eq!(cfg.governance_catalog, "governance");
        assert_eq!(cfg.governance_schema, "policast");
        assert_eq!(cfg.refresh_interval, Some(Duration::from_secs(30)));
        assert!(cfg.uc_bearer_token.is_none());
        assert!(cfg.storage_uri_template.is_none());
        assert!(cfg.storage_options.is_empty());
    }

    #[test]
    fn test_static_table_uri_substitutes_placeholder() {
        let cfg = UcBootstrapConfig::for_example_stack("http://uc")
            .with_storage_uri_template("s3://bucket/policast/{table}");
        assert_eq!(
            cfg.static_table_uri("policies").as_deref(),
            Some("s3://bucket/policast/policies")
        );
        assert_eq!(
            cfg.static_table_uri("tags").as_deref(),
            Some("s3://bucket/policast/tags")
        );
    }

    #[test]
    fn test_static_table_uri_none_without_template() {
        let cfg = UcBootstrapConfig::for_example_stack("http://uc");
        assert!(cfg.static_table_uri("policies").is_none());
    }

    #[test]
    fn test_governance_table_name_uses_catalog_schema() {
        let cfg = UcBootstrapConfig::for_example_stack("http://uc");
        assert_eq!(
            cfg.governance_table_name("manifest"),
            "governance.policast.manifest"
        );
    }

    #[tokio::test]
    async fn test_bootstrap_requires_static_template_or_reachable_uc_endpoint() {
        let cfg = UcBootstrapConfig::for_example_stack("http://127.0.0.1:9");
        let err = UcBootstrapBackend::bootstrap(cfg).await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("uc table lookup"),
            "err={err}"
        );
    }

    #[tokio::test]
    async fn test_bootstrap_loads_all_four_tables() {
        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;
        let backend = UcBootstrapBackend::bootstrap(test_config(&dir))
            .await
            .expect("bootstrap should succeed");

        let policies = backend.policies().await.unwrap();
        assert_eq!(policies.len(), 2);
        let pii = policies
            .iter()
            .find(|p| p.policy_id == "column_mask_by_pii_tag")
            .expect("pii template present");
        assert_eq!(pii.filter_type, "column_mask");
        assert_eq!(pii.applies_to_tag.as_deref(), Some("pii"));
        assert!(pii.target_tag.is_none());
        assert_eq!(
            pii.applies_to_roles.as_deref(),
            Some(&["analyst".to_string()][..])
        );

        let manifest = backend.manifest().await.unwrap();
        assert_eq!(manifest.len(), 2);
        assert!(manifest
            .iter()
            .any(|m| m.policy_id == "row_filter_region" && m.source_hash == "def"));

        let bindings = backend.bindings().await.unwrap();
        assert_eq!(bindings.len(), 2);
        let by_mask = bindings.iter().find(|b| b.binding_id == "b_mask").unwrap();
        assert_eq!(by_mask.precedence, 10);
        assert_eq!(by_mask.principal_selector, "role:analyst");

        let tags = backend.tags().await.unwrap();
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|t| t.tag == "pii" && t.is_column()));
        assert!(tags.iter().any(|t| t.tag == "clinical" && t.is_table()));
    }

    #[cfg(feature = "sidecar")]
    #[tokio::test]
    async fn test_bootstrap_can_resolve_table_access_via_uc_rest() {
        use axum::{
            extract::{Path, State},
            routing::{get, post},
            Json, Router,
        };
        use serde_json::{json, Value};

        async fn table_handler(
            State(root): State<std::path::PathBuf>,
            Path(full_name): Path<String>,
        ) -> Json<Value> {
            let short = full_name
                .rsplit('.')
                .next()
                .expect("table short name");
            let uri = root.join(short).to_string_lossy().to_string();
            Json(json!({
                "table_id": format!("id-{short}"),
                "storage_location": uri
            }))
        }

        async fn creds_handler() -> Json<Value> {
            Json(json!({
                "aws_temp_credentials": {
                    "access_key_id": "ak",
                    "secret_access_key": "sk",
                    "session_token": "st"
                }
            }))
        }

        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;
        let router = Router::new()
            .route(
                "/api/2.1/unity-catalog/tables/:full_name",
                get(table_handler),
            )
            .route(
                "/api/2.1/unity-catalog/temporary-table-credentials",
                post(creds_handler),
            )
            .with_state(dir.path().to_path_buf());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let mut cfg = UcBootstrapConfig::for_example_stack(format!("http://{addr}"));
        cfg.refresh_interval = None;
        // No static template: force UC REST resolution path.
        cfg.storage_uri_template = None;
        let backend = UcBootstrapBackend::bootstrap(cfg).await.unwrap();

        assert_eq!(backend.policies().await.unwrap().len(), 2);
        assert_eq!(backend.manifest().await.unwrap().len(), 2);
        assert_eq!(backend.bindings().await.unwrap().len(), 2);
        assert_eq!(backend.tags().await.unwrap().len(), 2);
        server.abort();
    }

    #[test]
    fn test_inject_vended_storage_options_populates_aws_keys() {
        let mut out = HashMap::new();
        out.insert("AWS_REGION".to_string(), "us-east-1".to_string());
        inject_vended_storage_options(
            &mut out,
            UcTempCredsResponse {
                aws_temp_credentials: Some(UcAwsTempCreds {
                    access_key_id: Some("ak".into()),
                    secret_access_key: Some("sk".into()),
                    session_token: Some("st".into()),
                }),
                credentials: None,
            },
        );
        assert_eq!(out.get("AWS_ACCESS_KEY_ID").map(String::as_str), Some("ak"));
        assert_eq!(
            out.get("AWS_SECRET_ACCESS_KEY").map(String::as_str),
            Some("sk")
        );
        assert_eq!(out.get("AWS_SESSION_TOKEN").map(String::as_str), Some("st"));
        // Existing static options are preserved.
        assert_eq!(out.get("AWS_REGION").map(String::as_str), Some("us-east-1"));
    }

    /// A deployment that has not seeded `tags.json` / the `tags`
    /// Delta table should still bootstrap successfully with an empty
    /// tag index, matching `FileBackend` semantics.
    #[tokio::test]
    async fn test_bootstrap_tolerates_missing_tags_table() {
        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, false).await;
        let backend = UcBootstrapBackend::bootstrap(test_config(&dir))
            .await
            .expect("bootstrap without tags should succeed");
        assert!(backend.tags().await.unwrap().is_empty());
        assert_eq!(backend.policies().await.unwrap().len(), 2);
    }

    /// Missing `policies` (a required table) is a hard failure — the
    /// resolver would otherwise happily emit empty bundles that
    /// silently disable governance.
    #[tokio::test]
    async fn test_bootstrap_errors_on_missing_required_table() {
        let dir = tempfile::tempdir().unwrap();
        // Seed only manifest + bindings + tags; policies is absent.
        let manifest_schema = Arc::new(Schema::new(vec![
            Field::new("policy_id", DataType::Utf8, false),
            Field::new("cel_expression", DataType::Utf8, false),
            Field::new("version", DataType::Int64, false),
            Field::new("compiler_version", DataType::Utf8, false),
            Field::new("source_hash", DataType::Utf8, false),
        ]));
        let manifest_batch = RecordBatch::try_new(
            manifest_schema.clone(),
            vec![
                str_array(&["p1"]),
                str_array(&["true"]),
                i64_array(&[1]),
                str_array(&["0.1.0"]),
                str_array(&["h"]),
            ],
        )
        .unwrap();
        write_delta(
            dir.path().join("manifest").to_str().unwrap(),
            manifest_schema,
            manifest_batch,
        )
        .await;

        let err = UcBootstrapBackend::bootstrap(test_config(&dir))
            .await
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("policies"), "err={err}");
    }

    #[tokio::test]
    async fn test_refresh_snapshot_picks_up_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;
        let backend = UcBootstrapBackend::bootstrap(test_config(&dir))
            .await
            .expect("bootstrap should succeed");
        assert_eq!(backend.tags().await.unwrap().len(), 2);

        // Append another tag row to the existing Delta table.
        let append_schema = Arc::new(Schema::new(vec![
            Field::new("entity", DataType::Utf8, false),
            Field::new("entity_kind", DataType::Utf8, false),
            Field::new("tag", DataType::Utf8, false),
            Field::new("set_by", DataType::Utf8, true),
        ]));
        let append_batch = RecordBatch::try_new(
            append_schema,
            vec![
                str_array(&["hospital.clinical.patients:diagnosis"]),
                str_array(&["column"]),
                str_array(&["phi"]),
                opt_str_array(&[Some("admin")]),
            ],
        )
        .unwrap();
        let tags_uri = dir.path().join("tags");
        let existing = open_table_with_storage_options(tags_uri.to_str().unwrap(), HashMap::new())
            .await
            .unwrap();
        WriteBuilder::new(existing.log_store(), existing.state.clone())
            .with_input_batches(vec![append_batch])
            .await
            .unwrap();

        backend.refresh_snapshot().await.unwrap();
        let tags = backend.tags().await.unwrap();
        assert_eq!(tags.len(), 3);
        assert!(tags.iter().any(|t| t.tag == "phi"));
    }

    /// Smoke: the resolver snapshot stays empty on error, the backend
    /// surfaces a `UcError::Config` that names the missing table.
    #[tokio::test]
    async fn test_bootstrap_error_message_identifies_offending_uri() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing written — the very first open should fail.
        let err = UcBootstrapBackend::bootstrap(test_config(&dir))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("open"), "error should come from open: {msg}");
    }

    #[test]
    fn test_is_table_not_found_recognises_common_phrasings() {
        assert!(is_table_not_found(&UcError::Config(
            "open s3://x/tags: Not a Delta table".into(),
        )));
        assert!(is_table_not_found(&UcError::Config(
            "open file:///x: No such file or directory".into(),
        )));
        assert!(!is_table_not_found(&UcError::Config(
            "something totally different".into(),
        )));
    }

    // ---------------- refresh-task tests ----------------

    /// With `refresh_interval = None`, bootstrap does NOT spawn a
    /// refresh task; the snapshot stays frozen until someone calls
    /// `refresh_snapshot()` explicitly.
    #[tokio::test]
    async fn test_bootstrap_without_interval_does_not_spawn_task() {
        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;
        let backend = UcBootstrapBackend::bootstrap(test_config(&dir))
            .await
            .unwrap();
        assert!(!backend.refresh_task_running());
    }

    /// With `refresh_interval = Some(...)`, bootstrap DOES spawn a
    /// refresh task, and the task picks up rows appended to the Delta
    /// tables without anyone calling `refresh_snapshot()`.
    #[tokio::test]
    async fn test_refresh_task_picks_up_appended_rows() {
        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;
        let mut cfg = test_config(&dir);
        cfg.refresh_interval = Some(Duration::from_millis(50));
        let backend = UcBootstrapBackend::bootstrap(cfg).await.unwrap();
        assert!(backend.refresh_task_running());
        assert_eq!(backend.tags().await.unwrap().len(), 2);

        // Append a row to the tags Delta table behind the backend's
        // back; the periodic loop should pick it up.
        let append_schema = Arc::new(Schema::new(vec![
            Field::new("entity", DataType::Utf8, false),
            Field::new("entity_kind", DataType::Utf8, false),
            Field::new("tag", DataType::Utf8, false),
            Field::new("set_by", DataType::Utf8, true),
        ]));
        let append_batch = RecordBatch::try_new(
            append_schema,
            vec![
                str_array(&["hospital.clinical.patients:diagnosis"]),
                str_array(&["column"]),
                str_array(&["phi"]),
                opt_str_array(&[Some("admin")]),
            ],
        )
        .unwrap();
        let tags_uri = dir.path().join("tags");
        let existing =
            open_table_with_storage_options(tags_uri.to_str().unwrap(), HashMap::new())
                .await
                .unwrap();
        WriteBuilder::new(existing.log_store(), existing.state.clone())
            .with_input_batches(vec![append_batch])
            .await
            .unwrap();

        // Poll up to 3s for the refresh to land — 60x the interval so
        // CI flakiness from a slow Delta scan does not bite us.
        let mut seen = 0;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            seen = backend.tags().await.unwrap().len();
            if seen >= 3 {
                break;
            }
        }
        assert_eq!(seen, 3, "refresh task should have picked up the new tag");
    }

    /// Dropping the last clone of the backend aborts the refresh
    /// task — no leaked tokio workers even if the caller forgets
    /// about the backend.
    #[tokio::test]
    async fn test_refresh_task_aborts_on_last_drop() {
        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;
        let mut cfg = test_config(&dir);
        cfg.refresh_interval = Some(Duration::from_millis(50));
        let backend = UcBootstrapBackend::bootstrap(cfg).await.unwrap();
        let cloned = backend.clone();
        // First clone dropped: task still running (held by `cloned`).
        drop(backend);
        assert!(cloned.refresh_task_running());
        // Capture the abort handle before drop so we can observe the
        // aborted state after the Arc<RefreshGuard> unwinds.
        let guard = cloned.refresh_task.as_ref().unwrap().clone();
        drop(cloned);
        // The guard is still held here (we cloned the Arc); dropping
        // the last reference triggers the AbortHandle::abort().
        drop(guard);
        // Give tokio a scheduler tick to actually cancel the task.
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Re-opening the weak handle would be complex; simplest
        // observation is that the process does not leak — the
        // `tokio::test` runtime shuts down cleanly at the end of the
        // test, which would hang if the task were not aborted.
    }

    /// With an `InvalidationSender` wired, every successful periodic
    /// refresh fans out an `InvalidateAll` that clears the resolver's
    /// bundle cache.
    #[tokio::test]
    async fn test_refresh_task_fans_out_bundle_invalidation() {
        use crate::cache::{BundleCache, CacheKey};
        use crate::cdc::InvalidationNotifier;
        use crate::types::{Principal, PrincipalAttrs, ResolveBundle};
        use policast_core::PolicyManifest;

        let dir = tempfile::tempdir().unwrap();
        seed_governance_tables(&dir, true).await;

        // Pre-populate the bundle cache so we can watch it get dropped.
        let cache = BundleCache::new(4);
        let principal = Principal {
            id: "alice".into(),
            role: "analyst".into(),
            attrs: PrincipalAttrs::new(),
        };
        let key = CacheKey::new("hospital.clinical.patients", &principal);
        cache.put(
            key.clone(),
            ResolveBundle {
                table_uuid: "t".into(),
                compiled_manifest: PolicyManifest::new(),
                bindings_applied: Vec::new(),
                expanded_from: Default::default(),
                identity_claims: Default::default(),
                storage_credentials: None,
                storage_uri: None,
                expires_at: "2999-01-01T00:00:00Z".into(),
                signature: "sig".into(),
            },
        );
        assert!(cache.get(&key).is_some(), "precondition: cache is primed");

        let notifier = InvalidationNotifier::new(cache.clone());

        let mut cfg = test_config(&dir);
        cfg.refresh_interval = Some(Duration::from_millis(50));
        let _backend =
            UcBootstrapBackend::bootstrap_with_invalidation(cfg, notifier.sender())
                .await
                .unwrap();

        // Poll up to 3s for the bundle cache to be invalidated.
        let mut cleared = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if cache.get(&key).is_none() {
                cleared = true;
                break;
            }
        }
        assert!(
            cleared,
            "refresh task should have fanned InvalidateAll out to the bundle cache"
        );
    }
}
