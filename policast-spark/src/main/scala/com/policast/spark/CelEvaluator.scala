package com.policast.spark

import dev.cel.common.ast.{CelConstant, CelExpr}
import dev.cel.parser.CelParserFactory
import dev.cel.common.types.{CelType, SimpleType}
import org.apache.spark.sql.catalyst.expressions._
import org.apache.spark.sql.types._

import java.util.{Map => JMap}
import scala.collection.JavaConverters._
import scala.collection.mutable

/** Structural translation of the portable CEL subset; unsupported nodes are errors. */
class CelEvaluator(schema: StructType) {
  private val parser = CelParserFactory.standardCelParserBuilder().build()

  private def fail(reason: String): Nothing = throw new IllegalArgumentException(reason)

  private def predicate(
      cel: String,
      output: Seq[AttributeReference],
      identity: QueryIdentity,
      table: Option[String] = None
  ): Expression = {
    val ast = parser.parse(cel).getAst.getExpr
    val expr = convert(ast, output, identity, table)
    require(expr.dataType == BooleanType, "policy predicate must be boolean")
    expr
  }

  private def convert(
      node: CelExpr,
      output: Seq[AttributeReference],
      identity: QueryIdentity,
      table: Option[String]
  ): Expression = {
    import CelExpr.ExprKind.Kind
    val expr: Expression = node.getKind match {
      case Kind.CONSTANT =>
        val c = node.constant()
        c.getKind match {
          case CelConstant.Kind.BOOLEAN_VALUE => Literal(c.booleanValue())
          case CelConstant.Kind.INT64_VALUE => Literal(c.int64Value())
          case CelConstant.Kind.DOUBLE_VALUE => Literal(c.doubleValue())
          case CelConstant.Kind.STRING_VALUE => Literal(c.stringValue())
          case CelConstant.Kind.NULL_VALUE => Literal.create(null, NullType)
          case _ => fail("unsupported CEL literal")
        }
      case Kind.SELECT =>
        val s = node.select()
        require(!s.testOnly() && s.operand().getKind == Kind.IDENT, "unsupported CEL member access")
        s.operand().ident().name() match {
          case "principal" =>
            Literal(identity.attribute(s.field()).getOrElse(fail("missing principal attribute")))
          case "resource" if s.field() == "table_name" && table.nonEmpty => Literal(table.get)
          case "resource" =>
            output.find(_.name == s.field()).getOrElse(fail("missing resource column"))
          case _ => fail("unsupported CEL object")
        }
      case Kind.CALL =>
        val call = node.call()
        require(!call.target().isPresent, "CEL methods are unsupported")
        // Translate every argument, even if another branch is a constant TRUE/FALSE.
        val args = call.args().asScala.toSeq.map(convert(_, output, identity, table))
        def unary(f: Expression => Expression): Expression = {
          require(args.size == 1, "invalid unary arity"); f(args.head)
        }
        def binary(f: (Expression, Expression) => Expression): Expression = {
          require(args.size == 2, "invalid binary arity")
          val (left, right) = numericPair(args.head, args(1))
          f(left, right)
        }
        call.function() match {
          case "_&&_" =>
            require(args.size >= 2, "invalid AND arity")
            args.reduceLeft(And)
          case "_||_" =>
            require(args.size >= 2, "invalid OR arity")
            args.reduceLeft(Or)
          case "!_" => unary(Not)
          case "-_" => unary(x => UnaryMinus(x))
          case "_==_" => binary(EqualTo)
          case "_!=_" => binary((a,b) => Not(EqualTo(a,b)))
          case "_<_" => binary(LessThan)
          case "_<=_" => binary(LessThanOrEqual)
          case "_>_" => binary(GreaterThan)
          case "_>=_" => binary(GreaterThanOrEqual)
          case "_+_" => binary((a,b) => Add(a,b))
          case "_-_" => binary((a,b) => Subtract(a,b))
          case "_*_" => binary((a,b) => Multiply(a,b))
          case "_/_" => binary((a,b) =>
            if (a.dataType == LongType && b.dataType == LongType) IntegralDivide(a,b)
            else Divide(a,b))
          case "_%_" => binary((a,b) => Remainder(a,b))
          case _ => fail("unsupported CEL operator")
        }
      case _ => fail("unsupported CEL expression")
    }
    require(expr.checkInputDataTypes().isSuccess, "invalid CEL operand types")
    // Binding and folding at the leaves preserves type checks on every branch.
    if (expr.foldable) Literal.create(expr.eval(), expr.dataType) else expr
  }

  private def numericPair(a: Expression, b: Expression): (Expression, Expression) = {
    def integral(t: DataType): Boolean = Set[DataType](ByteType, ShortType, IntegerType, LongType).contains(t)
    def floating(t: DataType): Boolean = t == FloatType || t == DoubleType
    if (integral(a.dataType) && integral(b.dataType)) (Cast(a, LongType), Cast(b, LongType))
    else if ((integral(a.dataType) || floating(a.dataType)) && (integral(b.dataType) || floating(b.dataType)))
      (Cast(a, DoubleType), Cast(b, DoubleType))
    else if (a.dataType == NullType) (Cast(a, b.dataType), b)
    else if (b.dataType == NullType) (a, Cast(b, a.dataType))
    else (a,b)
  }

  def celToSparkExpr(cel: String, output: Seq[AttributeReference], identity: QueryIdentity): Option[Expression] =
    Some(predicate(cel, output, identity))

  def celDenyOverrideToExpr(cel: String, output: Seq[AttributeReference], identity: QueryIdentity): Option[Expression] =
    Some(Not(Coalesce(Seq(predicate(cel, output, identity), Literal(false)))))

  def shouldMask(cel: String, identity: QueryIdentity, tableName: String = "patients"): Boolean = {
    val expr = predicate(cel, Seq.empty, identity, Some(tableName))
    require(expr.foldable, "mask must depend only on planning-time inputs")
    expr.eval() match {
      case value: java.lang.Boolean => value.booleanValue()
      case _ => fail("mask predicate must evaluate to boolean")
    }
  }

  def principalBindings(identity: QueryIdentity): JMap[String, Any] = {
    val result = new java.util.HashMap[String, Any]()
    identity.principalAttributes.foreach { case (k,v) => result.put(s"principal_$k", v) }
    result
  }

  /** Compatibility helper using structural AST binding, without textual substitutions. */
  def evaluate(celExpression: String, bindings: JMap[String, Any]): Boolean = {
    val attrs = bindings.asScala.collect {
      case (k, v: String) if k.startsWith("principal_") => k.stripPrefix("principal_") -> v
    }.toMap
    val identity = QueryIdentity(attrs.getOrElse("role", ""), attrs.get("region"), attrs.get("name"), attrs)
    val output = schema.fields.map(f => AttributeReference(f.name, f.dataType)()).toSeq
    val expr = predicate(celExpression, output, identity)
    val bound = expr.transform {
      case a: AttributeReference =>
        require(bindings.containsKey(s"resource_${a.name}"), "missing resource binding")
        Literal.create(bindings.get(s"resource_${a.name}"), a.dataType)
    }
    bound.eval() match {
      case v: java.lang.Boolean => v.booleanValue()
      case _ => fail("CEL predicate must evaluate to boolean")
    }
  }
}

object CelEvaluator {
  val CanonicalPrincipalAttrs: Set[String] = Set("role", "region", "name", "groups")

  def principalAttrsIn(cel: String): Set[String] = {
    import CelExpr.ExprKind.Kind
    def visit(e: CelExpr): Set[String] = e.getKind match {
      case Kind.SELECT if e.select().operand().getKind == Kind.IDENT &&
          e.select().operand().ident().name() == "principal" => Set(e.select().field())
      case Kind.CALL =>
        val call = e.call()
        call.args().asScala.flatMap(visit).toSet ++
          (if (call.target().isPresent) visit(call.target().get()) else Set.empty[String])
      case Kind.SELECT => visit(e.select().operand())
      case _ => Set.empty
    }
    visit(CelParserFactory.standardCelParserBuilder().build().parse(cel).getAst.getExpr)
  }

  private val cache: mutable.Map[StructType, CelEvaluator] = mutable.Map.empty
  def forSchema(schema: StructType): CelEvaluator = cache.synchronized {
    cache.getOrElseUpdate(schema, new CelEvaluator(schema))
  }

  def sparkTypeToCel(dataType: DataType): CelType = dataType match {
    case StringType | _: VarcharType | _: CharType => SimpleType.STRING
    case BooleanType => SimpleType.BOOL
    case ByteType | ShortType | IntegerType | LongType => SimpleType.INT
    case FloatType | DoubleType | _: DecimalType => SimpleType.DOUBLE
    case BinaryType => SimpleType.BYTES
    case TimestampType | TimestampNTZType | DateType => SimpleType.TIMESTAMP
    case _ => SimpleType.DYN
  }
}
