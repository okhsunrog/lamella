#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR

export WORKLOAD_PROFILE=realistic
exec "$SCRIPT_DIR/netns-load-test.sh"
