package com.policast.spark

import org.apache.spark.sql.SparkSession

/**
 * The identity of the user making the current query.
 *
 * `role`, `region` and `name` are the common, named fields; `attrs` carries
 * any additional principal attributes (e.g. `clearance`, `department`) so a
 * policy may reference `principal.<anything>` the deployment chooses to
 * populate. Resolved from Spark configuration properties at query time.
 */
case class QueryIdentity(
    role: String,
    region: Option[String],
    name: Option[String],
    attrs: Map[String, String] = Map.empty
) {

  /** Resolve a single principal attribute by name. */
  def attribute(field: String): Option[String] = field match {
    case "role"   => Some(role)
    case "region" => region
    case "name"   => name
    case other    => attrs.get(other)
  }

  /** All principal attributes as a flat string map, for CEL bindings. */
  def principalAttributes: Map[String, String] = {
    val base = Map("role" -> role) ++
      region.map("region" -> _).toMap ++
      name.map("name" -> _).toMap
    base ++ attrs
  }
}

object PolicastIdentity {
  private val Prefix = "spark.policast.user."

  /**
   * Build a [[QueryIdentity]] from `spark.policast.user.*` Spark conf
   * entries. `role` defaults to `analyst`; `region` and `name` populate the
   * named fields; every other `spark.policast.user.<attr>` becomes a generic
   * principal attribute so policies can reference it as `principal.<attr>`.
   */
  def fromConf(session: SparkSession): QueryIdentity = {
    val userAttrs: Map[String, String] = session.conf.getAll.collect {
      case (k, v) if k.startsWith(Prefix) => k.substring(Prefix.length) -> v
    }
    val role = userAttrs.getOrElse("role", "analyst")
    val region = userAttrs.get("region")
    val name = userAttrs.get("name")
    val extra = userAttrs -- Set("role", "region", "name")
    QueryIdentity(role, region, name, extra)
  }
}
