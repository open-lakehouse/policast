package com.policast.spark

import com.google.gson.{Gson, GsonBuilder}
import java.io.{FileReader, Reader, StringReader}
import scala.collection.JavaConverters._

case class AppliesTo(
    roles: java.util.List[String] = new java.util.ArrayList[String](),
    principals: java.util.List[String] = new java.util.ArrayList[String]()
)

case class CompiledPolicy(
    id: String,
    effect: String,
    filter_type: String,
    target_table: String,
    column: String,
    cel_expression: String,
    applies_to: AppliesTo,
    description: String
)

/** The compile-time footprint of `principal.*` attributes the policies need. */
case class PrincipalContract(
    required_attributes: java.util.List[String] = new java.util.ArrayList[String]()
)

case class PolicyManifest(
    version: String,
    policies: java.util.List[CompiledPolicy],
    principal_contract: PrincipalContract = null
) {

  /** Principal attributes the policy set requires, or empty if unset. */
  def requiredPrincipalAttributes: Seq[String] =
    Option(principal_contract)
      .map(_.required_attributes.asScala.toSeq)
      .getOrElse(Seq.empty)

  def policiesForTable(tableName: String): Seq[CompiledPolicy] = {
    policies.asScala.toSeq.filter { p =>
      p.target_table == tableName || p.target_table == "*"
    }
  }

  def rowFilters(tableName: String): Seq[CompiledPolicy] =
    policiesForTable(tableName).filter(_.filter_type == "row_filter")

  def columnMasks(tableName: String): Seq[CompiledPolicy] =
    policiesForTable(tableName).filter(_.filter_type == "column_mask")

  def denyOverrides(tableName: String): Seq[CompiledPolicy] =
    policiesForTable(tableName).filter(_.filter_type == "deny_override")

  def toJson: String = PolicyManifest.gson.toJson(this)
}

object PolicyManifest {
  private val gson: Gson = new GsonBuilder().setPrettyPrinting().create()

  def load(path: String): PolicyManifest = {
    val reader: Reader = new FileReader(path)
    try {
      gson.fromJson(reader, classOf[PolicyManifest])
    } finally {
      reader.close()
    }
  }

  def fromJson(json: String): PolicyManifest = {
    gson.fromJson(new StringReader(json), classOf[PolicyManifest])
  }
}
