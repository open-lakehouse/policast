//! Authenticated Unity Catalog REST client.
//!
//! A thin, typed wrapper over the subset of the Unity Catalog REST API
//! that policast-uc needs: reading table metadata
//! (`GET /api/2.1/unity-catalog/tables/{full_name}`) and vending
//! temporary table credentials
//! (`POST /api/2.1/unity-catalog/temporary-table-credentials`).
//!
//! The shape mirrors the Go console's `ucclient` (see
//! `unitycatalog-console/ui/server/internal/ucclient/ucclient.go`): a
//! single shared HTTP client, the catalog base path
//! `/api/2.1/unity-catalog`, an `Authorization: Bearer <token>` header
//! that is only attached when a token is present, and a ~60s timeout.
//!
//! ## Auth toggle
//!
//! [`AuthMode`] is the `auth=true|false` switch the service config
//! exposes:
//!
//! * [`AuthMode::Disabled`] — no `Authorization` header. Use this
//!   against a local docker UC running with `server.authorization=disable`.
//! * [`AuthMode::Bearer`] — a static bearer token attached to every
//!   request. Use this against the hosted UC
//!   (`https://uc.openlakehousedemos.dev`) running with auth enabled.
//!
//! OAuth token-exchange (`POST /api/1.0/unity-control/auth/tokens`,
//! RFC 8693) is intentionally out of scope for phase one; a static
//! token is enough for both the demo and CI.

use std::collections::HashMap;

use reqwest::StatusCode;
use serde::Deserialize;

use crate::error::UcError;

/// Unity Catalog REST prefix (catalog API).
pub const CATALOG_BASE: &str = "/api/2.1/unity-catalog";

/// Whether to authenticate UC REST calls, and with what.
///
/// `Disabled` is the local-docker path (`auth=false`); `Bearer` is the
/// hosted path (`auth=true`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AuthMode {
    /// Send no `Authorization` header (UC `server.authorization=disable`).
    #[default]
    Disabled,
    /// Send `Authorization: Bearer <token>` on every request.
    Bearer(String),
}

impl AuthMode {
    /// Build an `AuthMode` from an `auth_enabled` flag and an optional
    /// token. When auth is enabled the token must be present and
    /// non-empty, otherwise this is a configuration error.
    pub fn from_flag(auth_enabled: bool, token: Option<String>) -> Result<Self, UcError> {
        if !auth_enabled {
            return Ok(AuthMode::Disabled);
        }
        match token {
            Some(t) if !t.is_empty() => Ok(AuthMode::Bearer(t)),
            _ => Err(UcError::Config(
                "auth is enabled but no UC bearer token was provided".into(),
            )),
        }
    }

    /// True when a bearer token will be sent.
    pub fn is_enabled(&self) -> bool {
        matches!(self, AuthMode::Bearer(_))
    }
}

/// The read/write intent passed to the credential-vending endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableOperation {
    /// Read-only access (`SELECT`).
    Read,
    /// Read-write access (`SELECT` + `UPDATE`); needed for the policy
    /// edit write-back path.
    ReadWrite,
}

impl TableOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            TableOperation::Read => "READ",
            TableOperation::ReadWrite => "READ_WRITE",
        }
    }
}

/// Subset of UC's `TableInfo` that we consume. `storage_location` is
/// the canonical field; `location` is accepted as a fallback for the
/// Delta-native load response shape.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UcTableInfo {
    #[serde(default)]
    pub table_id: Option<String>,
    #[serde(default)]
    pub storage_location: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
}

impl UcTableInfo {
    /// The canonical storage URI, preferring `storage_location` then
    /// `location`.
    pub fn storage_uri(&self) -> Option<&str> {
        self.storage_location
            .as_deref()
            .or(self.location.as_deref())
    }
}

/// Subset of UC's `TemporaryCredentials` response. Only the AWS shape
/// and the generic `credentials` map are consumed today.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct UcTempCredsResponse {
    #[serde(default)]
    pub aws_temp_credentials: Option<UcAwsTempCreds>,
    #[serde(default)]
    pub credentials: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct UcAwsTempCreds {
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct UcTempCredsRequest<'a> {
    table_id: Option<&'a str>,
    table_name: &'a str,
    operation: &'a str,
}

/// Result of a credential-vending attempt. `Unsupported` is returned
/// when the UC build does not expose the endpoint (404/405); callers
/// then fall back to whatever static storage options they already hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendOutcome {
    Vended(UcTempCredsResponse),
    Unsupported,
}

/// Typed, authenticated Unity Catalog REST client.
#[derive(Debug, Clone)]
pub struct UcRestClient {
    base_url: String,
    auth: AuthMode,
    http: reqwest::Client,
}

impl UcRestClient {
    /// Build a client for `base_url` (e.g. `https://uc.openlakehousedemos.dev`
    /// or `http://unitycatalog:8080`) with the given auth mode.
    pub fn new(base_url: impl Into<String>, auth: AuthMode) -> Self {
        Self::with_http(base_url, auth, default_http_client())
    }

    /// Build a client over a caller-supplied `reqwest::Client` (useful
    /// for sharing one connection pool, or for tests).
    pub fn with_http(base_url: impl Into<String>, auth: AuthMode, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
            http,
        }
    }

    /// The base URL this client was configured with (trailing slash
    /// stripped).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether this client authenticates its requests.
    pub fn auth(&self) -> &AuthMode {
        &self.auth
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            AuthMode::Bearer(token) if !token.is_empty() => req.bearer_auth(token),
            _ => req,
        }
    }

    /// `GET /api/2.1/unity-catalog/tables/{full_name}` — fetch table
    /// metadata (storage location + table id).
    pub async fn get_table(&self, full_name: &str) -> Result<UcTableInfo, UcError> {
        let url = format!("{}{CATALOG_BASE}/tables/{full_name}", self.base_url);
        let resp = self
            .apply_auth(self.http.get(&url))
            .send()
            .await
            .map_err(|e| UcError::Config(format!("uc table lookup {full_name}: {e}")))?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(UcError::Config(format!(
                "uc table lookup {full_name} failed ({code}): {body}"
            )));
        }
        resp.json::<UcTableInfo>()
            .await
            .map_err(|e| UcError::Config(format!("uc table lookup decode {full_name}: {e}")))
    }

    /// `POST /api/2.1/unity-catalog/temporary-table-credentials` — vend
    /// short-lived storage credentials for `operation`.
    ///
    /// A 404/405 (older UC build without the endpoint) is mapped to
    /// [`VendOutcome::Unsupported`] rather than an error so the read
    /// path can fall back to static storage options. All other non-2xx
    /// statuses are hard errors.
    pub async fn vend_table_credentials(
        &self,
        table_id: Option<&str>,
        full_name: &str,
        operation: TableOperation,
    ) -> Result<VendOutcome, UcError> {
        let url = format!(
            "{}{CATALOG_BASE}/temporary-table-credentials",
            self.base_url
        );
        let body = UcTempCredsRequest {
            table_id,
            table_name: full_name,
            operation: operation.as_str(),
        };
        let resp = self
            .apply_auth(self.http.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| UcError::Config(format!("uc temporary creds request {full_name}: {e}")))?;
        match resp.status() {
            s if s.is_success() => {
                let payload: UcTempCredsResponse = resp.json().await.map_err(|e| {
                    UcError::Config(format!("uc temporary creds decode {full_name}: {e}"))
                })?;
                Ok(VendOutcome::Vended(payload))
            }
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => Ok(VendOutcome::Unsupported),
            other => {
                let body = resp.text().await.unwrap_or_default();
                Err(UcError::Config(format!(
                    "uc temporary creds {full_name} failed ({other}): {body}"
                )))
            }
        }
    }

    /// Resolve a table's storage URI and the storage options needed to
    /// open it for `operation`, starting from `base_storage_options`
    /// (e.g. a static region/endpoint). Vended AWS credentials are
    /// merged on top when the endpoint is available.
    pub async fn resolve_table_access(
        &self,
        full_name: &str,
        operation: TableOperation,
        base_storage_options: &HashMap<String, String>,
    ) -> Result<ResolvedTableAccess, UcError> {
        let table = self.get_table(full_name).await?;
        let uri = table.storage_uri().map(str::to_string).ok_or_else(|| {
            UcError::Config(format!(
                "uc table lookup {full_name}: missing storage_location/location"
            ))
        })?;

        let mut storage_options = base_storage_options.clone();
        if let VendOutcome::Vended(payload) = self
            .vend_table_credentials(table.table_id.as_deref(), full_name, operation)
            .await?
        {
            inject_vended_storage_options(&mut storage_options, payload);
        }

        Ok(ResolvedTableAccess {
            uri,
            table_id: table.table_id,
            storage_options,
        })
    }
}

/// A table's resolved storage URI + the options to open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTableAccess {
    pub uri: String,
    pub table_id: Option<String>,
    pub storage_options: HashMap<String, String>,
}

/// Merge UC-vended credentials into a storage-options map (the keys
/// delta-rs / object_store expects). Existing static options (region,
/// endpoint, ...) are preserved unless overwritten by a vended value.
pub fn inject_vended_storage_options(
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

fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_mode_from_flag() {
        assert_eq!(
            AuthMode::from_flag(false, None).unwrap(),
            AuthMode::Disabled
        );
        assert_eq!(
            AuthMode::from_flag(false, Some("tok".into())).unwrap(),
            AuthMode::Disabled
        );
        assert_eq!(
            AuthMode::from_flag(true, Some("tok".into())).unwrap(),
            AuthMode::Bearer("tok".into())
        );
        assert!(AuthMode::from_flag(true, None).is_err());
        assert!(AuthMode::from_flag(true, Some(String::new())).is_err());
    }

    #[test]
    fn test_auth_mode_is_enabled() {
        assert!(!AuthMode::Disabled.is_enabled());
        assert!(AuthMode::Bearer("t".into()).is_enabled());
    }

    #[test]
    fn test_table_operation_as_str() {
        assert_eq!(TableOperation::Read.as_str(), "READ");
        assert_eq!(TableOperation::ReadWrite.as_str(), "READ_WRITE");
    }

    #[test]
    fn test_base_url_trailing_slash_stripped() {
        let c = UcRestClient::new("http://uc:8080/", AuthMode::Disabled);
        assert_eq!(c.base_url(), "http://uc:8080");
    }

    #[test]
    fn test_table_info_storage_uri_prefers_storage_location() {
        let info = UcTableInfo {
            table_id: Some("id".into()),
            storage_location: Some("s3://b/a".into()),
            location: Some("s3://b/fallback".into()),
        };
        assert_eq!(info.storage_uri(), Some("s3://b/a"));

        let only_location = UcTableInfo {
            table_id: None,
            storage_location: None,
            location: Some("s3://b/fallback".into()),
        };
        assert_eq!(only_location.storage_uri(), Some("s3://b/fallback"));

        let none = UcTableInfo {
            table_id: None,
            storage_location: None,
            location: None,
        };
        assert_eq!(none.storage_uri(), None);
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

    #[test]
    fn test_inject_vended_storage_options_merges_generic_credentials() {
        let mut out = HashMap::new();
        inject_vended_storage_options(
            &mut out,
            UcTempCredsResponse {
                aws_temp_credentials: None,
                credentials: Some(HashMap::from([(
                    "s3.access-key-id".to_string(),
                    "k".to_string(),
                )])),
            },
        );
        assert_eq!(out.get("s3.access-key-id").map(String::as_str), Some("k"));
    }

    /// A one-shot HTTP server that captures the first request's head and
    /// replies with `body`. Used to assert auth-header behavior without
    /// pulling in a mock framework.
    async fn spawn_capture_server(
        body: &str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            req
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn test_get_table_sends_bearer_when_enabled() {
        let (addr, captured) =
            spawn_capture_server(r#"{"table_id":"id","storage_location":"s3://b/x"}"#).await;
        let client = UcRestClient::new(format!("http://{addr}"), AuthMode::Bearer("tok123".into()));
        let info = client.get_table("c.s.t").await.unwrap();
        assert_eq!(info.table_id.as_deref(), Some("id"));
        assert_eq!(info.storage_uri(), Some("s3://b/x"));
        let req = captured.await.unwrap().to_lowercase();
        assert!(
            req.contains("authorization: bearer tok123"),
            "expected bearer header, got request head:\n{req}"
        );
    }

    #[tokio::test]
    async fn test_get_table_omits_auth_when_disabled() {
        let (addr, captured) =
            spawn_capture_server(r#"{"table_id":"id","storage_location":"s3://b/x"}"#).await;
        let client = UcRestClient::new(format!("http://{addr}"), AuthMode::Disabled);
        client.get_table("c.s.t").await.unwrap();
        let req = captured.await.unwrap().to_lowercase();
        assert!(
            !req.contains("authorization"),
            "expected no Authorization header, got request head:\n{req}"
        );
    }

    #[tokio::test]
    async fn test_vend_credentials_unsupported_maps_to_outcome() {
        // A server that 404s the vend endpoint -> VendOutcome::Unsupported.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        });
        let client = UcRestClient::new(format!("http://{addr}"), AuthMode::Disabled);
        let outcome = client
            .vend_table_credentials(Some("id"), "c.s.t", TableOperation::Read)
            .await
            .unwrap();
        assert_eq!(outcome, VendOutcome::Unsupported);
        handle.await.unwrap();
    }
}
