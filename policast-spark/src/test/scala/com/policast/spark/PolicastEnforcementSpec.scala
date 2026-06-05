package com.policast.spark

import org.apache.spark.sql.SparkSession
import org.scalatest.BeforeAndAfterAll
import org.scalatest.funsuite.AnyFunSuite
import org.scalatest.matchers.should.Matchers

import java.io.File
import java.nio.file.Files

/**
 * End-to-end coverage for the Catalyst rule rewrites: a real SparkSession with
 * the plugin + extensions registered, enforcing the shipped (pre-expanded)
 * manifest against a `patients` catalog table.
 *
 * Verifies, per role, the row filters, deny-override, and column masks — plus a
 * chained re-projection that guards the masking reference-remap (masking must
 * survive a downstream operator that referenced the raw column).
 */
class PolicastEnforcementSpec extends AnyFunSuite with Matchers with BeforeAndAfterAll {

  private var spark: SparkSession = _
  private var warehouse: File = _

  override def beforeAll(): Unit = {
    // Use the same pre-expanded manifest the demo image ships (concrete
    // target_table / column targets), written to a temp file for the plugin.
    val in = getClass.getResourceAsStream("/examples/policies/manifest.json")
    require(in != null, "expected /examples/policies/manifest.json on the test classpath")
    val manifestJson =
      try scala.io.Source.fromInputStream(in, "UTF-8").mkString
      finally in.close()
    val manifestFile = File.createTempFile("policast-manifest", ".json")
    manifestFile.deleteOnExit()
    Files.write(manifestFile.toPath, manifestJson.getBytes("UTF-8"))

    warehouse = Files.createTempDirectory("policast-warehouse").toFile

    spark = SparkSession
      .builder()
      .appName("policast-enforcement-spec")
      .master("local[2]")
      .config("spark.ui.enabled", "false")
      .config("spark.log.level", "WARN")
      .config("spark.sql.shuffle.partitions", "2")
      .config("spark.sql.warehouse.dir", warehouse.getAbsolutePath)
      .config("spark.plugins", "com.policast.spark.PolicastPlugin")
      .config("spark.sql.extensions", "com.policast.spark.PolicastExtensions")
      .config("spark.policast.manifest.path", manifestFile.getAbsolutePath)
      .getOrCreate()

    val session = spark
    import session.implicits._
    Seq(
      (1001, "Alice Johnson", "123-45-6789", "Hypertension", "us-east", "Dr. Smith", false),
      (1002, "Bob Martinez", "234-56-7890", "Diabetes", "us-west", "Dr. Lee", false),
      (1003, "Carol White", "345-67-8901", "Asthma", "us-east", "Dr. Smith", false),
      (1005, "Eva Chen", "567-89-0123", "Anemia", "us-west", "Dr. Lee", true),
      (1009, "Irene Lopez", "901-23-4567", "Diabetes", "us-east", "Dr. Patel", true)
    ).toDF("patient_id", "name", "ssn", "diagnosis", "region", "treating_physician", "legal_hold")
      .write
      .mode("overwrite")
      .saveAsTable("patients")
  }

  override def afterAll(): Unit = {
    if (spark != null) spark.stop()
  }

  /** Set the principal identity the rules read from `spark.policast.user.*`. */
  private def as(role: String, region: Option[String] = None, name: Option[String] = None): Unit = {
    spark.conf.set("spark.policast.user.role", role)
    region.fold(spark.conf.unset("spark.policast.user.region")) { r =>
      spark.conf.set("spark.policast.user.region", r)
    }
    name.fold(spark.conf.unset("spark.policast.user.name")) { n =>
      spark.conf.set("spark.policast.user.name", n)
    }
  }

  test("analyst: row-filtered to region, legal-hold excluded, ssn/diagnosis masked") {
    as("analyst", region = Some("us-east"))
    val rows =
      spark.sql("SELECT patient_id, ssn, diagnosis, region, legal_hold FROM patients").collect()

    rows.map(_.getInt(0)).toSet shouldBe Set(1001, 1003) // us-east, non-legal-hold (1009 is legal-hold)
    rows.foreach { r =>
      r.getString(1) shouldBe "***" // ssn (pii)
      r.getString(2) shouldBe "***" // diagnosis (phi)
      r.getString(3) shouldBe "us-east"
      r.getBoolean(4) shouldBe false
    }
  }

  test("masking survives a downstream re-projection (reference remap)") {
    as("analyst", region = Some("us-east"))
    // The outer projection references the masked column produced below it.
    // Before the rewrite remap this resolved back to the raw scan value.
    val values =
      spark.sql("SELECT ssn FROM patients").selectExpr("ssn AS s").collect().map(_.getString(0)).toSet
    values shouldBe Set("***")
  }

  test("physician: row-filtered to own patients, columns unmasked") {
    as("physician", name = Some("Dr. Smith"))
    val rows = spark.sql("SELECT patient_id, ssn, diagnosis, treating_physician FROM patients").collect()

    rows.map(_.getInt(0)).toSet shouldBe Set(1001, 1003)
    rows.foreach { r =>
      r.getString(1) should not be "***"
      r.getString(2) should not be "***"
      r.getString(3) shouldBe "Dr. Smith"
    }
  }

  test("admin: all non-legal-hold rows, columns unmasked") {
    as("admin")
    val rows = spark.sql("SELECT patient_id, ssn FROM patients").collect()

    rows.map(_.getInt(0)).toSet shouldBe Set(1001, 1002, 1003) // legal-hold 1005/1009 excluded
    rows.foreach(_.getString(1) should not be "***")
  }

  test("legal: legal-hold rows are visible") {
    as("legal")
    val ids =
      spark.sql("SELECT patient_id FROM patients WHERE legal_hold = true").collect().map(_.getInt(0)).toSet
    ids shouldBe Set(1005, 1009)
  }

  test("rules are idempotent: repeated identical queries are stable") {
    as("analyst", region = Some("us-east"))
    val q = "SELECT ssn FROM patients"
    val first = spark.sql(q).collect().map(_.getString(0)).toSet
    val second = spark.sql(q).collect().map(_.getString(0)).toSet
    first shouldBe Set("***")
    second shouldBe first
  }
}
