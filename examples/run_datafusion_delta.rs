//! End-to-end demo: compile Cedar policies, create a local Delta table,
//! then query it with DataFusion governance (row filters + column masks).
//!
//! Run with:
//!   cargo run --example run_datafusion_delta --features delta
//!
//! Requires: policast-core, policast-datafusion crates in the workspace.

use std::sync::Arc;

use datafusion::arrow::array::{BooleanArray, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::*;
use deltalake::kernel::engine::arrow_conversion::TryIntoKernel;
use deltalake::operations::create::CreateBuilder;
use deltalake::operations::write::WriteBuilder;

use policast_core::{parse_policies, PolicyManifest};
use policast_datafusion::cel_filter::QueryIdentity;
use policast_datafusion::delta::wrap_delta_table;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Policast-CEL: DataFusion + Delta Lake Governance Demo ===\n");

    // ---------------------------------------------------------------
    // Step 1: Compile Cedar policies into a manifest
    // ---------------------------------------------------------------
    println!("Step 1: Compiling Cedar policies...\n");

    let cedar_sources = &[
        include_str!("policies/row_filter.cedar"),
        include_str!("policies/column_mask.cedar"),
        include_str!("policies/deny_legal_hold.cedar"),
    ];

    let mut manifest = PolicyManifest::new();
    for src in cedar_sources {
        let parsed = parse_policies(src)?;
        manifest.compile_policies(&parsed)?;
    }

    println!("Compiled {} policies:", manifest.policies.len());
    for p in &manifest.policies {
        println!(
            "  [{:?}] {} -> CEL: {}",
            p.filter_type, p.id, p.cel_expression
        );
    }
    println!();

    // ---------------------------------------------------------------
    // Step 2: Create a local Delta table with patient data
    // ---------------------------------------------------------------
    println!("Step 2: Creating Delta table...\n");

    let tmp_dir = tempfile::tempdir()?;
    let table_path = tmp_dir.path().join("patients_delta");
    let table_uri = table_path.to_str().unwrap();

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

    // deltalake 0.32 / delta_kernel: arrow<->kernel conversions moved off the
    // std `TryFrom` impls onto kernel-specific `TryIntoKernel`/`TryFromArrow`.
    let delta_schema: deltalake::kernel::Schema = schema.as_ref().try_into_kernel()?;
    let table = CreateBuilder::new()
        .with_location(table_uri)
        .with_columns(delta_schema.fields().cloned())
        .await?;

    // deltalake 0.32: WriteBuilder::new now takes `Option<EagerSnapshot>`
    // (the table's loaded snapshot) rather than `Option<DeltaTableState>`.
    let table = WriteBuilder::new(table.log_store(), Some(table.snapshot()?.snapshot().clone()))
        .with_input_batches(vec![batch])
        .await?;

    println!("  Delta table created at: {table_uri}");
    println!(
        "  Version: {}",
        table.version().unwrap_or_default()
    );
    println!();

    // ---------------------------------------------------------------
    // Step 3: Query as "analyst" in "us-east" via GovernedTable
    // ---------------------------------------------------------------
    println!("Step 3: Query as analyst (region=us-east)\n");

    let analyst_identity = QueryIdentity {
        role: "analyst".into(),
        region: Some("us-east".into()),
        name: None,
    };

    // deltalake 0.32: `DeltaTable` no longer implements `TableProvider`, so
    // `wrap_delta_table` is now async + fallible (it builds a provider).
    let governed = wrap_delta_table(
        table,
        manifest.clone(),
        "patients",
        analyst_identity,
    )
    .await?;

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed))?;

    println!("  Governance applied: row filters + column masks");
    println!("  Analyst sees only us-east, non-legal-hold rows");
    println!("  SSN column is masked for analyst role\n");

    let df = ctx
        .sql("SELECT patient_id, name, ssn, diagnosis, region, legal_hold FROM patients")
        .await?;
    df.show().await?;

    // ---------------------------------------------------------------
    // Step 4: Show the compiled manifest JSON
    // ---------------------------------------------------------------
    println!("\n=== Compiled Policy Manifest (JSON) ===\n");
    println!("{}", manifest.to_json()?);

    Ok(())
}
