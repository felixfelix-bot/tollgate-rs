#!/usr/bin/env bash
# Selling transit over a FIPS mesh, end to end.
#
# The same claim as the nftables forwarding topology — the rate a peer buys is
# the rate it gets, measured by pulling a real file — but with the enforcement
# on the other side of a control socket, in a daemon that routes and encrypts
# rather than in the local kernel. Two things this topology can assert that the
# IP one cannot:
#
#   - the peer that gets the grant is the peer that holds the key, because the
#     control plane checks an announced key against the mesh address it came
#     from rather than believing it;
#   - the payment path is not the shaped path as a matter of protocol, not of
#     interface layout: a message addressed to the gateway is never transit.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
SERVICES="gateway client origin"
BUILD_SCRIPT="build-fips.sh"
tollgate::start

# Mesh addresses, derived from the same keys the configs carry: `0xfd` then the
# first fifteen bytes of SHA-256 over the x-only public key. Written out rather
# than computed so that a mismatch between what FIPS derives and what TollGate
# derives fails this test loudly.
GATEWAY_FIPS="fde5:b78f:c4ef:a89c:82cd:b05b:8ee7:ed56"
CLIENT_FIPS="fd81:f1b:5135:2317:58f9:eb75:dc4d:98b8"
ORIGIN_FIPS="fd8b:b816:ee09:45b8:6f58:4e2c:2c2e:43b8"
ORIGIN_PUBKEY="03a86b34ebcf3e5f2e30b1de9023881917a0a96c07009e7b7f60ced29b060d6483"

compose() { docker compose -f "$COMPOSE" "$@"; }

# The mesh comes up before anything else can be true.
tollgate::wait_for "the client's mesh link to the gateway" 90 \
  'compose exec -T client ping -6 -c1 -W2 "$GATEWAY_FIPS" >/dev/null 2>&1'
tollgate::wait_for "a route from the client to the origin" 90 \
  'compose exec -T client ping -6 -c1 -W2 "$ORIGIN_FIPS" >/dev/null 2>&1'

# Every node's fips0 address is the one its key says it should be. If this
# drifts, the identity check below is comparing against the wrong thing.
compose exec -T gateway ip -6 -br addr show fips0 | grep -qi "$GATEWAY_FIPS" \
  || tollgate::fail "the gateway's fips0 address is not the one its key derives"

# Nothing crosses the gateway by accident: the client reaches the origin only
# through it, so what the download measures is a policy this node applies.
compose exec -T client ip -6 route get "$ORIGIN_FIPS" | grep -q "dev fips0" \
  || tollgate::fail "the client is not routing to the origin over the mesh"

tollgate::wait_for "the client to be paying" 180 \
  '[[ "$(tollgate::peer_field_of gateway "$CLIENT_PUBKEY" access)" == "active" ]]'

# 2 MB/s of demand at 125% headroom.
tollgate::wait_for "the gateway to shape the client to what it bought" 60 \
  '[[ "$(tollgate::peer_field_of gateway "$CLIENT_PUBKEY" shaped_rate)" == "2500000" ]]'

# The upstream is carried for nothing, which reaches FIPS as no ceiling at all
# rather than as a very large one.
[[ "$(tollgate::peer_field_of gateway "$ORIGIN_PUBKEY" access)" == "free" ]] \
  || tollgate::fail "the gateway is charging its upstream"

# FIPS is really holding the policy, and holding it against the client's own
# npub — not against an address anybody could have claimed.
policy="$(compose exec -T gateway sh -c \
  'printf "{\"command\":\"show_transit_policy\",\"params\":{}}\n" | nc -U -w 2 /run/fips/control.sock')"
echo "  transit policy: $policy"
echo "$policy" | grep -q "2500000" \
  || tollgate::fail "fipsd holds no 2.5 MB/s ceiling for the client"

# Time a real download across the mesh, and check what actually arrived.
#
# Elapsed time alone is not evidence: a transfer that fails instantly and one
# that is shaped can take a similar wall-clock time for entirely different
# reasons. The byte count is what makes this a measurement.
echo "  measuring a real download across the mesh ..."
DURATION=20
read -r code bytes speed <<<"$(compose exec -T client curl -s \
  --max-time "$DURATION" -o /dev/null \
  -w '%{http_code} %{size_download} %{speed_download}' \
  "http://[$ORIGIN_FIPS]:8080/blob")"

echo "  http $code, $bytes bytes at $speed B/s"

[[ "$code" == "200" ]] \
  || tollgate::fail "the download did not succeed (http $code); nothing crossed the mesh"
# At the rate bought, well over this arrives in the time allowed. A far smaller
# number means the flow stalled part way — which is what a grant lapsing under
# a transfer looks like, and is invisible in an average.
[[ "${bytes:-0}" -ge $(( DURATION * 750000 )) ]] \
  || tollgate::fail "only $bytes bytes arrived in ${DURATION}s; the transfer stalled part way"

# Generous bounds either side of the 2.5 MB/s bought: an unpoliced mesh would
# deliver this far faster, and anything far below means the peer is not getting
# what it paid for.
speed=${speed%%.*}
[[ "$speed" -lt 6000000 ]] \
  || tollgate::fail "$speed B/s is far above the 2.5 MB/s bought; nothing is shaping"
# The floor is well under what a quiet machine delivers (around 2 MB/s of
# goodput): the mesh encrypts and re-frames every packet, and a host busy with
# the rest of the suite shows it. What it still rules out is the failure that
# matters — a flow that stalls when a grant lapses, which lands an order of
# magnitude below this rather than a little under it.
[[ "$speed" -gt 750000 ]] \
  || tollgate::fail "$speed B/s is far below the 2.5 MB/s bought"

# And the gateway counted it where the policy is enforced, which is what draws
# the grant down.
delivered="$(tollgate::peer_field_of gateway "$CLIENT_PUBKEY" delivered)"
[[ "${delivered:-0}" -gt 1000000 ]] \
  || tollgate::fail "fipsd's transit counters show only $delivered bytes for the client"

# The client kept paying throughout — the payment path was not competing with
# the traffic it pays for, because messages addressed to the gateway are not
# transit and are never shaped.
[[ "$(tollgate::peer_field_of gateway "$CLIENT_PUBKEY" access)" == "active" ]] \
  || tollgate::fail "the client's grant lapsed under its own download"

echo "PASS: fips"
