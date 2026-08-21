#!/usr/bin/env bash
set -euo pipefail

echo "[uc-full-init] starting bootstrap"
echo "[uc-full-init] store root: ${POLICAST_STORE_ROOT:-examples/uc/store}"
echo "[uc-full-init] uc endpoint: ${POLICAST_UC_ENDPOINT:-http://unitycatalog:8080}"
default_storage_template='s3://policast-demo/governance/policast/{table}'
storage_template="${POLICAST_UC_STORAGE_URI_TEMPLATE:-$default_storage_template}"
echo "[uc-full-init] storage template: ${storage_template}"

uc_endpoint="${POLICAST_UC_ENDPOINT:-http://unitycatalog:8080}"
for _ in $(seq 1 120); do
  if curl -fsS --max-time 2 "${uc_endpoint}" >/dev/null 2>&1; then
    echo "[uc-full-init] unitycatalog reachable at ${uc_endpoint}"
    break
  fi
  sleep 1
done

echo "[uc-full-init] ddl files to apply:"
for ddl in examples/uc/ddl/*.sql; do
  echo "  - ${ddl}"
done

echo "[uc-full-init] seeding governance delta tables"
seed_args=(
  -q
  -p policast-uc
  --bin policast-uc-seed
  --features sidecar,uc-bootstrap
  --
  --store-root "${POLICAST_STORE_ROOT:-examples/uc/store}"
  --storage-uri-template "${storage_template}"
  --overwrite
)

if [[ "${storage_template}" == s3://* ]]; then
  seed_args+=(
    --storage-option "AWS_ENDPOINT_URL=${RUSTFS_ENDPOINT:-http://rustfs:9000}"
    --storage-option "AWS_ACCESS_KEY_ID=${RUSTFS_ACCESS_KEY:-rustfsadmin}"
    --storage-option "AWS_SECRET_ACCESS_KEY=${RUSTFS_SECRET_KEY:-rustfsadmin}"
    --storage-option "AWS_REGION=${S3_REGION:-us-east-1}"
    --storage-option "AWS_ALLOW_HTTP=true"
  )
fi

cargo run "${seed_args[@]}"

echo "[uc-full-init] bootstrap complete"
