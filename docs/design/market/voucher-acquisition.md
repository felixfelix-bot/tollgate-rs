# Voucher Acquisition

How a peer comes to hold vouchers is **not the protocol's business**, any
more than how it came to hold sats. A peer arrives holding vouchers for the
node it wants service from, or it does not get service.

This document collects the routes that exist. None of them needs protocol
support, and a node offering any of them is acting as a market participant
rather than executing a protocol phase.

---

## No Bootstrap Mechanism

Systems that price delivery in money need a way for a peer to pay before it
can reach a mint — the circular problem a *bootstrap token* solves. There is
nothing circular here: the mint you need is the peer you are already talking
to, so a peer can mint, swap and fund against its counterparty over the
peering link alone, with no upstream connectivity at all. Sessions begin at
channel establishment.

**What that costs:** nothing guarantees a peer holding only sats can walk up
to any node and get connected. A local swap (below) recovers it for nodes that
choose to offer one, but no node is obliged to — which also means no
constrained device is obliged to implement it.

---

## Routes

### Mint over Lightning

The peer asks the node's mint for a quote, pays the Lightning invoice, and
receives vouchers (NUT-04). Standard Cashu, no TollGate involvement.

Needs the peer to have connectivity already — another peering, a cellular
link, or the minimum flow allowance
([tollgate-vouchers.md](../core/tollgate-vouchers.md)).

### Direct purchase from the issuer

The node sells its own vouchers for sats, at whatever price it likes. This
is the whole mechanism for most peerings and needs no market to exist.

Because the issuer is the counterparty, settlement is free for it: it is
selling a claim it will honor by delivering, not moving money.

### Local sat swap

The most useful route in practice. The peer hands over sat-denominated
tokens and the node returns vouchers — **for any mint it accepts**,
not only its own ([tollgate-vouchers.md](../core/tollgate-vouchers.md)). The
node has upstream connectivity and can verify the sats with their mint.

A node advertises willingness at its **market endpoint**, not in the payment
protocol — see [market-protocol.md](market-protocol.md). Swapping for the
node's own vouchers is a plain Cashu mint operation (NUT-04); only
cross-mint trades need the market endpoints at all.

This recovers the walk-up case the old bootstrap token existed for: a peer
arriving with only sats gets served without reaching anywhere else. The
difference is that it is an offer a node chooses to make rather than
something every node must implement.

Because the node can hand back vouchers from any mint it accepts, a peer that
intends to move between several nodes can ask for whichever mint is most
widely taken, and stop swapping at every hop.

### Cross-mint swap

Trading one issuer's vouchers for another's, which is what a market needs.
**No working implementation exists.** The options are:

| Approach | Problem |
|---|---|
| Lightning hop | Fees, seconds of latency, liquidity requirements |
| NUT-11/NUT-14 hash-locked swap | Several round trips, both mints online, a counterparty required |
| Trusted exchange | Reintroduces a central party, defeating the point |

None survives running once per grant, so market purchases have
to be made in bulk and drawn down slowly. That concentrates issuer risk in
whatever is being held — see [issuer-risk.md](issuer-risk.md).

This is the main thing blocking the price signal. It does **not** block
operation, and multi-mint acceptance reduces how often anyone needs it: a
voucher usable at several nodes does not have to be swapped when moving
between them, and a relay can spend what it receives straight upstream.

---

## Open Problems

| Problem | Notes |
|---|---|
| Cross-mint atomic swap | No working Cashu implementation. Blocks the price signal, not operation. |
| First connection with no connectivity | A peer holding only sats and having no other link depends on some node choosing to offer a local swap. Nothing guarantees one will. |
| Bulk holding | Amortizing expensive swaps means holding a large position in one issuer's vouchers, which is exactly the exposure the design otherwise tries to keep to one grant. |
| Choosing an accepted set | A node has to decide which mints to take at all — accept or refuse, with no haircut to soften the choice. Neighbors and upstreams it already buys from are the safe ones; a distant hub buys reach at the cost of needing connectivity to verify. |
