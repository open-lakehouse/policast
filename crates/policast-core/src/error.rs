use thiserror::Error;

#[derive(Error, Debug)]
pub enum PolicastError {
    #[error("Cedar parse error: {0}")]
    CedarParse(String),

    #[error("CEL emission error: {0}")]
    CelEmit(String),

    #[error("Policy manifest error: {0}")]
    Manifest(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Policy cache error: {0}")]
    Cache(String),
}
