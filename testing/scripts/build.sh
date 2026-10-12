#!/usr/bin/env bash
# Build the single TollGate test image, reused by every topology.
#
# Tagged `tollgate-test:latest`. Re-run after changing Rust code; topologies can
# then reuse it with SKIP_BUILD=1.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$TESTING_DIR/.." && pwd)"

[[ -f "$ROOT/Cargo.toml" ]] || { echo "no Cargo.toml at $ROOT" >&2; exit 1; }

# BuildKit is required for the Dockerfile's cache mounts. Default in modern
# Docker; forced here so incremental rebuilds are fast regardless of config.
export DOCKER_BUILDKIT=1

echo "building tollgate-test:latest ..."
docker build \
    -t tollgate-test:latest \
    -f "$TESTING_DIR/docker/Dockerfile" \
    "$ROOT"

echo "done: tollgate-test:latest"
