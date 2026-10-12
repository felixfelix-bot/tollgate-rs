//! The resource adapter: what actually delivers, and what counts it.
//!
//! Core decides *what* to enforce; an adapter enforces it. For network
//! forwarding that means two numbers per peer — an **access level** and a
//! **shaping rate** — and two counters.
//!
//! Both are per peer, and they are orthogonal. A grant buys a rate, so a binary
//! gate cannot express what was sold; and an unpaid peer is not simply blocked,
//! because the minimum flow allowance is itself a rate. Every peer therefore
//! carries both at all times.
//!
//! Two implementations:
//!
//! - [`Loopback`] shapes and meters a dedicated socket in userspace. It
//!   forwards nobody's traffic, but the shaping and the counters are real, so it
//!   exercises the whole protocol on any platform.
//! - [`Nftables`] gates and shapes the kernel's own forwarding path with
//!   nftables and `tc`. This is the one that carries somebody else's packets,
//!   and it needs Linux and `CAP_NET_ADMIN`.
//! - [`Fips`] sets the same two numbers on a FIPS node through its control
//!   socket, and lets the mesh enforce them. The only one whose peers are
//!   authenticated before this node hears of them.

use std::net::IpAddr;

use tollgate_core::access::AccessLevel;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;

#[cfg(unix)]
mod fips;
mod loopback;
#[cfg(target_os = "linux")]
mod nftables;

#[cfg(unix)]
pub use fips::Fips;
pub use loopback::Loopback;
#[cfg(target_os = "linux")]
pub use nftables::Nftables;

/// What core needs of whatever is delivering the resource.
pub trait ResourceAdapter: Send + Sync + std::fmt::Debug {
    /// Note the address a peer reaches us from.
    ///
    /// The TollGate session identifies a peer by public key; the kernel
    /// identifies it by address. This is where the two are tied together, and
    /// an adapter that does not gate by address ignores it.
    fn register(&self, peer: PubKey, addr: IpAddr);

    /// Apply an access level decided by core.
    fn set_access(&self, peer: PubKey, access: AccessLevel);

    /// Apply a shaping rate decided by core, in units per second.
    ///
    /// Already includes the minimum flow allowance as its floor, so an adapter
    /// applies one number and needs to know nothing about grants.
    fn set_shaping_rate(&self, peer: PubKey, rate: u64);

    /// Cumulative units delivered to and received from a peer.
    fn counters(&self, peer: PubKey) -> Counters;

    /// Units per second we want to pull from this peer. Drives the buyer.
    fn demand(&self, peer: PubKey) -> u64;

    /// Set what we want to pull from a peer.
    fn set_demand(&self, peer: PubKey, rate: u64);

    /// The rate a peer is currently shaped to.
    fn shaping_rate(&self, peer: PubKey) -> u64;

    /// Every peer the adapter is tracking.
    fn peers(&self) -> Vec<PubKey>;

    /// Forget a peer that has gone away.
    fn remove(&self, peer: PubKey);
}
