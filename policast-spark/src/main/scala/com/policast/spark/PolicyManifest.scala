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
    description: String,
    binding_selectors: java.util.List[String] = null,
    target_tag: String = null,
    applies_to_tag: String = null
) {
  def applies(identity: QueryIdentity): Boolean = {
    def includes(xs: java.util.List[String], value: String): Boolean =
      Option(xs).forall(v => v.isEmpty || v.asScala.contains(value))
    val principal = identity.attribute("principal_id").orElse(identity.name).getOrElse("")
    val scope = Option(applies_to).forall(a =>
      includes(a.roles, identity.role) && includes(a.principals, principal))
    if (!scope) return false
    val selectors = Option(binding_selectors).map(_.asScala.toSeq).getOrElse(Seq.empty)
    var needsGroups = false
    val matching = selectors.map {
      case "*" => true
      case s if s.startsWith("role:") => s.stripPrefix("role:") == identity.role
      case s if s.startsWith("principal:") => s.stripPrefix("principal:") == principal
      case s if s.startsWith("group:") =>
        identity.attribute("groups") match {
          case Some(groups) => groups.split(",").map(_.trim).contains(s.stripPrefix("group:"))
          case None => needsGroups = true; false
        }
      case _ => throw new IllegalArgumentException(s"policy $id: unknown binding selector")
    }
    val result = selectors.isEmpty || matching.contains(true)
    require(result || !needsGroups, s"policy $id requires authoritative group claims")
    result
  }

  def validateApplicable(table: String): Unit = {
    require(target_tag == null && applies_to_tag == null, s"policy $id on $table requires tag expansion")
    val expected = if (filter_type == "row_filter" || filter_type == "row_grant") "permit" else "forbid"
    require(effect == expected, s"policy $id on $table has invalid effect")
    if (filter_type == "column_mask") require(column != null && column.nonEmpty, s"policy $id requires a column")
  }
}

case class PrincipalContract(
    required_attributes: java.util.List[String] = new java.util.ArrayList[String]()
)

case class PolicyManifest(
    version: String,
    policies: java.util.List[CompiledPolicy],
    principal_contract: PrincipalContract = null,
    manifest_schema_version: Int = 1,
    row_grant_targets: java.util.List[String] = null
) {
  def requiredPrincipalAttributes: Seq[String] =
    Option(principal_contract).flatMap(c => Option(c.required_attributes)).map(_.asScala.toSeq).getOrElse(Seq.empty)

  def policiesForTable(tableName: String): Seq[CompiledPolicy] =
    policies.asScala.toSeq.filter(p => PolicyManifest.tableMatches(p.target_table, tableName))

  def requiresGrant(table: String): Boolean =
    Option(row_grant_targets).exists(_.asScala.exists(PolicyManifest.tableMatches(_, table))) ||
      policiesForTable(table).exists(_.filter_type == "row_grant")

  def rowFilters(table: String): Seq[CompiledPolicy] = policiesForTable(table).filter(_.filter_type == "row_filter")
  def rowGrants(table: String): Seq[CompiledPolicy] = policiesForTable(table).filter(_.filter_type == "row_grant")
  def columnMasks(table: String): Seq[CompiledPolicy] = policiesForTable(table).filter(_.filter_type == "column_mask")
  def denyOverrides(table: String): Seq[CompiledPolicy] = policiesForTable(table).filter(_.filter_type == "deny_override")
  def toJson: String = PolicyManifest.gson.toJson(this)

  def validate(): PolicyManifest = {
    require(manifest_schema_version >= 0 && manifest_schema_version <= 2, "unsupported manifest schema version")
    require(policies != null, "manifest policies are required")
    policies.asScala.foreach { p =>
      require(p != null && p.id != null && p.target_table != null && p.cel_expression != null, "invalid policy fields")
      require(Set("row_filter", "row_grant", "deny_override", "column_mask").contains(p.filter_type), "unknown policy kind")
      require(Set("permit", "forbid").contains(p.effect), "unknown policy effect")
    }
    this
  }
}

object PolicyManifest {
  private val gson: Gson = new GsonBuilder().setPrettyPrinting().create()

  def tableMatches(pattern: String, table: String): Boolean = {
    pattern == "*" || pattern == table ||
      (pattern.endsWith(".*") && table.contains(".") &&
        table.substring(0, table.lastIndexOf('.')) == pattern.stripSuffix(".*")) ||
      (pattern.contains(".") && pattern.substring(pattern.lastIndexOf('.') + 1) == table) ||
      (table.contains(".") && table.substring(table.lastIndexOf('.') + 1) == pattern)
  }

  def load(path: String): PolicyManifest = {
    val reader: Reader = new FileReader(path)
    try Option(gson.fromJson(reader, classOf[PolicyManifest])).getOrElse(
      throw new IllegalArgumentException("null policy manifest")).validate()
    finally reader.close()
  }

  def fromJson(json: String): PolicyManifest =
    Option(gson.fromJson(new StringReader(json), classOf[PolicyManifest])).getOrElse(
      throw new IllegalArgumentException("null policy manifest")).validate()
}
