//! Delta CDF-driven cache invalidation.
//!
//! The UC policy store exposes its compiled manifest as a Delta table
//! with Change Data Feed enabled. Long-running engine sessions can
//! register a listener that tails CDF and drops cache entries for any
//! policies that change. This module provides the glue.
//!
//! A full CDF integration requires `deltalake` with the CDF reader —
//! we keep that behind the `policast-datafusion` feature flag, since
//! adding `deltalake` to `policast-uc` would pull a large dependency
//! graph into a crate that otherwise only does HTTP. Instead we
//! expose a channel-driven notifier that the DataFusion/UC bridge
//! code feeds from the reader it already has.
//!
//! ```no_run
//! use policast_uc::cdc::{InvalidationNotifier, InvalidationEvent};
//! use policast_uc::cache::BundleCache;
//!
//! # fn setup() -> (InvalidationNotifier, BundleCache) {
//! let cache = BundleCache::new(128);
//! let notifier = InvalidationNotifier::new(cache.clone());
//! (notifier, cache)
//! # }
//! ```

use tokio::sync::mpsc;

use crate::cache::BundleCache;

/// An event pushed to the notifier when a manifest row is written or
/// deleted. Consumers never produce these directly — the CDF reader
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidationEvent {
    /// A specific policy changed. All cached bundles that include this
    /// policy id should be dropped. Currently we implement this by
    /// dropping all bundles whose target table matches, since policy
    /// ids are not stored on the cache key.
    PolicyChanged {
        policy_id: String,
        target_table: Option<String>,
    },
    /// A catch-all: invalidate everything (e.g. on compaction).
    InvalidateAll,
}

/// Sender half; hand this to whatever is reading the Delta CDF stream
/// and it will push events into the notifier.
#[derive(Clone)]
pub struct InvalidationSender {
    tx: mpsc::UnboundedSender<InvalidationEvent>,
}

impl InvalidationSender {
    pub fn send(&self, event: InvalidationEvent) -> Result<(), InvalidationEvent> {
        self.tx.send(event).map_err(|e| e.0)
    }
}

/// Notifier wrapping a cache. Spawns a task that drains
/// `InvalidationEvent`s and invalidates cache entries accordingly.
pub struct InvalidationNotifier {
    sender: InvalidationSender,
    _task: tokio::task::JoinHandle<()>,
}

impl InvalidationNotifier {
    pub fn new(cache: BundleCache) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<InvalidationEvent>();
        let task = tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                match evt {
                    InvalidationEvent::PolicyChanged { target_table, .. } => {
                        if let Some(table) = target_table {
                            cache.invalidate_all_for_table(&table);
                        } else {
                            cache.invalidate_all_for_table("*");
                        }
                    }
                    InvalidationEvent::InvalidateAll => {
                        cache.invalidate_all();
                    }
                }
            }
        });
        Self {
            sender: InvalidationSender { tx },
            _task: task,
        }
    }

    pub fn sender(&self) -> InvalidationSender {
        self.sender.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{BundleCache, CacheKey};
    use crate::types::{Principal, PrincipalAttrs, ResolveBundle};
    use policast_core::PolicyManifest;

    fn bundle() -> ResolveBundle {
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
        }
    }

    fn principal(role: &str) -> Principal {
        Principal {
            id: "alice".into(),
            role: role.into(),
            attrs: PrincipalAttrs::new(),
        }
    }

    #[tokio::test]
    async fn test_policy_changed_invalidates_target_table() {
        let cache = BundleCache::new(4);
        let k_patients = CacheKey::new("patients", &principal("analyst"));
        let k_orders = CacheKey::new("orders", &principal("analyst"));
        cache.put(k_patients.clone(), bundle());
        cache.put(k_orders.clone(), bundle());

        let notifier = InvalidationNotifier::new(cache.clone());
        notifier
            .sender()
            .send(InvalidationEvent::PolicyChanged {
                policy_id: "p1".into(),
                target_table: Some("patients".into()),
            })
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(cache.get(&k_patients).is_none());
        assert!(cache.get(&k_orders).is_some());
    }

    #[tokio::test]
    async fn test_invalidate_all_clears_cache() {
        let cache = BundleCache::new(4);
        let k = CacheKey::new("patients", &principal("analyst"));
        cache.put(k.clone(), bundle());
        let notifier = InvalidationNotifier::new(cache.clone());
        notifier
            .sender()
            .send(InvalidationEvent::InvalidateAll)
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(cache.get(&k).is_none());
    }
}
