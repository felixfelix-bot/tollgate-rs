# TollGate Metering

This document specifies how TollGate counts units delivered between peers, what those counts are used for, and the trait the implementation provides.

**Metering is local.** Counts are not exchanged, not signed, and not an input to any payment. A peer buys a grant in advance ([tollgate-vouchers.md](tollgate-vouchers.md)) and the provider draws that grant down as traffic passes; the counters are how it knows when the grant is spent. Access control ([tollgate-access-control.md](tollgate-access-control.md)) decides whether delivery happens; metering counts what was delivered.

---

## What is Metered

Each node meters two things per peer, link-local: **units delivered to it** and **units received from it**.

```
For peer B:
  delivered_to_B  += 1,000,000     (B's download)
  received_from_B +=    50,000     (B's upload)
```

Both counters feed the shaper. The peer's grant is drawn down by the sum, with its uploads weighted by the received multiplier:

```
consumed += delivered + received × received_multiplier
```

The counters themselves stay raw. The weighting is applied when drawing down the grant, so what the meter reports and what the shaper charges stay separable.

All units crossing the link are metered — including TollGate protocol messages and locally-addressed resources. Distinguishing control plane from data plane at the metering layer adds complexity for negligible savings (protocol messages are tiny relative to delivered resources).

## Cumulative Counter Model

Counters are **cumulative since session start** (the ChannelReady baseline). They are compared against `authorized`, the cumulative total the peer has signed for, and the difference is what the peer may still spend.

Nothing here is reported to the peer. The payer knows what it signed for; the provider knows what it delivered. Neither has to convince the other, because the money moved before the traffic did.

Grant state and the TopUp message are in [tollgate-protocol.md](tollgate-protocol.md).

---

## What The Payer Measures

The payer runs the same two counters, and they answer a different question: **is this provider worth buying from again?**

```
A bought   512,000 units over a 5 s window
A received 460,000 units in that window
gap                52,000                 ~10%
```

**Under-delivery is measurable one-sided.** The payer knows exactly what it bought and exactly what arrived, both from its own counters, with nothing to take on trust from the provider. The gap may be transit loss, deliberate shaping, or a provider taking payment and delivering less, and the payer cannot tell which — but it does not need to. All three mean the same thing about whether to keep buying here.

That makes it an input to **connection choice**: which peer to buy from, how large a grant to risk, whether to keep the peering at all. Delivered rate against purchased rate is a per-provider score a node can accumulate over time and across sessions, and acting on it needs no protocol, no cooperation, and no message.

This replaces the two-sided reconciliation of the metered-settlement model, where both sides exchanged counters, compared them, and split the difference by rule. That machinery existed because the counts decided how much money moved. They no longer do, so the disagreement they used to arbitrate cannot arise.

**What is one-sided is the evidence, not the measurement.** A payer can act on what it sees but cannot show it to anyone else, so a provider that skims a few percent from every peer is invisible outside those peers. The old cross-check was weak evidence — it relied on a counterparty reporting honestly about itself — but it was evidence. What is left is the same visibility problem the market layer already has for refused redemption ([issuer-risk.md](../market/issuer-risk.md)), and it wants the same answer.

**Revisit if this bites.** If skimming turns out to be common in practice, a reporting path can be added later without disturbing anything here: the counters, their names, and their locality all stay, and what gets added is a way to publish a delivered-against-purchased ratio. Nothing in the payment path depends on that not existing, which is why it is left out for now rather than designed around.

---

## ResourceAdapter Trait (Metering Members)

The `ResourceAdapter` trait spans both access control and metering. The metering-related members:

```rust
pub trait ResourceAdapter: Send + Sync {
    /// Subscribe to metering counter updates for a peer. The implementation
    /// pushes cumulative unit counts as they change. Core draws the peer's
    /// grant down against them and shapes when it is spent.
    fn subscribe_meter(&self, peer: &Pubkey) -> Result<MeterStream, AdapterError>;

    /// Get resource metrics for a peer. Not used for pricing — delivery has
    /// no price — but available to the operator for capacity and health
    /// decisions. None for resources without metrics.
    fn peer_metrics(&self, peer: &Pubkey) -> Option<PeerMetrics>;

    // ... access control members documented in tollgate-access-control.md
}

/// Continuous metering counter stream. Implementation pushes updates as delivery proceeds.
pub struct MeterStream {
    /// Cumulative units delivered TO this peer (outbound)
    pub delivered: watch::Receiver<u64>,
    /// Cumulative units received FROM this peer (inbound)
    pub received: watch::Receiver<u64>,
}
```

### PeerMetrics

Available from FIPS MMP or equivalent. Not used for pricing or access control — delivery has no price and metrics are peer-reported, so letting them set a price would let the peer set its own. Exposed for operator visibility and capacity decisions. Opaque to core; each adapter provides what's relevant for its resource type.

```rust
pub enum MetricValue {
    Float(f64),
    Int(i64),
    Text(String),
    Bool(bool),
}

pub type PeerMetrics = HashMap<String, MetricValue>;
```

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Metering target | Both directions of the link, per peer | The peer's grant is drawn down by both; the counters were already there |
| Counter model | Cumulative since session start, not deltas | Compares directly against the cumulative total the peer has signed for |
| Counter delivery | Push/stream (watch channels) | Continuous updates from adapter; core draws down the grant as they arrive |
| Counter names | `delivered` and `received`, unchanged | The payment model changed, the measurement did not. Both already mean the peer's download and upload |
| Reporting | None — counters stay local | Payment happens before delivery, so no shared number decides how much money moves and there is nothing to reconcile |
| Multiplier | Applied when drawing down the grant, not when counting | Keeps what the meter reports separable from what the shaper charges |
| Transit loss | The payer's cost | A grant is consumed whether or not packets arrive. The payer measures its own throughput and stops buying; no tolerance, no threshold, no message |
| Under-delivery detection | One-sided, from the payer's own counters | Delivered rate against purchased rate needs nothing from the provider. Feeds connection choice: which peer to buy from, how large a grant to risk |
| Under-delivery evidence | None — a payer can act but cannot prove | Accepted for now. Same visibility gap the market layer has for refused redemption, and revisitable as a reporting path if skimming proves common |
| Peer metrics | Opaque map (key → value), never an input to price | The peer controls its own metrics, so pricing from them lets it price itself |
