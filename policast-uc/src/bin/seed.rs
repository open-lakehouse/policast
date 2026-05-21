//! `policast-uc-seed` — one-shot binary that publishes the four
//! governance Delta tables from a flat-file store root into an object
//! store (typically MinIO for the compose `uc-full` profile, or real
//! S3 / ADLS / GCS for production).
//!
//! Example against a local MinIO running in the compose stack:
//!
//! ```bash
//! cargo run -p policast-uc --bin policast-uc-seed \
//!     --features sidecar,uc-bootstrap -- \
//!     --store-root examples/uc/store \
//!     --storage-uri-template s3://policast-demo/governance/policast/{table} \
//!     --storage-option AWS_ENDPOINT_URL=http://minio:9000 \
//!     --storage-option AWS_ACCESS_KEY_ID=minioadmin \
//!     --storage-option AWS_SECRET_ACCESS_KEY=minioadmin \
//!     --storage-option AWS_REGION=us-east-1 \
//!     --storage-option AWS_ALLOW_HTTP=true
//! ```
//!
//! The target table is refused if it already exists; pass `--overwrite`
//! to publish a fresh snapshot on top (each Delta commit is additive,
//! so the resolver's "last row wins per id" semantics still hold).

use std::path::PathBuf;

use clap::Parser;

use policast_uc::backend::FileBackend;
use policast_uc::seed::{seed_from_backend, SeedConfig};

#[derive(Parser, Debug)]
#[command(
    name = "policast-uc-seed",
    about = "Publish the flat-file store into governance Delta tables"
)]
struct Cli {
    /// Flat-file store root containing policies.json / manifest.json /
    /// bindings.json / tags.json.
    #[arg(long)]
    store_root: PathBuf,

    /// URI template with a literal `{table}` placeholder. Example:
    /// `s3://policast-demo/governance/policast/{table}`.
    #[arg(long)]
    storage_uri_template: String,

    /// Repeatable K=V pair forwarded verbatim to delta-rs's
    /// `object_store` backend. Same shape as the sidecar binary.
    #[arg(long = "storage-option", value_parser = parse_kv)]
    storage_options: Vec<(String, String)>,

    /// Append to any table that already exists instead of failing
    /// loudly. The default is to refuse, which protects against
    /// double-publishing a stale manifest by accident.
    #[arg(long)]
    overwrite: bool,
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

    let backend = FileBackend::new(&cli.store_root);

    let cfg = SeedConfig::new(&cli.storage_uri_template)
        .with_storage_options(cli.storage_options.clone())
        .with_overwrite(cli.overwrite);

    eprintln!("policast-uc-seed:");
    eprintln!("  store root:  {}", cli.store_root.display());
    eprintln!("  target template: {}", cli.storage_uri_template);
    eprintln!("  storage options: {} entries", cli.storage_options.len());
    eprintln!("  overwrite:   {}", cli.overwrite);

    let report = seed_from_backend(&backend, &cfg).await?;
    eprintln!(
        "seed complete: policies={} manifest={} bindings={} tags={}",
        report.policies, report.manifest, report.bindings, report.tags
    );
    Ok(())
}
