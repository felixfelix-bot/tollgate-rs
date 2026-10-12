#!/usr/bin/env bash
# Demand becomes a purchase, and a purchase becomes a shaped rate.
#
# The whole loop in one assertion: the client wants 2 MB/s, buys 125% of it,
# and the gateway shapes it to exactly what it bought — a number neither node
# ever sends the other.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

tollgate::wait_for "the gateway to shape the client to what it bought" 90 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

# The payer's own view has to agree, and it is computed independently.
tollgate::wait_for "the client to agree on the rate it bought" 30 \
  '[[ "$(tollgate::peer_field client bought_rate)" == "2500000" ]]'

# A grant is spendable within a window; it should be live, not lapsed.
expires="$(tollgate::peer_field gateway grant_expires_in_ms)"
[[ "${expires:-0}" -gt 0 ]] || tollgate::fail "the grant lapsed instead of being renewed"

# And bytes actually moved.
delivered="$(tollgate::peer_field gateway delivered)"
[[ "${delivered:-0}" -gt 1000000 ]] \
  || tollgate::fail "only $delivered bytes delivered; the data plane is not carrying traffic"

echo "PASS: purchase"
