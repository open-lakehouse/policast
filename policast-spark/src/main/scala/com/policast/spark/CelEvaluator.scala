package com.policast.spark

import dev.cel.common.CelAbstractSyntaxTree
import dev.cel.common.types.{CelType, SimpleType}
import dev.cel.compiler.{CelCompiler, CelCompilerFactory}
import dev.cel.runtime.{CelRuntime, CelRuntimeFactory}
import org.apache.spark.sql.catalyst.expressions.{
  AttributeReference,
  EqualTo,
  Expression,
  IsNotNull,
  Literal,
  Not,
  Or
}
import org.apache.spark.sql.types._

import java.util.{Map => JMap}
import scala.collection.mutable

/**
 * Bridges compiled CEL expressions to Spark Catalyst expressions.
 *
 * Each instance is bound to a specific Spark StructType and lazily builds
 * a CEL compiler whose variable declarations mirror the schema's fields
 * (prefixed with `resource_`) plus a fixed set of `principal_*` variables.
 */
class CelEvaluator(schema: StructType) {

  private lazy val compiler: CelCompiler = {
    val builder = CelCompilerFactory.standardCelCompilerBuilder()
      .addVar("principal_role", SimpleType.STRING)
      .addVar("principal_region", SimpleType.STRING)
      .addVar("principal_name", SimpleType.STRING)

    schema.fields.foreach { field =>
      builder.addVar(s"resource_${field.name}", CelEvaluator.sparkTypeToCel(field.dataType))
    }

    builder.build()
  }

  private lazy val runtime: CelRuntime =
    CelRuntimeFactory.standardCelRuntimeBuilder().build()

  private lazy val resourceFieldNames: Seq[String] = schema.fieldNames.toSeq

  /**
   * Evaluate a CEL boolean expression using cel-java with the given bindings.
   */
  def evaluate(celExpression: String, bindings: JMap[String, Any]): Boolean = {
    try {
      val normalized = normalizeCelForRuntime(celExpression)
      val ast: CelAbstractSyntaxTree = compiler.compile(normalized).getAst
      val program = runtime.createProgram(ast)
      program.eval(bindings).asInstanceOf[Boolean]
    } catch {
      case e: Exception =>
        System.err.println(s"[WARN] Policast CEL evaluation failed: ${e.getMessage}")
        false
    }
  }

  /**
   * Translate a row-filter CEL expression into a Catalyst Expression.
   *
   * Handles patterns like:
   *   (resource.region == principal.region)
   *   (resource.treating_physician == principal.name)
   */
  def celToSparkExpr(
      cel: String,
      output: Seq[AttributeReference],
      identity: QueryIdentity
  ): Option[Expression] = {
    if (cel.contains("resource.region") && cel.contains("principal.region")) {
      identity.region.flatMap { region =>
        findAttribute(output, "region").map { attr =>
          EqualTo(attr, Literal(region, StringType))
        }
      }
    }
    else if (cel.contains("resource.treating_physician") && cel.contains("principal.name")) {
      identity.name.flatMap { name =>
        findAttribute(output, "treating_physician").map { attr =>
          EqualTo(attr, Literal(name, StringType))
        }
      }
    }
    else {
      None
    }
  }

  /**
   * Translate a deny-override CEL expression into a Catalyst Expression.
   *
   * For `(resource.legal_hold == true) && !(principal.role == "legal")`,
   * if the user is NOT "legal", produce a filter that excludes legal_hold rows.
   */
  def celDenyOverrideToExpr(
      cel: String,
      output: Seq[AttributeReference],
      identity: QueryIdentity
  ): Option[Expression] = {
    if (cel.contains("resource.legal_hold") && cel.contains("principal.role")) {
      if (identity.role != "legal") {
        findAttribute(output, "legal_hold").map { attr =>
          Or(
            EqualTo(attr, Literal(false, BooleanType)),
            Not(IsNotNull(attr))
          )
        }
      } else {
        None
      }
    } else {
      None
    }
  }

  /**
   * Determine if a column mask should apply for the given identity.
   *
   * Returns true when the user's role is NOT in the exempted set.
   */
  def shouldMask(cel: String, identity: QueryIdentity): Boolean = {
    if (cel.contains("principal.role")) {
      val role = identity.role
      if (cel.contains("\"admin\"") && role == "admin") return false
      if (cel.contains("\"physician\"") && role == "physician") return false
    }
    true
  }

  /**
   * Normalize a policast CEL expression into a form that cel-java can parse.
   * Replaces `resource.X` and `principal.X` with flat variable names,
   * driven by the schema rather than hardcoded field names.
   */
  private def normalizeCelForRuntime(cel: String): String = {
    var result = cel
      .replace("resource.table_name", "\"_table_\"")

    // Replace longest field names first to avoid partial-match collisions
    resourceFieldNames.sortBy(-_.length).foreach { name =>
      result = result.replace(s"resource.$name", s"resource_$name")
    }

    result
      .replace("principal.role", "principal_role")
      .replace("principal.region", "principal_region")
      .replace("principal.name", "principal_name")
  }

  private def findAttribute(
      output: Seq[AttributeReference],
      name: String
  ): Option[AttributeReference] = {
    output.find(_.name.equalsIgnoreCase(name))
  }
}

object CelEvaluator {

  private val cache: mutable.Map[StructType, CelEvaluator] =
    mutable.Map.empty

  /** Obtain a CelEvaluator for the given schema, reusing a cached instance when possible. */
  def forSchema(schema: StructType): CelEvaluator = {
    cache.getOrElseUpdate(schema, new CelEvaluator(schema))
  }

  /** Map a Spark DataType to the closest CEL SimpleType. */
  def sparkTypeToCel(dataType: DataType): CelType = dataType match {
    case StringType              => SimpleType.STRING
    case _: VarcharType          => SimpleType.STRING
    case _: CharType             => SimpleType.STRING
    case BooleanType             => SimpleType.BOOL
    case ByteType                => SimpleType.INT
    case ShortType               => SimpleType.INT
    case IntegerType             => SimpleType.INT
    case LongType                => SimpleType.INT
    case FloatType               => SimpleType.DOUBLE
    case DoubleType              => SimpleType.DOUBLE
    case _: DecimalType          => SimpleType.DOUBLE
    case BinaryType              => SimpleType.BYTES
    case TimestampType           => SimpleType.TIMESTAMP
    case TimestampNTZType        => SimpleType.TIMESTAMP
    case DateType                => SimpleType.TIMESTAMP
    case _                       => SimpleType.DYN
  }
}
