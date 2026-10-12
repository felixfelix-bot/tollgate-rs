#!/usr/bin/env bash
# An oversized purchase is refused, and the payer re-buys at the rate named.
#
# The gateway will sell at most 3 MB/s to one peer; the client wants 8. It
# should end up at exactly the cap rather than at nothing, and within a round
# trip — declining to ratchet already leaves the payer's money untouched, so
# the refusal exists to carry a rate back.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

tollgate::wait_for "the client to settle at the gateway's cap" 90 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "3000000" ]]'

# It should be shaped at the cap, not blocked and not at the allowance.
access="$(tollgate::peer_field gateway access)"
[[ "$access" == "active" ]] || tollgate::fail "expected active at the cap, got $access"

# The refusal should be visible to the operator on both sides — a peer pinned
# at a limit with nothing saying why is the thing this logging exists for.
tollgate::logs gateway | grep -q "refused a peer's purchase" \
  || tollgate::fail "the gateway did not log refusing the purchase"
tollgate::logs client | grep -q "a peer refused our purchase" \
  || tollgate::fail "the client did not log being refused"

echo "PASS: refusal"
