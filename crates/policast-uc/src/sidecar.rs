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
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        };
        (status, body).into_response()
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
}
