package com.policast.spark

import org.apache.spark.sql.SparkSession
import org.apache.spark.sql.catalyst.expressions.{
  Alias,
  AttributeReference,
  Expression,
  Literal
}
import org.apache.spark.sql.catalyst.plans.logical.{
  Filter,
  LogicalPlan,
  Project
}
import org.apache.spark.sql.catalyst.rules.Rule
import org.apache.spark.sql.execution.datasources.LogicalRelation
import org.apache.spark.sql.types.{StringType, StructField, StructType}

/**
 * Catalyst optimizer rule that injects row-level filters derived from
 * compiled Cedar/CEL policies in the manifest.
 *
 * For each table scan, this rule:
 * 1. Looks up the table in the policy manifest
 * 2. Resolves the current user's identity
 * 3. Translates applicable row-filter and deny-override CEL expressions
 *    into Catalyst Filter nodes
 */
class PolicastRowFilterRule(session: SparkSession) extends Rule[LogicalPlan] {
  private lazy val manifest: Option[PolicyManifest] = loadManifest()

  override def apply(plan: LogicalPlan): LogicalPlan = {
    manifest match {
      case None => plan
      case Some(m) =>
        plan.transformUp {
          case rel @ LogicalRelation(_, output, catalogTable, _) =>
            val tableName = catalogTable
              .map(_.identifier.table)
              .getOrElse("")

            val schema = StructType(output.map(a => StructField(a.name, a.dataType)))
            val evaluator = CelEvaluator.forSchema(schema)
            val identity = resolveIdentity()
            val filters = buildRowFilters(m, tableName, output, identity, evaluator)
            val denyFilters = buildDenyOverrideFilters(m, tableName, output, identity, evaluator)

            val allFilters = filters ++ denyFilters
            allFilters.foldLeft(rel: LogicalPlan) { (child, filterExpr) =>
              Filter(filterExpr, child)
            }
        }
    }
  }

  private def loadManifest(): Option[PolicyManifest] = {
    Option(session.conf.getOption(PolicastDriverPlugin.ManifestKey))
      .flatten
      .map(PolicyManifest.fromJson)
  }

  private def resolveIdentity(): QueryIdentity =
    PolicastIdentity.fromConf(session)

  private def buildRowFilters(
      manifest: PolicyManifest,
      tableName: String,
      output: Seq[AttributeReference],
      identity: QueryIdentity,
      evaluator: CelEvaluator
  ): Seq[Expression] = {
    manifest.rowFilters(tableName).flatMap { policy =>
      evaluator.celToSparkExpr(policy.cel_expression, output, identity)
    }
  }

  private def buildDenyOverrideFilters(
      manifest: PolicyManifest,
      tableName: String,
      output: Seq[AttributeReference],
      identity: QueryIdentity,
      evaluator: CelEvaluator
  ): Seq[Expression] = {
    manifest.denyOverrides(tableName).flatMap { policy =>
      evaluator.celDenyOverrideToExpr(policy.cel_expression, output, identity)
    }
  }
}

/**
 * Catalyst optimizer rule that rewrites projections to apply column masks.
 *
 * For masked columns, the original column reference is replaced with a
 * conditional expression that returns "***" for non-exempt users.
 */
class PolicastColumnMaskRule(session: SparkSession) extends Rule[LogicalPlan] {
  private lazy val manifest: Option[PolicyManifest] = loadManifest()

  override def apply(plan: LogicalPlan): LogicalPlan = {
    manifest match {
      case None => plan
      case Some(m) =>
        plan.transformUp {
          case project @ Project(projectList, child) =>
            val identity = resolveIdentity()
            val masks = collectMasks(m, child, identity)

            if (masks.isEmpty) {
              project
            } else {
              val newProjectList = projectList.map { expr =>
                expr match {
                  case attr: AttributeReference if masks.contains(attr.name) =>
                    Alias(Literal("***", StringType), attr.name)(
                      attr.exprId,
                      attr.qualifier
                    )
                  case other => other
                }
              }
              Project(newProjectList, child)
            }
        }
    }
  }

  private def loadManifest(): Option[PolicyManifest] = {
    Option(session.conf.getOption(PolicastDriverPlugin.ManifestKey))
      .flatten
      .map(PolicyManifest.fromJson)
  }

  private def resolveIdentity(): QueryIdentity =
    PolicastIdentity.fromConf(session)

  private def collectMasks(
      manifest: PolicyManifest,
      child: LogicalPlan,
      identity: QueryIdentity
  ): Set[String] = {
    val tables = extractTables(child)

    tables.flatMap { case (tableName, schema) =>
      val evaluator = CelEvaluator.forSchema(schema)
      manifest.columnMasks(tableName).flatMap { policy =>
        if (evaluator.shouldMask(policy.cel_expression, identity)) {
          Option(policy.column)
        } else {
          None
        }
      }
    }.toSet
  }

  private def extractTables(plan: LogicalPlan): Seq[(String, StructType)] = {
    plan.collect {
      case LogicalRelation(_, output, catalogTable, _) =>
        val name = catalogTable.map(_.identifier.table).getOrElse("")
        val schema = StructType(output.map(a => StructField(a.name, a.dataType)))
        (name, schema)
    }.filter(_._1.nonEmpty)
  }
}
