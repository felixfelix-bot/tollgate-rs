# FIPS Feature Requests for TollGate Integration

This document consolidates all FIPS modifications required for tollgate-rs integration. `tollgate-net` and FIPS run as independent binaries and communicate over FIPS's control socket. Each feature below is framed as a generic capability FIPS exposes on that socket. Each feature is referenced from the relevant TollGate design doc.

---

## Critical (Required for v1)

### 1. Per-Peer Forwarding Policy

**What**: Control-socket command to set a per-peer forwarding policy — `local_only` or `full`.

**Behavior**:
- `local_only`: Only accept traffic FROM this peer addressed TO this node. Drop all transit traffic (addressed to other nodes) from this peer. Do not forward traffic from other nodes to this peer.
- `full`: Normal forwarding — no restrictions.

**Default for new peers must be `local_only`** — closes the race window between FIPS authenticating a peer and `tollgate-net` setting the access level. No traffic is forwarded for a peer until the operator (`tollgate-net` or any other consumer) explicitly allows it.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 2. Per-Peer Forwarding Rate

**What**: Control-socket command to set a per-peer forwarding rate, in bytes per second, enforced **at the forwarding decision**.

**Behavior**:
- The rate bounds bytes FIPS forwards **toward** that peer — its download. `0` blocks; an absent value means unlimited.
- Enforcement is a token bucket whose burst is bounded well under a second. Capacity left unused must **not** accumulate: a peer that idles and then bursts is precisely what the payment model exists to prevent.
- The setting must be changeable several times per second and take effect immediately. A payer may buy a new rate as often as `min_window_ms` allows — 200 ms by default — and a grant takes effect when it arrives, with no acknowledgement.
- Orthogonal to the forwarding policy in feature 1. A `local_only` peer still has a rate, because the minimum flow allowance is a rate rather than an exemption.

**Why this cannot be done outside FIPS**: TollGate sells a **rate**, not an allowance. A grant is a quantity paired with a window, and what it buys is one divided by the other ([tollgate-vouchers.md](core/tollgate-vouchers.md)). The binary `local_only`/`full` policy in feature 1 cannot express 3.12 MiB/s, so without this the provider has no way to deliver what it sold.

Shaping outside FIPS only reaches part of the traffic. Each node is a distinct `fd00::/8` address on the TUN interface, so `tc` against that interface can shape traffic **terminating at or originating from** this node. **Transit never traverses the TUN** — it is forwarded inside FIPS — and transit is exactly what a gateway sells. So the enforcement point has to be the forwarding path itself.

**Note on the direction that is not shaped**: a peer's *upload* is not rate-limited by this. TollGate charges uploads through the `received_multiplier`, which draws down that peer's own grant faster rather than capping its ingress. A peer that pushes more simply exhausts its grant sooner and falls to the allowance.

**Complexity**: Medium — the forwarding path exists; this adds a per-peer bucket check before the forward and a control-socket setter.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-vouchers.md](core/tollgate-vouchers.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 3. Bloom Filter Exclusion

**What**: Bloom filter inclusion is inferred from the forwarding policy — `local_only` peers are excluded; `full` peers are included. This is a derived behavior of the policy from feature 1, not a separate API.

**Behavior**:
- Peers with `local_only` policy are excluded from outbound bloom filters (their node_addr is not advertised to other peers)
- Peers with `full` policy are included normally
- When a peer transitions from `full` to `local_only`, removal should be **delayed by 30 seconds** to avoid bloom filter flapping when a peer temporarily exhausts its payment balance. If the peer recovers within the delay, the removal is cancelled.
- When a peer transitions from `local_only` to `full`, inclusion is immediate.

**Why**: If an unpaid peer appears in bloom filters, other nodes may route traffic through it, only to have it dropped at the gate — wasting bandwidth and causing delivery failures.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 4. Per-Peer Traffic Counter Livestream

**What**: Control-socket subscription that livestreams per-peer rx/tx byte counts.

**Current state**: FIPS **already tracks per-peer link stats** via `LinkStats` on each peer (`peer.link_stats().bytes_sent`, `peer.link_stats().bytes_recv`). These count all link-layer bytes sent/received per peer, which is exactly what TollGate needs (all bytes are metered, including protocol overhead — negligible).

**What's needed**: Add a control-socket subscription that pushes per-peer counter updates as they change (or at a reasonable rate, e.g., once per second). A consumer subscribes once per peer and receives a stream of `{node_addr, bytes_sent_total, bytes_recv_total}` updates. `tollgate-net` draws each peer's grant down against the latest received value — no polling needed.

**Complexity**: Low — the data already exists internally, just needs to be exposed as a streaming subscription on the socket.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-metering.md](core/tollgate-metering.md)

---

### 5. Peer Lifecycle Events

**What**: Control-socket event stream announcing peer connect / disconnect.

**Events**:
- **Peer authenticated**: emitted after Noise IK handshake completes. Provides the peer's compressed public key (33 bytes) and node_addr (16 bytes).
- **Peer disconnected**: emitted when a peer link is lost (timeout, orderly disconnect, or error). Provides the same identifiers.

**Why**: `tollgate-net` (or any external consumer) needs to create per-peer state on connect (set initial `local_only` policy, begin protocol exchange) and clean up on disconnect (close channels, queue settlement).

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

## High Priority

### 6. MMP Metrics Subscription

**What**: Control-socket subscription that streams per-peer MMP (Metrics Measurement Protocol) state changes.

**Metrics needed**:
- `srtt_ms` (smoothed round-trip time)
- `loss_rate` (packet loss fraction)
- `smoothed_etx` (expected transmission count)
- `goodput_bps` (throughput in bytes/sec)
- `jitter` (latency variance)
- Trend indicators (rising/falling/stable) for RTT, loss, goodput

**Current state**: These metrics exist in FIPS and are queryable on-demand via the existing `show_mmp` control-socket command. For TollGate, polling would work but is wasteful when the values change continuously.

**What's needed**: A subscription mode on the existing socket — consumer subscribes once and receives pushed updates as MMP state changes (or at a coalesced rate). `tollgate-net` keeps the latest value cached and reads it when the operator asks. The `show_mmp` query mode can stay alongside for tooling.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-metering.md](core/tollgate-metering.md)

---

## Future

### 7. FSP Port Dispatch for TollGate

**What**: Register a dedicated FSP (FIPS Session Protocol) port for TollGate message delivery.

**Why**: The initial implementation uses HTTP over the FIPS IPv6 adapter, which works but adds HTTP overhead. A native FSP port would allow direct CBOR message delivery without HTTP framing.

**Priority**: Low — the IPv6 adapter approach works today. This is a performance optimization.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

### 8. Payment-Aware Routing

**What**: Allow the forwarding decision to consider payment status — well-paying peers get more favorable routing decisions.

**How this might work**: When `find_next_hop()` ranks candidate peers, include a payment quality signal (e.g., "this peer pays on time, has high balance, good payment history") as a factor alongside tree distance and link cost.

**Why**: Creates a market incentive — peers that pay more get better service. This aligns network quality with economic incentives.

**Priority**: Low — not needed for initial deployment. Requires careful design to avoid routing instability.

**Referenced in**: [tollgate-metering.md](core/tollgate-metering.md), [peering-fips.md](network-peering/peering-fips.md)

---

## Internet Exit & Tunneling

### 9. TUN/TAP Virtual Interface for Internet Exit

**What**: A FIPS node with legacy internet connectivity can act as an exit node for other FIPS-only nodes that have no IP stack. Traffic from FIPS-only nodes is bridged to the legacy internet through a virtual interface on the exit node.

**Problem**: FIPS nodes that speak only the FIPS protocol (no IP address, no DHCP, no IP firewall) cannot reach services on the legacy internet (websites, DNS, etc.) without a bridge through a node that has both FIPS and a WAN link.

**Approach**: Use a lightweight tunnel (GRE recommended — see feature 10) between the FIPS-only node and the exit node. The exit node performs NAT and forwards tunneled traffic to its WAN interface.

**Why not WireGuard**: FIPS provides end-to-end encryption natively. WireGuard adds ~80 bytes/packet of redundant crypto overhead. On constrained links (LoRa, mesh hops) this is significant.

**Phase plan**:
- **Phase 1**: Binary allow/deny on the tunnel interface. Price = 0. No TollGate payments — just prove connectivity.
- **Phase 2**: Attach TollGate pricing to the tunnel interface (nft counters, tc shaping, or BPF on the GRE device). Granular Cashu micropayments for metered access.
- **Phase 3**: Full FIPS-only network — remove all IP internally. Only loopback + FIPS interface + GRE tunnel. Multiple exit nodes form a market (users choose cheapest/best-rated exit).

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

### 10. GRE Tunnel Setup API (Control Socket)

**What**: Control-socket command to configure a GRE (Generic Routing Encapsulation) tunnel between the local FIPS node and a remote FIPS node acting as an internet exit.

**Why GRE**: GRE has only 4 bytes of header overhead (24 bytes total with outer IP), is kernel-space on Linux/OpenWrt (`kmod-ip-gre`), carries all IP traffic types (TCP/UDP/ICMP), and adds zero redundant encryption since FIPS is already E2E encrypted.

**Comparison of tunneling options**:

| Option | Overhead | OpenWrt Package | Encryption | Verdict |
|--------|----------|-----------------|------------|---------|
| **GRE** | 24 bytes | `kmod-ip-gre` | None (FIPS already E2E) | **Best** — lightest viable |
| IPIP | 20 bytes | `kmod-ipip` | None | Good, but no multicast |
| WireGuard | ~80 bytes | `wireguard-tools` | Redundant over FIPS | Wasteful |
| TUN/TAP | Varies | `kmod-tun` | None | Not standalone — needs carrier |
| PPPoE | 8 bytes | `ppp-mod-pppoe` | None | Wrong use case (ISP last-mile) |

**What's needed**: A control-socket command that:
1. Creates a GRE interface (`gre0`) pointing at a remote FIPS node's address
2. Sets MTU to 1476 (1500 - 24 GRE overhead)
3. Optionally applies forwarding/NAT rules on the exit node
4. Optionally applies firewall rules (allow/deny per peer) for Phase 1 binary gating

**Proof of concept**: Install `kmod-ip-gre kmod-gre luci-proto-gre` on 3 OpenWrt routers. Router A (has WAN): GRE endpoints for B and C + NAT masquerade. Routers B and C: GRE endpoint to A + default route through tunnel. Disable IP stack on B and C (loopback only + FIPS + GRE).

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

### 11. Per-FIPS-Instance Pricing Profiles

**What**: Support for running multiple complete FIPS instances on a single device, each with its own pricing profile, to handle heterogeneous peer media.

**Problem**: A device peering over LoRa (slow, expensive per MB) and a device peering over fiber (fast, cheap per MB) cannot charge the same price per megabyte. A single FIPS instance has a single pricing model.

**Solution**: Run multiple complete FIPS instances, each bound to a different physical interface, with its own pricing configuration. The FIPS protocol itself does **not** need to change — instance management is at the TollGate layer.

**How**: `tollgate-net` manages multiple FIPS daemon processes (or multiple FIPS interfaces within a single daemon), each with:
- Its own peer set (bound to a specific radio/interface)
- Its own pricing model (per-MB, per-second, or flat-rate)
- Its own access policy

**Example**: A reseller router with two upstream peers:
- Fiber peer: 0.001 sats/MB, high throughput
- LoRa peer: 0.1 sats/MB, low throughput, backup only

Each FIPS instance advertises independently. Customers connect to whichever instance offers the best price/throughput for their needs.

**Referenced in**: [tollgate-pricing.md](core/tollgate-pricing.md), [peering-fips.md](network-peering/peering-fips.md)

---

### 12. TTL/Ping Proximity Signal

**What**: Use TTL (Time-To-Live) and ping latency as a signal for physical proximity, exposed via the control socket.

**Use cases**:
- **Museum pairing**: A FIPS device on a museum exhibit only pairs with visitors who can prove physical proximity (sub-10ms ping, TTL ≤ N). Prevents thousands of remote visitors from overwhelming the device.
- **Balloon DoS protection**: An ESP32 on a high-altitude balloon connecting two continents over the ocean can reject or rate-limit pings from the far side based on TTL + latency, protecting constrained hardware from DoS.
- **Gamification**: "Been there" certificates — a FIPS node issues a signed certificate to a visitor who proves proximity. Collectible like ham radio call signs.

**What's needed**:
- Control-socket query: `ping_proximity <node_addr>` → returns `{srtt_ms, ttl, hop_count_estimate}`
- Optional per-peer policy: `max_ping_ms` / `max_ttl` thresholds that automatically reject connections from peers too far away
- Combine with rate limiting and price-based gating for layered DoS protection

**Complexity**: Low — ping data already exists in MMP metrics (`srtt_ms`). TTL extraction requires reading the IP header of incoming packets, which is available at the FIPS transport layer.

---

## Wi-Fi Mesh Architecture

The physical layer recommendation for TollGate deployments:

- **802.11s mesh** for router-to-router backbone. Best-supported mode on MediaTek mt76 chipsets (GL-MT6000, GL-MT3000). Provides multi-hop forwarding (HWMP protocol) and self-healing.
- **Standard Wi-Fi AP** for phone-to-router connections. Phones cannot join 802.11s directly (no Android/iOS API). Phones connect via normal Wi-Fi SSID to the nearest router.
- **FIPS as application overlay** on top of whatever IP connectivity exists. FIPS runs as a daemon on each device, providing identity, encryption, and routing independent of the physical layer.

**Not recommended**:
- Wi-Fi Direct (star topology, deprecated on Android 13+, fragile on mt76)
- Wi-Fi Aware/NAN (not supported on MediaTek chips, inconsistent Android vendor support)

---

## Summary

| # | Feature | Priority | Complexity |
|---|---------|----------|-----------|
| 1 | Per-peer forwarding policy (`local_only`/`full`) | Critical | Medium |
| 2 | **Per-peer forwarding rate (bytes/sec, at the forwarding decision)** | Critical | Medium |
| 3 | Bloom filter exclusion (inferred from policy) | Critical | Medium |
| 4 | Per-peer traffic counter livestream (control socket) | Critical | Low — data exists |
| 5 | Peer lifecycle event stream (control socket) | Critical | Low |
| 6 | MMP metrics streaming subscription | High | Low |
| 7 | FSP port dispatch | Future | Medium |
| 8 | Payment-aware routing | Future | High |
| 9 | TUN/TAP virtual interface for internet exit | High | Medium |
| 10 | GRE tunnel setup API (control socket) | High | Medium |
| 11 | Per-FIPS-instance pricing profiles | Future | Medium |
| 12 | TTL/Ping proximity signal | Future | Low |

Feature 2 is the one the voucher model added. Features 1 and 3–5 describe a
binary gate, which was sufficient when payment bought a balance and service
stopped once it ran out. A grant now buys a **rate**, so the provider needs to
deliver an arbitrary bytes-per-second figure rather than an on/off decision, and
nothing in FIPS can express that today.
