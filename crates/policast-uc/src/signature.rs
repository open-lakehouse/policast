//! HMAC-SHA256 signing and verification for `ResolveBundle`s.
//!
//! The resolver signs every bundle it returns so engines can fail
//! closed on tampering between resolve and enforce. The signature is
//! computed over the canonical JSON serialization of the bundle with
//! its `signature` field set to the empty string, and prefixed with
//! the literal tag `"hmac-sha256:"` in hex.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::UcError;
use crate::types::ResolveBundle;

const SIG_TAG: &str = "hmac-sha256:";

type HmacSha256 = Hmac<Sha256>;

/// Compute the signature string for a bundle using the given secret.
pub fn sign(bundle: &ResolveBundle, secret: &[u8]) -> Result<String, UcError> {
    let canon = bundle.canonical_for_signing();
    let body = serde_json::to_vec(&canon)?;
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|e| UcError::Invalid(format!("bad HMAC key: {e}")))?;
    mac.update(&body);
    let tag = mac.finalize().into_bytes();
    Ok(format!("{SIG_TAG}{}", hex::encode(tag)))
}

/// Return the bundle with `signature` populated.
pub fn sign_bundle(mut bundle: ResolveBundle, secret: &[u8]) -> Result<ResolveBundle, UcError> {
    bundle.signature.clear();
    bundle.signature = sign(&bundle, secret)?;
    Ok(bundle)
}

/// Verify a bundle's signature. Returns `Ok(())` only for valid
/// signatures; all other cases return [`UcError::BadSignature`].
pub fn verify(bundle: &ResolveBundle, secret: &[u8]) -> Result<(), UcError> {
    let observed = bundle.signature.as_str();
    let expected = sign(bundle, secret)?;
    if constant_time_eq(observed.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(UcError::BadSignature)
    }
}

/// Timing-safe equality. Avoids leaking secret bytes via early-exit
/// comparisons when an attacker can observe latency.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResolveBundle;
    use policast_core::PolicyManifest;

    fn sample() -> ResolveBundle {
        ResolveBundle {
            table_uuid: "uuid".into(),
            compiled_manifest: PolicyManifest::new(),
            bindings_applied: Vec::new(),
            expanded_from: Default::default(),
            identity_claims: Default::default(),
            storage_credentials: None,
            storage_uri: None,
            expires_at: "2030-01-01T00:00:00Z".into(),
            signature: String::new(),
        }
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let secret = b"secret-key-1";
        let b = sign_bundle(sample(), secret).unwrap();
        assert!(b.signature.starts_with("hmac-sha256:"));
        verify(&b, secret).unwrap();
    }

    #[test]
    fn test_wrong_secret_fails() {
        let b = sign_bundle(sample(), b"s1").unwrap();
        let err = verify(&b, b"s2").unwrap_err();
        matches!(err, UcError::BadSignature);
    }

    #[test]
    fn test_tampered_field_fails() {
        let mut b = sign_bundle(sample(), b"s").unwrap();
        b.table_uuid = "tampered".into();
        let err = verify(&b, b"s").unwrap_err();
        matches!(err, UcError::BadSignature);
    }

    #[test]
    fn test_signature_is_deterministic() {
        let s1 = sign_bundle(sample(), b"s").unwrap().signature;
        let s2 = sign_bundle(sample(), b"s").unwrap().signature;
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_constant_time_eq_behavior() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
