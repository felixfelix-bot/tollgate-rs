# Voucher Price Signal

Every node uses the same unit for a given resource — bytes for network
forwarding — but each node issues its own vouchers. The unit is shared; the
issuer is not. A 1 KiB voucher from a reliable gateway and a 1 KiB voucher
from an unreliable one both claim 1024 bytes, and they are worth different
amounts.

**That difference is the signal.** What an issuer's vouchers sell for,
compared to the quantity printed on them, is a direct measure of how much
the network expects that issuer to deliver.

| Selling price of a 1 KiB voucher | What it means |
|---|---|
| 1.00 KiB | Full value — redemption expected in full |
| 0.70 KiB | Issued more than it can deliver, unreliable, or in low demand |
| 1.05 KiB | In demand — access to this node is scarce |

It is public, updated continuously, and set by people who lose money if they
get it wrong. Nothing in the payment protocol produces anything comparable:
there, a node that takes payment and delivers poorly is noticed only by the
peer that already paid it.

---

## No Price Goes Below Zero

A voucher's price is what someone will give for it, and the floor is zero —
worthless, not a liability. Nothing in the design pays a party to take a
voucher off someone's hands.

Where an unwanted flow does need pricing, it is the **received multiplier**
([tollgate-vouchers.md](../core/tollgate-vouchers.md)): a node charges more
for carrying a peer's outgoing traffic when that traffic is unwelcome or its
uplink is scarce. The number is unsigned, so no arrangement anywhere in the
design pays a peer to accept something.

That keeps the signal simple to read: a voucher trading below face value means
the market doubts the issuer, and nothing else.

---

## Liquidity

This is the weakest part of the design.

Each issuer is its own small, thin market. Anyone making that market has to
hold vouchers, and holding them means taking the risk that an anonymous
router stops redeeming — see [issuer-risk.md](issuer-risk.md). The business
case for providing that liquidity is unproven, and it does not obviously
improve with scale: more issuers means more books, each thinner.

Three things make it less bad than it first looks:

- **The market is optional.** Value moves without it, via direct purchase
  from the issuer ([voucher-acquisition.md](voucher-acquisition.md)). A market
  that never appears costs the reliability signal, not the network.
- **Issuers are natural market makers in their own paper.** A node always
  wants to sell its own vouchers and is always willing to redeem them, so
  each book has one committed participant by construction.
- **Multi-mint acceptance concentrates demand without anyone planning it.**
  Because a node can accept any mint denominated in the same unit
  ([tollgate-vouchers.md](../core/tollgate-vouchers.md)), the vouchers that
  many nodes happen to accept become the ones worth holding. Those books get
  deep while the rest stay shallow, and the deep ones start functioning as
  money for the network. Nothing designates a hub currency; acceptance
  decisions produce one.

That last point cuts the other way too. A voucher accepted everywhere is
systemically important, and its issuer's failure stops being one router's
problem.

---

## Open Problems

| Problem | Notes |
|---|---|
| Liquidity | Per-issuer books are small and thin; the case for making them is unproven. |
| Observability of failure | The signal only prices reliability if failures to redeem are visible to people who are not the victim. Nothing currently makes them visible — see [issuer-risk.md](issuer-risk.md). |
| Quoting a price for a peer's vouchers | Every node has to price every peer's vouchers, continuously. Refusing foreign vouchers by default avoids the question but does not answer it. |
