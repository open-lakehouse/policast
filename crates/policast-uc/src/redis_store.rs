//! Redis-backed materialized PolicyStore.
//!
//! Unity Catalog's `governance.policast.*` Delta tables remain the
//! source of truth. This module is the **materialized serving store**:
//! a [`materialize`](RedisManifestStore::materialize) step compiles the
//! governance state into one [`PolicyManifest`] per concrete table and
//! writes each as **binary protobuf** under the `policast:v2:manifest:`
//! keyspace. Serving a query is then a single Redis `GET` + protobuf
//! decode + role filter, with no Delta scan on the hot path.
//!
//! JSON stays the canonical/signed wire format for the resolver and
//! engines; protobuf is used purely as the compact at-rest encoding
//! here (see [`policast_core::proto`]).
//!
//! Gated behind the `redis` Cargo feature.

use async_trait::async_trait;
use redis::AsyncCommands;

use policast_core::policy_store::{PolicyQuery, PolicyStore, ResolvedPolicies};
use policast_core::{PolicastError, PolicyManifest};

use crate::error::UcError;
use crate::store::ResolverCore;

/// Default key prefix for materialized per-table manifests. The `v2`
/// marks the binary-protobuf era and keeps it disjoint from the JSON
/// `policast:v1:resolved:*` cache in `policast-core`.
pub const DEFAULT_KEY_PREFIX: &str = "policast:v2:manifest:";

/// A materialized PolicyStore backed by Redis.
///
/// Cheap to clone (the connection manager is internally `Arc`-like and
/// reconnects transparently).
#[derive(Clone)]
pub struct RedisManifestStore {
    conn: redis::aio::ConnectionManager,
    key_prefix: String,
}

impl RedisManifestStore {
    /// Connect to Redis at `url` (e.g. `redis://127.0.0.1:6379`).
    pub async fn connect(url: &str) -> Result<Self, UcError> {
        let client = redis::Client::open(url)
            .map_err(|e| UcError::Config(format!("redis client open: {e}")))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| UcError::Config(format!("redis connect: {e}")))?;
        Ok(Self::from_connection_manager(conn))
    }

    /// Build from an existing connection manager so one Redis pool can
    /// be shared across stores/caches.
    pub fn from_connection_manager(conn: redis::aio::ConnectionManager) -> Self {
        Self {
            conn,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
        }
    }

    /// Override the key prefix (e.g. for multi-tenant Redis).
    pub fn with_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = prefix.into();
        self
    }

    fn key(&self, table: &str) -> String {
        format!("{}{}", self.key_prefix, table)
    }

    /// Store the compiled manifest for `table` as binary protobuf.
    pub async fn put_manifest(
        &self,
        table: &str,
        manifest: &PolicyManifest,
    ) -> Result<(), UcError> {
        let bytes = manifest.to_protobuf();
        let mut conn = self.conn.clone();
        let _: () = conn
            .set(self.key(table), bytes)
            .await
            .map_err(|e| UcError::Config(format!("redis set {table}: {e}")))?;
        Ok(())
    }

    /// Fetch and decode the compiled manifest for `table`, if present.
    pub async fn get_manifest(&self, table: &str) -> Result<Option<PolicyManifest>, UcError> {
        let mut conn = self.conn.clone();
        let raw: Option<Vec<u8>> = conn
            .get(self.key(table))
            .await
            .map_err(|e| UcError::Config(format!("redis get {table}: {e}")))?;
        match raw {
            Some(bytes) => {
                let manifest = PolicyManifest::from_protobuf(&bytes).map_err(|_| {
                    UcError::Invalid(format!("invalid Redis policy manifest for {table}"))
                })?;
                Ok(Some(manifest))
            }
            None => Ok(None),
        }
    }

    /// Recompile every concrete table's manifest from `core`'s backend
    /// and write them to Redis. Returns the list of tables written.
    ///
    /// This is the "flush UC updates into the PolicyStore" step: call it
    /// after the governance snapshot refreshes (see the bootstrap
    /// refresh loop) so a policy edit in UC propagates to the serving
    /// store.
    pub async fn materialize(&self, core: &ResolverCore) -> Result<Vec<String>, UcError> {
        let tables = core.materialize_tables().await?;
        let mut written = Vec::with_capacity(tables.len());
        for (table, manifest) in tables {
            self.put_manifest(&table, &manifest).await?;
            written.push(table);
        }
        Ok(written)
    }
}

#[async_trait]
impl PolicyStore for RedisManifestStore {
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
        let mut manifest = self
            .get_manifest(&query.table)
            .await
            .map_err(|e| match e {
                UcError::Invalid(_) => PolicastError::Manifest(e.to_string()),
                _ => PolicastError::Cache(e.to_string()),
            })?
            .ok_or(PolicastError::CacheMiss)?;
        manifest.record_grant_targets();
        let mut matching = Vec::new();
        for p in manifest.policies {
            if p.applies_to_identity(&query.principal_role, &query.principal_id, None)? {
                matching.push(p);
            }
        }
        manifest.policies = matching;
        let bindings_applied = manifest.policies.iter().map(|p| p.id.clone()).collect();

        Ok(ResolvedPolicies {
            manifest,
            identity_claims: Default::default(),
            bindings_applied,
        })
    }
}

/// A "Redis first, fall back to inner" PolicyStore decorator.
///
/// On `resolve` it tries the materialized Redis store; on a miss (no
/// materialized entry for the table) or a Redis error it delegates to
/// `inner` (e.g. an in-memory/file store), so a cold or unavailable
/// Redis never takes governance offline.
pub struct RedisFirstPolicyStore<S: PolicyStore> {
    redis: RedisManifestStore,
    inner: S,
}

impl<S: PolicyStore> RedisFirstPolicyStore<S> {
    pub fn new(redis: RedisManifestStore, inner: S) -> Self {
        Self { redis, inner }
    }
}

#[async_trait]
impl<S: PolicyStore> PolicyStore for RedisFirstPolicyStore<S> {
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
        match self.redis.resolve(query).await {
            Ok(resolved) => Ok(resolved),
            // Miss or Redis hiccup: fall back to the inner store.
            Err(
                PolicastError::Cache(_)
                | PolicastError::CacheMiss
                | PolicastError::ResolutionRequired(_),
            ) => self.inner.resolve(query).await,
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileBackend;

    fn examples_store_core() -> ResolverCore {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/uc/store");
        ResolverCore::new(std::sync::Arc::new(FileBackend::new(root)), b"s".to_vec())
    }

    /// `materialize_tables` should produce a non-empty per-table
    /// manifest for the shipped patients table, fully tag-expanded.
    #[tokio::test]
    async fn test_materialize_tables_expands_patients() {
        let core = examples_store_core();
        let tables = core.materialize_tables().await.unwrap();
        let (_, manifest) = tables
            .iter()
            .find(|(t, _)| t == "hospital.clinical.patients")
            .expect("patients table should materialize");
        let ids: Vec<&str> = manifest.policies.iter().map(|p| p.id.as_str()).collect();
        // Tag-expanded ids carry the (table[:column]) coordinates.
        assert!(ids
            .iter()
            .any(|id| id.starts_with("column_mask_by_pii_tag@hospital.clinical.patients:ssn")));
        assert!(ids
            .iter()
            .any(|id| id.starts_with("row_filter_region@hospital.clinical.patients")));
    }

    /// The protobuf bytes a materialized manifest encodes to must decode
    /// back to the same manifest (the Redis at-rest format).
    #[tokio::test]
    async fn test_materialized_manifest_protobuf_roundtrips() {
        let core = examples_store_core();
        let tables = core.materialize_tables().await.unwrap();
        for (_, manifest) in &tables {
            let bytes = manifest.to_protobuf();
            let back = PolicyManifest::from_protobuf(&bytes).unwrap();
            assert_eq!(manifest, &back);
        }
    }

    fn redis_url() -> Option<String> {
        std::env::var("POLICAST_TEST_REDIS_URL").ok()
    }

    /// Live Redis round-trip + materialize + serve. Skipped unless
    /// `POLICAST_TEST_REDIS_URL` is set so `cargo test` stays hermetic.
    #[tokio::test]
    async fn test_redis_materialize_and_serve() {
        let Some(url) = redis_url() else {
            eprintln!("skipping: POLICAST_TEST_REDIS_URL not set");
            return;
        };
        let store = RedisManifestStore::connect(&url)
            .await
            .expect("connect")
            .with_key_prefix("policast-test:v2:manifest:");
        let core = examples_store_core();

        let written = store.materialize(&core).await.expect("materialize");
        assert!(written.contains(&"hospital.clinical.patients".to_string()));

        let q = PolicyQuery {
            table: "hospital.clinical.patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.expect("serve from redis");
        assert!(!resolved.manifest.policies.is_empty());

        // A miss falls back through RedisFirstPolicyStore to the inner
        // file store.
        let inner = policast_core::FileManifestStore::from_manifest(PolicyManifest::new());
        let decorated = RedisFirstPolicyStore::new(store.clone(), inner);
        let miss_q = PolicyQuery {
            table: "does.not.exist".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        // Inner returns an empty manifest rather than erroring.
        let resolved = decorated.resolve(&miss_q).await.expect("fallback");
        assert!(resolved.manifest.policies.is_empty());
    }
}
