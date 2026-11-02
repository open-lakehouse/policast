package com.policast.spark

import org.apache.spark.sql.SparkSession
import org.apache.spark.sql.catalyst.expressions._
import org.apache.spark.sql.catalyst.plans.logical.{Filter, LogicalPlan, Project}
import org.apache.spark.sql.catalyst.rules.Rule
import org.apache.spark.sql.catalyst.trees.TreeNodeTag
import org.apache.spark.sql.execution.datasources.LogicalRelation
import org.apache.spark.sql.types.{StringType, StructField, StructType}
import scala.collection.mutable
import scala.util.control.NonFatal

object PolicastRowFilterRule {
  private val Applied = TreeNodeTag[Boolean]("policast.governanceApplied")
}

/** One boundary: raw-row policy filtering, then masking, then user operators. */
class PolicastRowFilterRule(session: SparkSession) extends Rule[LogicalPlan] {
  private lazy val manifest: Option[PolicyManifest] =
    session.conf.getOption("spark.policast.manifest.path")
      .orElse(session.sparkContext.getConf.getOption("spark.policast.manifest.path"))
      .filter(_.nonEmpty).map(PolicyManifest.load)

  override def apply(plan: LogicalPlan): LogicalPlan = manifest match {
    case None => plan
    case Some(m) =>
      val identity = PolicastIdentity.fromConf(session)
      val rewrites = mutable.Map.empty[ExprId, Attribute]
      plan.transformUp {
        case rel @ LogicalRelation(_, output, catalogTable, _, _)
            if rel.getTagValue(PolicastRowFilterRule.Applied).isEmpty =>
          val table = catalogTable.map(_.identifier.unquotedString).getOrElse("")
          val schema = StructType(output.map(a => StructField(a.name, a.dataType)))
          val evaluator = CelEvaluator.forSchema(schema)
          val policies = m.policiesForTable(table).filter(_.applies(identity))
          val grants = mutable.ArrayBuffer.empty[Expression]
          val filters = mutable.ArrayBuffer.empty[Expression]
          val masks = mutable.Set.empty[String]
          policies.foreach { p =>
            try {
              p.validateApplicable(table)
              p.filter_type match {
                case "row_grant" =>
                  grants += evaluator.celToSparkExpr(p.cel_expression, output, identity).get
                case "row_filter" =>
                  filters += evaluator.celToSparkExpr(p.cel_expression, output, identity).get
                case "deny_override" =>
                  filters += evaluator.celDenyOverrideToExpr(p.cel_expression, output, identity).get
                case "column_mask" =>
                  require(output.exists(_.name == p.column), "missing mask column")
                  if (evaluator.shouldMask(p.cel_expression, identity, table.split('.').last)) masks += p.column
                case _ => throw new IllegalArgumentException("unknown policy kind")
              }
            } catch {
              case NonFatal(_) =>
                // No expression/identity values in diagnostics.
                throw new IllegalArgumentException(s"policy ${p.id} on table $table: invalid, unsupported, or missing-attribute policy")
            }
          }
          if (m.requiresGrant(table)) filters += grants.reduceLeftOption(Or).getOrElse(Literal(false))
          rel.setTagValue(PolicastRowFilterRule.Applied, true)
          val filtered = filters.foldLeft(rel: LogicalPlan)((child, expr) => Filter(expr, child))
          if (masks.isEmpty) filtered
          else {
            val projection = output.map { attr =>
              if (masks.contains(attr.name)) {
                require(attr.dataType == StringType, "only string masks are supported")
                val masked = Alias(Literal("***"), attr.name)(qualifier = attr.qualifier)
                rewrites(attr.exprId) = masked.toAttribute
                masked
              } else attr
            }
            Project(projection, filtered)
          }
        case other =>
          if (rewrites.isEmpty) other
          else other.transformExpressions {
            case ar: AttributeReference if rewrites.contains(ar.exprId) => rewrites(ar.exprId)
          }
      }
  }
}

/** Compatibility name; both entrypoints share one idempotent governance boundary. */
class PolicastColumnMaskRule(session: SparkSession) extends PolicastRowFilterRule(session)
