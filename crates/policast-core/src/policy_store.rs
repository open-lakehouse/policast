//! A pluggable store for compiled policies.
//!
//! Before policast-cel started targeting Unity Catalog, engines read a
//! `PolicyManifest` from a JSON file on disk. With a catalog-native
//! store (see `research/unity-catalog-policy-store.md`) the resolution
//! path needs to answer more than "give me the manifest" — it needs to
//! know which policies apply to a given principal/table pair, and it
//! needs to do so via an async call that talks over the network.
//!
//! The `PolicyStore` trait is that abstraction. Two impls live here:
//!
//! - [`FileManifestStore`] — the current behavior, reading a JSON
//!   manifest file and applying every policy in it. Useful for offline
//!   development and as the default in unit tests.
//! - A separate `UnityCatalogPolicyStore` lives in the `policast-uc`
//!   crate; it implements the same trait against a REST resolver.
//!
//! Engines depend on the trait, not on a concrete impl, so swapping
//! stores is a one-line change.
//!
//! ```no_run
//! use policast_core::policy_store::{PolicyStore, FileManifestStore, PolicyQuery};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let store = FileManifestStore::from_path("examples/policies/manifest.json")?;
//! let query = PolicyQuery {
//!     table: "patients".into(),
//!     principal_id: "alice".into(),
//!     principal_role: "analyst".into(),
//! };
//! let resolved = store.resolve(&query).await?;
//! assert!(!resolved.manifest.policies.is_empty());
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::PolicastError;
use crate::model::CompiledPolicy;
use crate::policy_manifest::PolicyManifest;

/// Inputs to a policy resolution: which table is being queried and by
/// whom.
///
/// Kept intentionally small — attributes beyond role live on the
/// resolver side (UC), and the resolver returns them as
/// `ResolvedPolicies::identity_claims`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyQuery {
    /// The table being scanned. Either a short table name (for the
    /// file-based store) or a three-part UC name.
    pub table: String,
    /// The querying principal's identifier (e.g. email, username).
    pub principal_id: String,
    /// The principal's role, used by `applies_to` filtering and by CEL
    /// `principal.role` references.
    pub principal_role: String,
}

/// The result of a policy resolution: the compiled manifest that applies
/// to this query, plus any identity claims the resolver supplied
/// (region, groups, etc.) beyond what the caller already knew.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPolicies {
    /// The subset of the full manifest that applies to this
    /// `(table, principal)` pair.
    pub manifest: PolicyManifest,
    /// Extra principal attributes resolved by the store (e.g.
    /// `region`, `groups`) that the engine should feed into CEL
    /// evaluation.
    #[serde(default)]
    pub identity_claims: std::collections::BTreeMap<String, String>,
    /// The policy ids that were actually bound to this table for this
    /// principal. Useful for logging/auditing.
    #[serde(default)]
    pub bindings_applied: Vec<String>,
}

/// Async pluggable store for compiled policies.
///
/// See module docs for the two built-in impls.
#[async_trait]
pub trait PolicyStore: Send + Sync {
    /// Resolve the policies that apply to `query.table` for
    /// `query.principal_*`.
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError>;
}

/// File-backed `PolicyStore` that reads a full `PolicyManifest` from
/// disk once at construction and filters it per-query.
///
/// This preserves the pre-UC behavior exactly: every policy whose
/// `target_table` matches (including wildcard) or whose `applies_to`
/// role matches is returned.
pub struct FileManifestStore {
    manifest: PolicyManifest,
}

impl FileManifestStore {
    /// Read and parse a manifest JSON file.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, PolicastError> {
        let text = std::fs::read_to_string(path)?;
        let manifest = PolicyManifest::from_json(&text)?;
        Ok(Self { manifest })
    }

    /// Construct directly from an in-memory manifest (useful for
    /// tests).
    pub fn from_manifest(manifest: PolicyManifest) -> Self {
        Self { manifest }
    }

    /// Borrow the underlying manifest.
    pub fn manifest(&self) -> &PolicyManifest {
        &self.manifest
    }
}

#[async_trait]
impl PolicyStore for FileManifestStore {
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
        let matching: Vec<&CompiledPolicy> = self
            .manifest
            .policies
            .iter()
            .filter(|p| {
                table_matches(&p.target_table, &query.table)
                    && role_applies(p, &query.principal_role)
            })
            .collect();

        let bindings_applied: Vec<String> = matching.iter().map(|p| p.id.clone()).collect();

        let manifest = PolicyManifest {
            version: self.manifest.version.clone(),
            policies: matching.into_iter().cloned().collect(),
            principal_contract: self.manifest.principal_contract.clone(),
        };

        Ok(ResolvedPolicies {
            manifest,
            identity_claims: Default::default(),
            bindings_applied,
        })
    }
}

/// Match a policy's `target_table` against the query's table name.
///
/// Supports `*` (any table), `a.b.*` (any table in schema `a.b`), and
/// exact matches. Also accepts a bare table name matching the last
/// segment of a three-part name — this keeps the POC's short names
/// (`patients`) working alongside UC's `catalog.schema.table`.
pub fn table_matches(pattern: &str, table: &str) -> bool {
    if pattern == "*" || pattern == table {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        if let Some(dot) = table.rfind('.') {
            return &table[..dot] == prefix;
        }
    }
    // Tolerate three-part vs bare-name mismatches in either direction
    // so the existing file-manifest demos keep working when pointed at
    // UC-shaped target_table values.
    if let Some(dot) = pattern.rfind('.') {
        if &pattern[dot + 1..] == table {
            return true;
        }
    }
    if let Some(dot) = table.rfind('.') {
        if &table[dot + 1..] == pattern {
            return true;
        }
    }
    false
}

/// Returns true if the policy's `applies_to` (if any) includes the
/// principal's role. A missing `applies_to` means the policy applies to
/// everyone.
fn role_applies(policy: &CompiledPolicy, role: &str) -> bool {
    match &policy.applies_to {
        None => true,
        Some(a) => a.roles.is_empty() || a.roles.iter().any(|r| r == role),
    }
}

// ---------------------------------------------------------------------------
// Caching layer
// ---------------------------------------------------------------------------
//
// Policy resolution is read-heavy and, for a network-backed store like
// the UC resolver, latency-sensitive on the query hot path. The types
// below add an optional cache-aside layer in front of *any*
// `PolicyStore`:
//
// - [`ResolvedCache`] is the pluggable cache backend trait.
// - [`InMemoryCache`] is a dependency-free, process-local impl (good
//   for tests and single-process engines).
// - `RedisCache` (behind the `redis` Cargo feature) is a shared,
//   persistent cache across processes — see the bottom of this file.
// - [`CachedPolicyStore`] is the decorator that wires an inner store
//   to a cache.
//
// The Redis key scheme ([`resolved_cache_key`]) and the `ResolvedCache`
// abstraction are intentionally independent of the decorator so a future
// source-of-truth `RedisPolicyStore` can reuse the same keyspace and
// connection handling.

/// How a [`CachedPolicyStore`] reacts when its cache backend errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheFailMode {
    /// Treat cache errors as a miss and fall back to the inner store;
    /// failed writes are ignored. Keeps queries serving when the cache
    /// (e.g. Redis) is briefly unavailable. This is the default.
    #[default]
    FallBackToInner,
    /// Surface cache errors as [`PolicastError::Cache`]. Use when a
    /// degraded cache should fail the request rather than silently fall
    /// through to the backing store.
    Strict,
}

/// Pluggable cache backend for [`ResolvedPolicies`].
///
/// Implementations are keyed by the opaque string produced by
/// [`resolved_cache_key`]. They must be cheap to clone/share (the
/// decorator holds one) and safe to call concurrently.
#[async_trait]
pub trait ResolvedCache: Send + Sync {
    /// Fetch a cached entry. Returns `Ok(None)` on a miss (including an
    /// entry that has expired).
    async fn get(&self, key: &str) -> Result<Option<ResolvedPolicies>, PolicastError>;

    /// Store `value` under `key` with a time-to-live of `ttl`.
    async fn put(
        &self,
        key: &str,
        value: &ResolvedPolicies,
        ttl: Duration,
    ) -> Result<(), PolicastError>;
}

/// Build the cache key for a query. Stable and human-readable so the
/// keyspace can be inspected directly (e.g. `KEYS policast:v1:*` in
/// Redis). Components are sanitized so the `:` delimiter is unambiguous.
pub fn resolved_cache_key(query: &PolicyQuery) -> String {
    format!(
        "policast:v1:resolved:{}:{}:{}",
        sanitize_key_part(&query.table),
        sanitize_key_part(&query.principal_role),
        sanitize_key_part(&query.principal_id),
    )
}

/// Replace characters that would break the `:`-delimited key layout.
fn sanitize_key_part(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' | ' ' | '\t' | '\r' | '\n' => '_',
            other => other,
        })
        .collect()
}

/// Process-local [`ResolvedCache`] backed by a `Mutex<HashMap>` with
/// per-entry expiry. Always available (no extra dependencies); useful as
/// a lightweight default and in tests. Expired entries are evicted
/// lazily on read.
#[derive(Clone, Default)]
pub struct InMemoryCache {
    inner: Arc<Mutex<HashMap<String, (Instant, ResolvedPolicies)>>>,
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of (not-yet-evicted) entries currently held.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("InMemoryCache mutex poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ResolvedCache for InMemoryCache {
    async fn get(&self, key: &str) -> Result<Option<ResolvedPolicies>, PolicastError> {
        let mut guard = self.inner.lock().expect("InMemoryCache mutex poisoned");
        match guard.get(key) {
            Some((expires_at, _)) if Instant::now() >= *expires_at => {
                guard.remove(key);
                Ok(None)
            }
            Some((_, value)) => Ok(Some(value.clone())),
            None => Ok(None),
        }
    }

    async fn put(
        &self,
        key: &str,
        value: &ResolvedPolicies,
        ttl: Duration,
    ) -> Result<(), PolicastError> {
        let expires_at = Instant::now() + ttl;
        self.inner
            .lock()
            .expect("InMemoryCache mutex poisoned")
            .insert(key.to_string(), (expires_at, value.clone()));
        Ok(())
    }
}

/// A cache-aside [`PolicyStore`] decorator.
///
/// On `resolve` it checks `cache` first; on a miss it calls the wrapped
/// `inner` store and writes the result back with `ttl`. `fail_mode`
/// controls behavior when the cache backend errors (see
/// [`CacheFailMode`]).
///
/// Pair it with [`InMemoryCache`] for a process-local cache or with
/// `RedisCache` (the `redis` feature) for a shared cache across
/// processes:
///
/// ```no_run
/// # use std::time::Duration;
/// # use policast_core::policy_store::{CachedPolicyStore, FileManifestStore, InMemoryCache};
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let inner = FileManifestStore::from_path("examples/policies/manifest.json")?;
/// let cached = CachedPolicyStore::new(inner, InMemoryCache::new(), Duration::from_secs(60));
/// # let _ = cached;
/// # Ok(())
/// # }
/// ```
pub struct CachedPolicyStore<S: PolicyStore, C: ResolvedCache> {
    inner: S,
    cache: C,
    ttl: Duration,
    fail_mode: CacheFailMode,
}

impl<S: PolicyStore, C: ResolvedCache> CachedPolicyStore<S, C> {
    /// Wrap `inner` with `cache`, caching entries for `ttl`. Defaults to
    /// [`CacheFailMode::FallBackToInner`].
    pub fn new(inner: S, cache: C, ttl: Duration) -> Self {
        Self {
            inner,
            cache,
            ttl,
            fail_mode: CacheFailMode::default(),
        }
    }

    /// Override the cache failure behavior.
    pub fn with_fail_mode(mut self, fail_mode: CacheFailMode) -> Self {
        self.fail_mode = fail_mode;
        self
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    pub fn cache(&self) -> &C {
        &self.cache
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Apply [`CacheFailMode`] to a cache error: swallow it (fall back)
    /// or propagate it (strict).
    fn on_cache_error(&self, err: PolicastError) -> Result<(), PolicastError> {
        match self.fail_mode {
            CacheFailMode::FallBackToInner => Ok(()),
            CacheFailMode::Strict => Err(err),
        }
    }
}

#[async_trait]
impl<S: PolicyStore, C: ResolvedCache> PolicyStore for CachedPolicyStore<S, C> {
    async fn resolve(&self, query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
        let key = resolved_cache_key(query);

        match self.cache.get(&key).await {
            Ok(Some(hit)) => return Ok(hit),
            Ok(None) => {}
            Err(e) => self.on_cache_error(e)?,
        }

        let resolved = self.inner.resolve(query).await?;

        if let Err(e) = self.cache.put(&key, &resolved, self.ttl).await {
            self.on_cache_error(e)?;
        }

        Ok(resolved)
    }
}

/// Shared, persistent [`ResolvedCache`] backed by Redis.
///
/// Values are stored as JSON under [`resolved_cache_key`] keys with a
/// per-entry TTL (`SET key value EX ttl`). The async connection manager
/// transparently reconnects, so transient drops surface as
/// [`PolicastError::Cache`] errors — which a [`CachedPolicyStore`] in
/// [`CacheFailMode::FallBackToInner`] turns into inner-store calls.
///
/// Gated behind the `redis` Cargo feature.
#[cfg(feature = "redis")]
#[derive(Clone)]
pub struct RedisCache {
    conn: redis::aio::ConnectionManager,
    key_prefix: Option<String>,
}

#[cfg(feature = "redis")]
impl RedisCache {
    /// Connect to Redis at `url` (e.g. `redis://127.0.0.1:6379`).
    pub async fn connect(url: &str) -> Result<Self, PolicastError> {
        let client = redis::Client::open(url)
            .map_err(|e| PolicastError::Cache(format!("redis client open: {e}")))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| PolicastError::Cache(format!("redis connect: {e}")))?;
        Ok(Self {
            conn,
            key_prefix: None,
        })
    }

    /// Build from an existing connection manager so a single Redis pool
    /// can be shared across multiple caches/stores.
    pub fn from_connection_manager(conn: redis::aio::ConnectionManager) -> Self {
        Self {
            conn,
            key_prefix: None,
        }
    }

    /// Namespace every key with `prefix` (useful for multi-tenant or
    /// shared Redis instances). Applied as `"{prefix}{key}"`.
    pub fn with_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    fn namespaced(&self, key: &str) -> String {
        match &self.key_prefix {
            Some(p) => format!("{p}{key}"),
            None => key.to_string(),
        }
    }
}

#[cfg(feature = "redis")]
#[async_trait]
impl ResolvedCache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<ResolvedPolicies>, PolicastError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn
            .get(self.namespaced(key))
            .await
            .map_err(|e| PolicastError::Cache(format!("redis get: {e}")))?;
        match raw {
            Some(s) => {
                let value = serde_json::from_str(&s)
                    .map_err(|e| PolicastError::Cache(format!("redis decode: {e}")))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    async fn put(
        &self,
        key: &str,
        value: &ResolvedPolicies,
        ttl: Duration,
    ) -> Result<(), PolicastError> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        let payload = serde_json::to_string(value)
            .map_err(|e| PolicastError::Cache(format!("redis encode: {e}")))?;
        // Redis TTL granularity is seconds; never set a 0 TTL (which
        // SETEX rejects) — clamp to at least 1s.
        let secs = ttl.as_secs().max(1);
        let _: () = conn
            .set_ex(self.namespaced(key), payload, secs)
            .await
            .map_err(|e| PolicastError::Cache(format!("redis set_ex: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppliesTo, Effect, FilterType};

    fn policy(id: &str, table: &str, roles: Option<Vec<&str>>) -> CompiledPolicy {
        CompiledPolicy {
            id: id.into(),
            effect: Effect::Permit,
            filter_type: FilterType::RowFilter,
            target_table: table.into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: "true".into(),
            applies_to: roles.map(|rs| AppliesTo {
                roles: rs.into_iter().map(String::from).collect(),
                principals: Vec::new(),
            }),
            description: None,
        }
    }

    fn store_with(policies: Vec<CompiledPolicy>) -> FileManifestStore {
        FileManifestStore::from_manifest(PolicyManifest {
            version: "1.0".into(),
            policies,
            principal_contract: None,
        })
    }

    #[test]
    fn test_table_matches_exact_and_wildcard() {
        assert!(table_matches("*", "patients"));
        assert!(table_matches("patients", "patients"));
        assert!(table_matches(
            "hospital.clinical.*",
            "hospital.clinical.patients"
        ));
        assert!(!table_matches(
            "hospital.foo.*",
            "hospital.clinical.patients"
        ));
        assert!(table_matches("hospital.clinical.patients", "patients"));
        assert!(table_matches("patients", "hospital.clinical.patients"));
        assert!(!table_matches("orders", "patients"));
    }

    #[tokio::test]
    async fn test_resolve_filters_by_table() {
        let store = store_with(vec![
            policy("p1", "patients", None),
            policy("p2", "orders", None),
        ]);
        let q = PolicyQuery {
            table: "patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        assert_eq!(resolved.manifest.policies.len(), 1);
        assert_eq!(resolved.manifest.policies[0].id, "p1");
        assert_eq!(resolved.bindings_applied, vec!["p1".to_string()]);
    }

    #[tokio::test]
    async fn test_resolve_filters_by_role() {
        let store = store_with(vec![
            policy("analyst_only", "patients", Some(vec!["analyst"])),
            policy("physician_only", "patients", Some(vec!["physician"])),
            policy("open_to_all", "patients", None),
        ]);
        let q = PolicyQuery {
            table: "patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        let ids: Vec<&str> = resolved
            .manifest
            .policies
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(ids, vec!["analyst_only", "open_to_all"]);
    }

    #[tokio::test]
    async fn test_resolve_wildcard_table() {
        let store = store_with(vec![policy("global", "*", None)]);
        let q = PolicyQuery {
            table: "anything".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        assert_eq!(resolved.manifest.policies.len(), 1);
    }

    #[tokio::test]
    async fn test_resolve_empty_applies_to_means_all_roles() {
        let store = store_with(vec![policy("p1", "patients", Some(vec![]))]);
        let q = PolicyQuery {
            table: "patients".into(),
            principal_id: "alice".into(),
            principal_role: "analyst".into(),
        };
        let resolved = store.resolve(&q).await.unwrap();
        assert_eq!(resolved.manifest.policies.len(), 1);
    }

    // -----------------------------------------------------------------
    // Caching layer
    // -----------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn query(table: &str, role: &str, id: &str) -> PolicyQuery {
        PolicyQuery {
            table: table.into(),
            principal_id: id.into(),
            principal_role: role.into(),
        }
    }

    /// Inner store that counts how many times it is hit and always
    /// returns a single-policy manifest.
    struct CountingStore {
        calls: Arc<AtomicUsize>,
    }

    impl CountingStore {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl PolicyStore for CountingStore {
        async fn resolve(&self, _query: &PolicyQuery) -> Result<ResolvedPolicies, PolicastError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResolvedPolicies {
                manifest: PolicyManifest {
                    version: "1.0".into(),
                    policies: vec![policy("p1", "patients", None)],
                },
                identity_claims: Default::default(),
                bindings_applied: vec!["p1".into()],
            })
        }
    }

    /// Cache backend whose every operation fails — used to exercise
    /// [`CacheFailMode`].
    struct FailingCache;

    #[async_trait]
    impl ResolvedCache for FailingCache {
        async fn get(&self, _key: &str) -> Result<Option<ResolvedPolicies>, PolicastError> {
            Err(PolicastError::Cache("boom".into()))
        }
        async fn put(
            &self,
            _key: &str,
            _value: &ResolvedPolicies,
            _ttl: Duration,
        ) -> Result<(), PolicastError> {
            Err(PolicastError::Cache("boom".into()))
        }
    }

    #[test]
    fn test_resolved_cache_key_is_stable() {
        let a = resolved_cache_key(&query("patients", "analyst", "alice"));
        let b = resolved_cache_key(&query("patients", "analyst", "alice"));
        assert_eq!(a, b);
        assert_eq!(a, "policast:v1:resolved:patients:analyst:alice");
    }

    #[test]
    fn test_resolved_cache_key_is_sensitive_to_each_field() {
        let base = resolved_cache_key(&query("patients", "analyst", "alice"));
        assert_ne!(
            base,
            resolved_cache_key(&query("orders", "analyst", "alice"))
        );
        assert_ne!(
            base,
            resolved_cache_key(&query("patients", "physician", "alice"))
        );
        assert_ne!(
            base,
            resolved_cache_key(&query("patients", "analyst", "bob"))
        );
    }

    #[test]
    fn test_resolved_cache_key_sanitizes_delimiters() {
        let key = resolved_cache_key(&query("hospital.clinical.patients", "data analyst", "a:b"));
        assert_eq!(
            key,
            "policast:v1:resolved:hospital.clinical.patients:data_analyst:a_b"
        );
    }

    #[tokio::test]
    async fn test_cached_store_miss_then_hit_calls_inner_once() {
        let inner = CountingStore::new();
        let calls = inner.calls.clone();
        let cached = CachedPolicyStore::new(inner, InMemoryCache::new(), Duration::from_secs(60));
        let q = query("patients", "analyst", "alice");

        let first = cached.resolve(&q).await.unwrap();
        let second = cached.resolve(&q).await.unwrap();

        assert_eq!(first.manifest.policies.len(), 1);
        assert_eq!(second.manifest.policies.len(), 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call should be served from cache"
        );
        assert_eq!(cached.cache().len(), 1);
    }

    #[tokio::test]
    async fn test_cached_store_distinct_principals_do_not_share() {
        let inner = CountingStore::new();
        let calls = inner.calls.clone();
        let cached = CachedPolicyStore::new(inner, InMemoryCache::new(), Duration::from_secs(60));

        cached
            .resolve(&query("patients", "analyst", "alice"))
            .await
            .unwrap();
        cached
            .resolve(&query("patients", "analyst", "bob"))
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cached.cache().len(), 2);
    }

    #[tokio::test]
    async fn test_cached_store_ttl_expiry_reresolves() {
        let inner = CountingStore::new();
        let calls = inner.calls.clone();
        let cached = CachedPolicyStore::new(inner, InMemoryCache::new(), Duration::from_millis(5));
        let q = query("patients", "analyst", "alice");

        cached.resolve(&q).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Let the entry expire, then resolve again; the inner store must
        // be consulted a second time.
        std::thread::sleep(Duration::from_millis(20));
        cached.resolve(&q).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_cached_store_falls_back_on_cache_error() {
        let inner = CountingStore::new();
        let calls = inner.calls.clone();
        // Default fail mode is FallBackToInner.
        let cached = CachedPolicyStore::new(inner, FailingCache, Duration::from_secs(60));

        let resolved = cached.resolve(&query("patients", "analyst", "alice")).await;
        assert!(
            resolved.is_ok(),
            "cache errors should fall back to inner store"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_cached_store_strict_surfaces_cache_error() {
        let inner = CountingStore::new();
        let cached = CachedPolicyStore::new(inner, FailingCache, Duration::from_secs(60))
            .with_fail_mode(CacheFailMode::Strict);

        let err = cached
            .resolve(&query("patients", "analyst", "alice"))
            .await
            .expect_err("strict mode should surface cache errors");
        assert!(matches!(err, PolicastError::Cache(_)));
    }

    #[tokio::test]
    async fn test_in_memory_cache_roundtrip_and_expiry() {
        let cache = InMemoryCache::new();
        let value = ResolvedPolicies {
            manifest: PolicyManifest {
                version: "1.0".into(),
                policies: vec![policy("p1", "patients", None)],
            },
            identity_claims: Default::default(),
            bindings_applied: vec!["p1".into()],
        };

        cache
            .put("k", &value, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            cache
                .get("k")
                .await
                .unwrap()
                .unwrap()
                .manifest
                .policies
                .len(),
            1
        );

        cache
            .put("k2", &value, Duration::from_millis(1))
            .await
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get("k2").await.unwrap().is_none());
        assert!(!cache.is_empty(), "the non-expired key should remain");
    }

    /// `RedisCache` round-trip against a live server. Skipped unless
    /// `POLICAST_TEST_REDIS_URL` is set so the default `cargo test` stays
    /// hermetic. Requires `--features redis`.
    #[cfg(feature = "redis")]
    #[tokio::test]
    async fn test_redis_cache_roundtrip() {
        let Ok(url) = std::env::var("POLICAST_TEST_REDIS_URL") else {
            eprintln!("skipping: POLICAST_TEST_REDIS_URL not set");
            return;
        };
        let cache = RedisCache::connect(&url)
            .await
            .expect("connect to test redis")
            .with_key_prefix("policast-test:");
        let value = ResolvedPolicies {
            manifest: PolicyManifest {
                version: "1.0".into(),
                policies: vec![policy("p1", "patients", None)],
            },
            identity_claims: Default::default(),
            bindings_applied: vec!["p1".into()],
        };
        let key = resolved_cache_key(&query("patients", "analyst", "redis-test"));
        cache
            .put(&key, &value, Duration::from_secs(30))
            .await
            .unwrap();
        let got = cache
            .get(&key)
            .await
            .unwrap()
            .expect("entry should be present");
        assert_eq!(got.manifest.policies[0].id, "p1");
    }
}
