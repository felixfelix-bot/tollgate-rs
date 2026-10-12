# TollGate Peering: FIPS Mesh Networks

This document describes how `tollgate-net` integrates with [FIPS](https://github.com/nicobao/fips) (Free Internetworking Peering System) — the **ideal** deployment target. FIPS provides everything TollGate wants from a network layer: cryptographic peer authentication, encrypted forwarding, self-organizing mesh routing, and rich per-link metrics. The doc covers the FIPS-specific `ResourceAdapter` implementation, how `tollgate-net` hooks into FIPS internals, and what FIPS modifications are required.

## Overview

FIPS provides everything TollGate needs from a network layer:
- **Peer authentication**: Noise IK handshakes mutually authenticate every peer
- **Encrypted forwarding**: All traffic is encrypted hop-by-hop (FMP) and end-to-end (FSP)
- **Self-organizing topology**: Spanning tree + bloom filters for routing without central coordination
- **Per-peer metrics**: MMP provides SRTT, loss, ETX, goodput, jitter per link
- **Session layer**: FSP provides port-based service dispatch for TollGate messages

`tollgate-net` and the FIPS daemon are independent binaries communicating over FIPS's control socket. FIPS exposes generic per-peer capabilities (forwarding policy, lifecycle events, livestreamed rx/tx counters, MMP metrics); `tollgate-net` consumes them.

Per-peer counters are pushed as a livestream subscription, so `tollgate-net` always has a fresh value to draw each peer's grant down against. Socket overhead is negligible.

---

## Integration Points

TollGate hooks into FIPS at five points:

![FIPS Integration Points](diagrams/fips-integration.svg)
<details><summary>Text version</summary>

```
  ┌──────────────── FIPS Node ────────────────────┐
  │                                                │
  │  ┌───────────┐  ┌───────────┐  ┌───────────┐  │
  │  │Forwarding │  │  Bloom    │  │   MMP     │  │
  │  │  policy   │  │  Filter   │  │  Metrics  │  │
  │  └─────┬─────┘  └─────┬─────┘  └─────┬─────┘  │
  │       ①│              ②│             ③│        │
  │  ┌─────┴───────────────┴──────────────┴─────┐  │
  │  │       TollGate FIPS Adapter              │  │
  │  └─────┬────────────────────────────┬───────┘  │
  │       ④│                           ⑤│          │
  │  ┌─────┴─────┐              ┌──────┴──────┐   │
  │  │   Peer    │              │    IPv6     │   │
  │  │ Lifecycle │              │   Adapter   │   │
  │  └───────────┘              └─────────────┘   │
  └────────────────────────────────────────────────┘

  ① Per-peer forwarding policy (local_only / full)
  ② Bloom filter exclusion (inferred from policy)
  ③ MMP metrics for operator visibility
  ④ Peer connect/disconnect callbacks
  ⑤ TollGate message transport (raw TCP over IPv6)
```
</details>

### 1. Per-Peer Forwarding Policy

TollGate sets a per-peer forwarding policy in FIPS. For blocked peers (`None`, `Suspended`), FIPS restricts the peer to **local-only traffic**:

- Traffic **from** this peer addressed **to this node**: Allowed (the peer can still communicate with us — TollGate protocol, payment negotiation)
- Traffic **from** this peer addressed **to other nodes** (transit): Dropped
- Traffic **from other nodes** destined **to or through** this peer: Not forwarded to this peer

For allowed peers (`Active`, `Free`), FIPS forwards normally — no restrictions.

This is a data-plane policy, not a control-plane hook. FIPS simply needs to know: "for peer X, restrict to local-only" or "for peer X, forward normally."

**Required FIPS change**: Ability to set a per-peer forwarding policy — `local_only` (blocked) or `full` (allowed). The **default policy for new peers must be `local_only`** — so that a newly connected peer doesn't get full forwarding in the window between FIPS authenticating it and TollGate detecting it. FIPS enforces this in its existing forwarding path.

### 2. Bloom Filter Exclusion

TollGate controls which peers appear in bloom filter computation. Unpaid peers (`None`, `Suspended`) are excluded — their node_addr is not added to the bloom filter advertised to other peers. This prevents traffic from being routed toward a peer that will have it dropped at the gate.

When a peer's access level changes:
- `None` -> `Active`/`Free`: Add to bloom filters immediately, trigger FilterAnnounce
- `Active` -> `Suspended`: Remove from bloom filters **after a delay** (default: 30 seconds) to avoid flapping. If the peer recovers (tops up, funds new channel) within the delay, the removal is cancelled and the peer stays visible. This prevents rapid bloom filter churn when a peer temporarily exhausts balance.
- `Suspended` -> `Active`: Re-add to bloom filters immediately (cancel any pending removal)

**Required FIPS change**: An API to include/exclude specific peers from bloom filter computation, inferred from the forwarding policy.

### 3. MMP Metrics Feed

TollGate exposes FIPS MMP metrics through `peer_metrics()`. They are **not** inputs to any price — delivery costs one voucher per unit ([tollgate-vouchers.md](../core/tollgate-vouchers.md)), and metrics are peer-influenced, so pricing from them would let a peer price itself. They exist for operator visibility and capacity decisions. Per-peer metrics are available after the Noise IK handshake completes and MMP starts reporting:

| MMP Metric | TollGate use |
|-----------|-------------|
| `srtt_ms` | Link latency; operator visibility |
| `loss_rate` | Wasted forwarding effort; operator visibility |
| `etx` | Direct forwarding cost metric |
| `smoothed_etx` | Stable cost baseline |
| `goodput_bps` | Capacity utilization / congestion signal |
| `jitter` | Service quality indicator |
| Trend indicators | Predict near-future conditions |

The adapter subscribes to MMP metric updates over the FIPS control socket and exposes them via `peer_metrics()`. The subscription pushes per-peer state changes; `tollgate-net` reads the latest cached value when the pricing engine asks.

### 4. Peer Lifecycle Events

FIPS notifies TollGate when peers connect and disconnect:

- **Peer authenticated** (Noise IK handshake complete): TollGate creates a new peer state, sets access to `None`, begins TollGate protocol exchange
- **Peer disconnected** (link lost or orderly disconnect): TollGate cleans up peer state, closes any open channels, queues settlement

**Required FIPS change**: Callbacks for peer connect/disconnect events, providing the peer's public key and node_addr.

### 5. TollGate Protocol Transport

Initially, TollGate protocol messages travel over **raw TCP through the FIPS IPv6 adapter**. FIPS provides an IPv6 TUN interface (`fips0`) that maps each peer's npub to an `fd00::/8` address, and TollGate opens a TCP connection to port 4747 on it — the same transport as IP peering, riding on the FIPS mesh.

Nothing needs to be wrapped in TLS: the Noise IK handshake has already authenticated and encrypted the link before TollGate sees the peer. This is what lets the transport stay as thin as it is.

This approach works today without any FIPS modifications to the session layer.

**Future**: When FSP port-based service dispatch is available, TollGate can register on a dedicated FSP port for more efficient message delivery — eliminating HTTP overhead. This is an optimization, not a requirement for the initial implementation.

---

## ResourceAdapter Implementation

### set_peer_access()

Maps TollGate access levels to FIPS forwarding policy and bloom filter state:

```rust
fn set_peer_access(&self, peer: &Pubkey, access: AccessLevel) -> Result<(), AdapterError> {
    let node_addr = NodeAddr::from_pubkey(peer);

    match access {
        AccessLevel::None | AccessLevel::Suspended => {
            // Restrict peer to local-only traffic (no transit forwarding)
            // Bloom filter exclusion is inferred — restricted peers are excluded
            self.node.set_peer_forwarding_policy(node_addr, ForwardingPolicy::LocalOnly);
        }
        AccessLevel::Active | AccessLevel::Free => {
            // Allow full forwarding for this peer
            // Bloom filter inclusion is inferred — allowed peers are included
            self.node.set_peer_forwarding_policy(node_addr, ForwardingPolicy::Full);
        }
    }
    Ok(())
}
```

FIPS enforces the policy in its existing forwarding path. `LocalOnly` means only traffic addressed to this node is accepted from the peer; all transit is dropped.

### subscribe_meter()

FIPS livestreams per-peer rx/tx byte counters over the control socket. The adapter subscribes once per peer and wraps the stream as a `MeterStream`:

```rust
fn subscribe_meter(&self, peer: &Pubkey) -> Result<MeterStream, AdapterError> {
    let node_addr = NodeAddr::from_pubkey(peer);

    // Subscribe to FIPS's per-peer counter livestream over the control socket.
    // Each push updates the watch channel; tollgate-core draws the peer's grant down against it.
    let (delivered_tx, delivered) = watch::channel(0);
    let (received_tx, received) = watch::channel(0);
    self.subscribe_peer_counters(node_addr, delivered_tx, received_tx);

    Ok(MeterStream { delivered, received })
}
```

**Required FIPS change**: control-socket subscription that livestreams per-peer rx/tx byte counts. FIPS already tracks `LinkStats` internally — needs a generic per-peer counter feed exposed on the socket.

### peer_metrics()

Direct read from the peer's MMP state:

```rust
fn peer_metrics(&self, peer: &Pubkey) -> Option<PeerMetrics> {
    let node_addr = NodeAddr::from_pubkey(peer);
    let mmp = self.node.get_peer_mmp(node_addr)?;

    Some(PeerMetrics {
        srtt_ms: Some(mmp.srtt_ms),
        loss_rate: Some(mmp.loss_rate),
        etx: Some(mmp.smoothed_etx),
        goodput_bps: Some(mmp.goodput_bps),
        jitter_ms: Some(mmp.jitter as u32),
    })
}
```

Backed by a streaming subscription on the FIPS control socket, not per-call polling.

---

## Peer Identification

FIPS peers are identified by:
- **Public key**: secp256k1 compressed public key (33 bytes) — same as TollGate's peer identifier
- **node_addr**: SHA-256 hash of public key, truncated to 16 bytes — used in packet headers and bloom filters
- **FIPS address**: `0xfd` followed by the first 15 bytes of the node_addr — the peer's IPv6 address on `fips0`

The adapter maps between them as needed. TollGate protocol uses pubkey; FIPS forwarding uses node_addr; the control plane arrives from the FIPS address. All three mappings are deterministic and derived locally (`crates/tollgate-net/src/fips.rs`), so nothing has to be asked of the daemon.

### Verifying the peer

An `Announce` is unauthenticated — it is the first thing a stranger says. On plain IP there is nothing to check it against, and an adapter that binds pubkey to address does so on the peer's own say-so: claim a paying peer's key and your address rides their grant.

A FIPS address is a commitment to a key. The mesh routes to it only for the node that completed the Noise IK handshake for that key, so an impostor cannot receive at the address it would have to claim. `forwarding.mode: fips` therefore turns on the check (`wire::Identify::Fips`): every control connection, accepted or dialled, must come from the FIPS address of the key it announces, or it is dropped before a session exists.

Two consequences worth stating:

- The control plane has to be bound where mesh peers reach it — `network.listen` on the node's `fips0` address, or on `[::]`. A connection arriving on plain IPv4 is refused under this mode rather than waved through, which is the point: otherwise a node that also answered on its uplink would have a way in that skips the handshake.
- `tollgated` and `fipsd` on the same node must run the same key. The check compares a TollGate pubkey against a FIPS address; if the two daemons hold different identities, every peer's derived address is somebody else's.

---

## What MMP Metrics Are Good For

FIPS MMP provides the richest metric set of any TollGate deployment target.
An earlier design fed it straight into a pricing formula — `price = base x
etx x (1 + srtt_ms / 100)`, mirroring FIPS's own link cost. That is gone,
and deliberately so: **the peer being priced is the peer that influences the
metrics**, so a peer could degrade its own link to move its own price. Under
vouchers there is no formula to attack, because delivery has no price.

What the metrics remain good for:

- **Operator visibility.** Which links are healthy, which are degrading.
- **Capacity decisions.** How much to issue vouchers against, and how fast
  to let a peer draw them against a grant
  ([tollgate-vouchers.md](../core/tollgate-vouchers.md)).
- **Deciding what to sell vouchers for.** An operator watching its own links
  degrade may choose to issue less or price higher on the market. That is a
  human or policy decision outside the protocol, not a formula the
  counterparty can manipulate.

---

## FIPS Feature Requests Summary

The following FIPS modifications are required for TollGate integration. Full details in [FIPS_FEATURE_REQUESTS.md](../FIPS_FEATURE_REQUESTS.md).

| Feature | Purpose | Priority |
|---------|---------|----------|
| Per-peer forwarding policy | Set `local_only` or `full` forwarding per peer (default: `local_only`) | Critical |
| Bloom filter exclusion | Withhold unpaid peers from bloom filter computation | Critical |
| Per-peer traffic counters | Outbound/inbound byte counts per peer | Critical |
| Peer lifecycle callbacks | Notify on peer connect/disconnect | Critical |
| MMP metrics access | Direct read of per-peer MMP state | High |
| Future: FSP port dispatch | TollGate messages on native FSP port | Low (future, optimization) |
| Future: payment-aware routing | Well-paying peers get favorable routing | Low (future) |

---

## Differences from IP Peering

| Aspect | FIPS | IP |
|--------|------|-----|
| Integration | `tollgate-net` and `fips` are separate binaries; communicate over the FIPS control socket | `tollgate-net` standalone binary; uses kernel firewall + accounting |
| Forwarding policy | FIPS per-peer `local_only`/`full` (control socket) | nftables/iptables rules |
| Bloom filters | Controlled by `tollgate-net` (control socket) | N/A |
| Metering counters | Livestreamed by FIPS per-peer (control socket) | Firewall accounting |
| Metrics | MMP (SRTT, loss, ETX, goodput, jitter) — control-socket subscription | None / coarse |
| Peer discovery | Automatic (FIPS mesh protocol) | Dynamic probing / static |
| Authentication | Noise IK (automatic) | Unauthenticated (default) |
| Announced identity | Checked against the address it arrives from | Taken on trust |
| Message transport | Raw TCP over IPv6 adapter (initially), FSP port (future) | Raw TCP, or HTTP/WS if a network requires it |
| Control plane overhead | Negligible; livestreamed counters | Per-peer firewall rule installs/removes |

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Integration model | Separate binaries; `tollgate-net` talks to FIPS over the control socket | FIPS exposes generic capabilities; independent release cycles |
| Counter delivery | FIPS livestreams per-peer rx/tx over the control socket | `tollgate-net` always has a fresh value at metering-interval snapshot time without polling |
| Forwarding policy | Per-peer `local_only` or `full`, enforced by FIPS | Simple data-plane policy, not a control-plane hook |
| Default new-peer policy | `local_only` | Closes race window between FIPS auth and TollGate detection |
| Bloom filter control | Inferred from forwarding policy, with 30s removal delay | Prevents flapping on temporary balance exhaustion |
| Metering counters | Per-peer watch channels from FIPS | Continuous push, drawn against the peer's grant |
| Metrics | Streaming subscription on the control socket | Pricing engine reads cached values; no per-call IPC |
| Message transport (initial) | Raw TCP over the FIPS IPv6 adapter | Works today with no FIPS session-layer changes, and needs no TLS — Noise IK already encrypts the link |
| Message transport (future) | Native FSP port | Optimization; removes the TCP handshake per session |
| MMP metrics | Exposed for visibility, never an input to price | The peer influences its own metrics, so pricing from them lets it price itself |
| Peer identification | pubkey <-> node_addr mapping | Deterministic, same keypair serves both |

---

## Per-Peer Rate

**Not yet expressible over FIPS.** A grant buys a rate, and the control-socket
surface FIPS exposes today is a binary forwarding policy — `local_only` or
`full` — which cannot say 3.12 MiB/s.

Shaping outside FIPS only reaches part of the traffic: each node is a distinct
`fd00::/8` address on the TUN interface, so `tc` there can shape what terminates
at or originates from this node, but **transit never traverses the TUN** and
transit is what a gateway sells.

So this is a FIPS-side change, requested as feature 2 in
[FIPS_FEATURE_REQUESTS.md](../FIPS_FEATURE_REQUESTS.md). Until it lands, a
TollGate node over FIPS can gate delivery but cannot deliver a bought rate, and
the IP adapter ([peering-ip.md](peering-ip.md)) is the only substrate where the
full model runs.

---

## Internet Exit (FIPS → Legacy Internet)

A FIPS node with legacy internet connectivity can act as an **exit node** for
FIPS-only nodes that have no IP stack. This bridges the FIPS network to the
legacy internet.

### GRE Tunnel Approach

The recommended tunneling mechanism is **GRE** (Generic Routing
Encapsulation):

- **4 bytes** header overhead (24 bytes total with outer IP) — far lighter
  than WireGuard's ~80 bytes
- **No redundant encryption** — FIPS already provides end-to-end encryption
- **Kernel-space** on OpenWrt (`kmod-ip-gre`)
- Carries all IP traffic types (TCP, UDP, ICMP)

### Exit Node Architecture

```
  FIPS-only node ──FIPS mesh──→ Exit node ──NAT──→ Legacy internet
  (no IP stack)                 (FIPS + WAN)
         │                           │
         └───── GRE tunnel ─────────┘
               (MTU 1476)
```

### Phased Implementation

| Phase | Goal | Payments |
|-------|------|----------|
| 1 | Binary allow/deny on GRE tunnel | None (price = 0) |
| 2 | TollGate RS pricing on tunnel interface | Cashu micropayments via nft/tc/BPF |
| 3 | Full FIPS-only network (no IP internally) | Market for exit access |

### Proof of Concept

Three OpenWrt routers. Router A has WAN. Routers B and C have FIPS only.
All three run FIPS and can ping each other via `npub.fips` addresses.
GRE tunnels from B→A and C→A. NAT masquerade on A. B and C can reach
the legacy internet through A's GRE tunnel.

Test progression: Docker → QEMU → physical routers.

---

## Wi-Fi Mesh Architecture

The physical layer recommendation for TollGate deployments using
GL-MT6000 (MT7986) and GL-MT3000 (MT7981) routers:

- **802.11s mesh** for router-to-router backbone (best-supported alternative
  mode on MediaTek mt76 driver, provides multi-hop forwarding via HWMP and
  self-healing)
- **Standard Wi-Fi AP** for phone-to-router connections (phones cannot join
  802.11s directly — no Android/iOS API)
- **FIPS as application overlay** on top of whatever IP connectivity exists

Not recommended: Wi-Fi Direct (fragile on mt76, star topology, deprecated on
Android 13+), Wi-Fi Aware/NAN (not supported on MediaTek chips).
