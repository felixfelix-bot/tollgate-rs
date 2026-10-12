//! Gating and shaping the kernel's own forwarding path.
//!
//! This is the adapter that carries somebody else's packets. Two mechanisms,
//! because access and rate are different questions:
//!
//! - **nftables** decides *whether* a packet is forwarded, and counts what was.
//!   Membership of a named set is the gate, so changing a peer's access is one
//!   set element rather than a rule rewrite.
//! - **`tc`** decides *how fast*. One HTB class per peer, whose rate is
//!   replaced as grants arrive.
//!
//! Both are driven by shelling out to `nft` and `tc`. That is deliberate: the
//! netlink crates would pull a large dependency and a lot of unsafe for
//! something invoked a handful of times per peer per second, and `nft -j`
//! already speaks JSON.
//!
//! # What is shaped, and what is not
//!
//! Only the peer's **download** — bytes we forward toward it. Its upload is
//! charged through the `received_multiplier`, which drains that peer's own
//! grant faster rather than capping its ingress, so a peer that pushes harder
//! exhausts its grant sooner and falls to the allowance. No ingress policer is
//! involved, and that is a design choice rather than an omission.
//!
//! And only traffic we **forward**, never traffic that terminates here. The
//! distinction is load-bearing rather than tidy. A peer whose grant has lapsed
//! falls to the minimum flow allowance, and the whole point of that allowance is
//! to leave it able to reach us and buy its way back up. Shaping by destination
//! address would put the control plane and the mint in the same class as the
//! bulk transit that just exhausted the grant, so a peer saturating its link
//! would starve the very messages that would have renewed it — a deadlock that
//! ends with the session dropped as stale.
//!
//! So the forward hook marks what it forwards and `tc` classifies on that mark.
//! Locally generated traffic never traverses that hook, is never marked, and
//! falls through to the qdisc's default: unshaped.
//!
//! # Requirements
//!
//! Linux, `CAP_NET_ADMIN`, and `net.ipv4.ip_forward=1`. Without the capability
//! every command fails and the node would gate nothing while believing it had,
//! so construction probes for it and refuses to start rather than pretending.

use std::collections::HashMap;
use std::net::IpAddr;
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tollgate_core::access::AccessLevel;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;
use tracing::{debug, warn};

use super::ResourceAdapter;

/// The nftables table everything lives in, so teardown is one command.
const TABLE: &str = "tollgate";

/// Burst allowed by a peer's class, in milliseconds of its rate.
///
/// Deliberately tiny, because a grant is a quantity as well as a rate. Burst is
/// permission to run ahead of the rate, and a peer that runs ahead spends the
/// quantity before the window that sized it has elapsed — so the grant lapses
/// early, the class collapses to the allowance with a full window's worth of
/// packets in flight, and the transfer it was carrying stalls for seconds
/// recovering. A quarter-second of burst was enough to do that on every few
/// windows.
///
/// It cannot be zero: HTB needs enough to pass one full-sized packet per timer
/// tick, and a class that cannot is one that never sends. Ten milliseconds
/// clears that on any plausible tick rate while staying far inside the lead a
/// buyer renews on.
const BURST_MS: u64 = 10;

/// High bits of the packet mark this adapter sets, with the peer's class in the
/// low bits.
///
/// The mark is a field the whole box shares, so a bare small integer would be
/// asking to collide with whatever else marks packets here. This is not a
/// reservation — nothing enforces one — but it is distinctive enough that a
/// collision is a deliberate choice rather than an accident.
const MARK_BASE: u32 = 0x7011_0000;

/// One peer, as the kernel knows it.
#[derive(Debug, Clone)]
struct Peer {
    addr: IpAddr,
    /// The tc class minor number. Small, stable, and unique per peer.
    classid: u16,
    access: AccessLevel,
    rate: u64,
}

/// Gates with nftables, shapes with `tc`.
#[derive(Debug)]
pub struct Nftables {
    /// Interface facing the peers, where their classes live.
    interface: String,
    peers: Mutex<HashMap<PubKey, Peer>>,
    /// Hands out class minor numbers. Starts at 2 because 1 is the qdisc root.
    next_classid: Mutex<u16>,
}

impl Nftables {
    /// Set up the table, the base chain and the root qdisc.
    ///
    /// Fails rather than degrading: an adapter that cannot install rules would
    /// forward everything while reporting that it was gating, which is worse
    /// than not starting.
    pub fn new(interface: &str) -> Result<Self> {
        let adapter = Self {
            interface: interface.to_string(),
            peers: Mutex::new(HashMap::new()),
            next_classid: Mutex::new(2),
        };
        adapter.install()?;
        Ok(adapter)
    }

    fn install(&self) -> Result<()> {
        // Start from a clean table so a restart does not inherit rules whose
        // peers are gone. Deleting a table that is not there is not an error
        // worth caring about.
        let _ = nft(&["delete", "table", "inet", TABLE]);

        nft(&["add", "table", "inet", TABLE]).context("create the nftables table")?;

        // Peers allowed to have traffic forwarded. Access control is membership
        // of this set, so a change is one element rather than a rule rewrite.
        // Two sets. `known` is every peer we have a session with; `allowed` is
        // the subset we will forward for. Splitting them is what lets one rule
        // gate every peer: traffic for an address we have never heard of is
        // none of our business, since this node may be forwarding other things
        // too, but a peer we know and have not allowed must be dropped.
        for set in ["known", "allowed"] {
            nft(&["add", "set", "inet", TABLE, set, "{ type ipv4_addr; }"])
                .with_context(|| format!("create the {set} set"))?;
        }

        nft(&[
            "add",
            "chain",
            "inet",
            TABLE,
            "forward",
            "{ type filter hook forward priority 0; policy accept; }",
        ])
        .context("create the forward chain")?;

        // One rule per direction, covering every peer for the life of the node.
        nft(&[
            "add", "rule", "inet", TABLE, "forward", "ip", "saddr", "@known", "ip", "saddr", "!=",
            "@allowed", "drop",
        ])
        .context("gate traffic from known peers")?;
        nft(&[
            "add", "rule", "inet", TABLE, "forward", "ip", "daddr", "@known", "ip", "daddr", "!=",
            "@allowed", "drop",
        ])
        .context("gate traffic toward known peers")?;

        // The root qdisc every peer's class hangs off. `replace` so a restart
        // does not fail on one that already exists.
        //
        // The default class is deliberately one that is never created: HTB sends
        // traffic it cannot classify straight to the device, unshaped. That is
        // what carries this node's own packets — the control plane, the mint —
        // and anything else the box is doing, none of which any peer bought.
        tc(&[
            "qdisc",
            "replace",
            "dev",
            &self.interface,
            "root",
            "handle",
            "1:",
            "htb",
            "default",
            "9999",
        ])
        .context("install the root qdisc")?;

        Ok(())
    }

    /// Rule and class names derived from the peer, so they can be found again
    /// without keeping kernel handles around.
    fn counter_names(peer: PubKey) -> (String, String) {
        let tag = hex::encode(&peer.0[..6]);
        (format!("d{tag}"), format!("r{tag}"))
    }
}

impl ResourceAdapter for Nftables {
    fn register(&self, peer: PubKey, addr: IpAddr) {
        let mut peers = self.peers.lock().expect("not poisoned");
        if peers.contains_key(&peer) {
            return;
        }

        let classid = {
            let mut next = self.next_classid.lock().expect("not poisoned");
            let id = *next;
            *next = next.saturating_add(1);
            id
        };

        let (delivered, received) = Self::counter_names(peer);
        let ip = addr.to_string();

        // Known from now on, so the two gate rules apply to it.
        let _ = nft(&[
            "add",
            "element",
            "inet",
            TABLE,
            "known",
            &format!("{{ {ip} }}"),
        ]);

        // Named counters, so reading them back is one JSON dump rather than a
        // walk over rule handles.
        for name in [&delivered, &received] {
            let _ = nft(&["add", "counter", "inet", TABLE, name]);
        }
        // Counting and marking are the same rule: a packet in the forward hook
        // headed for this peer is exactly the packet its class should shape, so
        // the classification is made here rather than inferred from addresses
        // later.
        let mark = MARK_BASE | classid as u32;
        let _ = nft(&[
            "add",
            "rule",
            "inet",
            TABLE,
            "forward",
            "ip",
            "daddr",
            &ip,
            "counter",
            "name",
            &delivered,
            "meta",
            "mark",
            "set",
            &format!("{mark:#x}"),
        ]);
        let _ = nft(&[
            "add", "rule", "inet", TABLE, "forward", "ip", "saddr", &ip, "counter", "name",
            &received,
        ]);

        // A class, and a filter that selects it by the mark set above. Created
        // once; only the rate is replaced afterwards.
        let class = format!("1:{classid}");
        let _ = tc(&[
            "class",
            "replace",
            "dev",
            &self.interface,
            "parent",
            "1:",
            "classid",
            &class,
            "htb",
            "rate",
            "8bit",
        ]);
        let _ = tc(&[
            "filter",
            "replace",
            "dev",
            &self.interface,
            "protocol",
            "ip",
            "parent",
            "1:",
            "prio",
            "1",
            "handle",
            &format!("{mark:#x}"),
            "fw",
            "flowid",
            &class,
        ]);

        peers.insert(
            peer,
            Peer {
                addr,
                classid,
                access: AccessLevel::None,
                rate: 0,
            },
        );
        debug!(%peer, %addr, classid, "peer registered with the kernel");
    }

    fn set_access(&self, peer: PubKey, access: AccessLevel) {
        let mut peers = self.peers.lock().expect("not poisoned");
        let Some(entry) = peers.get_mut(&peer) else {
            return;
        };
        // Only act on a change. nftables refuses to delete an element that is
        // not there, so re-applying the same level would log an error for
        // having done nothing.
        if entry.access.delivery_allowed() == access.delivery_allowed() {
            entry.access = access;
            return;
        }
        entry.access = access;

        let element = format!("{{ {} }}", entry.addr);
        let result = if access.delivery_allowed() {
            nft(&["add", "element", "inet", TABLE, "allowed", &element])
        } else {
            nft(&["delete", "element", "inet", TABLE, "allowed", &element])
        };
        if let Err(e) = result {
            warn!(%peer, ?access, error = %e, "could not update the allowed set");
        }
    }

    fn set_shaping_rate(&self, peer: PubKey, rate: u64) {
        let mut peers = self.peers.lock().expect("not poisoned");
        let Some(entry) = peers.get_mut(&peer) else {
            return;
        };
        entry.rate = rate;

        // tc wants bits per second, and refuses a rate of zero. The floor is
        // the minimum flow allowance, which core has already applied, so zero
        // here means an adapter call arrived before any grant — one byte per
        // second is the closest honest thing to "effectively nothing".
        let bits = rate.saturating_mul(8).max(8);
        let burst = rate.saturating_mul(BURST_MS) / 1_000;

        let class = format!("1:{}", entry.classid);
        if let Err(e) = tc(&[
            "class",
            "replace",
            "dev",
            &self.interface,
            "parent",
            "1:",
            "classid",
            &class,
            "htb",
            "rate",
            &format!("{bits}bit"),
            "burst",
            &format!("{}b", burst.max(1_600)),
        ]) {
            warn!(%peer, rate, error = %e, "could not set the peer's rate");
        }
    }

    fn counters(&self, peer: PubKey) -> Counters {
        let (delivered, received) = Self::counter_names(peer);
        // One dump for both directions: the table is small, and two processes
        // per peer per tick would be the expensive part of metering.
        let all = read_counters();
        Counters {
            delivered: all.get(delivered.as_str()).copied().unwrap_or(0),
            received: all.get(received.as_str()).copied().unwrap_or(0),
        }
    }

    fn demand(&self, _peer: PubKey) -> u64 {
        // A forwarding node does not want traffic of its own: what it buys
        // upstream is driven by what its customers pull through it, which the
        // node computes from the meters rather than from an intention.
        0
    }

    fn set_demand(&self, _peer: PubKey, _rate: u64) {}

    fn shaping_rate(&self, peer: PubKey) -> u64 {
        self.peers
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|p| p.rate)
            .unwrap_or(0)
    }

    fn peers(&self) -> Vec<PubKey> {
        self.peers
            .lock()
            .expect("not poisoned")
            .keys()
            .copied()
            .collect()
    }

    fn remove(&self, peer: PubKey) {
        let Some(entry) = self.peers.lock().expect("not poisoned").remove(&peer) else {
            return;
        };
        let ip = entry.addr.to_string();
        for set in ["allowed", "known"] {
            let _ = nft(&[
                "delete",
                "element",
                "inet",
                TABLE,
                set,
                &format!("{{ {ip} }}"),
            ]);
        }
        // The filter first, then the class it points at: a class still selected
        // by a filter is in use, and the kernel refuses to delete it. Both go,
        // because a peering that ends and starts again gets a fresh classid —
        // so a filter left behind is a permanent one, and enough churn fills
        // the qdisc with filters selecting classes that no longer exist.
        let mark = MARK_BASE | entry.classid as u32;
        let _ = tc(&[
            "filter",
            "delete",
            "dev",
            &self.interface,
            "parent",
            "1:",
            "protocol",
            "ip",
            "prio",
            "1",
            "handle",
            &format!("{mark:#x}"),
            "fw",
        ]);
        let _ = tc(&[
            "class",
            "delete",
            "dev",
            &self.interface,
            "classid",
            &format!("1:{}", entry.classid),
        ]);
    }
}

impl Drop for Nftables {
    fn drop(&mut self) {
        // Leaving rules behind would silently gate traffic for a node that is
        // no longer running.
        let _ = nft(&["delete", "table", "inet", TABLE]);
        let _ = tc(&["qdisc", "delete", "dev", &self.interface, "root"]);
    }
}

fn nft(args: &[&str]) -> Result<String> {
    run("nft", args)
}

fn tc(args: &[&str]) -> Result<String> {
    run("tc", args)
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;

    if !output.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every named counter in the table, by name, with its byte total.
///
/// `list counters table` rather than a lookup per name: one process, and the
/// per-name form is not accepted by every nft build.
fn read_counters() -> HashMap<String, u64> {
    let Ok(json) = nft(&["-j", "list", "counters", "table", "inet", TABLE]) else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json) else {
        return HashMap::new();
    };

    parsed["nftables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let counter = entry.get("counter")?;
            Some((
                counter["name"].as_str()?.to_owned(),
                counter["bytes"].as_u64()?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(seed: u8) -> PubKey {
        let mut b = [seed; 33];
        b[0] = 0x02;
        PubKey(b)
    }

    #[test]
    fn counter_names_are_distinct_per_peer_and_per_direction() {
        // They are how a counter is found again without keeping kernel handles,
        // so a collision would silently merge two peers' traffic.
        let (a_in, a_out) = Nftables::counter_names(peer(1));
        let (b_in, b_out) = Nftables::counter_names(peer(2));

        assert_ne!(a_in, a_out, "the two directions must not share a counter");
        assert_ne!(a_in, b_in, "two peers must not share a counter");
        assert_ne!(a_out, b_out);
    }

    #[test]
    fn counter_names_are_valid_nftables_identifiers() {
        // nftables will not accept a name starting with a digit, which a raw
        // hex key often would.
        let (delivered, received) = Nftables::counter_names(peer(0xAB));
        for name in [delivered, received] {
            assert!(name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()));
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }
}
