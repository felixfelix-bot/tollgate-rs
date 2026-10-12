# Market Protocol

Buying and swapping vouchers is **not part of paying for delivery**. It has
its own endpoints, its own messages, and no dependency on a TollGate session.

The boundary is stated in
[tollgate-protocol.md](../core/tollgate-protocol.md): no amount in any
TollGate message is denominated in money, no TollGate message buys or swaps
anything, and a node that offers no market services works normally.

**What does stay in the payment protocol** is the set of mints a node accepts
as payment, with a settlement ratio for each. A peer has to know what it can
pay with before it can pay, so that belongs in the Offer message. Those
ratios say how many units of delivery a voucher is credited as — they are not
exchange rates against money.

---

## Most Of This Is Already Cashu

Buying a node's own vouchers needs no new protocol. It is a standard Cashu
mint operation against the `mint_url` the node advertises:

| Operation | Standard |
|---|---|
| Ask what it costs, get an invoice | NUT-04 mint quote |
| Pay and receive vouchers | NUT-04 mint |
| Return vouchers for money | NUT-05 melt |
| Discover what the mint supports | NUT-06 info |

A node that only sells its own vouchers therefore implements **nothing** from
this document. Its Cashu mint is the whole market interface.

The endpoints below exist only for what Cashu has no answer to: pricing and
exchanging vouchers **across** mints.

---

## Endpoints

Served under a path prefix distinct from the payment protocol:

```
payment    POST /tollgate/v1/exchange        GET /tollgate/v1/ws
market     GET  /tollgate/market/v1/info
           POST /tollgate/market/v1/quote
           POST /tollgate/market/v1/swap
```

The market prefix **may be served by a different process, a different host,
or a third party**. A node that wants to offer swaps without running a market
itself points at somebody else's. Nothing in the payment path depends on it
being reachable, or existing.

Encoding is JSON rather than the payment protocol's CBOR. These calls are
infrequent, human-debuggable, and share a shape with the Cashu mint API that
sits beside them — compactness buys nothing here.

### `GET /tollgate/market/v1/info`

What this market will do. Discovery lives here rather than in the Offer
message, so a node can start or stop offering market services without
touching any payment session.

```json
{
  "version": 1,
  "sells": ["https://gateway.example.com/mint"],
  "buys":  ["https://neighbor.example.com/mint",
            "https://hub.example.com/mint"],
  "sat_swap": true,
  "cross_mint_swap": false,
  "quote_ttl_seconds": 30
}
```

`sells` and `buys` are mint URLs, not units — the unit is fixed by the
resource. `sat_swap` means it will trade vouchers against sat-denominated
tokens. `cross_mint_swap` means it will trade one mint's vouchers for
another's.

### `POST /tollgate/market/v1/quote`

Ask what it will give. A quote is an offer to trade, valid for
`quote_ttl_seconds`, and creates no obligation on the asker.

```json
{ "give": {"mint": "sat-mint-url", "amount": 300},
  "want": {"mint": "https://gateway.example.com/mint"} }
```

```json
{ "quote_id": "…", "want_amount": 1048576, "expires_at": 1730000000 }
```

The response says how much of `want` the asker gets. Both directions are
explicit, so the same endpoint covers buying, selling, and cross-mint trades.

### `POST /tollgate/market/v1/swap`

Execute a quote. The asker sends the tokens it promised; the market returns
the tokens it quoted.

```json
{ "quote_id": "…", "tokens": ["cashuA…"] }
```

```json
{ "tokens": ["cashuA…"] }
```

**Atomicity is unsolved.** As written, one side moves first and trusts the
other to complete. Making this atomic across two mints needs a hash-locked
construction (NUT-11/NUT-14 style) with both mints online, and no working
implementation exists — see
[voucher-acquisition.md](voucher-acquisition.md). Until then, exposure is one
swap's worth, and the sensible mitigation is to keep swaps small or to swap
only with a counterparty that has something to lose.

---

## Why Keep Them Apart

**The payment protocol stays small and auditable.** Its whole job is: agree
what is accepted, fund channels, count units, settle. Nothing in it needs to
know what money is.

**Market failure is not service failure.** A market that is down, illiquid,
or absent does not stop anyone delivering or paying. A peer that already
holds vouchers is unaffected.

**Constrained devices can skip it.** An ESP32 selling its own capacity runs
the payment protocol and a Cashu mint. It implements none of this.

**Market makers are not obliged to be routers.** Separating the endpoints
means a party that wants to make markets in voucher paper can do so without
forwarding a single byte, and a router can offer swaps by pointing at one.

**The unsolved parts stay contained.** Atomic cross-mint swap and the
reliability signal are the two weakest pieces of the whole design
([issuer-risk.md](issuer-risk.md)). Keeping them behind a separate interface
means their absence is a missing feature rather than a hole in the payment
path.

---

## Open Problems

| Problem | Notes |
|---|---|
| Swap atomicity | One side moves first. A hash-locked construction needs both mints online and has no working implementation. |
| Quote honesty | Nothing binds a market to honor a quote it issued. Exposure is one swap; reputation is the only correction, and it has the same observability problem as issuer default. |
| Market discovery | How a peer finds a market at all, if the node it is talking to does not run one. Unspecified — a well-known path on the peer, a directory, or out-of-band. |
