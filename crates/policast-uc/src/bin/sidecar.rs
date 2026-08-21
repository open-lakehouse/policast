//! `policast-uc-sidecar` — stand-alone Axum service implementing the
//! `/policies/resolve` contract described in
//! `research/unity-catalog-policy-store.md`.
//!
//! Two backends are supported:
//!
//! * `--backend file` (default) — reads `policies.json` / `manifest.json`
//!   / `bindings.json` / `tags.json` from `--store-root`. Zero UC
//!   dependency, ideal for development and the base compose stack.
//!
//! * `--backend uc-bootstrap` — snapshots the four governance Delta
//!   tables (`policies`, `manifest`, `bindings`, `tags`) on startup
//!   and refreshes them every `--uc-refresh-interval-secs`.
//!
//!   Two table-access modes are supported:
//!   1. static URI template + storage options (`--uc-storage-*`) for
//!      RustFS/local demos,
//!   2. per-table UC REST location + temporary credential vending when
//!      `--uc-storage-uri-template` is omitted.
//!
//!   Requires the `uc-bootstrap` Cargo feature.
//!
//! File-backed example:
//!
//! ```bash
//! cargo run -p policast-uc --bin policast-uc-sidecar --features sidecar -- \
//!     --listen 127.0.0.1:8765 \
//!     --store-root examples/uc/store \
//!     --signing-secret-env POLICAST_UC_SECRET
//! ```
//!
//! UC-bootstrap example (RustFS-backed compose demo):
//!
//! ```bash
//! cargo run -p policast-uc --bin policast-uc-sidecar \
//!     --features sidecar,uc-bootstrap -- \
//!     --listen 0.0.0.0:8765 \
//!     --backend uc-bootstrap \
//!     --uc-storage-uri-template s3://policast-demo/governance/policast/{table} \
//!     --uc-storage-option AWS_ENDPOINT_URL=http://rustfs:9000 \
//!     --uc-storage-option AWS_ACCESS_KEY_ID=... \
//!     --uc-storage-option AWS_SECRET_ACCESS_KEY=... \
//!     --uc-storage-option AWS_REGION=us-east-1 \
//!     --uc-storage-option AWS_ALLOW_HTTP=true \
//!     --uc-refresh-interval-secs 30 \
//!     --signing-secret-env POLICAST_UC_SECRET
//! ```
//!
//! UC REST-vended credential example:
//!
//! ```bash
//! cargo run -p policast-uc --bin policast-uc-sidecar \
//!     --features sidecar,uc-bootstrap -- \
//!     --listen 0.0.0.0:8765 \
//!     --backend uc-bootstrap \
//!     --uc-endpoint http://unitycatalog:8080 \
//!     --uc-bearer-token-env UC_BEARER_TOKEN \
//!     --uc-refresh-interval-secs 30 \
//!     --signing-secret-env POLICAST_UC_SECRET
//! ```

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use policast_uc::sidecar::file_sidecar;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum BackendKind {
    /// Flat JSON files at `--store-root`.
    File,
    /// Unity Catalog / Delta-backed snapshot + refresh task.
    /// Requires the `uc-bootstrap` Cargo feature.
    #[value(name = "uc-bootstrap")]
    UcBootstrap,
}

#[derive(Parser, Debug)]
#[command(
    name = "policast-uc-sidecar",
    about = "Resolver sidecar for the policast-cel UC policy store"
)]
struct Cli {
    /// `host:port` to listen on.
    #[arg(long, default_value = "127.0.0.1:8765")]
    listen: String,

    /// Which backend to use behind the sidecar. Defaults to `file`
    /// so existing `--store-root` invocations keep working unchanged.
    #[arg(long, value_enum, default_value_t = BackendKind::File)]
    backend: BackendKind,

    /// Directory containing policies.json / manifest.json / bindings.json
    /// (+ optional tags.json). Required when `--backend file`; ignored
    /// otherwise.
    #[arg(long)]
    store_root: Option<PathBuf>,

    /// Name of an environment variable holding the HMAC signing secret.
    #[arg(long, default_value = "POLICAST_UC_SECRET")]
    signing_secret_env: String,

    // -------- UC bootstrap backend options --------
    //
    // These flags are only meaningful when `--backend uc-bootstrap`
    // AND the binary was compiled with `--features uc-bootstrap`.
    // We accept them unconditionally at parse time so `--help` is
    // identical across feature configurations; the handler below
    // errors if the feature is missing.
    /// Optional URI template for the four governance Delta tables. The literal
    /// substring `{table}` is replaced with `policies` / `manifest` /
    /// `bindings` / `tags` on open. Example:
    /// `s3://policast-demo/governance/policast/{table}`.
    ///
    /// If omitted, the backend resolves table locations via UC REST
    /// (`GET /api/2.1/unity-catalog/tables/{full_name}`).
    #[arg(long)]
    uc_storage_uri_template: Option<String>,

    /// Key=value pair forwarded verbatim to `delta-rs`'s object_store
    /// backend. May be repeated. Typical RustFS shape:
    ///
    /// ```text
    /// --uc-storage-option AWS_ENDPOINT_URL=http://rustfs:9000
    /// --uc-storage-option AWS_ACCESS_KEY_ID=...
    /// --uc-storage-option AWS_SECRET_ACCESS_KEY=...
    /// --uc-storage-option AWS_REGION=us-east-1
    /// --uc-storage-option AWS_ALLOW_HTTP=true
    /// ```
    #[arg(long = "uc-storage-option", value_parser = parse_kv)]
    uc_storage_options: Vec<(String, String)>,

    /// How often the refresh task re-snapshots the governance tables
    /// (seconds). `0` disables the refresh task entirely (one-shot
    /// snapshot; use for tests and debugging).
    #[arg(long, default_value_t = 30)]
    uc_refresh_interval_secs: u64,

    /// Unity Catalog REST endpoint used for per-table metadata and
    /// temporary credential vending in UC mode. Defaults to the hosted
    /// demo catalog; point it at `http://unitycatalog:8080` for local
    /// docker.
    #[arg(long, default_value = "https://uc.openlakehousedemos.dev")]
    uc_endpoint: String,

    /// Whether UC REST calls are authenticated. `enable` attaches a
    /// static bearer token from `--uc-bearer-token-env` (the hosted-UC
    /// path); `disable` sends no Authorization header (local docker with
    /// `server.authorization=disable`).
    #[arg(long, value_enum, default_value_t = AuthToggle::Disable)]
    uc_auth: AuthToggle,

    /// Name of an env var containing a UC bearer token. Required when
    /// `--uc-auth enable`; ignored when `--uc-auth disable`.
    #[arg(long)]
    uc_bearer_token_env: Option<String>,

    /// Catalog holding the four `policast` governance tables.
    #[arg(long, default_value = "governance")]
    governance_catalog: String,

    /// Schema (inside `--governance-catalog`) holding the governance tables.
    #[arg(long, default_value = "policast")]
    governance_schema: String,

    /// Role that may edit any policy regardless of ownership (the
    /// owner-aware edit API otherwise requires the requester to be the
    /// policy's `@owner`). Only meaningful with `--backend uc-bootstrap`.
    #[arg(long, default_value = "governance_admin")]
    admin_role: String,

    /// Redis URL for the materialized protobuf PolicyStore (e.g.
    /// `redis://127.0.0.1:6379`). When set (and the binary was built
    /// with `--features redis`), the governance snapshot is materialized
    /// into Redis and policy edits re-flush it. Only meaningful with
    /// `--backend uc-bootstrap`.
    #[arg(long)]
    redis_url: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum AuthToggle {
    /// Authenticate UC REST calls with a static bearer token.
    Enable,
    /// Send no Authorization header (local docker / auth off).
    Disable,
}

fn parse_kv(input: &str) -> Result<(String, String), String> {
    let (k, v) = input
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got `{input}`"))?;
    if k.is_empty() {
        return Err(format!("empty key in `{input}`"));
    }
    Ok((k.to_string(), v.to_string()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let secret = std::env::var(&cli.signing_secret_env).map_err(|_| {
        format!(
            "env var {} is not set; use --signing-secret-env to change",
            cli.signing_secret_env
        )
    })?;

    let router = match cli.backend {
        BackendKind::File => build_file_router(&cli, secret.into_bytes())?,
        BackendKind::UcBootstrap => build_uc_bootstrap_router(&cli, secret.into_bytes()).await?,
    };

    let listener = tokio::net::TcpListener::bind(&cli.listen).await?;
    let addr = listener.local_addr()?;
    eprintln!("policast-uc-sidecar listening on http://{addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn build_file_router(
    cli: &Cli,
    secret: Vec<u8>,
) -> Result<axum::Router, Box<dyn std::error::Error>> {
    let root = cli
        .store_root
        .as_ref()
        .ok_or("--store-root is required when --backend file")?;
    eprintln!("  backend: file");
    eprintln!("  store root: {}", root.display());
    Ok(file_sidecar(root, secret))
}

#[cfg(feature = "uc-bootstrap")]
async fn build_uc_bootstrap_router(
    cli: &Cli,
    secret: Vec<u8>,
) -> Result<axum::Router, Box<dyn std::error::Error>> {
    use std::collections::HashMap;
    use std::time::Duration;

    use std::sync::Arc;

    use policast_uc::backend::ResolveBackend;
    use policast_uc::editor::PolicyEditor;
    use policast_uc::sidecar::{editor_router, uc_bootstrap_sidecar, EditorState};
    use policast_uc::uc_bootstrap::{UcBootstrapBackend, UcBootstrapConfig};
    use policast_uc::AuthMode;

    let storage_options: HashMap<String, String> = cli.uc_storage_options.iter().cloned().collect();

    let refresh_interval = if cli.uc_refresh_interval_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(cli.uc_refresh_interval_secs))
    };

    // Resolve the auth toggle into a concrete AuthMode. When enabled,
    // the bearer token must be supplied via --uc-bearer-token-env.
    let auth_enabled = cli.uc_auth == AuthToggle::Enable;
    let token = match cli.uc_bearer_token_env.as_ref() {
        Some(env) => Some(std::env::var(env).map_err(|_| {
            format!("env var {env} is not set; use --uc-bearer-token-env to change")
        })?),
        None => None,
    };
    let auth = AuthMode::from_flag(auth_enabled, token)?;

    let mut cfg = UcBootstrapConfig::for_example_stack(cli.uc_endpoint.clone())
        .with_storage_options(storage_options)
        .with_auth(auth.clone());
    cfg.governance_catalog = cli.governance_catalog.clone();
    cfg.governance_schema = cli.governance_schema.clone();

    if let Some(template) = cli.uc_storage_uri_template.as_ref() {
        cfg = cfg.with_storage_uri_template(template);
    }
    cfg.refresh_interval = refresh_interval;

    eprintln!("  backend: uc-bootstrap");
    eprintln!("  uc endpoint:    {}", cli.uc_endpoint);
    eprintln!(
        "  uc auth:        {}",
        if auth.is_enabled() {
            "enabled (bearer)"
        } else {
            "disabled"
        }
    );
    eprintln!(
        "  storage template: {}",
        cli.uc_storage_uri_template
            .as_deref()
            .unwrap_or("<resolved via UC REST>")
    );
    eprintln!(
        "  storage options:  {} entries",
        cli.uc_storage_options.len()
    );
    eprintln!(
        "  refresh interval: {}",
        refresh_interval
            .map(|d| format!("{}s", d.as_secs()))
            .unwrap_or_else(|| "disabled".into())
    );

    // The editor reuses the same governance-table config for its
    // READ_WRITE write-back path.
    let editor_cfg = cfg.clone();

    let backend = UcBootstrapBackend::bootstrap(cfg)
        .await
        .map_err(|e| format!("uc-bootstrap backend failed to start: {e}"))?;

    // Best-effort liveness log so operators can see that the initial
    // snapshot loaded something sensible before the sidecar starts
    // accepting resolve traffic.
    {
        let policies = backend
            .policies()
            .await
            .map(|p| p.len())
            .unwrap_or_default();
        let tags = backend.tags().await.map(|t| t.len()).unwrap_or_default();
        eprintln!(
            "  initial snapshot: {} policies, {} tag rows",
            policies, tags
        );
    }

    // Owner-aware policy edit API.
    let editor = Arc::new(PolicyEditor::new(editor_cfg, cli.admin_role.clone()));
    eprintln!("  admin role:     {}", cli.admin_role);
    let editor_state = EditorState::new(editor);

    // Optional Redis materialized store + edit reflush.
    #[cfg(feature = "redis")]
    let editor_state = {
        let mut state = editor_state;
        if let Some(url) = cli.redis_url.as_ref() {
            use policast_uc::redis_store::RedisManifestStore;
            use policast_uc::sidecar::Reflush;
            let store = RedisManifestStore::connect(url)
                .await
                .map_err(|e| format!("redis connect ({url}): {e}"))?;
            let tables = backend
                .materialize_into(&store)
                .await
                .map_err(|e| format!("initial redis materialize: {e}"))?;
            eprintln!("  redis: materialized {} table(s) to {url}", tables.len());
            state = state.with_reflush(Reflush {
                backend: backend.clone(),
                store,
            });
        }
        state
    };

    #[cfg(not(feature = "redis"))]
    if cli.redis_url.is_some() {
        eprintln!(
            "  warning: --redis-url set but the binary was built without \
             the `redis` feature; ignoring"
        );
    }

    let base = uc_bootstrap_sidecar(backend, secret);
    Ok(base.merge(editor_router(editor_state)))
}

#[cfg(not(feature = "uc-bootstrap"))]
async fn build_uc_bootstrap_router(
    _cli: &Cli,
    _secret: Vec<u8>,
) -> Result<axum::Router, Box<dyn std::error::Error>> {
    Err(
        "this binary was built without the `uc-bootstrap` Cargo feature; \
         rebuild with `--features sidecar,uc-bootstrap` to enable \
         --backend uc-bootstrap"
            .into(),
    )
}
