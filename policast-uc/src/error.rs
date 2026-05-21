use thiserror::Error;

/// Errors the UC policy store can raise.
#[derive(Error, Debug)]
pub enum UcError {
    /// The HMAC signature on a returned bundle did not verify. The
    /// caller should discard the bundle and refuse to scan.
    #[error("resolve bundle signature did not verify")]
    BadSignature,

    /// The bundle has already expired.
    #[error("resolve bundle expired at {0}")]
    Expired(String),

    /// The resolver returned an error status.
    #[error("resolve endpoint error: {0}")]
    Resolve(String),

    /// Wrapping serde-json failures.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Wrapping io failures (flat-file backends).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Wrapping core errors so the `PolicyStore` impl can surface them.
    #[error("policast-core error: {0}")]
    Core(#[from] policast_core::PolicastError),

    /// Wrapping reqwest failures.
    #[cfg(feature = "client")]
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// Bad configuration (missing env var, malformed URL).
    #[error("configuration error: {0}")]
    Config(String),

    /// Malformed input to signature/cache/etc.
    #[error("invalid argument: {0}")]
    Invalid(String),
}
