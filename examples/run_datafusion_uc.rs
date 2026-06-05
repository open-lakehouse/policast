//! End-to-end demo: resolve a governed Delta table via the UC-style
//! policy store (the Axum sidecar backed by `examples/uc/store/`),
//! then query it with DataFusion.
//!
//! The same Cedar→CEL→FilterExec/ProjectionExec enforcement path is
//! exercised; only the manifest *source* changes. Two identities are
//! resolved back-to-back (analyst vs physician) so you can see the
//! different physical plans and different results.
//!
//! Run with:
//!   cargo run --example run_datafusion_uc --features "uc delta"
//!
//! Requires: policast-core, policast-uc, policast-datafusion.

use std::sync::Arc;

use datafusion::arrow::array::{BooleanArray, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::*;
use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
use deltalake::operations::create::CreateBuilder;
use deltalake::operations::write::WriteBuilder;

use policast_datafusion::uc::{wrap_bundle, UcTableOptions};
use policast_uc::backend::FileBackend;
use policast_uc::store::ResolverCore;
use policast_uc::types::{Principal, PrincipalAttrs, ResolveRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Policast-CEL: UC Policy Store (DataFusion) Demo ===\n");

    // ---------------------------------------------------------------
    // Step 1: Materialize a local Delta table to scan. In production
    // the Delta URI + credentials come back on the ResolveBundle; here
    // we create a local table and tell the GovernedTable to use it via
    // storage_uri_override.
    // ---------------------------------------------------------------
    println!("Step 1: Create a local Delta table of patients...\n");
    let tmp_dir = tempfile::tempdir()?;
    let table_uri = tmp_dir
        .path()
        .join("patients_delta")
        .to_str()
        .unwrap()
        .to_string();
    seed_patients_delta(&table_uri).await?;
    println!("  Delta table: {table_uri}\n");

    // ---------------------------------------------------------------
    // Step 2: Stand up the resolver against the flat-file store in
    // examples/uc/store. This is the same contract the
    // `policast-uc-sidecar` binary serves over HTTP; for the demo we
    // call it in-process.
    // ---------------------------------------------------------------
    println!("Step 2: Build the resolver against examples/uc/store ...\n");
    // CARGO_MANIFEST_DIR points at policast-datafusion because that's
    // the crate this example is attached to. Walk up one level to the
    // workspace root.
    let store_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("examples/uc/store");
    let backend = Arc::new(FileBackend::new(store_root));
    let secret = b"policast-uc-demo-secret".to_vec();
    let core =
        ResolverCore::new(backend, secret.clone()).with_storage_uri_template(table_uri.clone());

    // ---------------------------------------------------------------
    // Step 3: Resolve for an analyst in us-east and show the filtered
    // query.
    // ---------------------------------------------------------------
    println!("Step 3: Resolve as analyst (region=us-east)\n");
    let analyst = Principal {
        id: "alice@hospital.com".into(),
        role: "analyst".into(),
        attrs: PrincipalAttrs::new().with("region", "us-east"),
    };
    let analyst_bundle = core
        .resolve(&ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: analyst.clone(),
            requested_action: "query".into(),
        })
        .await?;
    println!(
        "  bundle.bindings_applied = {:?}",
        analyst_bundle.bindings_applied
    );
    println!(
        "  identity_claims        = {:?}\n",
        analyst_bundle.identity_claims
    );

    let governed_analyst =
        wrap_bundle(analyst_bundle, "patients", UcTableOptions::default()).await?;
    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed_analyst))?;
    let df = ctx
        .sql("SELECT patient_id, name, ssn, diagnosis, region, legal_hold FROM patients")
        .await?;
    df.show().await?;
    println!();

    // ---------------------------------------------------------------
    // Step 4: Resolve for a physician and show the different result.
    // ---------------------------------------------------------------
    println!("Step 4: Resolve as physician (name=Dr. Smith)\n");
    let physician = Principal {
        id: "dr-smith@hospital.com".into(),
        role: "physician".into(),
        attrs: PrincipalAttrs::new().with("name", "Dr. Smith"),
    };
    let phys_bundle = core
        .resolve(&ResolveRequest {
            table: "hospital.clinical.patients".into(),
            principal: physician.clone(),
            requested_action: "query".into(),
        })
        .await?;
    println!(
        "  bundle.bindings_applied = {:?}",
        phys_bundle.bindings_applied
    );
    println!(
        "  identity_claims        = {:?}\n",
        phys_bundle.identity_claims
    );

    let governed_phys = wrap_bundle(phys_bundle, "patients", UcTableOptions::default()).await?;
    let ctx2 = SessionContext::new();
    ctx2.register_table("patients", Arc::new(governed_phys))?;
    let df2 = ctx2
        .sql("SELECT patient_id, name, ssn, diagnosis, region, legal_hold, treating_physician FROM patients")
        .await?;
    df2.show().await?;

    Ok(())
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
