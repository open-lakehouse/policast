//! policast-playground — a governed-query playground.
//!
//! Phase two of the policast-uc rethink. The playground lets you:
//!
//! * load one or more Cedar policies (compiled to CEL on the fly),
//! * pick a Unity Catalog table and a test principal (id/role/attrs),
//! * optionally register Unity Catalog `routine_body=SQL` functions as
//!   DataFusion scalar UDFs (so you can test a function before saving it),
//! * run SQL and see the **governed** result — the same row filters and
//!   column masks the engines enforce, applied through
//!   [`policast_datafusion::GovernedTable`].
//!
//! The crate is both a library (the [`engine`]) and a binary (an Axum
//! server that serves a self-contained SPA and a `POST /api/query`
//! endpoint).
//!
//! ## Layering
//!
//! * [`engine::execute`] is the low-level, fully testable path: it takes
//!   an already-resolved table URI + storage options and runs the
//!   governed query against the local/remote Delta table. No UC REST.
//! * [`engine::PlaygroundService`] adds the UC step: it resolves the
//!   table's storage location + temporary credentials via
//!   [`policast_uc::UcRestClient`] (honoring the `auth=on/off` toggle),
//!   then calls [`engine::execute`].

pub mod engine;
pub mod functions;
pub mod server;

pub use engine::{
    seed_demo_patients, ExecuteParams, FunctionDef, PlaygroundRequest, PlaygroundService,
    QueryResult, DEMO_TABLE_NAME,
};

use thiserror::Error;

/// Errors surfaced by the playground.
#[derive(Debug, Error)]
pub enum PlaygroundError {
    /// A Cedar policy failed to compile.
    #[error("policy compile error: {0}")]
    Compile(String),

    /// A Unity Catalog SQL function failed to register as a UDF.
    #[error("function `{name}` registration failed: {message}")]
    Function { name: String, message: String },

    /// Opening / scanning the backing Delta table failed.
    #[error("delta table error: {0}")]
    Delta(String),

    /// DataFusion planning / execution failed (often a bad SQL or a
    /// missing identity attribute referenced by a policy).
    #[error("query error: {0}")]
    Query(String),

    /// Talking to Unity Catalog (metadata or credential vending) failed.
    #[error("unity catalog error: {0}")]
    Uc(#[from] policast_uc::UcError),

    /// Bad request / configuration.
    #[error("invalid request: {0}")]
    Invalid(String),
}
