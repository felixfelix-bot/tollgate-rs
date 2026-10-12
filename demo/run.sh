#!/usr/bin/env bash
#
# Two TollGate nodes on one machine, with traffic that ramps.
#
# A gateway sells access and a client buys it. The client's traffic generator
# climbs, the client buys more capacity as it climbs, and the gateway shapes it
# to exactly what it bought. The gateway's max_rate then refuses a purchase and
# names a rate it will take, which the client re-buys at within one round trip.
#
#   ./demo/run.sh              # run for 40 seconds
#   ./demo/run.sh 90           # run for 90 seconds
#   DEBUG=1 ./demo/run.sh      # include every protocol message
#
set -euo pipefail

DURATION="${1:-40}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
BIN="$ROOT/target/release/tollgated"

# What the gateway will sell to any one peer. Set well under where the ramp
# ends, so the refusal path is on screen rather than only in the tests.
#
# These are binary-round on purpose. The unit being sold is the byte and a grant
# decomposes into power-of-two proofs, so 12 MiB/s is a handful of proofs where
# 12 MB/s is a number with bits set all the way down.
GATEWAY_MAX_RATE=$((12 * 1024 * 1024))
DEMAND_START=$((512 * 1024))
DEMAND_STEP=$((2 * 1024 * 1024))
DEMAND_STEP_SECONDS=4

# What a gateway's vouchers cost, in sats from the money mint below. A megabyte
# to the sat, as in the docker topologies — cheap enough that the fake mint's
# ceiling never gets in the way.
BYTES_PER_SAT=1000000

# The money. A sat mint with cdk's fake Lightning backend, which pays its own
# invoices, standing in for something like minibits: the client mints sats
# here and trades them at the gateway's market for byte vouchers.
SAT_MINT_PORT=8085
SAT_MINT="http://127.0.0.1:$SAT_MINT_PORT"
SAT_MINT_CONTAINER=tollgate-demo-sat-mint

# Traffic every peer gets without paying. Keep it small — it is given away, and
# it is what a peer falls back to whenever its grant lapses.
MINIMUM_FLOW=4096

cleanup() {
  [[ -n "${GATEWAY_PID:-}" ]] && kill "$GATEWAY_PID" 2>/dev/null || true
  [[ -n "${CLIENT_PID:-}" ]] && kill "$CLIENT_PID" 2>/dev/null || true
  docker rm -f "$SAT_MINT_CONTAINER" >/dev/null 2>&1 || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

if ! docker info >/dev/null 2>&1; then
  echo "the demo needs docker, to run the sat mint the client pays with" >&2
  exit 1
fi

echo "building..."
cargo build --release --bin tollgated --bin tolltop --manifest-path "$ROOT/Cargo.toml" 2>&1 | tail -1

# Peers are dialed by public key, so each side has to know the other's before
# either starts.
"$BIN" --show-identity > "$WORK/gateway.id"
"$BIN" --show-identity > "$WORK/client.id"
key() { grep "$1" "$2" | awk '{print $2}'; }

GATEWAY_PUB=$(key pubkey "$WORK/gateway.id")
GATEWAY_SEC=$(key secret_key "$WORK/gateway.id")
CLIENT_SEC=$(key secret_key "$WORK/client.id")

cat > "$WORK/gateway.yaml" <<YAML
identity:
  secret_key: "$GATEWAY_SEC"
mint:
  # Peers fund their channels against this, so it has to be reachable by them —
  # which it always is, being the node they are already talking to.
  url: "http://127.0.0.1:3338"
  listen: "127.0.0.1:3338"
  unit: "byte"
vouchers:
  accepted_mints:
    - "http://127.0.0.1:3338"
  # An unsigned surcharge on what the client pushes at us. The net rate is m-1,
  # so 2 charges an upload at the same rate as a download.
  received_multiplier: 2
access:
  minimum_flow:
    enabled: true
    bytes_per_second: $MINIMUM_FLOW
grants:
  # The payer picks any window in this range, per grant, without negotiating.
  window_range_ms: [200, 30000]
  max_rate: $GATEWAY_MAX_RATE
market:
  # What the gateway sells its vouchers for.
  accept:
    - mint: "$SAT_MINT"
      unit: sat
      bytes_per_unit: $BYTES_PER_SAT
wallet:
  mint: "$SAT_MINT"
  unit: sat
network:
  listen: "127.0.0.1:4747"
YAML

cat > "$WORK/client.yaml" <<YAML
identity:
  secret_key: "$CLIENT_SEC"
mint:
  url: "http://127.0.0.1:3339"
  listen: "127.0.0.1:3339"
  unit: "byte"
vouchers:
  accepted_mints:
    - "http://127.0.0.1:3339"
access:
  minimum_flow:
    enabled: true
    bytes_per_second: $MINIMUM_FLOW
buying:
  # A short window keeps the forfeit small when the rate is raised early.
  window_ms: 1000
  renew_lead_ms: 300
wallet:
  # Where the client buys the sats it pays the gateway's market with. Empty to
  # begin with; the first purchase tops it up, which the fake backend settles
  # at once.
  mint: "$SAT_MINT"
  unit: sat
network:
  listen: "127.0.0.1:4749"
peers:
  "$GATEWAY_PUB":
    endpoint: "127.0.0.1:4747"
YAML

LOG_LEVEL="info"
[[ -n "${DEBUG:-}" ]] && LOG_LEVEL="tollgate_net=debug,info"

# Short socket paths: AF_UNIX truncates at ~104 bytes, and a temp dir plus a
# long name overruns it.
GATEWAY_SOCK=/tmp/tollgate-demo-gateway.sock
CLIENT_SOCK=/tmp/tollgate-demo-client.sock

docker rm -f "$SAT_MINT_CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$SAT_MINT_CONTAINER" -p "127.0.0.1:$SAT_MINT_PORT:8085" \
  -e CDK_MINTD_URL="$SAT_MINT" \
  -v "$ROOT/testing/mint/mintd.toml:/etc/cdk/mintd.toml:ro" \
  cashubtc/mintd:latest cdk-mintd --config /etc/cdk/mintd.toml >/dev/null

for _ in $(seq 50); do
  curl -sf "$SAT_MINT/v1/keys" >/dev/null 2>&1 && break
  sleep 0.2
done
if ! curl -sf "$SAT_MINT/v1/keys" >/dev/null 2>&1; then
  echo "the sat mint did not start:" >&2
  docker logs "$SAT_MINT_CONTAINER" >&2
  exit 1
fi

RUST_LOG="$LOG_LEVEL" "$BIN" -c "$WORK/gateway.yaml" --control-socket "$GATEWAY_SOCK" > "$WORK/gateway.log" 2>&1 &
GATEWAY_PID=$!

# Wait for the gateway's mint to answer before starting the client: the client
# cannot acquire vouchers, and so cannot fund a channel, until it does.
for _ in $(seq 50); do
  curl -sf "http://127.0.0.1:3338/v1/keys" >/dev/null 2>&1 && break
  sleep 0.2
done

# --demand is the starting offered load; --ramp steps it up every interval.
RUST_LOG="$LOG_LEVEL" "$BIN" -c "$WORK/client.yaml" --control-socket "$CLIENT_SOCK" \
  --demand "$DEMAND_START" --ramp "$DEMAND_STEP" --ramp-interval "$DEMAND_STEP_SECONDS" \
  > "$WORK/client.log" 2>&1 &
CLIENT_PID=$!
sleep 1

# A node that could not bind dies immediately, and the table below would then
# quietly show zeros forever rather than saying so.
for node in gateway client; do
  pid_var="$(tr '[:lower:]' '[:upper:]' <<< "$node")_PID"
  if ! kill -0 "${!pid_var}" 2>/dev/null; then
    echo "the $node did not start:" >&2
    cat "$WORK/$node.log" >&2
    exit 1
  fi
done

# Binary units throughout, because the unit being sold is the byte and grants
# decompose into power-of-two proofs: 1 MiB/s is a round number of proofs where
# 1 MB/s is not.
rate() {
  awk -v v="${1:-0}" 'BEGIN {
    if (v + 0 <= 0)         { printf "0 B/s" }
    else if (v >= 1048576)  { printf "%.2f MiB/s", v / 1048576 }
    else if (v >= 1024)     { printf "%.1f KiB/s", v / 1024 }
    else                    { printf "%d B/s", v }
  }'
}

cat <<BANNER

  gateway  control 4747, data 4748, mint 3338   sells access, max_rate $(rate "$GATEWAY_MAX_RATE") to one peer
  client   control 4749, data 4750, mint 3339   demand from $(rate "$DEMAND_START"), +$(rate "$DEMAND_STEP") every ${DEMAND_STEP_SECONDS}s
  sat mint $SAT_MINT_PORT (docker, fake Lightning)             the money: $BYTES_PER_SAT bytes of the gateway's vouchers to the sat

  Each node runs its own Cashu mint with a byte-denominated keyset. The client
  mints sats at the sat mint, trades them at the gateway's market for the
  gateway's vouchers, funds a Spilman channel with them, and every purchase
  below is a signed balance update on that channel.

  demand   what the client's traffic generator wants. The only input; everything
           else below is a consequence of it.
  shaped   what the gateway will let the client draw. Not negotiated — it is
           what the client bought, and the gateway derives it from the payment
           alone. Measured on the gateway.
  down     bytes actually arriving, counted off the socket. Measured on the
           client.

  Neither node is ever told the other's number. The client knows what it signed
  for, the gateway knows what it delivered, and nothing reconciles them —
  the money moved before the traffic did.

  For a live view of either node, in another terminal:
      tolltop --socket $GATEWAY_SOCK
      tolltop --socket $CLIENT_SOCK

BANNER

printf '%7s  %12s  %12s  %14s   %s\n' "time" "demand" "shaped" "down" "note"
printf '%7s  %12s  %12s  %14s   %s\n' "-------" "------------" "------------" "--------------" "----"

START=$SECONDS
prev_shaped=""

while (( SECONDS - START < DURATION )); do
  sleep 1
  # The gateway knows what it is shaping the client to; the client knows what
  # is arriving. Neither number is exchanged — that is the point.
  shaped=$(grep -o 'shaped=[0-9]*' "$WORK/gateway.log" | tail -1 | cut -d= -f2 || true)
  demand=$(grep -o 'demand=[0-9]*' "$WORK/client.log" | tail -1 | cut -d= -f2 || true)
  down=$(grep -o 'down=[0-9]*' "$WORK/client.log" | tail -1 | cut -d= -f2 || true)
  : "${shaped:=0}" "${demand:=0}" "${down:=0}"

  # Annotate only the rows that are doing something, so the ones that are stand
  # out instead of every row carrying text.
  note=""
  if (( shaped <= MINIMUM_FLOW )); then
    note="nothing bought yet — minimum flow allowance only"
  elif (( shaped == GATEWAY_MAX_RATE )) && [[ "$shaped" != "$prev_shaped" ]]; then
    note="asked above the gateway's cap: refused, re-bought at the cap"
  elif [[ -n "$prev_shaped" ]] && (( prev_shaped <= MINIMUM_FLOW && shaped > prev_shaped )); then
    note="first purchase — no grant in force, so nothing forfeited"
  elif [[ -n "$prev_shaped" ]] && (( shaped > prev_shaped )); then
    note="demand rose: bought more, forfeiting the old grant's remainder"
  elif [[ -n "$prev_shaped" ]] && (( shaped < prev_shaped )); then
    note="demand fell: renewed lower once the old grant lapsed"
  elif (( shaped == GATEWAY_MAX_RATE )); then
    note="at the gateway's cap"
  elif (( down * 10 < shaped * 9 )); then
    note="shaper still filling after the raise"
  fi

  printf '%6ss  %12s  %12s  %14s   %s\n' \
    "$((SECONDS - START))" "$(rate "$demand")" "$(rate "$shaped")" "$(rate "$down")" "$note"

  prev_shaped="$shaped"
done

echo
echo "logs kept until this script exits: $WORK"
echo "  gateway: $WORK/gateway.log"
echo "  client:  $WORK/client.log"
