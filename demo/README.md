# Demo

```
./demo/run.sh          # 40 seconds
./demo/run.sh 90       # longer
DEBUG=1 ./demo/run.sh  # every protocol message, not just the summary
```

Two nodes on one machine. A **gateway** sells network access; a **client** buys
it. The client's traffic generator ramps, and everything else follows from that.

## What you are looking at

```
   time        demand        shaped            down   note
     3s    2.50 MiB/s     4.0 KiB/s       4.0 KiB/s   nothing bought yet — minimum flow allowance only
     4s    2.50 MiB/s    3.12 MiB/s      2.81 MiB/s   first purchase — no grant in force, so nothing forfeited
     8s    4.50 MiB/s    5.62 MiB/s      5.38 MiB/s   demand rose: bought more, forfeiting the old grant's remainder
    12s    6.50 MiB/s    8.12 MiB/s      6.88 MiB/s   demand rose: bought more, forfeiting the old grant's remainder
    21s   10.50 MiB/s   12.00 MiB/s     11.59 MiB/s   asked above the gateway's cap: refused, re-bought at the cap
    22s   10.50 MiB/s   12.00 MiB/s     12.00 MiB/s   at the gateway's cap
```

### The columns

| Column | What it is | Measured on |
|---|---|---|
| **demand** | What the client's traffic generator wants. The only input — everything else is a consequence of it. | client |
| **shaped** | What the gateway will let the client draw. Not negotiated: it is what the client *bought*, and the gateway derives it from the payment alone. | gateway |
| **down** | Bytes actually arriving, counted off the socket. | client |
| **note** | Only on rows where something happens, so those stand out. | — |

**Neither node is ever told the other's number.** The client knows what it
signed for; the gateway knows what it delivered. Nothing reconciles them,
because the money moved before the traffic did.

Rates are in binary units because the unit being sold is the byte and a grant
decomposes into power-of-two proofs — 12 MiB/s is a handful of proofs where
12 MB/s is a number with bits set all the way down.

### The notes

| Note | Meaning |
|---|---|
| `nothing bought yet — minimum flow allowance only` | No grant exists. The client is on the free trickle that lets it reach a mint at all. |
| `first purchase — no grant in force, so nothing forfeited` | The one purchase that costs no remainder. |
| `demand rose: bought more, forfeiting the old grant's remainder` | A rate raised mid-window. Whatever was left of the previous grant burns at that moment. |
| `demand fell: renewed lower once the old grant lapsed` | Buying cheaper mid-window would still forfeit the expensive grant, so the client waits for the deadline instead. |
| `asked above the gateway's cap: refused, re-bought at the cap` | `TopUpReject` carrying a rate the gateway *would* take, answered inside one round trip. |
| `at the gateway's cap` | Standing state: the client wants more than `max_rate` and is held there. |
| `shaper still filling after the raise` | The token bucket has not yet accrued a full second at the new rate. |

A blank note is a steady row: the grant in force covers demand, and the client
is renewing at the same rate as its window rolls over.

## The four things worth pointing at

**1. Nothing is delivered before payment.** At `t=2s` the client is held at
4096 B/s — the minimum flow allowance. That is not generosity, it is what breaks
the bootstrap circle: a peer holding no vouchers cannot acquire any without some
connectivity, so it gets a trickle and no more.

**2. A purchase takes effect on arrival.** Demand rises at `t=3s`; the client is
drawing 3.1 MB/s by `t=4s`. There is no acknowledgement in that path — the
signed state is cumulative, so a TopUp is idempotent and the client may start
using a rate in the same breath as buying it. A lost message costs nothing; the
next one carries the correct total.

**3. Raising the rate costs the remainder.** Every step up forfeits whatever was
left of the grant in force. That is what makes the product bandwidth rather than
a stored quantity of bytes — capacity is perishable, and the buyer carries that
risk on the seconds it bought. The client's algorithm has hysteresis for exactly
this reason: it only jumps early when demand rises by half again, because a
large jump costs ~2.5% of the new grant while a small one costs ~67%.

**4. The gateway refuses before taking the money.** At `t=20s` the client asks
for more than the gateway's `max_rate` of 12 MB/s. The gateway declines to
ratchet — which already leaves the client's money untouched, since an unclaimed
state is worth nothing to it — and says what it *would* take. The client
re-buys at 12 MB/s inside one round trip. This is only possible because the
payer states the rate it wants up front for a bounded horizon.

## What is real and what is not

Real: the protocol, both TCP planes, the CBOR encoding, the signatures over
channel updates, the shaper, the meters, the admission control, and the bytes —
`measured down` is counted off a socket, not simulated.

Not real: **nothing is at stake.** The channel backend
(`tollgate_net::channel::LocalChannels`) keeps the protocol's shape — real
identifiers, real capacities, real rollover — without its cryptography. There
are no Cashu proofs, so no ecash moves and settlement is a no-op. It is the
right thing for showing how the protocol behaves and the wrong thing for holding
value. A Spilman backend implements the same trait and the layers above it do
not change.

Also simulated: where the traffic comes from. A generator stands in for a user,
and the gateway forwards to nothing. Replacing that is nftables and a TUN
device, or a FIPS delivery filter.

## Configuration on show

The gateway's config sets `received_multiplier: 2`, which surcharges what the
client pushes *at* it. The net rate is `m - 1`, so `2` charges an upload at the
same rate as a download. It is unsigned deliberately: a node can decline traffic
as hard as it likes, but can never pay a bonus on top of what it already owes
for delivery.

Neither config contains a price, because delivery has none. One voucher buys one
unit. What a unit costs in money is settled where vouchers are sold, which the
protocol never sees.
