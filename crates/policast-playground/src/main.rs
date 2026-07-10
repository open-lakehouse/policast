//! `policast-playground` — serves the governed-query playground.
//!
//! ```bash
//! # Against the hosted UC with auth on:
//! cargo run -p policast-playground -- \
//!     --listen 127.0.0.1:8088 \
//!     --uc-endpoint https://uc.openlakehousedemos.dev \
//!     --uc-auth enable --uc-bearer-token-env UC_TOKEN
//!
//! # Against local docker UC (auth off) backed by MinIO:
//! cargo run -p policast-playground -- \
//!     --uc-endpoint http://localhost:8080 --uc-auth disable \
//!     --uc-storage-option AWS_ENDPOINT_URL=http://localhost:9000 \
//!     --uc-storage-option AWS_ALLOW_HTTP=true \
//!     --uc-storage-option AWS_REGION=us-east-1
//! ```

use std::collections::HashMap;

use clap::{Parser, ValueEnum};

use policast_playground::engine::PlaygroundService;
use policast_playground::server::router;
use policast_uc::AuthMode;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum AuthToggle {
    /// Authenticate UC REST calls with a static bearer token.
    Enable,
    /// Send no Authorization header (local docker / auth off).
    Disable,
}

#[derive(Parser, Debug)]
#[command(
    name = "policast-playground",
    about = "Governed-query policy playground"
)]
struct Cli {
    /// `host:port` to listen on.
    #[arg(long, default_value = "127.0.0.1:8088")]
    listen: String,

    /// Unity Catalog REST endpoint.
    #[arg(long, default_value = "https://uc.openlakehousedemos.dev")]
    uc_endpoint: String,

    /// Whether UC REST calls are authenticated.
    #[arg(long, value_enum, default_value_t = AuthToggle::Disable)]
    uc_auth: AuthToggle,

    /// Env var holding the UC bearer token. Required when `--uc-auth enable`.
    #[arg(long)]
    uc_bearer_token_env: Option<String>,

    /// `KEY=VALUE` storage option forwarded to delta-rs (repeatable);
    /// e.g. `AWS_REGION`, `AWS_ENDPOINT_URL`, `AWS_ALLOW_HTTP`.
    #[arg(long = "uc-storage-option", value_parser = parse_kv)]
    uc_storage_options: Vec<(String, String)>,

    /// `NAME=URI` local table override (repeatable). Tables listed here
    /// are opened directly from the URI, bypassing UC — handy for local
    /// dev / offline use, e.g.
    /// `--local-table hospital.clinical.patients=file:///data/patients`.
    #[arg(long = "local-table", value_parser = parse_kv)]
    local_tables: Vec<(String, String)>,

    /// Seed a self-contained demo `patients` Delta table at this URI
    /// (idempotent) and register it as `hospital.clinical.patients` via
    /// a local-table override. With this set the playground works with
    /// no UC at all — point the UI at `hospital.clinical.patients`.
    #[arg(long)]
    demo_table: Option<String>,

    /// Default row cap for query results.
    #[arg(long, default_value_t = policast_playground::engine::DEFAULT_LIMIT)]
    default_limit: usize,
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

    let auth_enabled = cli.uc_auth == AuthToggle::Enable;
    let token = match cli.uc_bearer_token_env.as_ref() {
        Some(env) => Some(std::env::var(env).map_err(|_| {
            format!("env var {env} is not set; use --uc-bearer-token-env to change")
        })?),
        None => None,
    };
    let auth = AuthMode::from_flag(auth_enabled, token)?;

    let storage_options: HashMap<String, String> = cli.uc_storage_options.iter().cloned().collect();

    let mut service = PlaygroundService::new(cli.uc_endpoint.clone(), auth.clone())
        .with_storage_options(storage_options)
        .with_default_limit(cli.default_limit);

    // Optional self-contained demo table: seed it + register the override.
    if let Some(uri) = cli.demo_table.as_ref() {
        policast_playground::seed_demo_patients(uri).await?;
        eprintln!(
            "  demo table:  {} -> {uri}",
            policast_playground::DEMO_TABLE_NAME
        );
        service = service.with_local_table(policast_playground::DEMO_TABLE_NAME, uri);
    }
    for (name, uri) in &cli.local_tables {
        service = service.with_local_table(name, uri);
    }

    let app = router(service);

    let listener = tokio::net::TcpListener::bind(&cli.listen).await?;
    let addr = listener.local_addr()?;
    eprintln!("policast-playground listening on http://{addr}");
    eprintln!("  uc endpoint: {}", cli.uc_endpoint);
    eprintln!(
        "  uc auth:     {}",
        if auth.is_enabled() {
            "enabled (bearer)"
        } else {
            "disabled"
        }
    );
    axum::serve(listener, app).await?;
    Ok(())
}
