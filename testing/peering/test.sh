#!/usr/bin/env bash
# Two nodes find each other, fund channels in both directions, and reach Active.
#
# Nothing is bought here — this is only the opening sequence, which has to work
# before any of the payment tests mean anything.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

# Funding a channel means the client acquires the gateway's vouchers from the
# gateway's mint first, so this waits on a mint coming up as well as a peering.
tollgate::wait_for "the gateway to see the client paying" 90 \
  '[[ "$(tollgate::peer_field gateway access)" == "active" ]]'

tollgate::wait_for "the client to see the gateway paying" 90 \
  '[[ "$(tollgate::peer_field client access)" == "active" ]]'

# Two channels is the default, not an exception: each side pays for what it
# received, so both owe and both fund.
[[ "$(tollgate::peer_field gateway incoming_channels | wc -l)" -ge 1 ]] \
  || tollgate::fail "the gateway has no channel the client pays it on"
[[ -n "$(tollgate::peer_field client outgoing_channel)" ]] \
  || tollgate::fail "the client has no channel to pay the gateway on"

# The two sides should name the same channel: the client's outgoing channel is
# the gateway's incoming one, which is the two ratchets agreeing.
client_out="$(tollgate::snapshot client | jq -r '.peers[0].outgoing_channel.id')"
gateway_in="$(tollgate::snapshot gateway | jq -r '.peers[0].incoming_channels[0].id')"
[[ "$client_out" == "$gateway_in" ]] \
  || tollgate::fail "channel mismatch: client pays on $client_out, gateway sees $gateway_in"

echo "PASS: peering"
