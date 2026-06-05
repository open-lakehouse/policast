//! End-to-end demo: compile Cedar policies, then query patients with
//! DataFusion governance (row filters + column masks).
//!
//! Run with:
//!   cargo run --example run_datafusion
//!
//! Requires: policast-core, policast-datafusion crates in the workspace.

use std::sync::Arc;

use datafusion::arrow::array::{BooleanArray, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::*;

use policast_core::{parse_policies, PolicyManifest};
use policast_datafusion::cel_filter::{build_column_masks, build_row_filters, QueryIdentity};
use policast_datafusion::GovernedTable;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Policast-CEL: DataFusion Governance Demo ===\n");

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
    // Step 2: Create in-memory patients table
    // ---------------------------------------------------------------
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

    let mem_table: Arc<dyn datafusion::datasource::TableProvider> =
        Arc::new(MemTable::try_new(schema.clone(), vec![vec![batch]])?);

    // ---------------------------------------------------------------
    // Step 3: Query as "analyst" in "us-east"
    // ---------------------------------------------------------------
    println!("Step 2: Query as analyst (region=us-east)\n");

    let analyst_identity = QueryIdentity {
        role: "analyst".into(),
        region: Some("us-east".into()),
        name: None,
    };

    let governed = GovernedTable::new(
        Arc::clone(&mem_table),
        manifest.clone(),
        "patients",
        analyst_identity.clone(),
    );

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed))?;

    let df = ctx
        .sql("SELECT patient_id, name, ssn, diagnosis, region, legal_hold FROM patients")
        .await?;
    println!("  Row filters applied: analyst sees only us-east, legal_hold=false rows");
    println!("  Column masks: SSN and diagnosis should be masked for analyst role\n");

    let masks = build_column_masks(&manifest, "patients", &analyst_identity);
    println!("  Masked columns: {:?}", masks);
    println!();

    df.show().await?;

    // ---------------------------------------------------------------
    // Step 4: Query as "physician" Dr. Smith
    // ---------------------------------------------------------------
    println!("\nStep 3: Query as physician (name=Dr. Smith)\n");

    let physician_identity = QueryIdentity {
        role: "physician".into(),
        region: None,
        name: Some("Dr. Smith".into()),
    };

    let row_filters = build_row_filters(&manifest, "patients", &physician_identity)?;
    let col_masks = build_column_masks(&manifest, "patients", &physician_identity);

    println!("  Row filters (count): {}", row_filters.len());
    println!("  Masked columns: {:?}", col_masks);
    println!("  (Physicians see SSN and diagnosis unmasked)");

    // ---------------------------------------------------------------
    // Step 5: Show the compiled manifest JSON
    // ---------------------------------------------------------------
    println!("\n=== Compiled Policy Manifest (JSON) ===\n");
    println!("{}", manifest.to_json()?);

    Ok(())
}
