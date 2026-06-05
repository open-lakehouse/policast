use std::collections::BTreeMap;

/// Pluggable source of `principal.*` attributes used to bind a compiled
/// CEL policy to a concrete querying user at planning time.
///
/// The enforcement path never assumes a fixed set of principal fields:
/// every `principal.<attr>` reference in a policy is resolved through
/// this trait. Ship a [`QueryIdentity`](crate::cel_filter::QueryIdentity)
/// for the common `role`/`region`/`name` shape, an [`AttrIdentity`] for a
/// fully dynamic attribute bag, or implement the trait directly to bridge
/// an existing identity system.
///
/// Implementations must be `Send + Sync` so a provider can live behind an
/// `Arc` inside a `TableProvider` shared across DataFusion's async runtime.
pub trait PrincipalProvider: Send + Sync {
    /// Resolve a single principal attribute by name, returning `None` when
    /// the identity does not carry it. A `None` for an attribute a policy
    /// references surfaces as a [`CelConvertError::MissingIdentityField`].
    ///
    /// [`CelConvertError::MissingIdentityField`]: crate::cel_to_expr::CelConvertError::MissingIdentityField
    fn attribute(&self, name: &str) -> Option<String>;

    /// Snapshot of every principal attribute the identity carries.
    ///
    /// Used to populate the CEL runtime context for column-mask decisions,
    /// where the whole `principal` record is evaluated at once rather than
    /// one field at a time.
    fn principal_attributes(&self) -> BTreeMap<String, String>;
}

/// A fully dynamic identity backed by a string→string attribute map.
///
/// This is the open-ended counterpart to
/// [`QueryIdentity`](crate::cel_filter::QueryIdentity): it imposes no fixed
/// schema, so policies may reference any `principal.<attr>` the caller
/// chooses to populate (e.g. `principal.clearance`, `principal.department`).
/// It mirrors the `PrincipalAttrs` bag on the resolver wire contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttrIdentity(pub BTreeMap<String, String>);

impl AttrIdentity {
    /// Create an empty identity.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Builder-style insert, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }
}

impl<K, V> FromIterator<(K, V)> for AttrIdentity
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl PrincipalProvider for AttrIdentity {
    fn attribute(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }

    fn principal_attributes(&self) -> BTreeMap<String, String> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attr_identity_builder_and_lookup() {
        let id = AttrIdentity::new()
            .with("role", "analyst")
            .with("clearance", "secret");
        assert_eq!(id.attribute("role").as_deref(), Some("analyst"));
        assert_eq!(id.attribute("clearance").as_deref(), Some("secret"));
        assert_eq!(id.attribute("missing"), None);
    }

    #[test]
    fn test_attr_identity_snapshot() {
        let id = AttrIdentity::new()
            .with("role", "physician")
            .with("region", "us-east");
        let attrs = id.principal_attributes();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs.get("role").map(String::as_str), Some("physician"));
        assert_eq!(attrs.get("region").map(String::as_str), Some("us-east"));
    }

    #[test]
    fn test_attr_identity_from_iter() {
        let id: AttrIdentity = [("role", "legal"), ("region", "eu-west")]
            .into_iter()
            .collect();
        assert_eq!(id.attribute("role").as_deref(), Some("legal"));
        assert_eq!(id.attribute("region").as_deref(), Some("eu-west"));
    }

    /// A provider used behind `&dyn` exercises object-safety of the trait.
    #[test]
    fn test_dyn_dispatch() {
        let id = AttrIdentity::new().with("role", "admin");
        let dyn_ref: &dyn PrincipalProvider = &id;
        assert_eq!(dyn_ref.attribute("role").as_deref(), Some("admin"));
    }
}
