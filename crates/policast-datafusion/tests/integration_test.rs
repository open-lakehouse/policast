use std::sync::Arc;

use datafusion::arrow::array::{BooleanArray, Int64Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::*;

use policast_core::model::{CompiledPolicy, Effect, FilterType};
use policast_core::PolicyManifest;
use policast_datafusion::cel_filter::QueryIdentity;
use policast_datafusion::GovernedTable;

fn patients_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("patient_id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("ssn", DataType::Utf8, false),
        Field::new("region", DataType::Utf8, false),
        Field::new("legal_hold", DataType::Boolean, false),
    ]))
}

fn patients_batch() -> RecordBatch {
    RecordBatch::try_new(
        patients_schema(),
        vec![
            Arc::new(StringArray::from(vec!["1001", "1002", "1003", "1004"])),
            Arc::new(StringArray::from(vec!["Alice", "Bob", "Carol", "David"])),
            Arc::new(StringArray::from(vec![
                "111-11-1111",
                "222-22-2222",
                "333-33-3333",
                "444-44-4444",
            ])),
            Arc::new(StringArray::from(vec![
                "us-east", "us-west", "us-east", "eu-west",
            ])),
            Arc::new(BooleanArray::from(vec![false, false, true, false])),
        ],
    )
    .unwrap()
}

fn memtable() -> Arc<dyn datafusion::datasource::TableProvider> {
    Arc::new(MemTable::try_new(patients_schema(), vec![vec![patients_batch()]]).unwrap())
}

fn full_manifest() -> PolicyManifest {
    PolicyManifest {
        version: "1.0".into(),
        policies: vec![
            CompiledPolicy {
                id: "region_filter".into(),
                effect: Effect::Permit,
                filter_type: FilterType::RowFilter,
                target_table: "patients".into(),
                column: None,
                target_tag: None,
                applies_to_tag: None,
                cel_expression: "(resource.region == principal.region)".into(),
                applies_to: None,
                description: None,
            },
            CompiledPolicy {
                id: "deny_legal_hold".into(),
                effect: Effect::Forbid,
                filter_type: FilterType::DenyOverride,
                target_table: "patients".into(),
                column: None,
                target_tag: None,
                applies_to_tag: None,
                cel_expression:
                    "(resource.legal_hold == true) && !(principal.role == \"legal\")"
                        .into(),
                applies_to: None,
                description: None,
            },
            CompiledPolicy {
                id: "mask_ssn".into(),
                effect: Effect::Forbid,
                filter_type: FilterType::ColumnMask,
                target_table: "patients".into(),
                column: Some("ssn".into()),
                target_tag: None,
                applies_to_tag: None,
                cel_expression:
                    "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))"
                        .into(),
                applies_to: None,
                description: None,
            },
        ],
        principal_contract: None,
    }
}

#[tokio::test]
async fn test_analyst_sees_only_own_region_and_masked_ssn() {
    let identity = QueryIdentity {
        role: "analyst".into(),
        region: Some("us-east".into()),
        name: None,
    };

    let governed = GovernedTable::new(memtable(), full_manifest(), "patients", identity);

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed)).unwrap();

    let df = ctx
        .sql("SELECT patient_id, name, ssn, region, legal_hold FROM patients ORDER BY patient_id")
        .await
        .unwrap();

    let batches = df.collect().await.unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];

    // Should only see us-east rows with legal_hold=false → Alice (1001)
    // Carol (1003) has legal_hold=true and us-east, so she's excluded by deny override
    assert_eq!(
        batch.num_rows(),
        1,
        "analyst should see 1 row (us-east, non-legal-hold)"
    );

    let patient_ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(patient_ids.value(0), "1001");

    let ssn_col = batch
        .column(2)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(ssn_col.value(0), "***", "SSN should be masked for analyst");
}

#[tokio::test]
async fn test_admin_sees_all_rows_unmasked() {
    let manifest = PolicyManifest {
        version: "1.0".into(),
        policies: vec![CompiledPolicy {
            id: "mask_ssn".into(),
            effect: Effect::Forbid,
            filter_type: FilterType::ColumnMask,
            target_table: "patients".into(),
            column: Some("ssn".into()),
            target_tag: None,
            applies_to_tag: None,
            cel_expression:
                "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))"
                    .into(),
            applies_to: None,
            description: None,
        }],
        principal_contract: None,
    };

    let identity = QueryIdentity {
        role: "admin".into(),
        region: None,
        name: None,
    };

    let governed = GovernedTable::new(memtable(), manifest, "patients", identity);

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed)).unwrap();

    let df = ctx
        .sql("SELECT patient_id, ssn FROM patients ORDER BY patient_id")
        .await
        .unwrap();
    let batches = df.collect().await.unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];

    assert_eq!(batch.num_rows(), 4, "admin sees all rows (no row filters)");

    let ssn_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(
        ssn_col.value(0),
        "111-11-1111",
        "admin SSN should NOT be masked"
    );
}

#[tokio::test]
async fn test_legal_user_sees_legal_hold_rows() {
    let manifest = PolicyManifest {
        version: "1.0".into(),
        policies: vec![CompiledPolicy {
            id: "deny_legal_hold".into(),
            effect: Effect::Forbid,
            filter_type: FilterType::DenyOverride,
            target_table: "patients".into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: "(resource.legal_hold == true) && !(principal.role == \"legal\")"
                .into(),
            applies_to: None,
            description: None,
        }],
        principal_contract: None,
    };

    let identity = QueryIdentity {
        role: "legal".into(),
        region: None,
        name: None,
    };

    let governed = GovernedTable::new(memtable(), manifest, "patients", identity);

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed)).unwrap();

    let df = ctx
        .sql("SELECT patient_id FROM patients ORDER BY patient_id")
        .await
        .unwrap();
    let batches = df.collect().await.unwrap();
    let batch = &batches[0];

    assert_eq!(
        batch.num_rows(),
        4,
        "legal user should see all rows including legal_hold=true"
    );
}

#[tokio::test]
async fn test_deny_override_blocks_legal_hold() {
    let manifest = PolicyManifest {
        version: "1.0".into(),
        policies: vec![CompiledPolicy {
            id: "deny_legal_hold".into(),
            effect: Effect::Forbid,
            filter_type: FilterType::DenyOverride,
            target_table: "patients".into(),
            column: None,
            target_tag: None,
            applies_to_tag: None,
            cel_expression: "(resource.legal_hold == true) && !(principal.role == \"legal\")"
                .into(),
            applies_to: None,
            description: None,
        }],
        principal_contract: None,
    };

    let identity = QueryIdentity {
        role: "analyst".into(),
        region: None,
        name: None,
    };

    let governed = GovernedTable::new(memtable(), manifest, "patients", identity);

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed)).unwrap();

    let df = ctx
        .sql("SELECT patient_id, legal_hold FROM patients ORDER BY patient_id")
        .await
        .unwrap();
    let batches = df.collect().await.unwrap();
    let batch = &batches[0];

    assert_eq!(
        batch.num_rows(),
        3,
        "non-legal user should NOT see legal_hold=true row"
    );

    let patient_ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let ids: Vec<&str> = (0..batch.num_rows())
        .map(|i| patient_ids.value(i))
        .collect();
    assert!(
        !ids.contains(&"1003"),
        "Carol (legal_hold=true) should be excluded"
    );
}

#[tokio::test]
async fn test_no_policies_means_no_governance() {
    let manifest = PolicyManifest {
        version: "1.0".into(),
        policies: vec![],
        principal_contract: None,
    };

    let identity = QueryIdentity {
        role: "anyone".into(),
        region: None,
        name: None,
    };

    let governed = GovernedTable::new(memtable(), manifest, "patients", identity);

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed)).unwrap();

    let df = ctx
        .sql("SELECT COUNT(*) as cnt FROM patients")
        .await
        .unwrap();
    let batches = df.collect().await.unwrap();
    let batch = &batches[0];
    let count = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0);
    assert_eq!(count, 4, "no policies should mean all rows visible");
}

#[tokio::test]
async fn test_column_mask_with_select_star() {
    let manifest = PolicyManifest {
        version: "1.0".into(),
        policies: vec![CompiledPolicy {
            id: "mask_ssn".into(),
            effect: Effect::Forbid,
            filter_type: FilterType::ColumnMask,
            target_table: "patients".into(),
            column: Some("ssn".into()),
            target_tag: None,
            applies_to_tag: None,
            cel_expression:
                "(resource.table_name == \"patients\") && !((principal.role == \"admin\"))".into(),
            applies_to: None,
            description: None,
        }],
        principal_contract: None,
    };

    let identity = QueryIdentity {
        role: "viewer".into(),
        region: None,
        name: None,
    };

    let governed = GovernedTable::new(memtable(), manifest, "patients", identity);

    let ctx = SessionContext::new();
    ctx.register_table("patients", Arc::new(governed)).unwrap();

    let df = ctx
        .sql("SELECT * FROM patients ORDER BY patient_id")
        .await
        .unwrap();
    let batches = df.collect().await.unwrap();
    let batch = &batches[0];

    assert_eq!(batch.num_rows(), 4);

    let ssn_idx = batch
        .schema()
        .fields()
        .iter()
        .position(|f| f.name() == "ssn")
        .unwrap();
    let ssn_col = batch
        .column(ssn_idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    for i in 0..batch.num_rows() {
        assert_eq!(ssn_col.value(i), "***", "every SSN row should be masked");
    }
}
