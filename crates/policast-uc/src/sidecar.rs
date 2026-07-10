//! Axum sidecar implementing `/policies/resolve` against any
//! [`ResolveBackend`].
//!
//! This is the ship-today alternative to modifying Unity Catalog's own
//! REST surface. Clients cannot tell the difference between the
//! sidecar and an in-UC handler — the contract is identical.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json as JsonResponse, Response},
    routing::{get, post},
    Json, Router,
};

use crate::backend::ResolveBackend;
use crate::error::UcError;
use crate::store::ResolverCore;
use crate::types::{ResolveBundle, ResolveRequest};

/// Shared state for the sidecar handlers.
#[derive(Clone)]
pub struct SidecarState {
    pub core: Arc<ResolverCore>,
}

impl SidecarState {
    pub fn new(core: Arc<ResolverCore>) -> Self {
        Self { core }
    }
}

/// Build the sidecar `Router`. Wiring-only; the caller binds it.
pub fn router(state: SidecarState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/policies/resolve", post(resolve))
        .with_state(state)
}

/// Convenience constructor: build a sidecar around a file-backed store
/// at `store_root` using the given signing secret.
pub fn file_sidecar(store_root: impl AsRef<std::path::Path>, secret: impl Into<Vec<u8>>) -> Router {
    let backend: Arc<dyn ResolveBackend> = Arc::new(crate::backend::FileBackend::new(store_root));
    let core = Arc::new(ResolverCore::new(backend, secret.into()));
    router(SidecarState::new(core))
}

/// Convenience constructor: build a sidecar around a
/// [`crate::uc_bootstrap::UcBootstrapBackend`] that has already been
/// bootstrapped against the governance Delta tables.
///
/// The caller is responsible for the initial load (so that bind() can
/// fail loudly if the governance catalog is unreachable before the
/// sidecar starts accepting traffic). Once constructed, the backend's
/// own refresh task handles freshness.
#[cfg(feature = "uc-bootstrap")]
pub fn uc_bootstrap_sidecar(
    backend: crate::uc_bootstrap::UcBootstrapBackend,
    secret: impl Into<Vec<u8>>,
) -> Router {
    let backend: Arc<dyn ResolveBackend> = Arc::new(backend);
    let core = Arc::new(ResolverCore::new(backend, secret.into()));
    router(SidecarState::new(core))
}

async fn health() -> &'static str {
    "ok"
}

async fn resolve(
    State(state): State<SidecarState>,
    Json(req): Json<ResolveRequest>,
) -> Result<JsonResponse<ResolveBundle>, SidecarError> {
    let bundle = state.core.resolve(&req).await.map_err(SidecarError::from)?;
    Ok(JsonResponse(bundle))
}

/// Wire error returned by the sidecar. Wraps [`UcError`] with an HTTP
/// status code mapping.
pub struct SidecarError(UcError);

impl From<UcError> for SidecarError {
    fn from(err: UcError) -> Self {
        Self(err)
    }
}

impl IntoResponse for SidecarError {
    fn into_response(self) -> Response {
        let (status, body) = match &self.0 {
            UcError::Invalid(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            UcError::BadSignature => (StatusCode::UNAUTHORIZED, "bad signature".into()),
            UcError::Expired(ts) => (StatusCode::GONE, format!("expired at {ts}")),
            UcError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            UcError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        };
        (status, body).into_response()
    }
}

// ---------------------------------------------------------------------------
// Owner-aware policy edit API (feature = "uc-bootstrap").
//
// Three routes layered on top of the resolve sidecar:
//   GET  /policies/:id           -> current version (cedar, version, owner, cel)
//   POST /policies/:id/validate  -> compile-only dry run
//   PUT  /policies/:id           -> owner/admin-checked edit + version append
//
// On a successful edit, if a reflush target is configured, the
// governance snapshot is refreshed and re-materialized into Redis so
// the change propagates to the serving store.
// ---------------------------------------------------------------------------

#[cfg(feature = "uc-bootstrap")]
pub use editor_api::{editor_router, EditorState};

#[cfg(all(feature = "uc-bootstrap", feature = "redis"))]
pub use editor_api::Reflush;

#[cfg(feature = "uc-bootstrap")]
mod editor_api {
    use std::sync::Arc;

    use axum::{
        extract::{Path, State},
        response::Json as JsonResponse,
        routing::{get, post},
        Json, Router,
    };
    use serde::{Deserialize, Serialize};

    use super::SidecarError;
    use crate::editor::PolicyEditor;
    use crate::types::Principal;

    /// Shared state for the editor routes.
    #[derive(Clone)]
    pub struct EditorState {
        pub editor: Arc<PolicyEditor>,
        /// Optional reflush target: after a successful edit, refresh the
        /// governance snapshot and re-materialize it into Redis.
        #[cfg(feature = "redis")]
        pub reflush: Option<Reflush>,
    }

    /// Backend + materialized store used to reflush after an edit.
    #[cfg(feature = "redis")]
    #[derive(Clone)]
    pub struct Reflush {
        pub backend: crate::uc_bootstrap::UcBootstrapBackend,
        pub store: crate::redis_store::RedisManifestStore,
    }

    impl EditorState {
        pub fn new(editor: Arc<PolicyEditor>) -> Self {
            Self {
                editor,
                #[cfg(feature = "redis")]
                reflush: None,
            }
        }

        /// Wire a reflush target so edits propagate into Redis.
        #[cfg(feature = "redis")]
        pub fn with_reflush(mut self, reflush: Reflush) -> Self {
            self.reflush = Some(reflush);
            self
        }
    }

    /// Build the editor `Router` with `state` already applied, ready to
    /// merge into the resolve sidecar router.
    pub fn editor_router(state: EditorState) -> Router {
        Router::new()
            .route("/policies/:id", get(get_policy).put(put_policy))
            .route("/policies/:id/validate", post(validate_policy))
            .with_state(state)
    }

    #[derive(Serialize)]
    struct CurrentPolicyResponse {
        policy_id: String,
        version: i64,
        owner: Option<String>,
        created_by: String,
        cedar_source: String,
        cel_expression: Option<String>,
    }

    async fn get_policy(
        State(state): State<EditorState>,
        Path(id): Path<String>,
    ) -> Result<JsonResponse<CurrentPolicyResponse>, SidecarError> {
        let current = state
            .editor
            .get_current(&id)
            .await
            .map_err(SidecarError::from)?
            .ok_or_else(|| {
                SidecarError::from(crate::error::UcError::NotFound(format!("policy `{id}`")))
            })?;
        Ok(JsonResponse(CurrentPolicyResponse {
            policy_id: current.policy_id,
            version: current.version,
            owner: current.owner,
            created_by: current.created_by,
            cedar_source: current.cedar_source,
            cel_expression: current.cel_expression,
        }))
    }

    #[derive(Deserialize)]
    struct ValidateRequest {
        cedar: String,
    }

    #[derive(Serialize)]
    struct ValidateResponse {
        policy_id: String,
        owner: Option<String>,
        cel_expression: String,
    }

    async fn validate_policy(
        State(state): State<EditorState>,
        Path(_id): Path<String>,
        Json(req): Json<ValidateRequest>,
    ) -> Result<JsonResponse<ValidateResponse>, SidecarError> {
        let doc = state.editor.validate(&req.cedar).map_err(SidecarError::from)?;
        Ok(JsonResponse(ValidateResponse {
            policy_id: doc.policy.id,
            owner: doc.owner,
            cel_expression: doc.policy.cel_expression,
        }))
    }

    #[derive(Deserialize)]
    struct EditRequest {
        cedar: String,
        principal: Principal,
    }

    #[derive(Serialize)]
    struct EditResponse {
        policy_id: String,
        new_version: i64,
        owner: String,
        reflushed_tables: Option<Vec<String>>,
    }

    async fn put_policy(
        State(state): State<EditorState>,
        Path(id): Path<String>,
        Json(req): Json<EditRequest>,
    ) -> Result<JsonResponse<EditResponse>, SidecarError> {
        let outcome = state
            .editor
            .edit(&id, &req.cedar, &req.principal)
            .await
            .map_err(SidecarError::from)?;

        let reflushed = reflush_after_edit(&state).await?;

        Ok(JsonResponse(EditResponse {
            policy_id: outcome.policy_id,
            new_version: outcome.new_version,
            owner: outcome.owner,
            reflushed_tables: reflushed,
        }))
    }

    #[cfg(feature = "redis")]
    async fn reflush_after_edit(state: &EditorState) -> Result<Option<Vec<String>>, SidecarError> {
        let Some(reflush) = state.reflush.as_ref() else {
            return Ok(None);
        };
        reflush
            .backend
            .refresh_snapshot()
            .await
            .map_err(SidecarError::from)?;
        let tables = reflush
            .backend
            .materialize_into(&reflush.store)
            .await
            .map_err(SidecarError::from)?;
        Ok(Some(tables))
    }

    #[cfg(not(feature = "redis"))]
    async fn reflush_after_edit(_state: &EditorState) -> Result<Option<Vec<String>>, SidecarError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileBackend;
    use crate::signature::verify;
    use crate::types::{Principal, PrincipalAttrs};

    fn store_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/uc/store")
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let router = file_sidecar(store_root(), b"s".to_vec());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
        handle.abort();
    }

    #[tokio::test]
    async fn test_resolve_endpoint_roundtrip() {
        let router = file_sidecar(store_root(), b"s".to_vec());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new().with("region", "us-east"),
            },
            requested_action: "query".into(),
        };
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/policies/resolve"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let bundle: ResolveBundle = resp.json().await.unwrap();
        verify(&bundle, b"s").unwrap();
        assert!(!bundle.compiled_manifest.policies.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_resolve_uses_file_backend_directly() {
        // Smoke test that avoids a server - construct a backend and
        // core and call resolve in-process. The axum layer is only a
        // transport concern.
        let backend: Arc<dyn ResolveBackend> = Arc::new(FileBackend::new(store_root()));
        let core = ResolverCore::new(backend, b"s".to_vec());
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new(),
            },
            requested_action: "query".into(),
        };
        let bundle = core.resolve(&req).await.unwrap();
        assert!(!bundle.compiled_manifest.policies.is_empty());
    }

    /// Drives the uc-bootstrap constructor end-to-end through the Axum
    /// router. Mirrors `test_resolve_endpoint_roundtrip` but against a
    /// tempdir full of hand-built Delta fixtures — the same shape the
    /// production MinIO compose flow runs against.
    #[cfg(feature = "uc-bootstrap")]
    #[tokio::test]
    async fn test_uc_bootstrap_sidecar_roundtrip() {
        use crate::uc_bootstrap::{UcBootstrapBackend, UcBootstrapConfig};
        use datafusion::arrow::array::{
            builder::{Int32Builder, Int64Builder, ListBuilder, StringBuilder},
            ArrayRef,
        };
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
        use deltalake::operations::create::CreateBuilder;
        use deltalake::operations::write::WriteBuilder;
        use std::sync::Arc;

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
            let delta_schema: deltalake::kernel::Schema =
                schema.as_ref().try_into_kernel().unwrap();
            let table = CreateBuilder::new()
                .with_location(uri)
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
        }

        let dir = tempfile::tempdir().unwrap();

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
                str_array(&["row_filter_region"]),
                str_array(&["row_filter"]),
                str_array(&["hospital.clinical.patients"]),
                opt_str_array(&[None]),
                opt_str_array(&[None]),
                opt_str_array(&[None]),
                str_array(&["permit"]),
                list_str_array(&[Some(vec!["analyst"])]),
                opt_str_array(&[Some("region filter")]),
                i64_array(&[1]),
            ],
        )
        .unwrap();
        write_delta(
            dir.path().join("policies").to_str().unwrap(),
            policies_schema,
            policies_batch,
        )
        .await;

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
                str_array(&["row_filter_region"]),
                str_array(&["principal.region == resource.region"]),
                i64_array(&[1]),
                str_array(&["0.1.0"]),
                str_array(&["abc"]),
            ],
        )
        .unwrap();
        write_delta(
            dir.path().join("manifest").to_str().unwrap(),
            manifest_schema,
            manifest_batch,
        )
        .await;

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
                str_array(&["b1"]),
                str_array(&["row_filter_region"]),
                str_array(&["hospital.clinical.patients"]),
                str_array(&["*"]),
                i32_array(&[0]),
            ],
        )
        .unwrap();
        write_delta(
            dir.path().join("bindings").to_str().unwrap(),
            bindings_schema,
            bindings_batch,
        )
        .await;

        let template = format!("{}/{{table}}", dir.path().to_str().unwrap());
        let mut cfg =
            UcBootstrapConfig::for_example_stack("http://uc").with_storage_uri_template(template);
        cfg.refresh_interval = None;
        let backend = UcBootstrapBackend::bootstrap(cfg).await.unwrap();

        let router = uc_bootstrap_sidecar(backend, b"s".to_vec());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new().with("region", "us-east"),
            },
            requested_action: "query".into(),
        };
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/policies/resolve"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "resolve should succeed");
        let bundle: ResolveBundle = resp.json().await.unwrap();
        verify(&bundle, b"s").unwrap();
        assert!(
            bundle
                .bindings_applied
                .iter()
                .any(|id| id == "row_filter_region"),
            "expected row_filter_region in bindings_applied, got {:?}",
            bundle.bindings_applied
        );
        handle.abort();
    }

    /// Drives the editor routes (GET / validate / PUT) end-to-end
    /// against a tempdir of full-schema governance Delta tables.
    #[cfg(feature = "uc-bootstrap")]
    #[tokio::test]
    async fn test_editor_router_roundtrip() {
        use crate::editor::{seed_editable_tables, PolicyEditor, DEFAULT_ADMIN_ROLE};
        use crate::uc_bootstrap::UcBootstrapConfig;
        use serde_json::{json, Value};
        use std::sync::Arc;

        const V1: &str = r#"
            @id("rf")
            @filter_type("row_filter")
            @target_table("hospital.clinical.patients")
            @owner("alice@hospital.com")
            @roles("analyst")
            permit (principal, action == Action::"query", resource)
            when { resource.region == principal.region };
        "#;

        let dir = tempfile::tempdir().unwrap();
        let template = format!("{}/{{table}}", dir.path().to_str().unwrap());
        seed_editable_tables(&template, "rf", V1, Some("alice@hospital.com"), "alice@hospital.com")
            .await
            .unwrap();

        let cfg =
            UcBootstrapConfig::for_example_stack("http://uc").with_storage_uri_template(&template);
        let editor = Arc::new(PolicyEditor::new(cfg, DEFAULT_ADMIN_ROLE));
        let router = editor_router(EditorState::new(editor));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();

        // GET current version.
        let resp = client
            .get(format!("http://{addr}/policies/rf"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["version"], 1);
        assert_eq!(body["owner"], "alice@hospital.com");

        // Validate a proposed edit (compile-only).
        let v2 = V1.replace(
            "when { resource.region == principal.region };",
            "when { resource.region == principal.region }\nunless { principal.role == \"auditor\" };",
        );
        let resp = client
            .post(format!("http://{addr}/policies/rf/validate"))
            .json(&json!({ "cedar": v2 }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // PUT edit as a non-owner -> 403.
        let resp = client
            .put(format!("http://{addr}/policies/rf"))
            .json(&json!({
                "cedar": v2,
                "principal": { "id": "mallory@evil.com", "role": "analyst" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);

        // PUT edit as the owner -> 200, version bumps to 2.
        let resp = client
            .put(format!("http://{addr}/policies/rf"))
            .json(&json!({
                "cedar": v2,
                "principal": { "id": "alice@hospital.com", "role": "analyst" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["new_version"], 2);

        // GET now returns version 2.
        let resp = client
            .get(format!("http://{addr}/policies/rf"))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["version"], 2);

        // Unknown policy -> 404.
        let resp = client
            .get(format!("http://{addr}/policies/ghost"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        handle.abort();
    }
}
