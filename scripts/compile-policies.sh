#!/usr/bin/env bash
#
# Compile all Cedar policies in examples/policies/ into a single manifest.json
# using the policast CLI.
#
# Usage:
#   ./scripts/compile-policies.sh
#   ./scripts/compile-policies.sh -o /custom/path/manifest.json
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

OUTPUT="${ROOT_DIR}/examples/policies/manifest.json"

while getopts "o:" opt; do
    case $opt in
        o) OUTPUT="$OPTARG" ;;
        *) echo "Usage: $0 [-o output_path]"; exit 1 ;;
    esac
done

POLICY_FILES=$(find "${ROOT_DIR}/examples/policies" -name '*.cedar' -type f | sort)

echo "Compiling Cedar policies:"
for f in $POLICY_FILES; do
    echo "  - $(basename "$f")"
done

cargo run --release -p policast-core -- \
    --output "$OUTPUT" \
    --verbose \
    $POLICY_FILES

echo ""
echo "Manifest written to: $OUTPUT"
