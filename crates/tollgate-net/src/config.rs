//! The YAML an operator writes, and how it becomes a [`NodeConfig`].
//!
//! Every parameter has a default, so a minimal file only says what differs.
//! Delivery has no price to configure: one voucher buys one unit, and what a
//! unit costs in money is decided where vouchers are sold.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{GrantPolicy, NodePolicy, PeerPolicy};
use tollgate_protocol::{DEFAULT_PORT, PubKey};
use tracing::warn;

use crate::identity::Identity;
use crate::node::{NodeConfig, PeerConfig};
use crate::wire::Identify;

/// The whole configuration file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct File {
    /// Node identity.
    pub identity: IdentitySection,
    /// This node's own mint, and the unit it denominates in.
    pub mint: MintSection,
    /// Which mints this node takes payment in, and the default surcharge.
    pub vouchers: VouchersSection,
    /// What this node takes as payment for its own vouchers.
    pub market: MarketSection,
    /// Where it holds the money it pays peers with.
    pub wallet: WalletSection,
    /// The minimum flow allowance.
    pub access: AccessSection,
    /// Channel parameters.
    pub channels: ChannelsSection,
    /// Bounds on what a payer may buy in one purchase.
    pub grants: GrantsSection,
    /// How this node buys from its peers.
    pub buying: BuyingSection,
    /// Where to listen.
    pub network: NetworkSection,
    /// What actually delivers the resource.
    pub forwarding: ForwardingSection,
    /// A byte source clients can measure this node against.
    pub speedtest: SpeedtestSection,
    /// Per-peer overrides, keyed by hex-encoded compressed pubkey.
    pub peers: BTreeMap<String, PeerSection>,
}

/// Node identity.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdentitySection {
    /// 32-byte secp256k1 secret key in hex. Generated if absent.
    pub secret_key: Option<String>,
}

/// This node's own mint.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MintSection {
    /// Mint URL advertised to peers, and the one they fund channels against.
    ///
    /// It has to be reachable *by peers*, which it always is — it is the node
    /// they are already talking to.
    pub url: String,
    /// Where to serve the mint. Peers reach it at [`Self::url`].
    pub listen: String,
    /// Quantity unit. Fixed by the resource and identical across every node
    /// selling it.
    ///
    /// `"byte"` for network forwarding: a proof is then a claim on one byte of
    /// this node's capacity rather than on money.
    pub unit: String,
}

impl Default for MintSection {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3338".into(),
            listen: "0.0.0.0:3338".into(),
            unit: "byte".into(),
        }
    }
}

/// Which mints this node will take payment in.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct VouchersSection {
    /// Most preferred first, at least one. Defaults to this node's own mint.
    pub accepted_mints: Vec<String>,
    /// Unsigned surcharge on what a peer pushes at us. The net rate is `m - 1`,
    /// so `2` charges an upload like a download and `k + 1` charges it `k`
    /// times.
    pub received_multiplier: u16,
}

/// What this node takes as payment for its own vouchers.
///
/// Not part of the protocol: no TollGate message is denominated in money. A
/// buyer sends one of these mints' tokens and gets vouchers back, before any
/// session exists.
///
/// Empty means this node sells nothing here, which is a working configuration:
/// its peers have to obtain its vouchers somewhere else.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MarketSection {
    /// One entry per issuer whose paper this node will take.
    pub accept: Vec<AcceptedSection>,
}

/// One issuer's paper, and what a unit of it buys here.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedSection {
    /// The mint whose tokens this node will take as payment.
    pub mint: String,
    /// Which of that mint's keysets. A token in any other unit is refused.
    ///
    /// Defaults to `sat`, which is what a mint dealing in money issues. It has
    /// to be stated at all because a mint may run several: "a million bytes per
    /// unit" means different things against a sat and a cent.
    #[serde(default = "default_money_unit")]
    pub unit: String,
    /// Units of capacity one unit of that paper buys.
    ///
    /// Priced per issuer on purpose. A sat from a mint expected to honour its
    /// tokens is worth more than a sat from one that is not, and that
    /// difference is the whole point of a market in vouchers.
    pub bytes_per_unit: u64,
}

fn default_money_unit() -> String {
    "sat".into()
}

impl MarketSection {
    /// The price list, in the shape the market keeps it.
    pub fn accepted(&self) -> Vec<crate::market::Accepted> {
        self.accept
            .iter()
            .map(|a| crate::market::Accepted {
                mint: a.mint.clone(),
                unit: a.unit.clone(),
                bytes_per_unit: a.bytes_per_unit,
            })
            .collect()
    }
}

/// Where this node holds money, so it can buy what it pays peers with.
///
/// A node that only sells needs none of this: it is paid in its peers' money
/// and never has to hold any. One that buys transit has to arrive at its
/// upstream holding paper the upstream accepts, exactly as its own customers
/// have to arrive holding its.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct WalletSection {
    /// A mint that sells its paper for money. Empty means this node cannot buy.
    ///
    /// Defaults to a public one, because a node that buys transit has to hold
    /// somebody's money and this is a working answer rather than a preference.
    pub mint: String,
    /// The unit that mint denominates in.
    #[serde(default = "default_money_unit")]
    pub unit: String,
    /// Where the wallet database lives. Empty picks the state directory the
    /// packages keep across an upgrade.
    ///
    /// It holds bearer tokens: the file *is* the balance.
    pub file: String,
}

impl Default for WalletSection {
    fn default() -> Self {
        Self {
            mint: "https://mint.minibits.cash/Bitcoin".into(),
            unit: default_money_unit(),
            file: String::new(),
        }
    }
}

/// The minimum flow allowance.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccessSection {
    /// Traffic every peer gets without paying.
    pub minimum_flow: MinimumFlow,
}

/// Traffic given away.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MinimumFlow {
    /// Off by default: it is traffic given away, and it can be farmed.
    pub enabled: bool,
    /// Keep it small. Being a rate, it cannot be accumulated.
    pub bytes_per_second: u64,
}

/// Channel parameters.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelsSection {
    /// Units of capacity a new outgoing channel opens with.
    pub initial_capacity: u64,
    /// Percentage of capacity at which the funder starts a rollover.
    pub rollover_threshold_pct: u8,
    /// Drop a peer that has sent nothing at all for this long. Zero disables it.
    pub stale_timeout_seconds: u64,
}

impl Default for ChannelsSection {
    fn default() -> Self {
        Self {
            initial_capacity: 1_000_000_000,
            rollover_threshold_pct: 80,
            stale_timeout_seconds: 60,
        }
    }
}

/// What this node will accept when a peer buys capacity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrantsSection {
    /// `[min, max]` window in milliseconds. The payer picks any window in this
    /// range, per grant, without negotiating.
    pub window_range_ms: [u32; 2],
    /// Units per second this node will commit to one peer. Absent means the
    /// link is the only limit.
    pub max_rate: Option<u64>,
}

impl Default for GrantsSection {
    fn default() -> Self {
        Self {
            window_range_ms: [200, 30_000],
            max_rate: None,
        }
    }
}

/// How this node buys.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BuyingSection {
    /// Buy this percentage of observed demand.
    pub headroom_pct: u32,
    /// Only jump mid-window if the target exceeds the rate in force by this
    /// percentage. This hysteresis is what keeps forfeiture affordable.
    pub raise_threshold_pct: u32,
    /// Renew this long before the deadline.
    pub renew_lead_ms: u32,
    /// How long to respect a rate ceiling a provider named before testing
    /// whether capacity has freed up.
    pub cap_hold_ms: u64,
    /// Window to ask for, clamped to what the provider advertises.
    pub window_ms: u32,
    /// Never buy below this rate.
    pub min_rate: u64,
    /// Never buy above this rate — the operator's spending ceiling.
    pub max_rate: u64,
    /// Units per second to want from every peer, whether or not anything is
    /// asking for them.
    ///
    /// Zero — the default — means this node buys only what something observes
    /// demand for, which for a node that forwards is what its own customers
    /// pull through it. A node at the edge has no such signal: nothing measures
    /// how much of its own traffic it would like to be able to send, so an
    /// operator who wants it to keep a link paid for says how much here.
    ///
    /// It is a standing order, and it spends money: at 2 MB/s against a
    /// gateway charging 1550 sat/GiB, a day costs about 250,000 sat whether or
    /// not the link is used. `--demand` overrides it for one run.
    pub demand: u64,
}

impl Default for BuyingSection {
    fn default() -> Self {
        let d = BuyerPolicy::default();
        Self {
            headroom_pct: d.headroom_pct,
            raise_threshold_pct: d.raise_threshold_pct,
            renew_lead_ms: d.renew_lead_ms,
            cap_hold_ms: d.cap_hold_ms,
            window_ms: d.window_ms,
            min_rate: d.min_rate,
            max_rate: d.max_rate,
            demand: 0,
        }
    }
}

/// Where to listen.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkSection {
    /// Control-plane listen address. The data plane is the next port up.
    ///
    /// Under `forwarding.mode: fips` this has to be somewhere mesh peers reach
    /// — the node's own `fips0` address, or `[::]` — because a connection from
    /// anywhere else cannot prove whose key it is announcing and is refused.
    pub listen: String,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            listen: format!("0.0.0.0:{DEFAULT_PORT}"),
        }
    }
}

/// What actually delivers the resource.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForwardingSection {
    /// `loopback`, `nftables` or `fips`.
    ///
    /// `loopback` shapes and meters a socket of its own and forwards nobody's
    /// traffic — right for a demo or a test, and it runs anywhere. `nftables`
    /// gates and shapes the kernel's forwarding path, which is what actually
    /// sells transit, and needs Linux with `CAP_NET_ADMIN`. `fips` sells
    /// transit across a FIPS mesh instead, leaving the enforcement to the FIPS
    /// node and reaching it over its control socket — and, because a mesh
    /// address names a key, it is also the only mode in which a peer's
    /// announced identity is checked rather than believed.
    pub mode: ForwardingMode,
    /// Interface facing the peers, where their `tc` classes live.
    ///
    /// Only `nftables` uses this.
    pub interface: String,
    /// FIPS control socket to drive. Only `fips` uses this; empty means the
    /// same default path the FIPS daemon itself resolves.
    pub fips_socket: String,
}

impl Default for ForwardingSection {
    fn default() -> Self {
        Self {
            // The default has to run everywhere and gate nothing it does not
            // own: a node that silently installed firewall rules because of a
            // missing config line would be a nasty surprise.
            mode: ForwardingMode::Loopback,
            interface: "eth0".into(),
            fips_socket: String::new(),
        }
    }
}

/// Which adapter enforces access and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardingMode {
    /// A shaper and meter over a dedicated socket.
    Loopback,
    /// nftables and `tc` on the kernel forwarding path.
    Nftables,
    /// Per-peer transit policy on a FIPS node, over its control socket.
    Fips,
}

/// A byte source clients can measure this node against.
///
/// Off unless an operator turns it on. The endpoints are unauthenticated, and
/// on a node that is not gating transit they are a free firehose — see
/// [`crate::speedtest`] for why that is the right default even though the
/// intended deployment, behind a FIPS transit policy, is not one.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpeedtestSection {
    /// Whether to serve it at all.
    pub enabled: bool,
    /// Where to serve it.
    ///
    /// Defaults to every interface on both families, because the client this is
    /// for arrives over `fips0` on an IPv6 address and `0.0.0.0` would not hear
    /// it.
    pub listen: String,
    /// Ceiling on a single request, in either direction.
    pub max_bytes: u64,
    /// How many flows the page opens at once.
    pub streams: u8,
    /// How long the page discards before it starts counting.
    pub warmup_ms: u32,
    /// How long it counts for, after the warm-up.
    pub duration_ms: u32,
}

impl Default for SpeedtestSection {
    fn default() -> Self {
        let defaults = crate::speedtest::Config::default();
        Self {
            enabled: false,
            listen: "[::]:3339".into(),
            max_bytes: defaults.max_bytes,
            streams: defaults.streams,
            warmup_ms: defaults.warmup_ms,
            duration_ms: defaults.duration_ms,
        }
    }
}

impl SpeedtestSection {
    /// The address to serve on, and how.
    pub fn resolve(&self) -> Result<(SocketAddr, crate::speedtest::Config)> {
        let listen: SocketAddr = self
            .listen
            .parse()
            .with_context(|| format!("speedtest.listen {:?} is not an address", self.listen))?;
        // A test with no flows measures nothing, and one with no window divides
        // by zero on the page rather than here.
        if self.streams == 0 {
            bail!("speedtest.streams must be at least one");
        }
        if self.duration_ms == 0 {
            bail!("speedtest.duration_ms must be above zero");
        }
        Ok((
            listen,
            crate::speedtest::Config {
                max_bytes: self.max_bytes,
                streams: self.streams,
                warmup_ms: self.warmup_ms,
                duration_ms: self.duration_ms,
            },
        ))
    }
}

/// Per-peer overrides.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PeerSection {
    /// Do not charge this peer. One-sided, and not transitive.
    pub no_charge: bool,
    /// Refuse this peer entirely.
    pub blocked: bool,
    /// Override the node-wide received multiplier.
    pub received_multiplier: Option<u16>,
    /// Static endpoint to dial. Peers without one have to dial us, and
    /// everything else written here still applies to them when they do.
    pub endpoint: Option<String>,
}

impl File {
    /// Read a configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Resolve into what the node actually runs on.
    pub fn resolve(&self) -> Result<NodeConfig> {
        let identity = match &self.identity.secret_key {
            Some(hex) => Identity::from_hex(hex)?,
            None => Identity::generate(),
        };

        // An Offer with no mints is malformed, and a node that names none has
        // simply not said which of its own it means.
        let accepted_mints = if self.vouchers.accepted_mints.is_empty() {
            vec![self.mint.url.clone()]
        } else {
            self.vouchers.accepted_mints.clone()
        };

        let [min_window_ms, max_window_ms] = self.grants.window_range_ms;
        if min_window_ms == 0 || min_window_ms > max_window_ms {
            bail!("grants.window_range_ms must be a non-empty range starting above zero");
        }

        let policy = NodePolicy {
            unit: self.mint.unit.clone(),
            accepted_mints,
            received_multiplier: self.vouchers.received_multiplier,
            minimum_flow: if self.access.minimum_flow.enabled {
                self.access.minimum_flow.bytes_per_second
            } else {
                0
            },
            grants: GrantPolicy {
                min_window_ms,
                max_window_ms,
                max_rate: self.grants.max_rate,
            },
            initial_channel_capacity: self.channels.initial_capacity,
            stale_timeout_ms: self.channels.stale_timeout_seconds.saturating_mul(1_000),
            rollover_threshold_pct: self.channels.rollover_threshold_pct,
        };

        let buyer = BuyerPolicy {
            headroom_pct: self.buying.headroom_pct,
            raise_threshold_pct: self.buying.raise_threshold_pct,
            renew_lead_ms: self.buying.renew_lead_ms,
            cap_hold_ms: self.buying.cap_hold_ms,
            window_ms: self.buying.window_ms,
            min_rate: self.buying.min_rate,
            max_rate: self.buying.max_rate,
        };

        // A lead at least as long as the window it renews inside is not a
        // conservative setting, it is a contradiction: every grant starts
        // already inside its own renewal lead. Core clamps it rather than
        // looping, but an operator who wrote this meant something else.
        if self.buying.renew_lead_ms >= self.buying.window_ms {
            bail!(
                "buying.renew_lead_ms ({}) must be shorter than buying.window_ms ({})",
                self.buying.renew_lead_ms,
                self.buying.window_ms
            );
        }
        // Not an error: a node carrying nothing but small requests can live
        // with a short lead, and on an idle link it is free. It is a trap for
        // anything carrying TCP, and the failure — a flow that stalls for
        // seconds after a gap of a tenth of one — does not look like its cause.
        if buyer.lead_is_thin() {
            warn!(
                renew_lead_ms = self.buying.renew_lead_ms,
                window_ms = self.buying.window_ms,
                forfeit_pct = buyer.forfeit_pct(self.buying.window_ms),
                suggested_lead_ms = BuyerPolicy::MIN_SAFE_LEAD_MS,
                "a renewal this late lapses the grant under load; scale the \
                 window and the lead together to buy slack at the same cost"
            );
        }

        let listen: SocketAddr = self.network.listen.parse().with_context(|| {
            format!("network.listen {:?} is not an address", self.network.listen)
        })?;
        let mint_listen: SocketAddr = self
            .mint
            .listen
            .parse()
            .with_context(|| format!("mint.listen {:?} is not an address", self.mint.listen))?;

        let mut peers = Vec::new();
        for (key, section) in &self.peers {
            let pubkey = parse_pubkey(key)?;
            let policy = PeerPolicy {
                no_charge: section.no_charge,
                blocked: section.blocked,
                received_multiplier: section.received_multiplier,
            };
            // A peer with no endpoint is one that dials us. It is still carried
            // here, because the policy is the point: `blocked` on a peer that
            // calls in is exactly the case that matters, and dropping the entry
            // for want of an address would quietly admit it.
            peers.push(PeerConfig {
                pubkey,
                endpoint: section.endpoint.clone(),
                policy,
            });
        }

        // Not a knob of its own: what makes an announced key checkable is the
        // network carrying the control plane, and that is what `forwarding.mode`
        // already says. A FIPS node therefore verifies from the first
        // connection, with no second setting to forget.
        let identify = match self.forwarding.mode {
            ForwardingMode::Fips => Identify::Fips,
            ForwardingMode::Loopback | ForwardingMode::Nftables => Identify::Claimed,
        };

        Ok(NodeConfig {
            identity,
            policy,
            buyer,
            listen,
            identify,
            mint_listen,
            mint_url: self.mint.url.clone(),
            peers,
        })
    }
}

/// Parse a hex-encoded compressed public key.
fn parse_pubkey(s: &str) -> Result<PubKey> {
    let bytes = hex::decode(s).with_context(|| format!("peer key {s:?} is not hex"))?;
    let bytes: [u8; 33] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("peer key {s:?} is not 33 bytes"))?;
    Ok(PubKey(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_resolves_to_working_defaults() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let config = file.resolve().expect("resolve");

        assert_eq!(config.policy.unit, "byte");
        assert_eq!(config.policy.accepted_mints.len(), 1, "own mint by default");
        assert_eq!(
            config.policy.minimum_flow, 0,
            "the allowance is off by default"
        );
        assert_eq!(config.listen.port(), DEFAULT_PORT);
    }

    #[test]
    fn the_allowance_is_zero_unless_it_is_switched_on() {
        // A rate configured but not enabled is a trap worth closing: it reads as
        // "4 KiB/s free" but must mean nothing until `enabled` is set.
        let file: File = serde_yaml::from_str(
            "access:\n  minimum_flow:\n    enabled: false\n    bytes_per_second: 4096\n",
        )
        .expect("parse");
        assert_eq!(file.resolve().expect("resolve").policy.minimum_flow, 0);
    }

    #[test]
    fn a_fips_node_checks_a_peers_key_against_its_address() {
        let file: File = serde_yaml::from_str("forwarding:\n  mode: fips\n").expect("parse");
        assert_eq!(file.resolve().expect("resolve").identify, Identify::Fips);
    }

    #[test]
    fn a_node_on_plain_ip_has_nothing_to_check_a_key_against() {
        // Including nftables, which gates by address: there the announced key is
        // taken on trust, and the operator has to wrap the link itself.
        for mode in ["loopback", "nftables"] {
            let yaml = format!("forwarding:\n  mode: {mode}\n");
            let file: File = serde_yaml::from_str(&yaml).expect("parse");
            assert_eq!(
                file.resolve().expect("resolve").identify,
                Identify::Claimed,
                "{mode}"
            );
        }
    }

    #[test]
    fn a_lead_at_least_as_long_as_the_window_is_rejected() {
        // Not conservatism: every grant would start inside its own renewal
        // lead, so the operator meant something else.
        let file: File =
            serde_yaml::from_str("buying:\n  window_ms: 1000\n  renew_lead_ms: 1000\n")
                .expect("parse");
        assert!(file.resolve().is_err());
    }

    #[test]
    fn the_default_pair_is_accepted_and_is_not_thin() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let buyer = file.resolve().expect("resolve").buyer;
        assert!(!buyer.lead_is_thin());
        assert_eq!(buyer.forfeit_pct(buyer.window_ms), 30);
    }

    #[test]
    fn an_inverted_window_range_is_rejected() {
        let file: File =
            serde_yaml::from_str("grants:\n  window_range_ms: [30000, 200]\n").expect("parse");
        assert!(file.resolve().is_err());
    }

    #[test]
    fn a_peer_without_an_endpoint_is_not_dialled() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    no_charge: true\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert!(peers[0].endpoint.is_none(), "nothing to dial");
    }

    #[test]
    fn a_peer_that_dials_us_still_gets_the_policy_written_for_it() {
        // The case this exists for: a blocked peer has no endpoint, because a
        // node does not dial one it refuses to talk to. Dropping the entry for
        // want of an address would admit exactly the peer being turned away.
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    blocked: true\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert!(peers[0].policy.blocked);
    }

    #[test]
    fn a_peer_with_an_endpoint_is_dialled() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    endpoint: \"10.0.0.1:4747\"\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].endpoint.as_deref(), Some("10.0.0.1:4747"));
    }

    /// The configs the packages install have to parse, and they are not
    /// exercised by anything else — a typo in one is a router that will not
    /// start, found after it has shipped rather than before.
    #[test]
    fn the_configs_the_packages_install_are_valid() {
        for (name, text) in [
            (
                "openwrt",
                include_str!("../../../packaging/openwrt-ipk/files/etc/tollgate/tollgate.yaml"),
            ),
            (
                "macos",
                include_str!("../../../packaging/macos/tollgate.yaml"),
            ),
        ] {
            let file: File = serde_yaml::from_str(text)
                .unwrap_or_else(|e| panic!("the {name} package config does not parse: {e}"));
            let config = file
                .resolve()
                .unwrap_or_else(|e| panic!("the {name} package config does not resolve: {e:#}"));

            // Both ship without an identity, because one is generated at
            // install time. Anything else in them has to be usable as it is.
            assert_eq!(config.policy.unit, "byte", "{name}");
            assert!(config.policy.minimum_flow > 0, "{name}: no allowance");
        }
    }

    #[test]
    fn a_misspelt_key_is_an_error_rather_than_silently_ignored() {
        // `deny_unknown_fields` is what stops `recieved_multiplier` from
        // quietly meaning "default".
        assert!(serde_yaml::from_str::<File>("vouchers:\n  recieved_multiplier: 2\n").is_err());
    }
}
