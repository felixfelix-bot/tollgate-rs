#!/usr/bin/env bash
# Shared helpers for the topology tests.
#
# Every test follows the same shape: bring a compose topology up, wait for a
# condition to become true, assert, tear down. The waiting matters — these are
# real nodes on real sockets buying from a real mint, so nothing is instant and
# a fixed sleep is either flaky or slow.

set -euo pipefail

# The fixed identities the test configs use. Fixed rather than generated so a
# client can name its gateway's key in a checked-in config, and so a failure is
# reproducible.
export GATEWAY_PUBKEY="02f383c916faa702f91b5e836ed6f12d5f80fb1752ff4db28e6e77997036befd74"
export CLIENT_PUBKEY="039c0c6fe46003ada57010c7b4a6818ea9cf58ea2db12743bff3a9b0af253dbe01"

TESTING_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export TESTING_DIR

# Build the image unless the caller already has one.
#
# A topology that needs something other than the ordinary image — one that also
# carries a FIPS daemon, say — names its own build script in `BUILD_SCRIPT`.
tollgate::build() {
  if [[ -z "${SKIP_BUILD:-}" ]]; then
    "$TESTING_DIR/scripts/${BUILD_SCRIPT:-build.sh}"
  fi
}

# Bring the topology in the calling test's directory up.
tollgate::up() {
  docker compose -f "$COMPOSE" up -d --force-recreate
}

tollgate::down() {
  docker compose -f "$COMPOSE" down -v --remove-orphans >/dev/null 2>&1 || true
}

tollgate::logs() {
  docker compose -f "$COMPOSE" logs --no-color "$@" 2>&1 || true
}

# Read a node's control socket and print the JSON snapshot.
#
# The snapshot is what the node believes about itself, which is what the
# assertions are written against — reading logs would test the log format.
tollgate::snapshot() {
  local service="$1"
  # The socket writes one JSON snapshot and closes, so netcat reads it whole.
  # It is not HTTP, which is why curl is not the tool here.
  docker compose -f "$COMPOSE" exec -T "$service" \
    nc -U -w 2 /run/tollgate.sock </dev/null 2>/dev/null || echo '{}'
}

# One field of one peer, via jq. Prints nothing if there is no such peer yet.
tollgate::peer_field() {
  local service="$1" field="$2"
  tollgate::snapshot "$service" | jq -r ".peers[0].$field // empty" 2>/dev/null || true
}

# The same, for a node with more than one peer: says which one.
#
# `peers[0]` is whichever key sorts first, so a topology where a node sells to
# one peer and carries for another has to name the peer it means or it will
# assert against the wrong session half the time.
tollgate::peer_field_of() {
  local service="$1" pubkey="$2" field="$3"
  tollgate::snapshot "$service" \
    | jq -r --arg k "$pubkey" ".peers[] | select(.pubkey == \$k) | .$field // empty" \
      2>/dev/null || true
}

# Poll until `condition` succeeds, or fail the test.
#
# `condition` is a shell snippet evaluated repeatedly. Deadline-based rather
# than a fixed sleep because a mint has to come up and a channel has to be
# funded before anything can be bought.
tollgate::wait_for() {
  local label="$1" timeout="$2" condition="$3"
  local deadline=$(( SECONDS + timeout ))

  while (( SECONDS < deadline )); do
    if eval "$condition"; then
      echo "  ok: $label"
      return 0
    fi
    sleep 1
  done

  echo "TIMED OUT waiting for: $label" >&2
  echo "--- snapshots ---" >&2
  for svc in $SERVICES; do
    echo "[$svc] $(tollgate::snapshot "$svc")" >&2
  done
  echo "--- logs ---" >&2
  tollgate::logs >&2
  return 1
}

tollgate::fail() {
  echo "FAILED: $*" >&2
  tollgate::logs >&2
  return 1
}

# Standard preamble for a test script: build, tear down on exit, bring up.
tollgate::start() {
  COMPOSE="${COMPOSE:-$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)/docker-compose.yml}"
  SERVICES="${SERVICES:-gateway client}"
  export COMPOSE SERVICES

  tollgate::build
  trap tollgate::down EXIT INT TERM
  tollgate::down
  tollgate::up
}
