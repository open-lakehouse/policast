//! Protobuf mirror of the manifest model + serde<->proto conversions.
//!
//! JSON remains the **canonical, signed** wire format for the resolver
//! and engines (see `policast-uc::signature`). Protobuf is used purely
//! as a compact at-rest encoding — today, the binary `PolicyManifest`
//! blobs the materialized Redis PolicyStore serves.
//!
//! The generated types live in [`v1`] and are produced by `prost-build`
//! from [`proto/policast/v1/policast.proto`](../../../proto/policast/v1/policast.proto).
//! To keep `protoc`/`prost-build` out of the normal build graph, the
//! generated Rust is **checked in** at `src/proto/generated/policast.v1.rs`.
//!
//! ## Regenerating
//!
//! When the `.proto` changes, regenerate the checked-in file by
//! temporarily restoring a `build.rs` that calls
//! `prost_build::Config::out_dir("src/proto/generated").compile_protos(...)`
//! and running:
//!
//! ```text
//! POLICAST_PROTO_REGEN=1 cargo build -p policast-core --features proto
//! ```
//!
//! then removing the `build.rs` + `prost-build` build-dependency again.

use crate::model::{AppliesTo, CompiledPolicy, Effect, FilterType, PrincipalContract};
use crate::policy_manifest::PolicyManifest;

/// Schema/wire version of the proto contract we emit. Mirrors the
/// `manifest_schema_version` field; `0`/absent means "v1".
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// prost-generated message + enum types for `policast.v1`.
pub mod v1 {
    include!("proto/generated/policast.v1.rs");
}

/// Errors raised when decoding protobuf back into the canonical serde
/// model (e.g. an enum that arrived as `*_UNSPECIFIED`).
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
    /// A closed enum arrived as its zero/unspecified guard — we fail
    /// closed rather than silently aliasing it to a real variant.
    #[error("unspecified or unknown {kind} value `{value}` in policy `{policy_id}`")]
    UnspecifiedEnum {
        kind: &'static str,
        value: i32,
        policy_id: String,
    },
    #[error("protobuf manifest is missing required field `{0}`")]
    MissingField(&'static str),
}

// ---------------------------------------------------------------------------
// serde model -> proto
// ---------------------------------------------------------------------------

impl From<&Effect> for v1::Effect {
    fn from(e: &Effect) -> Self {
        match e {
            Effect::Permit => v1::Effect::Permit,
            Effect::Forbid => v1::Effect::Forbid,
        }
    }
}

impl From<&FilterType> for v1::FilterType {
    fn from(f: &FilterType) -> Self {
        match f {
            FilterType::RowFilter => v1::FilterType::RowFilter,
            FilterType::ColumnMask => v1::FilterType::ColumnMask,
            FilterType::DenyOverride => v1::FilterType::DenyOverride,
        }
    }
}

impl From<&AppliesTo> for v1::AppliesTo {
    fn from(a: &AppliesTo) -> Self {
        v1::AppliesTo {
            roles: a.roles.clone(),
            principals: a.principals.clone(),
        }
    }
}

impl From<&PrincipalContract> for v1::PrincipalContract {
    fn from(p: &PrincipalContract) -> Self {
        v1::PrincipalContract {
            required_attributes: p.required_attributes.clone(),
        }
    }
}

impl From<&CompiledPolicy> for v1::CompiledPolicy {
    fn from(p: &CompiledPolicy) -> Self {
        v1::CompiledPolicy {
            id: p.id.clone(),
            effect: v1::Effect::from(&p.effect) as i32,
            filter_type: v1::FilterType::from(&p.filter_type) as i32,
            target_table: p.target_table.clone(),
            column: p.column.clone(),
            target_tag: p.target_tag.clone(),
            applies_to_tag: p.applies_to_tag.clone(),
            cel_expression: p.cel_expression.clone(),
            applies_to: p.applies_to.as_ref().map(v1::AppliesTo::from),
            description: p.description.clone(),
        }
    }
}

impl From<&PolicyManifest> for v1::PolicyManifest {
    fn from(m: &PolicyManifest) -> Self {
        v1::PolicyManifest {
            version: m.version.clone(),
            policies: m.policies.iter().map(v1::CompiledPolicy::from).collect(),
            principal_contract: m
                .principal_contract
                .as_ref()
                .map(v1::PrincipalContract::from),
            manifest_schema_version: MANIFEST_SCHEMA_VERSION,
        }
    }
}

// ---------------------------------------------------------------------------
// proto -> serde model (fail-closed on unknown enums)
// ---------------------------------------------------------------------------

fn effect_from_proto(value: i32, policy_id: &str) -> Result<Effect, ProtoError> {
    match v1::Effect::try_from(value) {
        Ok(v1::Effect::Permit) => Ok(Effect::Permit),
        Ok(v1::Effect::Forbid) => Ok(Effect::Forbid),
        // Unspecified or an unrecognized number: fail closed.
        _ => Err(ProtoError::UnspecifiedEnum {
            kind: "effect",
            value,
            policy_id: policy_id.to_string(),
        }),
    }
}

fn filter_type_from_proto(value: i32, policy_id: &str) -> Result<FilterType, ProtoError> {
    match v1::FilterType::try_from(value) {
        Ok(v1::FilterType::RowFilter) => Ok(FilterType::RowFilter),
        Ok(v1::FilterType::ColumnMask) => Ok(FilterType::ColumnMask),
        Ok(v1::FilterType::DenyOverride) => Ok(FilterType::DenyOverride),
        _ => Err(ProtoError::UnspecifiedEnum {
            kind: "filter_type",
            value,
            policy_id: policy_id.to_string(),
        }),
    }
}

impl From<v1::AppliesTo> for AppliesTo {
    fn from(a: v1::AppliesTo) -> Self {
        AppliesTo {
            roles: a.roles,
            principals: a.principals,
        }
    }
}

impl From<v1::PrincipalContract> for PrincipalContract {
    fn from(p: v1::PrincipalContract) -> Self {
        PrincipalContract {
            required_attributes: p.required_attributes,
        }
    }
}

impl TryFrom<v1::CompiledPolicy> for CompiledPolicy {
    type Error = ProtoError;

    fn try_from(p: v1::CompiledPolicy) -> Result<Self, Self::Error> {
        let effect = effect_from_proto(p.effect, &p.id)?;
        let filter_type = filter_type_from_proto(p.filter_type, &p.id)?;
        Ok(CompiledPolicy {
            id: p.id,
            effect,
            filter_type,
            target_table: p.target_table,
            column: p.column,
            target_tag: p.target_tag,
            applies_to_tag: p.applies_to_tag,
            cel_expression: p.cel_expression,
            applies_to: p.applies_to.map(AppliesTo::from),
            description: p.description,
        })
    }
}

impl TryFrom<v1::PolicyManifest> for PolicyManifest {
    type Error = ProtoError;

    fn try_from(m: v1::PolicyManifest) -> Result<Self, Self::Error> {
        let policies = m
            .policies
            .into_iter()
            .map(CompiledPolicy::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PolicyManifest {
            version: m.version,
            policies,
            principal_contract: m.principal_contract.map(PrincipalContract::from),
        })
    }
}

impl PolicyManifest {
    /// Encode this manifest as binary protobuf (`policast.v1.PolicyManifest`).
    ///
    /// Used for at-rest storage such as the Redis PolicyStore. JSON
    /// (`to_json`) remains the canonical, signed wire format.
    pub fn to_protobuf(&self) -> Vec<u8> {
        use prost::Message;
        v1::PolicyManifest::from(self).encode_to_vec()
    }

    /// Decode a manifest from binary protobuf. Fails closed when an
    /// `effect`/`filter_type` arrives as its `*_UNSPECIFIED` guard.
    pub fn from_protobuf(bytes: &[u8]) -> Result<Self, ProtoError> {
        use prost::Message;
        let proto = v1::PolicyManifest::decode(bytes)?;
        PolicyManifest::try_from(proto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> PolicyManifest {
        PolicyManifest {
            version: "1.0".into(),
            policies: vec![
                CompiledPolicy {
                    id: "row_filter_region".into(),
                    effect: Effect::Permit,
                    filter_type: FilterType::RowFilter,
                    target_table: "hospital.clinical.patients".into(),
                    column: None,
                    target_tag: Some("clinical".into()),
                    applies_to_tag: None,
                    cel_expression: "resource.region == principal.region".into(),
                    applies_to: Some(AppliesTo {
                        roles: vec!["analyst".into()],
                        principals: vec![],
                    }),
                    description: Some("region filter".into()),
                },
                CompiledPolicy {
                    id: "mask_ssn".into(),
                    effect: Effect::Forbid,
                    filter_type: FilterType::ColumnMask,
                    target_table: "*".into(),
                    column: Some("ssn".into()),
                    target_tag: None,
                    applies_to_tag: Some("pii".into()),
                    cel_expression: "!(principal.role == \"admin\")".into(),
                    applies_to: None,
                    description: None,
                },
            ],
            principal_contract: Some(PrincipalContract {
                required_attributes: vec!["region".into(), "role".into()],
            }),
        }
    }

    #[test]
    fn test_protobuf_roundtrip_preserves_manifest() {
        let manifest = sample_manifest();
        let bytes = manifest.to_protobuf();
        let back = PolicyManifest::from_protobuf(&bytes).unwrap();
        assert_eq!(manifest, back);
    }

    #[test]
    fn test_protobuf_roundtrip_empty_manifest() {
        let manifest = PolicyManifest::new();
        let bytes = manifest.to_protobuf();
        let back = PolicyManifest::from_protobuf(&bytes).unwrap();
        assert_eq!(manifest, back);
    }

    #[test]
    fn test_schema_version_is_set_on_encode() {
        let proto = v1::PolicyManifest::from(&sample_manifest());
        assert_eq!(proto.manifest_schema_version, MANIFEST_SCHEMA_VERSION);
    }

    #[test]
    fn test_unspecified_effect_fails_closed() {
        // Hand-build a proto policy with effect = UNSPECIFIED (0).
        let proto = v1::PolicyManifest {
            version: "1.0".into(),
            policies: vec![v1::CompiledPolicy {
                id: "broken".into(),
                effect: v1::Effect::Unspecified as i32,
                filter_type: v1::FilterType::RowFilter as i32,
                target_table: "*".into(),
                cel_expression: "true".into(),
                ..Default::default()
            }],
            principal_contract: None,
            manifest_schema_version: MANIFEST_SCHEMA_VERSION,
        };
        use prost::Message;
        let bytes = proto.encode_to_vec();
        let err = PolicyManifest::from_protobuf(&bytes).unwrap_err();
        assert!(
            matches!(err, ProtoError::UnspecifiedEnum { kind: "effect", .. }),
            "expected fail-closed on unspecified effect, got {err:?}"
        );
    }

    #[test]
    fn test_unspecified_filter_type_fails_closed() {
        let proto = v1::PolicyManifest {
            version: "1.0".into(),
            policies: vec![v1::CompiledPolicy {
                id: "broken".into(),
                effect: v1::Effect::Permit as i32,
                filter_type: v1::FilterType::Unspecified as i32,
                target_table: "*".into(),
                cel_expression: "true".into(),
                ..Default::default()
            }],
            principal_contract: None,
            manifest_schema_version: MANIFEST_SCHEMA_VERSION,
        };
        use prost::Message;
        let bytes = proto.encode_to_vec();
        let err = PolicyManifest::from_protobuf(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                ProtoError::UnspecifiedEnum {
                    kind: "filter_type",
                    ..
                }
            ),
            "expected fail-closed on unspecified filter_type, got {err:?}"
        );
    }

    #[test]
    fn test_unknown_enum_number_fails_closed() {
        let proto = v1::PolicyManifest {
            version: "1.0".into(),
            policies: vec![v1::CompiledPolicy {
                id: "future".into(),
                effect: v1::Effect::Permit as i32,
                // A filter_type number this build does not know.
                filter_type: 99,
                target_table: "*".into(),
                cel_expression: "true".into(),
                ..Default::default()
            }],
            principal_contract: None,
            manifest_schema_version: MANIFEST_SCHEMA_VERSION,
        };
        use prost::Message;
        let bytes = proto.encode_to_vec();
        let err = PolicyManifest::from_protobuf(&bytes).unwrap_err();
        assert!(matches!(err, ProtoError::UnspecifiedEnum { value: 99, .. }));
    }
}
