#!/usr/bin/env bash
# Build the image the FIPS topology runs: fipsd and tollgated in one container.
#
# Three steps, because the two daemons come from two source trees:
#
#   1. tollgate-test:latest — the ordinary image, built by build.sh
#   2. fips-node:latest     — fipsd, built from `reference/fips`
#   3. tollgate-fips-test   — both binaries, plus the entrypoint that
#                             starts the mesh before anything else
#
# Step 2 needs a FIPS checkout at `reference/fips`, which is not part of this
# repository. Without one there is nothing to build against, and the script
# says so rather than producing an image that would fail at run time.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$TESTING_DIR/.." && pwd)"
FIPS="${FIPS_CHECKOUT:-$ROOT/reference/fips}"

if [[ ! -f "$FIPS/Cargo.toml" ]]; then
    echo "no FIPS checkout at $FIPS" >&2
    echo "clone one there, or point FIPS_CHECKOUT at it:" >&2
    echo "  git clone https://github.com/nicobao/fips $FIPS" >&2
    exit 1
fi

export DOCKER_BUILDKIT=1

"$TESTING_DIR/scripts/build.sh"

# The daemon is built from `git archive` rather than from the working tree: the
# tree carries a `target/` of several gigabytes that docker would upload as
# build context every time, and the archive is exactly the committed state.
echo "building fips-node:latest from $FIPS ..."
CONTEXT="$(mktemp -d)"
trap 'rm -rf "$CONTEXT"' EXIT
git -C "$FIPS" archive --format=tar "${FIPS_REF:-HEAD}" | tar -x -C "$CONTEXT"

docker build \
    -t fips-node:latest \
    -f "$TESTING_DIR/docker/Dockerfile.fipsd" \
    "$CONTEXT"

echo "building tollgate-fips-test:latest ..."
docker build \
    -t tollgate-fips-test:latest \
    -f "$TESTING_DIR/docker/Dockerfile.fips" \
    "$TESTING_DIR/docker"

echo "done: tollgate-fips-test:latest"
