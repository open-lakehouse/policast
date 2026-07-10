//! Unity Catalog policy store for policast-cel.
//!
//! This crate provides the client + resolver glue needed to use Unity
//! Catalog (OSS) as a Policy Decision Point (PDP) for Cedar/CEL
//! governance policies. See
//! [`research/unity-catalog-policy-store.md`](../../research/unity-catalog-policy-store.md)
//! for the full design.
//!
//! Two entry points matter:
//!
//! - [`client::UnityCatalogPolicyStore`] — implements
//!   [`policast_core::policy_store::PolicyStore`] against a REST
//!   `/policies/resolve` endpoint (UC or the sidecar).
//! - [`sidecar`] (feature = `"sidecar"`) — an Axum service that
//!   implements the same endpoint against a flat JSON store for
//!   development and tests, and against Delta tables in production.
//!
//! The wire protocol ([`types::ResolveRequest`] /
//! [`types::ResolveBundle`]) is shared between the two and is
//! HMAC-signed so engines fail-closed on tampering.

pub mod cache;
pub mod error;
pub mod signature;
pub mod store;
pub mod types;

#[cfg(feature = "client")]
pub mod client;

#[cfg(feature = "client")]
pub mod uc_rest;

#[cfg(feature = "sidecar")]
pub mod sidecar;

pub mod backend;
pub mod cdc;
pub mod exchange;

#[cfg(feature = "redis")]
pub mod redis_store;

#[cfg(feature = "uc-bootstrap")]
pub mod editor;

#[cfg(feature = "uc-bootstrap")]
pub mod uc_bootstrap;

#[cfg(feature = "uc-bootstrap")]
pub mod seed;

pub use error::UcError;
pub use types::{Principal, PrincipalAttrs, ResolveBundle, ResolveRequest};

#[cfg(feature = "client")]
pub use uc_rest::{AuthMode, TableOperation, UcRestClient};

#[cfg(feature = "redis")]
pub use redis_store::{RedisFirstPolicyStore, RedisManifestStore};
