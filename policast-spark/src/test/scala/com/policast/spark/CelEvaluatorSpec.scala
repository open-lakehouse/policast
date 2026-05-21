package com.policast.spark

import dev.cel.common.types.SimpleType
import org.apache.spark.sql.types._
import org.scalatest.funsuite.AnyFunSuite
import org.scalatest.matchers.should.Matchers

class CelEvaluatorSpec extends AnyFunSuite with Matchers {

  val patientsSchema: StructType = StructType(Seq(
    StructField("patient_id", IntegerType),
    StructField("name", StringType),
    StructField("ssn", StringType),
    StructField("diagnosis", StringType),
    StructField("region", StringType),
    StructField("treating_physician", StringType),
    StructField("legal_hold", BooleanType)
  ))

  val evaluator: CelEvaluator = CelEvaluator.forSchema(patientsSchema)

  test("shouldMask returns false for admin role") {
    val cel = """(resource.table_name == "patients") && !((principal.role == "admin") || (principal.role == "physician"))"""
    val identity = QueryIdentity("admin", None, None)
    evaluator.shouldMask(cel, identity) shouldBe false
  }

  test("shouldMask returns false for physician role") {
    val cel = """(resource.table_name == "patients") && !((principal.role == "admin") || (principal.role == "physician"))"""
    val identity = QueryIdentity("physician", None, None)
    evaluator.shouldMask(cel, identity) shouldBe false
  }

  test("shouldMask returns true for analyst role") {
    val cel = """(resource.table_name == "patients") && !((principal.role == "admin") || (principal.role == "physician"))"""
    val identity = QueryIdentity("analyst", None, None)
    evaluator.shouldMask(cel, identity) shouldBe true
  }

  test("shouldMask returns true for unknown role") {
    val cel = """(resource.table_name == "patients") && !((principal.role == "admin") || (principal.role == "physician"))"""
    val identity = QueryIdentity("intern", None, None)
    evaluator.shouldMask(cel, identity) shouldBe true
  }

  test("sparkTypeToCel maps String types") {
    CelEvaluator.sparkTypeToCel(StringType) shouldBe SimpleType.STRING
  }

  test("sparkTypeToCel maps Boolean type") {
    CelEvaluator.sparkTypeToCel(BooleanType) shouldBe SimpleType.BOOL
  }

  test("sparkTypeToCel maps integer types to INT") {
    CelEvaluator.sparkTypeToCel(IntegerType) shouldBe SimpleType.INT
    CelEvaluator.sparkTypeToCel(LongType) shouldBe SimpleType.INT
    CelEvaluator.sparkTypeToCel(ShortType) shouldBe SimpleType.INT
    CelEvaluator.sparkTypeToCel(ByteType) shouldBe SimpleType.INT
  }

  test("sparkTypeToCel maps floating-point types to DOUBLE") {
    CelEvaluator.sparkTypeToCel(FloatType) shouldBe SimpleType.DOUBLE
    CelEvaluator.sparkTypeToCel(DoubleType) shouldBe SimpleType.DOUBLE
    CelEvaluator.sparkTypeToCel(DecimalType(10, 2)) shouldBe SimpleType.DOUBLE
  }

  test("sparkTypeToCel maps timestamp types") {
    CelEvaluator.sparkTypeToCel(TimestampType) shouldBe SimpleType.TIMESTAMP
    CelEvaluator.sparkTypeToCel(DateType) shouldBe SimpleType.TIMESTAMP
  }

  test("sparkTypeToCel maps unknown types to DYN") {
    CelEvaluator.sparkTypeToCel(ArrayType(StringType)) shouldBe SimpleType.DYN
  }

  test("forSchema caches evaluators by schema") {
    val e1 = CelEvaluator.forSchema(patientsSchema)
    val e2 = CelEvaluator.forSchema(patientsSchema)
    e1 should be theSameInstanceAs e2
  }
}
