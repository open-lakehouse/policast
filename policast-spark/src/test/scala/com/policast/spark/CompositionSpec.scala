package com.policast.spark

import com.google.gson.{JsonArray, JsonParser}
import org.apache.spark.sql.{Row, SparkSession}
import org.apache.spark.sql.types._
import org.scalatest.BeforeAndAfterAll
import org.scalatest.funsuite.AnyFunSuite
import org.scalatest.matchers.should.Matchers
import java.nio.file.Files
import scala.collection.JavaConverters._

class CompositionSpec extends AnyFunSuite with Matchers with BeforeAndAfterAll {
  private val stream = getClass.getResourceAsStream("/composition/cases.json")
  private val source = scala.io.Source.fromInputStream(stream, "UTF-8")
  private val fixture = try JsonParser.parseString(source.mkString).getAsJsonObject finally source.close()
  private var spark: SparkSession = _

  override def beforeAll(): Unit = {
    val warehouse = Files.createTempDirectory("policast-composition-warehouse")
    spark = SparkSession.builder().master("local[2]").appName("policast-composition")
      .config("spark.ui.enabled", "false")
      .config("spark.log.level", "WARN")
      .config("spark.sql.shuffle.partitions", "2")
      .config("spark.sql.warehouse.dir", warehouse.toUri.toString)
      .config("spark.sql.extensions", "com.policast.spark.PolicastExtensions")
      .getOrCreate()
    val schema = StructType(Seq(
      StructField("id", LongType, nullable = false), StructField("region", StringType),
      StructField("name", StringType), StructField("legal_hold", BooleanType), StructField("ssn", StringType)
    ))
    val rows = fixture.getAsJsonArray("rows").iterator().asScala.map { element =>
      val r = element.getAsJsonObject
      Row(r.get("id").getAsLong,
        if (r.get("region").isJsonNull) null else r.get("region").getAsString,
        r.get("name").getAsString,
        if (r.get("legal_hold").isJsonNull) null else java.lang.Boolean.valueOf(r.get("legal_hold").getAsBoolean),
        r.get("ssn").getAsString)
    }.toList.asJava
    spark.createDataFrame(rows, schema).write.mode("overwrite").saveAsTable("patients")
  }

  override def afterAll(): Unit = {
    if (spark != null) spark.stop()
    SparkSession.clearActiveSession()
    SparkSession.clearDefaultSession()
  }

  fixture.getAsJsonArray("cases").iterator().asScala.foreach { element =>
    val c = element.getAsJsonObject
    for (variant <- 0 until 3) {
      test(s"${c.get("name").getAsString}: policy ordering $variant") {
        val manifest = c.getAsJsonObject("manifest").deepCopy()
        val policies = manifest.getAsJsonArray("policies").iterator().asScala.toSeq
        val reordered = new JsonArray()
        val selected = variant match {
          case 1 => policies.reverse
          case 2 => policies ++ policies
          case _ => policies
        }
        selected.foreach(reordered.add)
        manifest.add("policies", reordered)
        val file = Files.createTempFile("policast-composition", ".json")
        try {
          Files.writeString(file, manifest.toString)
          val session = spark.newSession()
          session.conf.set("spark.policast.manifest.path", file.toString)
          c.getAsJsonObject("principal").entrySet().asScala.foreach { entry =>
            session.conf.set(s"spark.policast.user.${entry.getKey}", entry.getValue.getAsString)
          }
          val sql = if (c.has("sql")) c.get("sql").getAsString else "SELECT id, ssn FROM patients ORDER BY id"
          if (c.has("error") && c.get("error").getAsBoolean) {
            intercept[Exception] { session.sql(sql).collect() }
          } else {
            val rows = session.sql(sql).collect()
            val expected = c.getAsJsonArray("expected_ids").iterator().asScala.map(_.getAsLong).toSeq
            withClue(session.sql(sql).queryExecution.analyzed.treeString) {
              rows.map(_.getLong(0)).toSeq shouldBe expected
            }
            if (c.has("masked") && c.get("masked").getAsBoolean)
              rows.foreach(r => r.getAs[String]("ssn") shouldBe "***")
          }
        } finally Files.deleteIfExists(file)
      }
    }
  }
}
