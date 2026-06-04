package com.policast.spark

import org.scalatest.funsuite.AnyFunSuite
import org.scalatest.matchers.should.Matchers

class PolicyManifestSpec extends AnyFunSuite with Matchers {

  val sampleJson: String =
    """{
      |  "version": "1.0",
      |  "policies": [
      |    {
      |      "id": "row_filter_region",
      |      "effect": "permit",
      |      "filter_type": "row_filter",
      |      "target_table": "patients",
      |      "cel_expression": "(resource.region == principal.region)"
      |    },
      |    {
      |      "id": "column_mask_ssn",
      |      "effect": "forbid",
      |      "filter_type": "column_mask",
      |      "target_table": "patients",
      |      "column": "ssn",
      |      "cel_expression": "(resource.table_name == \"patients\") && !((principal.role == \"admin\") || (principal.role == \"physician\"))"
      |    },
      |    {
      |      "id": "deny_legal_hold",
      |      "effect": "forbid",
      |      "filter_type": "deny_override",
      |      "target_table": "patients",
      |      "cel_expression": "(resource.legal_hold == true) && !(principal.role == \"legal\")"
      |    }
      |  ]
      |}""".stripMargin

  test("fromJson parses manifest correctly") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    manifest.version shouldBe "1.0"
    manifest.policies.size() shouldBe 3
  }

  test("policiesForTable filters by table name") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    manifest.policiesForTable("patients") should have length 3
    manifest.policiesForTable("orders") should have length 0
  }

  test("rowFilters returns only row_filter type") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    manifest.rowFilters("patients") should have length 1
    manifest.rowFilters("patients").head.id shouldBe "row_filter_region"
  }

  test("columnMasks returns only column_mask type") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    manifest.columnMasks("patients") should have length 1
    manifest.columnMasks("patients").head.id shouldBe "column_mask_ssn"
  }

  test("denyOverrides returns only deny_override type") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    manifest.denyOverrides("patients") should have length 1
    manifest.denyOverrides("patients").head.id shouldBe "deny_legal_hold"
  }

  test("toJson roundtrips correctly") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    val json = manifest.toJson
    val reloaded = PolicyManifest.fromJson(json)
    reloaded.policies.size() shouldBe manifest.policies.size()
  }

  test("requiredPrincipalAttributes is empty when principal_contract is absent") {
    val manifest = PolicyManifest.fromJson(sampleJson)
    manifest.requiredPrincipalAttributes shouldBe empty
  }

  test("principal_contract is parsed when present") {
    val json =
      """{
        |  "version": "1.0",
        |  "policies": [],
        |  "principal_contract": { "required_attributes": ["region", "role"] }
        |}""".stripMargin
    val manifest = PolicyManifest.fromJson(json)
    manifest.requiredPrincipalAttributes shouldBe Seq("region", "role")
  }
}
