# Integration tests

Docker topologies for `tollgate-net`. The workspace is compiled **once** into
`tollgate-test:latest`; every topology runs that same image with different
configs. Nothing is rebuilt per container.

```sh
testing/scripts/build.sh          # build the image
testing/peering/test.sh           # run one topology
SKIP_BUILD=1 testing/purchase/test.sh
```

## What each one asserts

| Test | Asserts |
|---|---|
| `peering/` | Two nodes find each other, fund a channel **in each direction**, and reach Active. The client's outgoing channel and the gateway's incoming channel are the same channel — the two ratchets agreeing. |
| `purchase/` | Demand becomes a purchase and a purchase becomes a shaped rate. The client wants 2 MB/s, buys 125% of it, and the gateway shapes it to exactly that — a number neither node ever sends the other. Real bytes move. |
| `refusal/` | The gateway sells at most 3 MB/s; the client wants 8. It ends up shaped at the cap rather than blocked, and both sides log the refusal. |
| `rollover/` | Channels sized to exhaust in seconds, so the test runs through several. Buying continues across each boundary. |
| `allowance/` | A client that buys nothing stays on the minimum flow allowance — not blocked, because that allowance is what lets a peer with no vouchers acquire some. |
| `forwarding/` | The gateway carries somebody else's packets: the client pulls a large file from a third host, every packet crosses the gateway, and nftables and `tc` hold it to the rate it bought. Asserts bytes, not elapsed time. |
| `fips/` | The same claim with the enforcement in a FIPS mesh instead of the local kernel — three nodes, all traffic over `fips0`, the gateway the only node the other two can reach. Also asserts what only a mesh can: the peer that gets the grant is the peer that holds the key. |

## Every node pays for what it gets

A peer's vouchers are sold, not given away, so each topology runs a
**`sat-mint`** — `cashubtc/mintd` with the fake Lightning backend, standing in
for something like minibits. A node mints sats there, hands them to the peer's
market, and gets that peer's byte vouchers back.

That is the whole payment mechanism: money in, vouchers out, one call. The
fake backend pays its own invoices instantly, which is the only simulated
step — the mint, the keysets, the swap that redeems the payment and the
spent-proof set behind it are all real.

`testing/mint/mintd.toml` configures it, and `market.accept` in each node's
config says whose paper it takes and at what price.

## How a test sees what happened

Through each node's **control socket**, not its logs. The socket serves a JSON
snapshot of what the node believes about itself, which is what the assertions
are written against; asserting on log lines would be testing the log format.

The exception is `refusal/`, which does check the logs — because there the
thing being asserted *is* that the operator can see why a peer is pinned at a
limit.

## The FIPS topology needs a second image

`fips/` runs two daemons per container — `fipsd` forwards and `tollgated`
sells — so it builds `tollgate-fips-test:latest` on top of the ordinary image:

```sh
testing/scripts/build-fips.sh     # tollgate-test, then fips-node, then both
SKIP_BUILD=1 testing/fips/test.sh
```

`fipsd` is built from a FIPS checkout at `reference/fips`, which is not part of
this repository; point `FIPS_CHECKOUT` elsewhere if yours lives somewhere else.
The image is built from `git archive` rather than from the working tree, so
only committed state reaches it — and docker is not asked to upload a
multi-gigabyte `target/` as build context.

Each node's `nsec` is the same secret key its `tollgated` runs as. It has to
be: the control plane checks a peer's announced key against the mesh address
the connection arrived from, and that address is derived from the key.

## Identities

The keypairs are fixed and checked in. A client has to name its gateway's
public key in its config, so generating them per run would mean generating the
configs too; fixed keys also make a failure reproducible.

They are test keys. They are in a public repository and control nothing but a
container that gives its vouchers away.

## What runs inside a container

Each node runs three listeners and a socket:

| | |
|---|---|
| `4747` | TollGate control plane — the protocol itself |
| `4748` | data plane — the bytes being bought and sold |
| `3338` | this node's Cashu mint, and the market endpoint that sells its vouchers |
| `/run/tollgate.sock` | snapshot for `tolltop` and for these tests |

The mint has to be reachable **by the peer**, because a peer funds its channel
against it — which is why the configs name compose service names rather than
`localhost`.

## Adding a topology

1. `docker-compose.yml` using `image: tollgate-test:latest`.
2. A `gateway.yaml` and `client.yaml`.
3. A `test.sh` that sources `../lib/common.sh` and calls `tollgate::start`.

A topology needing a different image names its build script in `BUILD_SCRIPT`
before calling `tollgate::start`, as `fips/` does.

`common.sh` carries the waiting: these are real nodes on real sockets buying
from a real mint, so nothing is instant and a fixed sleep is either flaky or
slow. `tollgate::wait_for` polls a condition to a deadline and dumps every
node's snapshot and logs when it gives up.
