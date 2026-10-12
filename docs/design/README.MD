# TollGate Design Documents

Protocol design specifications for TollGate — a permissionless payment
layer for metered resource delivery, built on Cashu ecash and Spilman
channels.

The core protocol is **resource-agnostic** — it works for network
forwarding (bytes), electricity (watt-hours), fluid delivery
(milliliters), or any metered resource. Network-specific integration
details live in the network-peering section.

**The protocol does not price anything.** Peers pay in vouchers — Cashu
tokens denominated in the resource itself, issued by the node that will
deliver it — and one voucher buys one unit
([tollgate-vouchers.md](core/tollgate-vouchers.md)). What a unit costs in
money is settled where vouchers are bought, which the market documents cover.

## Reading Order

Start with the introduction for the full architecture overview, then read
the core protocol documents in order. Market and network-peering documents
can be read independently.

### Core Protocol (Resource-Agnostic)

| Document | Description |
| -------- | ----------- |
| [tollgate-intro.md](core/tollgate-intro.md) | Introduction: goals, architecture, payment model, security |
| [tollgate-vouchers.md](core/tollgate-vouchers.md) | Vouchers: denomination, grants and windows, who pays, channels as state compression |
| [tollgate-protocol.md](core/tollgate-protocol.md) | Wire protocol: CBOR messages, interval flow, offers |
| [tollgate-payment-channels.md](core/tollgate-payment-channels.md) | Spilman channels: lifecycle, rollover, wallet trait |
| [tollgate-access-control.md](core/tollgate-access-control.md) | Access control: delivery gates, access levels, FIPS bloom filter visibility |
| [tollgate-metering.md](core/tollgate-metering.md) | Metering: local counters, what they are and are not used for, peer metrics |
| [tollgate-hazards.md](core/tollgate-hazards.md) | Constraints that exist because removing them reintroduces a known abuse |

### Configuration

| Document | Description |
| -------- | ----------- |
| [tollgate-configuration.md](core/tollgate-configuration.md) | YAML configuration reference: mint, vouchers, access, channels, peers |

### Market

Where vouchers get their money price. None of it is required to move value —
a node can sell its own vouchers and redeem them without any of this.

| Document | Description |
| -------- | ----------- |
| [market/README.md](market/README.md) | Index and relationship to the core protocol |
| [market-protocol.md](market/market-protocol.md) | Separate endpoints for buying and swapping, and why they stay out of the payment protocol |
| [voucher-acquisition.md](market/voucher-acquisition.md) | Lightning mint quotes, direct purchase, local swaps, cross-mint swaps |
| [voucher-price-signal.md](market/voucher-price-signal.md) | Selling price against face value as a reliability signal; liquidity |
| [issuer-risk.md](market/issuer-risk.md) | Overissuance, selling without redeeming, redemption congestion, shutdown |

### Network Integration (tollgate-net)

| Document | Description |
| -------- | ----------- |
| [peering-ip.md](network-peering/peering-ip.md) | Traditional IP networks: open access, HTTP/WebSocket, firewall rules |
| [peering-fips.md](network-peering/peering-fips.md) | FIPS mesh networks: native integration, MMP metrics, bloom filters |

### FIPS Changes

| Document | Description |
| -------- | ----------- |
| [FIPS_FEATURE_REQUESTS.md](FIPS_FEATURE_REQUESTS.md) | Required FIPS changes for TollGate integration |
