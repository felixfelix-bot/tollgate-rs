#!/usr/bin/env bash
# The gateway forwards the client's traffic, gates it with nftables, and shapes
# it with tc.
#
# This is the only topology where the bytes belong to somebody: the client pulls
# a large file from a third host, and every packet crosses the gateway. What is
# asserted is that the rate the client *bought* is the rate it *gets* — measured
# by timing a real download, not by reading a counter the node maintains itself.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
SERVICES="gateway client origin"
tollgate::start

tollgate::wait_for "the client to be paying" 120 \
  '[[ "$(tollgate::peer_field gateway access)" == "active" ]]'

# 2 MB/s of demand at 125% headroom.
tollgate::wait_for "the gateway to shape the client to what it bought" 60 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

# The gateway shapes on the interface facing the client, and which kernel name
# that is comes from docker rather than from us.
docker compose -f "$COMPOSE" exec -T gateway ip -br addr show eth0 | grep -q "172.28.0.20" \
  || tollgate::fail "eth0 is not the interface facing the client; gateway.yaml names the wrong one"

# Every packet has to cross the gateway, or the measurement below would be of
# a path that was never shaped.
docker compose -f "$COMPOSE" exec -T client ip route get 172.29.0.10 | grep -q "via 172.28.0.20" \
  || tollgate::fail "the client is not routing to the origin through the gateway"

# The kernel is really doing the work: rules and a class exist for this peer.
docker compose -f "$COMPOSE" exec -T gateway nft list table inet tollgate >/dev/null 2>&1 \
  || tollgate::fail "the gateway installed no nftables table"
docker compose -f "$COMPOSE" exec -T gateway tc class show dev eth0 | grep -q htb \
  || tollgate::fail "the gateway installed no tc class"

# Time a real download through the gateway, and check what actually arrived.
#
# Elapsed time alone is not evidence: a transfer that fails instantly and one
# that is shaped can take a similar wall-clock time for entirely different
# reasons, so the byte count is what makes this a measurement rather than a
# coincidence. An earlier version of this test passed while curl was moving
# zero bytes.
echo "  measuring a real download through the gateway ..."
#
# The blob is far larger than anything that will arrive in the time allowed, so
# the transfer is cut off by the deadline rather than by running out of file.
# That is what makes this a rate measurement: the numerator is fixed and the
# bytes are whatever the shaper let through. (`curl` reports `200` rather than
# `206` because the origin serves the whole file; it does not honour Range.)
DURATION=20
read -r code bytes speed <<<"$(docker compose -f "$COMPOSE" exec -T client curl -s \
  --max-time "$DURATION" -o /dev/null \
  -w '%{http_code} %{size_download} %{speed_download}' \
  http://172.29.0.10:8080/blob)"

echo "  http $code, $bytes bytes at $speed B/s"

[[ "$code" == "200" ]] \
  || tollgate::fail "the download did not succeed (http $code); nothing crossed the gateway"
# At the rate bought, well over this arrives in the time allowed. A far smaller
# number means the flow stalled part way — which is what a grant lapsing under
# a transfer looks like, and is invisible in an average.
[[ "${bytes:-0}" -ge $(( DURATION * 1500000 )) ]] \
  || tollgate::fail "only $bytes bytes arrived in ${DURATION}s; the transfer stalled part way"

# The rate bought was 2.5 MB/s. Generous bounds either side: an unshaped bridge
# would deliver this an order of magnitude faster, and anything far below would
# mean the peer is not getting what it paid for.
speed=${speed%%.*}
[[ "$speed" -lt 6000000 ]] \
  || tollgate::fail "$speed B/s is far above the 2.5 MB/s bought; nothing is shaping"
[[ "$speed" -gt 1500000 ]] \
  || tollgate::fail "$speed B/s is far below the 2.5 MB/s bought"

# And the gateway counted it in the kernel, which is what draws the grant down.
delivered="$(tollgate::peer_field gateway delivered)"
[[ "${delivered:-0}" -gt 1000000 ]] \
  || tollgate::fail "the gateway's nftables counters show only $delivered bytes"

echo "PASS: forwarding"
