#!/usr/bin/env bash
# Entry point for the `datafusion-demo` Compose service.
#
# Waits for the policast-uc-sidecar health endpoint, then runs the
# run_datafusion_uc_http example binary. All configuration is picked
# up from the environment (see docker/.env.example).
set -euo pipefail

ENDPOINT="${POLICAST_UC_ENDPOINT:-http://sidecar:8765}"
HEALTH_URL="${ENDPOINT%/}/health"
MAX_ATTEMPTS="${POLICAST_UC_WAIT_ATTEMPTS:-30}"
SLEEP_SECS="${POLICAST_UC_WAIT_SECS:-1}"

if [[ -z "${POLICAST_UC_SECRET:-}" ]]; then
  echo "error: POLICAST_UC_SECRET is not set" >&2
  exit 64
fi

echo "waiting for sidecar at ${HEALTH_URL} (up to $((MAX_ATTEMPTS * SLEEP_SECS))s) ..."
attempt=0
until curl -sf -o /dev/null "${HEALTH_URL}"; do
  attempt=$((attempt + 1))
  if (( attempt >= MAX_ATTEMPTS )); then
    echo "error: sidecar at ${HEALTH_URL} did not become ready in time" >&2
    exit 69
  fi
  sleep "${SLEEP_SECS}"
done
echo "sidecar is ready"

exec /usr/local/bin/run_datafusion_uc_http "$@"
