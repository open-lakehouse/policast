# AGENTS.md — policast-spark

## What this module owns

`policast-spark` is the Spark 4.1 plugin that enforces compiled policy manifests
through Catalyst optimizer rules. It handles:

- Spark plugin bootstrap (`SparkPlugin`, driver plugin).
- Manifest loading from Spark config.
- Row-filter and deny-override rule injection.
- Column-mask projection rewrites.
- CEL evaluation bridge for Spark schemas.

This module is Scala/SBT based and is not part of the Cargo workspace.

## Key files

- `src/main/scala/com/policast/spark/PolicastPlugin.scala`: Spark plugin entrypoint.
- `src/main/scala/com/policast/spark/PolicastDriverPlugin.scala`: manifest loader/config bridge.
- `src/main/scala/com/policast/spark/PolicastExtensions.scala`: registers optimizer rules.
- `src/main/scala/com/policast/spark/PolicastOptimizerRule.scala`: row-filter and mask rewrites.
- `src/main/scala/com/policast/spark/CelEvaluator.scala`: CEL-to-Catalyst translation and evaluation.
- `src/main/scala/com/policast/spark/PolicyManifest.scala`: manifest JSON model/parser.
- `src/test/scala/com/policast/spark/*.scala`: ScalaTest coverage.

## Build and test

- Compile:
  - `sbt compile`
- Run tests:
  - `sbt test`
- Build fat JAR (assembly plugin configured):
  - `sbt assembly`

Note: `build.sbt` honors an optional `MAVEN_PROXY_URL` environment variable. When set (e.g. a corporate Maven mirror), sbt resolves dependencies through that single proxy. When unset, it falls back to a public OSS resolver list. Maven Central is the implicit default in both cases.

To build with a proxy, use:
```bash
MAVEN_PROXY_URL=https://maven-proxy.{private-proxy}.com sbt -Dsbt.color=true assembly
```

## Runtime configuration

- `spark.plugins = com.policast.spark.PolicastPlugin`
- `spark.sql.extensions = com.policast.spark.PolicastExtensions`
- `spark.policast.manifest.path = /path/to/manifest.json`
- Optional identity inputs:
  - `spark.policast.user.role`
  - `spark.policast.user.region`
  - `spark.policast.user.name`

## Editing guardrails

- Keep Catalyst rules idempotent and safe when manifest is missing/invalid.
- Preserve fail-safe behavior for CEL evaluation failures.
- Add ScalaTest coverage for any new CEL translation pattern or rule rewrite.
