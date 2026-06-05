//! LRU cache with per-entry TTL for `ResolveBundle`s.
//!
//! The cache is keyed by `(table, principal_fingerprint)` so two
//! principals on the same table do not share entries but two requests
//! from the same principal do. TTL is set on insertion from the
//! bundle's `expires_at` so the cache cannot return an expired bundle.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::error::UcError;
use crate::types::{Principal, ResolveBundle};

/// A cache key uniquely identifies a `(table, principal)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub table: String,
    pub principal_fingerprint: String,
}

impl CacheKey {
    pub fn new(table: &str, principal: &Principal) -> Self {
        Self {
            table: table.to_string(),
            principal_fingerprint: fingerprint(principal),
        }
    }
}

/// Stable, deterministic digest of a principal. Includes role and
/// sorted attributes so two otherwise-identical principals collide but
/// any difference in attrs yields a different key.
pub fn fingerprint(principal: &Principal) -> String {
    let mut h = Sha256::new();
    h.update(principal.id.as_bytes());
    h.update(b"|");
    h.update(principal.role.as_bytes());
    h.update(b"|");
    for (k, v) in &principal.attrs.0 {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update(b"&");
    }
    hex::encode(h.finalize())
}

/// Thread-safe, cloneable LRU cache of resolve bundles.
#[derive(Clone)]
pub struct BundleCache {
    inner: Arc<Mutex<LruCache<CacheKey, ResolveBundle>>>,
}

impl BundleCache {
    /// Create a cache with capacity for `cap` entries. Panics if
    /// `cap == 0`; use [`BundleCache::new_checked`] for fallible
    /// construction.
    pub fn new(cap: usize) -> Self {
        Self::new_checked(cap).expect("cache capacity must be non-zero")
    }

    pub fn new_checked(cap: usize) -> Result<Self, UcError> {
        let n = NonZeroUsize::new(cap)
            .ok_or_else(|| UcError::Invalid("cache capacity must be > 0".into()))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(LruCache::new(n))),
        })
    }

    /// Look up a bundle. Returns `None` when missing or expired. When
    /// an entry is expired it is evicted on access.
    pub fn get(&self, key: &CacheKey) -> Option<ResolveBundle> {
        let mut guard = self.inner.lock();
        match guard.get(key) {
            Some(entry) if !is_expired(&entry.expires_at) => Some(entry.clone()),
            Some(_) => {
                guard.pop(key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, key: CacheKey, bundle: ResolveBundle) {
        self.inner.lock().put(key, bundle);
    }

    pub fn invalidate(&self, key: &CacheKey) {
        self.inner.lock().pop(key);
    }

    pub fn invalidate_all_for_table(&self, table: &str) {
        let mut guard = self.inner.lock();
        let keys: Vec<CacheKey> = guard
            .iter()
            .filter_map(|(k, _)| {
                if k.table == table {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for k in keys {
            guard.pop(&k);
        }
    }

    /// Drop every entry. O(n); preserves capacity.
    pub fn invalidate_all(&self) {
        let mut guard = self.inner.lock();
        let keys: Vec<CacheKey> = guard.iter().map(|(k, _)| k.clone()).collect();
        for k in keys {
            guard.pop(&k);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

fn is_expired(expires_at: &str) -> bool {
    let Ok(exp) = OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
    else {
        // Unparseable timestamps are treated as expired; fail closed.
        return true;
    };
    OffsetDateTime::now_utc() >= exp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Principal, PrincipalAttrs, ResolveBundle};
    use policast_core::PolicyManifest;

    fn alice() -> Principal {
        Principal {
            id: "alice".into(),
            role: "analyst".into(),
            attrs: PrincipalAttrs::new().with("region", "us-east"),
        }
    }

    fn bundle_with_expiry(exp: &str) -> ResolveBundle {
        ResolveBundle {
            table_uuid: "t".into(),
            compiled_manifest: PolicyManifest::new(),
            bindings_applied: Vec::new(),
            expanded_from: Default::default(),
            identity_claims: Default::default(),
            storage_credentials: None,
            storage_uri: None,
            expires_at: exp.into(),
            signature: "sig".into(),
        }
    }

    #[test]
    fn test_fingerprint_is_stable() {
        let a = alice();
        let b = alice();
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn test_fingerprint_differs_with_role() {
        let a = alice();
        let mut b = alice();
        b.role = "physician".into();
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn test_cache_put_get_roundtrip() {
        let cache = BundleCache::new(4);
        let key = CacheKey::new("patients", &alice());
        cache.put(key.clone(), bundle_with_expiry("2999-01-01T00:00:00Z"));
        let got = cache.get(&key).expect("should be cached");
        assert_eq!(got.table_uuid, "t");
    }

    #[test]
    fn test_cache_expired_entry_is_evicted() {
        let cache = BundleCache::new(4);
        let key = CacheKey::new("patients", &alice());
        cache.put(key.clone(), bundle_with_expiry("2000-01-01T00:00:00Z"));
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_unparseable_timestamp_is_evicted() {
        let cache = BundleCache::new(4);
        let key = CacheKey::new("patients", &alice());
        cache.put(key.clone(), bundle_with_expiry("not a date"));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn test_invalidate_all_for_table() {
        let cache = BundleCache::new(4);
        let k1 = CacheKey::new("patients", &alice());
        let mut bob = alice();
        bob.id = "bob".into();
        let k2 = CacheKey::new("patients", &bob);
        let k3 = CacheKey::new("orders", &alice());
        cache.put(k1.clone(), bundle_with_expiry("2999-01-01T00:00:00Z"));
        cache.put(k2.clone(), bundle_with_expiry("2999-01-01T00:00:00Z"));
        cache.put(k3.clone(), bundle_with_expiry("2999-01-01T00:00:00Z"));
        cache.invalidate_all_for_table("patients");
        assert!(cache.get(&k1).is_none());
        assert!(cache.get(&k2).is_none());
        assert!(cache.get(&k3).is_some());
    }

    #[test]
    fn test_zero_capacity_is_rejected() {
        assert!(BundleCache::new_checked(0).is_err());
    }
}
