package com.policast.spark

import org.apache.spark.sql.SparkSession
import org.apache.spark.sql.catalyst.expressions.{
  Alias,
  Attribute,
  AttributeReference,
  ExprId,
  Expression,
  Literal
}
import org.apache.spark.sql.catalyst.plans.logical.{
  Filter,
  LogicalPlan,
  Project
}
import org.apache.spark.sql.catalyst.rules.Rule
import org.apache.spark.sql.catalyst.trees.TreeNodeTag
import org.apache.spark.sql.execution.datasources.LogicalRelation
import org.apache.spark.sql.types.{StringType, StructField, StructType}

import scala.collection.mutable

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
object PolicastRowFilterRule {
  /** Marks a relation whose row filters have already been injected. */
  private val Applied = TreeNodeTag[Boolean]("policast.rowFilterApplied")
}

class PolicastRowFilterRule(session: SparkSession) extends Rule[LogicalPlan] {
  private lazy val manifest: Option[PolicyManifest] = loadManifest()

  override def apply(plan: LogicalPlan): LogicalPlan = {
    manifest match {
      case None => plan
      case Some(m) =>
        plan.transformUp {
          case rel @ LogicalRelation(_, output, catalogTable, _, _)
              if rel.getTagValue(PolicastRowFilterRule.Applied).isEmpty =>
            val tableName = catalogTable
              .map(_.identifier.table)
              .getOrElse("")

            val schema = StructType(output.map(a => StructField(a.name, a.dataType)))
            val evaluator = CelEvaluator.forSchema(schema)
            val identity = resolveIdentity()
            val filters = buildRowFilters(m, tableName, output, identity, evaluator)
            val denyFilters = buildDenyOverrideFilters(m, tableName, output, identity, evaluator)

            val allFilters = filters ++ denyFilters
            if (allFilters.isEmpty) {
              rel
            } else {
              // Tag the relation so the fixed-point optimizer does not re-wrap
              // it on later iterations — the injected filters are not otherwise
              // idempotent under transformUp.
              rel.setTagValue(PolicastRowFilterRule.Applied, true)
              allFilters.foldLeft(rel: LogicalPlan) { (child, filterExpr) =>
                Filter(filterExpr, child)
              }
            }
        }
    }
  }

  private def loadManifest(): Option[PolicyManifest] = {
    // Load from the configured path. The driver plugin's conf write lands on a
    // clone of SparkConf (SparkContext.getConf returns a copy), so we cannot
    // rely on it here; the path itself is set on the real SparkConf at
    // session-build time and survives the clone.
    session.sparkContext.getConf
      .getOption("spark.policast.manifest.path")
      .orElse(session.conf.getOption("spark.policast.manifest.path"))
      .filter(_.nonEmpty)
      .flatMap { path =>
        try Some(PolicyManifest.load(path))
        catch {
          case e: Exception =>
            System.err.println(
              s"[WARN] Policast: failed to load manifest from $path: ${e.getMessage}"
            )
            None
        }
      }
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

object PolicastColumnMaskRule {
  /** Marks a relation whose masking projection has already been injected. */
  private val Applied = TreeNodeTag[Boolean]("policast.columnMaskApplied")
}

/**
 * Catalyst optimizer rule that masks columns by rewriting the table scan.
 *
 * Each governed relation is wrapped in a projection that replaces masked
 * columns with a `"***"` literal (under a fresh attribute id). References to
 * the original columns are then remapped to the masked attribute throughout
 * the rest of the plan — including projections that other operators (e.g.
 * `show()`'s pretty-printing) inserted before optimization — so the mask is
 * honored regardless of how the result is consumed.
 */
class PolicastColumnMaskRule(session: SparkSession) extends Rule[LogicalPlan] {
  private lazy val manifest: Option[PolicyManifest] = loadManifest()

  override def apply(plan: LogicalPlan): LogicalPlan = {
    manifest match {
      case None => plan
      case Some(m) =>
        val identity = resolveIdentity()
        // old (raw) attribute id -> masked attribute, populated as relations
        // are wrapped (bottom-up), then applied to every operator above them.
        val rewrites = mutable.Map.empty[ExprId, Attribute]

        plan.transformUp {
          case rel @ LogicalRelation(_, output, catalogTable, _, _)
              if rel.getTagValue(PolicastColumnMaskRule.Applied).isEmpty =>
            val tableName = catalogTable.map(_.identifier.table).getOrElse("")
            val maskCols = maskedColumns(m, tableName, output, identity)
            if (maskCols.isEmpty) {
              rel
            } else {
              rel.setTagValue(PolicastColumnMaskRule.Applied, true)
              val projectList = output.map { attr =>
                if (maskCols.contains(attr.name)) {
                  val masked = Alias(Literal.create("***", StringType), attr.name)(
                    qualifier = attr.qualifier
                  )
                  rewrites(attr.exprId) = masked.toAttribute
                  masked
                } else {
                  attr
                }
              }
              Project(projectList, rel)
            }

          // Every operator above a masked relation: repoint references from the
          // raw column id to the masked attribute. The masking projection and
          // its relation are produced by the case above and are not revisited.
          case other =>
            if (rewrites.isEmpty) other
            else
              other.transformExpressions {
                case ar: AttributeReference if rewrites.contains(ar.exprId) =>
                  rewrites(ar.exprId)
              }
        }
    }
  }

  /** Column names to mask for `tableName` under the current identity. */
  private def maskedColumns(
      manifest: PolicyManifest,
      tableName: String,
      output: Seq[AttributeReference],
      identity: QueryIdentity
  ): Set[String] = {
    if (tableName.isEmpty) {
      Set.empty
    } else {
      val schema = StructType(output.map(a => StructField(a.name, a.dataType)))
      val evaluator = CelEvaluator.forSchema(schema)
      manifest
        .columnMasks(tableName)
        .flatMap { policy =>
          if (evaluator.shouldMask(policy.cel_expression, identity)) Option(policy.column)
          else None
        }
        .toSet
    }
  }

  private def loadManifest(): Option[PolicyManifest] = {
    // Load from the configured path. The driver plugin's conf write lands on a
    // clone of SparkConf (SparkContext.getConf returns a copy), so we cannot
    // rely on it here; the path itself is set on the real SparkConf at
    // session-build time and survives the clone.
    session.sparkContext.getConf
      .getOption("spark.policast.manifest.path")
      .orElse(session.conf.getOption("spark.policast.manifest.path"))
      .filter(_.nonEmpty)
      .flatMap { path =>
        try Some(PolicyManifest.load(path))
        catch {
          case e: Exception =>
            System.err.println(
              s"[WARN] Policast: failed to load manifest from $path: ${e.getMessage}"
            )
            None
        }
      }
  }

  private def resolveIdentity(): QueryIdentity =
    PolicastIdentity.fromConf(session)
}
