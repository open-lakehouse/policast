/**
 * End-to-end Spark demo for policast-cel governance.
 *
 * Run with spark-submit (after building the policast-spark jar):
 *
 *   spark-submit \
 *     --class com.policast.spark.examples.RunSpark \
 *     --conf spark.plugins=com.policast.spark.PolicastPlugin \
 *     --conf spark.sql.extensions=com.policast.spark.PolicastExtensions \
 *     --conf spark.policast.manifest.path=examples/policies/manifest.json \
 *     --conf spark.policast.user.role=analyst \
 *     --conf spark.policast.user.region=us-east \
 *     --jars policast-spark/target/scala-2.13/policast-spark-assembly-0.1.0.jar \
 *     policast-spark/target/scala-2.13/policast-spark-assembly-0.1.0.jar
 *
 * Alternatively, run in spark-shell or a notebook by setting the same
 * configurations on the SparkSession builder.
 */

package com.policast.spark.examples

import org.apache.spark.sql.SparkSession

object RunSpark {

  def main(args: Array[String]): Unit = {
    println("=== Policast-CEL: Spark Governance Demo ===\n")

    val manifestPath = if (args.length > 0) args(0) else "examples/policies/manifest.json"

    val spark = SparkSession.builder()
      .appName("policast-cel-demo")
      .master("local[*]")
      .config("spark.plugins", "com.policast.spark.PolicastPlugin")
      .config("spark.sql.extensions", "com.policast.spark.PolicastExtensions")
      .config("spark.policast.manifest.path", manifestPath)
      .getOrCreate()

    // Load the patients CSV
    val patients = spark.read
      .option("header", "true")
      .option("inferSchema", "true")
      .csv("examples/data/patients.csv")

    // Register as a catalog table (not a temp view): the governance rules
    // identify a table by its catalog identifier, which a file-backed temp
    // view does not carry, so policies targeting `patients` would not match.
    spark.sql("DROP TABLE IF EXISTS patients")
    patients.write.mode("overwrite").saveAsTable("patients")

    // ---------------------------------------------------------------
    // Scenario 1: Query as analyst in us-east
    // ---------------------------------------------------------------
    println("--- Scenario 1: Analyst in us-east ---\n")

    spark.conf.set("spark.policast.user.role", "analyst")
    spark.conf.set("spark.policast.user.region", "us-east")

    println("Expected: Only us-east rows, legal_hold=false excluded, SSN/diagnosis masked\n")

    spark.sql("""
      SELECT patient_id, name, ssn, diagnosis, region, legal_hold
      FROM patients
    """).show(truncate = false)

    // ---------------------------------------------------------------
    // Scenario 2: Query as physician (Dr. Smith)
    // ---------------------------------------------------------------
    println("\n--- Scenario 2: Physician Dr. Smith ---\n")

    spark.conf.set("spark.policast.user.role", "physician")
    spark.conf.set("spark.policast.user.name", "Dr. Smith")
    spark.conf.unset("spark.policast.user.region")

    println("Expected: Only Dr. Smith's patients, SSN/diagnosis visible\n")

    spark.sql("""
      SELECT patient_id, name, ssn, diagnosis, region, treating_physician
      FROM patients
    """).show(truncate = false)

    // ---------------------------------------------------------------
    // Scenario 3: Query as admin (sees everything)
    // ---------------------------------------------------------------
    println("\n--- Scenario 3: Admin (full access) ---\n")

    spark.conf.set("spark.policast.user.role", "admin")
    spark.conf.unset("spark.policast.user.region")
    spark.conf.unset("spark.policast.user.name")

    println("Expected: All rows visible, all columns unmasked\n")

    spark.sql("""
      SELECT patient_id, name, ssn, diagnosis, region, legal_hold
      FROM patients
    """).show(truncate = false)

    // ---------------------------------------------------------------
    // Scenario 4: Query as legal (can see legal_hold rows)
    // ---------------------------------------------------------------
    println("\n--- Scenario 4: Legal role (can see legal_hold) ---\n")

    spark.conf.set("spark.policast.user.role", "legal")

    println("Expected: legal_hold rows are visible\n")

    spark.sql("""
      SELECT patient_id, name, region, legal_hold
      FROM patients
      WHERE legal_hold = true
    """).show(truncate = false)

    spark.stop()
    println("\n=== Demo complete ===")
  }
}
