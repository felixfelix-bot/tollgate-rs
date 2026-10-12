#!/usr/bin/env bash
# A client that never buys stays on the minimum flow allowance.
#
# Not blocked: the allowance is what lets a peer holding no vouchers reach a
# mint and acquire some, and what keeps a link alive between grants. It is a
# floor on the shaper rather than a separate mechanism.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

tollgate::wait_for "the peering to come up" 90 \
  '[[ -n "$(tollgate::peer_field gateway access)" ]]'

# The client is started with no --demand, so it buys nothing.
sleep 10

shaped="$(tollgate::peer_field gateway shaped_rate)"
[[ "$shaped" == "4096" ]] \
  || tollgate::fail "expected the 4096 B/s allowance, got $shaped"

# A grant that never existed reads as lapsed, and that is a resting state
# rather than a fault — the peer is still a peer.
[[ "$(tollgate::peer_field gateway grant_expires_in_ms)" == "0" ]] \
  || tollgate::fail "a grant is live despite nothing being bought"

echo "PASS: allowance"
