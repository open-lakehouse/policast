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

  private lazy val runtime: CelRuntime =
    CelRuntimeFactory.standardCelRuntimeBuilder().build()

  private lazy val resourceFieldNames: Seq[String] = schema.fieldNames.toSeq

  /**
   * Build a CEL compiler whose principal variables cover the canonical
   * vocabulary plus any extra attributes referenced by the expression being
   * compiled. The principal surface is no longer hardcoded to
   * role/region/name, so policies may reference `principal.<anything>`.
   */
  private def buildCompiler(principalAttrs: Set[String]): CelCompiler = {
    val builder = CelCompilerFactory.standardCelCompilerBuilder()
    (CelEvaluator.CanonicalPrincipalAttrs ++ principalAttrs).foreach { attr =>
      builder.addVar(s"principal_$attr", SimpleType.STRING)
    }
    schema.fields.foreach { field =>
      builder.addVar(s"resource_${field.name}", CelEvaluator.sparkTypeToCel(field.dataType))
    }
    builder.build()
  }

  /**
   * Evaluate a CEL boolean expression using cel-java with the given bindings.
   */
  def evaluate(celExpression: String, bindings: JMap[String, Any]): Boolean = {
    try {
      val principalAttrs = CelEvaluator.principalAttrsIn(celExpression)
      val normalized = normalizeCelForRuntime(celExpression)
      val ast: CelAbstractSyntaxTree = buildCompiler(principalAttrs).compile(normalized).getAst
      val program = runtime.createProgram(ast)
      program.eval(bindings).asInstanceOf[Boolean]
    } catch {
      case e: Exception =>
        System.err.println(s"[WARN] Policast CEL evaluation failed: ${e.getMessage}")
        false
    }
  }

  /**
   * Build cel-java bindings for the `principal` from an identity, flattening
   * each attribute to a `principal_<attr>` variable matching the names
   * produced by [[normalizeCelForRuntime]].
   */
  def principalBindings(identity: QueryIdentity): JMap[String, Any] = {
    val bindings = new java.util.HashMap[String, Any]()
    identity.principalAttributes.foreach { case (key, value) =>
      bindings.put(s"principal_$key", value)
    }
    bindings
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
      identity.attribute("region").flatMap { region =>
        findAttribute(output, "region").map { attr =>
          EqualTo(attr, Literal(region, StringType))
        }
      }
    }
    else if (cel.contains("resource.treating_physician") && cel.contains("principal.name")) {
      identity.attribute("name").flatMap { name =>
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
      if (identity.attribute("role").getOrElse("") != "legal") {
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
      val role = identity.attribute("role").getOrElse("")
      if (cel.contains("\"admin\"") && role == "admin") return false
      if (cel.contains("\"physician\"") && role == "physician") return false
    }
    true
  }

  /**
   * Normalize a policast CEL expression into a form that cel-java can parse.
   * Replaces `resource.X` and `principal.X` with flat variable names. The
   * principal rewrite is generic (any `principal.<attr>` becomes
   * `principal_<attr>`) rather than a fixed role/region/name list.
   */
  private def normalizeCelForRuntime(cel: String): String = {
    var result = cel
      .replace("resource.table_name", "\"_table_\"")

    // Replace longest field names first to avoid partial-match collisions
    resourceFieldNames.sortBy(-_.length).foreach { name =>
      result = result.replace(s"resource.$name", s"resource_$name")
    }

    CelEvaluator.PrincipalRef.replaceAllIn(result, m => s"principal_${m.group(1)}")
  }

  private def findAttribute(
      output: Seq[AttributeReference],
      name: String
  ): Option[AttributeReference] = {
    output.find(_.name.equalsIgnoreCase(name))
  }
}

object CelEvaluator {

  /** The canonical principal attribute vocabulary, always declared. */
  val CanonicalPrincipalAttrs: Set[String] = Set("role", "region", "name", "groups")

  /** Matches `principal.<attr>` references in a CEL expression. */
  private val PrincipalRef = """principal\.([A-Za-z_][A-Za-z0-9_]*)""".r

  /** The set of principal attribute names referenced by a CEL expression. */
  def principalAttrsIn(cel: String): Set[String] =
    PrincipalRef.findAllMatchIn(cel).map(_.group(1)).toSet

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
