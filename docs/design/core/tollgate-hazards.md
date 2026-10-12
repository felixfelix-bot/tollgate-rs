# TollGate Hazards

Constraints that exist because removing them reintroduces a known abuse. Each
one below looks like an arbitrary restriction until you know what it prevents,
which is why the rationale travels with the rule.

Read this before adding anything that prices, routes, or gives capacity away.

---

## Never Pay A Peer To Send Or Accept Traffic

The design makes this **unrepresentable** rather than merely forbidden: the
`received_multiplier` is unsigned, so a node can charge more for carrying a
peer's traffic, charge nothing, but never pay for it
([tollgate-vouchers.md](tollgate-vouchers.md)). The section stays because the
temptation recurs, and because anything added later must preserve the
property.
Accepting traffic can be faked — the peer takes it, bills for it, and discards
it, having done no work. Under such a price, discarding becomes the most
profitable thing it can do, and metering cannot tell the difference because it
counts what crossed the link, not what the peer did next.

Three specific hazards:

- **Sink peers.** Advertise attractive terms for accepting traffic, receive,
  bill, discard. Zero cost, full revenue.
- **Cheapest route is a blackhole.** Peer identities are free, so a sink can
  undercut honest forwarders indefinitely.
- **Margin squeeze.** A peer floods a node whose terms for accepting traffic
  sit below that node's own onward cost.

Traffic is therefore always priced as **transit**, paid by whoever the traffic
is for, and verified by that party end-to-end. It stops paying when nothing
arrives.

**The general form:** never price the acceptance of something whose acceptance
cannot be checked. The hazard was never a negative sign; it is pricing the
acceptance of something whose acceptance can be faked.

---

## No Price-Aware Routing

TollGate does not influence path selection, and that is currently what defuses
the blackhole hazard above. Free identities plus any price for traffic
acceptance plus price-driven path selection selects for blackholes by
construction: the cheapest next hop is the one doing least work.

If price-aware routing is ever adopted it needs, first, a cost to holding an
identity, and second, a delivery signal the payer can verify. Neither exists.

---

## Metrics Are Never An Input To Price

Scaling price by link quality is tempting — `price = base × etx × (1 +
srtt_ms / 100)` mirrors FIPS's own link cost formula, and charging more for a
worse link looks like cost recovery.

It is unsafe. The metrics are measured **against the peer being priced**, so a
peer can degrade its own link to move its own price. With a positive price
that is self-limiting — the customer leaves. With a negative one it pays the
peer more for being worse, and the incentive runs the wrong way with no bound.

There is no formula to attack today, because delivery has no price. Prepaid
grants harden this further: counters are local, never exchanged, and decide no
payment at all ([tollgate-metering.md](tollgate-metering.md)). A peer cannot
move money by reporting anything, because it reports nothing.

Keep it that way. `peer_metrics()` exists for operator visibility and capacity
decisions, never as a price input, and nothing measured should acquire the
power to move a payment.

---

## Unspent Capacity Must Expire

A grant is a quantity paired with a window, and what is not drawn inside that
window is forfeit — both when the window ends and when a new grant replaces it
([tollgate-vouchers.md](tollgate-vouchers.md)). That looks harsh, and the
temptation is to soften it: credit the remainder into the next grant, let a
small allowance accumulate, extend the deadline instead of replacing it.

Each of those turns a rate back into a stored quantity. Capacity is
perishable — an unsold second is gone whether or not anyone paid for it — so a
claim that never expires lets a buyer accumulate cheaply off-peak and present
the whole position at peak, which is when the capacity is scarce. The seller
sold bandwidth and delivered volume.

Two bounds hold it, and both are needed:

- **Forfeiture** on replacement and at the deadline, so a claim cannot outlive
  its window.
- **`max_window_ms`**, so a window cannot be made long enough to span from
  off-peak to peak. Without it a payer defeats forfeiture by never letting a
  window end.

The same reasoning is why the minimum flow allowance is a rate rather than a
per-interval quantity. A quantity accumulates; a rate cannot.

---

## Free Peering Is Not Transitive

Deciding not to charge a peer means free for **that peer's own traffic**, never
free for anything that peer is nominally the beneficiary of. Otherwise an
uncharged peer becomes a way to launder free transit for others.

---

## Locks Must Survive Every Swap

Any spending condition on a proof — P2PK or otherwise — only holds if it
survives every swap **including change**. Otherwise the holder swaps the
locked proof for something else and the lock is gone in one step.

This is why the minimum flow allowance is issued unlocked: standard Cashu
mints do not preserve locks that way, so locking it would need modified mint
software ([tollgate-vouchers.md](tollgate-vouchers.md)).

---

## Free Identities Multiply Anything Given Away

Anything a node gives away per peer is multiplied by however many identities
an attacker creates, and one machine can run all of them over the same physical
link. Today that means the **minimum flow allowance**; it applies to anything
added later that grants per peer.

Every such mechanism needs an aggregate cap across all unpaid peers, not only
a per-peer one, plus ideally a cost to holding an identity — proof-of-work, a
deposit, or an operator allowlist. None is specified.

---

## Summary

| Rule | Prevents |
|---|---|
| Never pay a peer to send or accept traffic | Peers profiting from traffic nobody wants |
| No price-aware routing | Cheapest route being a blackhole |
| Metrics never price inputs | A peer degrading its link to move its own price |
| Unspent capacity expires, and windows are capped | Buying capacity off-peak to present at peak |
| Free peering not transitive | Laundered free transit |
| Locks survive every swap | Locks removed by swapping through change |
| Aggregate caps on anything granted per peer | Free identities multiplying anything given away |
