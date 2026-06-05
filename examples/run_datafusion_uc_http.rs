//! End-to-end demo: resolve a governed Delta table by **talking to the
//! running `policast-uc-sidecar` over HTTP**, then query it with
//! DataFusion.
//!
//! This is the HTTP twin of `run_datafusion_uc.rs`. The non-HTTP
//! variant builds a `ResolverCore` in-process and is useful for tests;
//! this variant proves the full wire contract end-to-end and is the
//! example exercised by the docker-compose `datafusion-demo` service.
//!
//! Configuration is env-driven so the same binary can run inside and
//! outside of Compose:
//!
//!   POLICAST_UC_ENDPOINT        default http://127.0.0.1:8765
//!   POLICAST_UC_SECRET          HMAC signing secret (required)
//!   POLICAST_PRINCIPAL_ID       default alice@hospital.com
//!   POLICAST_PRINCIPAL_ROLE    default analyst
//!   POLICAST_PRINCIPAL_REGION   default us-east     (only for analyst)
//!   POLICAST_PRINCIPAL_NAME     default Dr. Smith   (only for physician)
//!   POLICAST_TABLE              default hospital.clinical.patients
//!   POLICAST_GOVERNED_NAME      default: the last segment of
//!                               POLICAST_TABLE. The Cedar policies use
//!                               the short name (`patients`) in
//!                               `resource.table_name`; the UC resolver
//!                               uses the full three-part name. Keeping
//!                               the two separate lets column-mask CEL
//!                               like `resource.table_name == "patients"`
//!                               evaluate correctly while the resolve
//!                               request still talks to UC.
//!
//! Run with (after `docker compose up -d sidecar`):
//!   POLICAST_UC_SECRET=policast-uc-demo-secret \
//!     cargo run --example run_datafusion_uc_http --features "uc delta"

use std::sync::Arc;

use datafusion::arrow::array::{BooleanArray, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::*;
use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
use deltalake::operations::create::CreateBuilder;
use deltalake::operations::write::WriteBuilder;

use policast_datafusion::uc::{wrap_bundle, UcTableOptions};
use policast_uc::client::UcClientConfig;
use policast_uc::types::{Principal, PrincipalAttrs, ResolveRequest};

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Policast-CEL: UC Policy Store (HTTP) Demo ===\n");

    let endpoint = env_or("POLICAST_UC_ENDPOINT", "http://127.0.0.1:8765");
    let table = env_or("POLICAST_TABLE", "hospital.clinical.patients");
    // The Cedar policies in this repo reference the *short* name
    // (`resource.table_name == "patients"`) while UC addresses are
    // three-part. Accept an explicit override and otherwise derive the
    // short name from the last dotted segment.
    let governed_name = std::env::var("POLICAST_GOVERNED_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            table
                .rsplit('.')
                .next()
                .unwrap_or(table.as_str())
                .to_string()
        });

    println!("Step 1: Create a local Delta table of patients...");
    let tmp_dir = tempfile::tempdir()?;
    let table_uri = tmp_dir
        .path()
        .join("patients_delta")
        .to_str()
        .unwrap()
        .to_string();
    seed_patients_delta(&table_uri).await?;
    println!("  Delta table: {table_uri}\n");

    println!("Step 2: Build UcClient pointed at {endpoint}\n");
    let client = UcClientConfig::new(&endpoint)
        .with_signing_secret_env("POLICAST_UC_SECRET")?
        .build()?;

    let opts = UcTableOptions {
        storage_uri_override: Some(table_uri.clone()),
        ..Default::default()
    };

    println!("Step 3: Resolve over HTTP as the configured principal\n");
    let principal = principal_from_env();
    println!("  principal.id    = {}", principal.id);
    println!("  principal.role  = {}", principal.role);
    for (k, v) in principal.attrs.0.iter() {
        println!("  principal.{k:<6} = {v}");
    }
    println!();

    let req = ResolveRequest {
        table: table.clone(),
        principal: principal.clone(),
        requested_action: "query".into(),
    };
    let bundle = client.resolve(&req).await?;
    println!("  bindings applied: {:?}\n", bundle.bindings_applied);

    let governed = wrap_bundle(bundle, &governed_name, opts).await?;
    let ctx = SessionContext::new();
    ctx.register_table(governed_name.as_str(), Arc::new(governed))?;

    let sql = format!(
        "SELECT patient_id, name, ssn, diagnosis, region, legal_hold, treating_physician \
         FROM {governed_name}"
    );
    println!("Step 4: Query\n  {sql}\n");
    let df = ctx.sql(&sql).await?;
    df.show().await?;

    println!(
        "\n(Columns shown as `***` were masked by a column_mask policy \
         that applies to role `{}`.)",
        principal.role
    );

    Ok(())
}

fn principal_from_env() -> Principal {
    let id = env_or("POLICAST_PRINCIPAL_ID", "alice@hospital.com");
    let role = env_or("POLICAST_PRINCIPAL_ROLE", "analyst");

    let mut attrs = PrincipalAttrs::new();
    if let Ok(region) = std::env::var("POLICAST_PRINCIPAL_REGION") {
        if !region.is_empty() {
            attrs = attrs.with("region", region);
        }
    } else if role == "analyst" {
        attrs = attrs.with("region", "us-east");
    }
    if let Ok(name) = std::env::var("POLICAST_PRINCIPAL_NAME") {
        if !name.is_empty() {
            attrs = attrs.with("name", name);
        }
    } else if role == "physician" {
        attrs = attrs.with("name", "Dr. Smith");
    }

    Principal { id, role, attrs }
}

async fn seed_patients_delta(uri: &str) -> Result<(), Box<dyn std::error::Error>> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("patient_id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("ssn", DataType::Utf8, false),
        Field::new("diagnosis", DataType::Utf8, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("treating_physician", DataType::Utf8, false),
        Field::new("legal_hold", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec![
                "1001", "1002", "1003", "1004", "1005", "1006",
            ])),
            Arc::new(StringArray::from(vec![
                "Alice Johnson",
                "Bob Martinez",
                "Carol White",
                "David Kim",
                "Eva Chen",
                "Frank Brown",
            ])),
            Arc::new(StringArray::from(vec![
                "123-45-6789",
                "234-56-7890",
                "345-67-8901",
                "456-78-9012",
                "567-89-0123",
                "678-90-1234",
            ])),
            Arc::new(StringArray::from(vec![
                "Hypertension",
                "Diabetes Type 2",
                "Asthma",
                "Migraine",
                "Anemia",
                "Arthritis",
            ])),
            Arc::new(StringArray::from(vec![
                "us-east", "us-west", "us-east", "eu-west", "us-west", "us-east",
            ])),
            Arc::new(StringArray::from(vec![
                "Dr. Smith",
                "Dr. Lee",
                "Dr. Smith",
                "Dr. Mueller",
                "Dr. Lee",
                "Dr. Patel",
            ])),
            Arc::new(BooleanArray::from(vec![
                false, false, false, false, true, false,
            ])),
        ],
    )?;
    let delta_schema: deltalake::kernel::Schema = schema.as_ref().try_into_kernel()?;
    let table = CreateBuilder::new()
        .with_location(uri)
        .with_columns(delta_schema.fields().cloned())
        .await?;
    // deltalake 0.32: WriteBuilder::new takes `Option<EagerSnapshot>`.
    WriteBuilder::new(
        table.log_store(),
        Some(table.snapshot()?.snapshot().clone()),
    )
    .with_input_batches(vec![batch])
    .await?;
    Ok(())
}
