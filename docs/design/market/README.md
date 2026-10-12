# TollGate Market

TollGate's payment protocol does not price anything. A voucher is a claim on
one unit of a node's capacity, and redeeming it is delivery — one for one,
with no rate to quote ([tollgate-vouchers.md](../core/tollgate-vouchers.md)).

What a unit costs in money is settled where vouchers change hands. That is
what these documents cover: how a peer gets vouchers, what determines their
price, and what can go wrong with an issuer.

None of this is required to move value. A peer buys vouchers from the node
it wants service from and spends them there. The market layer exists to make
prices comparable across issuers and to surface which operators actually
deliver — the reliability signal the protocol itself cannot produce.

**The separation is structural.** No amount in any TollGate message is
denominated in money, no TollGate message buys or swaps anything, and market
operations live behind their own endpoints which may be served by a different
host or a third party ([market-protocol.md](market-protocol.md)). A node that
offers no market services is fully functional.

What *does* stay in the payment protocol is the set of mints a node accepts
as payment, with a settlement ratio for each — a peer has to know what it can
pay with before it can pay.

## Documents

| Document | Description |
| -------- | ----------- |
| [market-protocol.md](market-protocol.md) | Separate endpoints and wire format for buying and swapping — and why they are kept out of the payment protocol |
| [voucher-acquisition.md](voucher-acquisition.md) | How a peer comes to hold vouchers: Lightning mint quotes, direct purchase, local swaps, cross-mint swaps |
| [voucher-price-signal.md](voucher-price-signal.md) | Selling price against face value as a public measure of expected delivery; liquidity and market making |
| [issuer-risk.md](issuer-risk.md) | Overissuance, selling without redeeming, redemption congestion, operator shutdown |

## Relationship to the Core Protocol

| Question | Answered by |
| -------- | ----------- |
| What is a voucher, and how is it spent? | [tollgate-vouchers.md](../core/tollgate-vouchers.md) |
| What does delivery cost? | [tollgate-vouchers.md](../core/tollgate-vouchers.md) — one voucher per unit |
| Which mints does a node accept as payment? | The Offer message, [tollgate-protocol.md](../core/tollgate-protocol.md) |
| What does a voucher cost in sats? | Here |
| How do I buy or swap one? | [market-protocol.md](market-protocol.md) |
| How are payments batched? | [tollgate-payment-channels.md](../core/tollgate-payment-channels.md) |

The core protocol is deliberately ignorant of everything in this directory.
A node that never touches a market still works: it sells its own vouchers
for whatever it likes and redeems them on delivery.
