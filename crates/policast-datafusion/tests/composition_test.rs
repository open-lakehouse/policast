use std::sync::Arc;

use datafusion::arrow::array::{BooleanArray, Int64Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::MemTable;
use datafusion::prelude::SessionContext;
use policast_core::PolicyManifest;
use policast_datafusion::{identity::AttrIdentity, GovernedTable};
use serde_json::Value;

#[tokio::test]
async fn shared_composition_conformance() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/composition/cases.json"
    ))
    .unwrap();
    let rows = fixture["rows"].as_array().unwrap();
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("region", DataType::Utf8, true),
        Field::new("name", DataType::Utf8, false),
        Field::new("legal_hold", DataType::Boolean, true),
        Field::new("ssn", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|r| r["id"].as_i64().unwrap())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r["region"].as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r["name"].as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                rows.iter()
                    .map(|r| r["legal_hold"].as_bool())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r["ssn"].as_str()).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let inner = Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap());
    for case in fixture["cases"].as_array().unwrap() {
        let original: PolicyManifest = serde_json::from_value(case["manifest"].clone()).unwrap();
        // Order and duplication must never change enforcement or error outcomes.
        for variant in 0..3 {
            let mut manifest = original.clone();
            if variant == 1 {
                manifest.policies.reverse();
            }
            if variant == 2 {
                manifest.policies.extend(original.policies.clone());
            }
            let identity: AttrIdentity = case["principal"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect();
            let table_name = case["table"].as_str().unwrap_or("patients");
            let ctx = SessionContext::new();
            ctx.register_table(
                "patients",
                Arc::new(GovernedTable::new(
                    inner.clone(),
                    manifest,
                    table_name,
                    identity,
                )),
            )
            .unwrap();
            let sql = case["sql"]
                .as_str()
                .unwrap_or("SELECT id, ssn FROM patients ORDER BY id");
            let result = async { ctx.sql(sql).await?.collect().await }.await;
            if case["error"].as_bool().unwrap_or(false) {
                assert!(
                    result.is_err(),
                    "{} variant {variant} should reject query",
                    case["name"]
                );
                continue;
            }
            let batches =
                result.unwrap_or_else(|e| panic!("{} variant {variant}: {e}", case["name"]));
            let ids: Vec<i64> = batches
                .iter()
                .flat_map(|b| {
                    b.column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .values()
                        .to_vec()
                })
                .collect();
            let expected: Vec<i64> = case["expected_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            assert_eq!(ids, expected, "{} variant {variant}", case["name"]);
            if case["masked"].as_bool().unwrap_or(false) {
                for b in batches {
                    let ssn = b
                        .column_by_name("ssn")
                        .unwrap()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap();
                    assert!(ssn.iter().all(|v| v == Some("***")), "{}", case["name"]);
                }
            }
        }
    }
}
