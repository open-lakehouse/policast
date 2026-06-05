#!/usr/bin/env bash
# Entry point for the `spark-demo` Compose service.
#
# Runs the policast-spark governance demo through spark-submit. Unlike the
# DataFusion demo there is no sidecar: the Spark plugin enforces a *file*-based
# compiled manifest (spark.policast.manifest.path) by rewriting query plans via
# Catalyst optimizer rules. RunSpark walks analyst -> physician -> admin ->
# legal, so every role's row filters + tag-scoped column masks show in one run.
# Configuration is read from the environment (see docker/.env.example).
set -euo pipefail

JAR="${POLICAST_SPARK_JAR:-/opt/policast/policast-spark-assembly.jar}"
MANIFEST="${POLICAST_MANIFEST_PATH:-/opt/policast/examples/policies/manifest.json}"
DATA="${POLICAST_DATA_PATH:-/opt/policast/examples/data/patients.csv}"

if [[ ! -f "${MANIFEST}" ]]; then
  echo "error: compiled manifest not found at ${MANIFEST}" >&2
  echo "       run 'just compile' (or 'docker compose --profile tools run --rm compile') first" >&2
  exit 66
fi
if [[ ! -f "${DATA}" ]]; then
  echo "error: sample data not found at ${DATA}" >&2
  exit 66
fi

echo "running policast-spark demo"
echo "  jar      = ${JAR}"
echo "  manifest = ${MANIFEST}"
echo "  data     = ${DATA}"

exec "${SPARK_HOME}/bin/spark-submit" \
  --class com.policast.spark.examples.RunSpark \
  --master "local[*]" \
  --conf spark.log.level="${POLICAST_SPARK_LOG_LEVEL:-WARN}" \
  --conf spark.ui.enabled=false \
  --conf spark.sql.warehouse.dir=/tmp/spark-warehouse \
  --conf spark.plugins=com.policast.spark.PolicastPlugin \
  --conf spark.sql.extensions=com.policast.spark.PolicastExtensions \
  "${JAR}" \
  "${MANIFEST}" \
  "${DATA}" \
  "$@"
