//! Client for the `/policies/resolve` endpoint.
//!
//! `UnityCatalogPolicyStore` implements
//! [`policast_core::policy_store::PolicyStore`] over HTTP, so engines
//! can depend on the trait and swap between `FileManifestStore` and
//! the UC-backed resolver with a one-line change.
//!
//! In addition to HTTP resolution, this module exposes the building
//! blocks (config, bundle fetch, signature verification) so the
//! `policast-datafusion` crate can drive resolution manually when it
//! needs the storage credentials from the bundle to open a Delta
//! table.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use time::OffsetDateTime;

use policast_core::policy_store::{PolicyQuery, PolicyStore, ResolvedPolicies};
use policast_core::{PolicastError, PolicyManifest};

use crate::cache::{BundleCache, CacheKey};
use crate::error::UcError;
use crate::signature::verify;
use crate::types::{Principal, PrincipalAttrs, ResolveBundle, ResolveRequest};

/// Configuration for the HTTP client.
#[derive(Clone)]
pub struct UcClientConfig {
    pub endpoint: String,
    pub signing_secret: Vec<u8>,
    pub timeout: Duration,
    pub cache_capacity: usize,
    pub cdf_invalidation: bool,
}

impl UcClientConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            signing_secret: Vec::new(),
            timeout: Duration::from_secs(5),
            cache_capacity: 256,
            cdf_invalidation: false,
        }
    }

    pub fn with_signing_secret(mut self, secret: impl Into<Vec<u8>>) -> Self {
        self.signing_secret = secret.into();
        self
    }

    /// Read the signing secret from an environment variable.
    pub fn with_signing_secret_env(mut self, var: &str) -> Result<Self, UcError> {
        let s = std::env::var(var)
            .map_err(|_| UcError::Config(format!("env var {var} not set")))?;
        self.signing_secret = s.into_bytes();
        Ok(self)
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn with_cache_capacity(mut self, n: usize) -> Self {
        self.cache_capacity = n;
        self
    }

    pub fn with_cdf_invalidation(mut self, enabled: bool) -> Self {
        self.cdf_invalidation = enabled;
        self
    }

    pub fn build(self) -> Result<UcClient, UcError> {
        if self.endpoint.is_empty() {
            return Err(UcError::Config("endpoint must not be empty".into()));
        }
        if self.signing_secret.is_empty() {
            return Err(UcError::Config("signing_secret must be set".into()));
        }
        let http = Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(UcError::Http)?;
        Ok(UcClient {
            http,
            endpoint: self.endpoint,
            secret: self.signing_secret,
            cache: BundleCache::new_checked(self.cache_capacity)?,
            cdf_invalidation: self.cdf_invalidation,
        })
    }
}

/// HTTP client to the resolver.
pub struct UcClient {
    http: Client,
    endpoint: String,
    secret: Vec<u8>,
    cache: BundleCache,
    #[allow(dead_code)] // wired up by crate::cdc
    cdf_invalidation: bool,
}

impl UcClient {
    /// Resolve a bundle, using the in-memory cache when possible.
    pub async fn resolve(&self, req: &ResolveRequest) -> Result<ResolveBundle, UcError> {
        let key = CacheKey::new(&req.table, &req.principal);
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached);
        }
        let bundle = self.fetch(req).await?;
        verify(&bundle, &self.secret)?;
        if bundle_is_expired(&bundle.expires_at) {
            return Err(UcError::Expired(bundle.expires_at));
        }
        self.cache.put(key, bundle.clone());
        Ok(bundle)
    }

    async fn fetch(&self, req: &ResolveRequest) -> Result<ResolveBundle, UcError> {
        let url = format!("{}/policies/resolve", self.endpoint.trim_end_matches('/'));
        let resp = self.http.post(&url).json(req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(UcError::Resolve(format!("{status}: {body}")));
        }
        Ok(resp.json::<ResolveBundle>().await?)
    }

    pub fn cache(&self) -> &BundleCache {
        &self.cache
    }
}

fn bundle_is_expired(expires_at: &str) -> bool {
    let Ok(exp) = OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
    else {
        return true;
    };
    OffsetDateTime::now_utc() >= exp
}

/// `PolicyStore` impl over a running resolver. Use this in engines
/// that want to depend only on `policast-core::PolicyStore`.
pub struct UnityCatalogPolicyStore {
    client: Arc<UcClient>,
}

impl UnityCatalogPolicyStore {
    pub fn new(client: Arc<UcClient>) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Arc<UcClient> {
        &self.client
    }
}

#[async_trait]
impl PolicyStore for UnityCatalogPolicyStore {
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
        let req = ResolveRequest {
            table: query.table.clone(),
            principal: Principal {
                id: query.principal_id.clone(),
                role: query.principal_role.clone(),
                attrs: PrincipalAttrs::new(),
            },
            requested_action: "query".into(),
        };
        let bundle = self
            .client
            .resolve(&req)
            .await
            .map_err(|e| PolicastError::Manifest(e.to_string()))?;
        Ok(bundle_to_resolved(bundle))
    }
}

/// Convert a `ResolveBundle` into the core's `ResolvedPolicies` shape.
pub fn bundle_to_resolved(bundle: ResolveBundle) -> ResolvedPolicies {
    ResolvedPolicies {
        manifest: PolicyManifest {
            version: bundle.compiled_manifest.version,
            policies: bundle.compiled_manifest.policies,
            principal_contract: bundle.compiled_manifest.principal_contract,
        },
        identity_claims: bundle.identity_claims,
        bindings_applied: bundle.bindings_applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_requires_endpoint() {
        let cfg = UcClientConfig::new("").with_signing_secret(b"s".to_vec());
        assert!(cfg.build().is_err());
    }

    #[test]
    fn test_config_requires_secret() {
        let cfg = UcClientConfig::new("http://localhost");
        assert!(cfg.build().is_err());
    }

    #[test]
    fn test_config_build_ok() {
        let cfg = UcClientConfig::new("http://localhost").with_signing_secret(b"s".to_vec());
        assert!(cfg.build().is_ok());
    }

    #[test]
    fn test_expired_timestamp_detected() {
        assert!(bundle_is_expired("2000-01-01T00:00:00Z"));
        assert!(!bundle_is_expired("2999-01-01T00:00:00Z"));
        assert!(bundle_is_expired("not-a-date"));
    }

    #[test]
    fn test_bundle_to_resolved_preserves_fields() {
        let b = ResolveBundle {
            table_uuid: "t".into(),
            compiled_manifest: PolicyManifest::new(),
            bindings_applied: vec!["p1".into()],
            expanded_from: Default::default(),
            identity_claims: [("region".to_string(), "us".to_string())]
                .into_iter()
                .collect(),
            storage_credentials: None,
            storage_uri: None,
            expires_at: "2999-01-01T00:00:00Z".into(),
            signature: "sig".into(),
        };
        let r = bundle_to_resolved(b);
        assert_eq!(r.bindings_applied, vec!["p1".to_string()]);
        assert_eq!(r.identity_claims["region"], "us");
    }
}
