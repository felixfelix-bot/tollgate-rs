# Issuer Risk

A voucher is a claim on its issuer. Holding one means trusting that node to
deliver when the voucher comes back. This document collects the ways that
trust can fail, and what limits each.

The underlying position is stated in
[tollgate-vouchers.md](../core/tollgate-vouchers.md): payment is **not
zero-trust**. When the provider is the mint, the only party who can cheat is
also the party that would have to honor a refund. Nothing cryptographic
fixes that, so every entry below is bounded by policy or by reputation
rather than by proof.

---

## Overissuance

An issuer can mint more vouchers than its capacity can honor.

**What limits it:** everything the issuer gives away and everything it earns
use the same vouchers. A node cannot inflate its issuance without diluting
every outstanding claim, including the ones it sold for real money and the ones
its paying customers are holding. Overissuing punishes the issuer directly
rather than merely being detectable.

That is a structural improvement over giving away money, where printing costs
the issuer nothing.

---

## Selling Without Redeeming

An issuer hands vouchers to an accomplice, sells them for sats, and never
redeems any. The proceeds are real; the obligation is never honored.

**What limits it:** reputation, in principle — the issuer's vouchers should
trade down. But that only works if failures to redeem are **visible to
people who are not the victim**, and nothing in the design makes them
visible. A peer that gets refused knows; nobody else does.

**Some way to report a refused redemption is a prerequisite for the price
signal to price anything.** Until that exists, the reliability signal in
[voucher-price-signal.md](voucher-price-signal.md) measures demand and
little else. This is the largest unresolved problem in the market layer.

Note the attack needs a buyer, and a buyer for an unknown router's paper is
exactly what thin liquidity makes scarce. Small scale protects itself
somewhat; a well-regarded issuer that turns is the dangerous case.

---

## Redemption Congestion

A voucher says how much, but not when. Capacity is a rate and it is finite,
so everyone redeeming at once can exceed what the issuer can deliver —
**without the issuer doing anything wrong**.

This is not fraud, and treating it as fraud would mispriced honest nodes.
But a holder cannot distinguish "cannot serve you right now" from "will
never serve you", which means it feeds the same reputation channel as
genuine default.

**Unresolved.** Vouchers need a time element — an expiry, a validity window,
or an explicit redemption queue — and none is specified. This interacts with
whether the issuer can bound what it has promised at all.

---

## Operator Shutdown

An operator turns the node off with vouchers outstanding. The holders have
claims on capacity that no longer exists.

**Unspecified.** Options that have not been worked through: expiry so claims
age out, a wind-down period during which the issuer buys back its own paper,
or simply accepting the loss as the risk of holding one node's claims.

Related: an issuer that shuts down cleanly and one that absconds look
identical from outside, which again lands on the visibility problem above.

---

## Exposure Limits That Actually Hold

Everything above is bounded by one policy choice: **how many vouchers a
party holds at once.**

| Holder | Typical exposure | Notes |
|---|---|---|
| A peer buying service | One grant's worth | The payer chooses the window, so it chooses this bound directly |
| A peer that bought in bulk | The whole bag | Forced by expensive cross-mint swaps — see [voucher-acquisition.md](voucher-acquisition.md) |
| A market maker | Inventory across many issuers | The business that makes the price signal possible is also the one carrying this risk |
| A node accepting foreign mints | However much of that issuer's paper it holds | Directly controlled by which mints it accepts at all |

The last row is the good case: accepting a mint is a binary choice a node
makes deliberately, and it can stop at any session boundary.

---

## Open Problems

| Problem | Notes |
|---|---|
| **Visibility of refused redemption** | Reputation cannot price what nobody can observe. Prerequisite for the whole price signal. |
| **Redemption congestion** | Vouchers carry no time element, so honest capacity limits are indistinguishable from default. |
| Operator shutdown | No wind-down, expiry, or buy-back mechanism specified. |
| Bulk exposure | Expensive swaps push holders into large positions, defeating the one-interval bound. |
