//! Wire types for the `/policies/resolve` endpoint.
//!
//! These types are the stable contract between engines and the
//! resolver. They must remain backwards-compatible: add fields rather
//! than changing existing ones.

use std::collections::BTreeMap;

use policast_core::PolicyManifest;
use serde::{Deserialize, Serialize};

/// The principal making a query.
///
/// `attrs` carries arbitrary ABAC attributes (e.g. `region`) that the
/// resolver will fold into the manifest's `identity_claims` on
/// response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub attrs: PrincipalAttrs,
}

/// Arbitrary string→string attributes on a principal. Uses a
/// `BTreeMap` for stable iteration order so hashing (for cache keys
/// and signatures) is deterministic.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct PrincipalAttrs(pub BTreeMap<String, String>);

impl PrincipalAttrs {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// POST body for `/policies/resolve`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveRequest {
    pub table: String,
    pub principal: Principal,
    #[serde(default = "default_action")]
    pub requested_action: String,
}

fn default_action() -> String {
    "query".to_string()
}

/// Response body from `/policies/resolve`. Engines verify
/// [`ResolveBundle::signature`] before trusting any field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveBundle {
    pub table_uuid: String,
    pub compiled_manifest: PolicyManifest,
    #[serde(default)]
    pub bindings_applied: Vec<String>,
    /// Audit trail for Cedar-template expansion. Maps each expanded
    /// policy id (e.g. `column_mask_by_pii@hospital.clinical.patients:ssn`)
    /// to the template lineage that produced it
    /// (e.g. `column_mask_by_pii (applies_to_tag=pii)`).
    ///
    /// Empty for bundles that contain no tag-scoped templates, which
    /// is why this field uses `skip_serializing_if`: a bundle
    /// assembled from only concrete policies serializes identically
    /// to how it did before the templates feature shipped, so older
    /// clients never see a new field and HMAC signatures issued prior
    /// to the feature remain verifiable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub expanded_from: BTreeMap<String, String>,
    #[serde(default)]
    pub identity_claims: BTreeMap<String, String>,
    #[serde(default)]
    pub storage_credentials: Option<StorageCredentials>,
    #[serde(default)]
    pub storage_uri: Option<String>,
    /// RFC3339 timestamp.
    pub expires_at: String,
    /// HMAC-SHA256 signature over the canonical serialization of the
    /// bundle *with this field replaced by the empty string*. See
    /// [`crate::signature`].
    pub signature: String,
}

/// Short-lived storage credentials vended by the resolver.
///
/// Intentionally structureless beyond the common fields — the resolver
/// passes whatever the underlying cloud wants through `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageCredentials {
    #[serde(default)]
    pub aws_access_key_id: Option<String>,
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,
    #[serde(default)]
    pub aws_session_token: Option<String>,
    #[serde(default)]
    pub expiration: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, String>,
}

impl ResolveBundle {
    /// Return a copy of this bundle with the `signature` field zeroed,
    /// suitable for hashing/signing.
    pub fn canonical_for_signing(&self) -> Self {
        let mut c = self.clone();
        c.signature.clear();
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use policast_core::PolicyManifest;

    #[test]
    fn test_request_roundtrip() {
        let req = ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: Principal {
                id: "alice".into(),
                role: "analyst".into(),
                attrs: PrincipalAttrs::new().with("region", "us-east"),
            },
            requested_action: "query".into(),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: ResolveRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn test_bundle_roundtrip() {
        let bundle = ResolveBundle {
            table_uuid: "uuid-1".into(),
            compiled_manifest: PolicyManifest::new(),
            bindings_applied: vec!["p1".into()],
            expanded_from: Default::default(),
            identity_claims: [("region".to_string(), "us-east".to_string())]
                .into_iter()
                .collect(),
            storage_credentials: Some(StorageCredentials {
                aws_access_key_id: Some("AKIA".into()),
                ..Default::default()
            }),
            storage_uri: Some("s3://bucket/patients_delta".into()),
            expires_at: "2030-01-01T00:00:00Z".into(),
            signature: "abc".into(),
        };
        let s = serde_json::to_string(&bundle).unwrap();
        let back: ResolveBundle = serde_json::from_str(&s).unwrap();
        assert_eq!(bundle, back);
    }

    #[test]
    fn test_canonical_for_signing_clears_signature() {
        let bundle = ResolveBundle {
            table_uuid: "t".into(),
            compiled_manifest: PolicyManifest::new(),
            bindings_applied: Vec::new(),
            expanded_from: Default::default(),
            identity_claims: Default::default(),
            storage_credentials: None,
            storage_uri: None,
            expires_at: "2030-01-01T00:00:00Z".into(),
            signature: "NOTEMPTY".into(),
        };
        let canon = bundle.canonical_for_signing();
        assert!(canon.signature.is_empty());
        assert_eq!(canon.table_uuid, "t");
    }

    #[test]
    fn test_principal_attrs_ordering_is_stable() {
        let a = PrincipalAttrs::new().with("b", "2").with("a", "1");
        let b = PrincipalAttrs::new().with("a", "1").with("b", "2");
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }
}
