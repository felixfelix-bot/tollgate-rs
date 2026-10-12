//! A TollGate node: the first deployment of the protocol, selling network
//! access.
//!
//! Everything here is the host side of the sans-IO boundary. `tollgate-core`
//! decides what is owed and what to deliver; this crate supplies the sockets,
//! the clock, the signer, the channel backend and the thing that actually
//! moves bytes.
//!
//! - [`identity`] — the keypair, and the signature over a channel update. Core
//!   holds no keys.
//! - [`wire`] — the raw-TCP control plane, length-prefixed CBOR.
//! - [`dataplane`] — the resource itself: real bytes on a real socket, shaped
//!   to what was bought.
//! - [`adapter`] — the delivery gate and the meters.
//! - [`channel`] — payment channels behind a trait.
//! - [`mint`] — this node's own mint, with a byte-denominated keyset.
//! - [`market`] — selling those vouchers. A separate protocol on its own path,
//!   and the one piece that is deliberately stubbed.
//! - [`speedtest`] — a byte source on the mesh, so a client can measure the
//!   path it paid for rather than the path to somebody's CDN.
//! - [`node`] — the driver that connects all of it to core.
//! - [`config`] — the YAML the operator writes.
//! - [`control`] — a local socket publishing what the node is doing, which is
//!   what `tolltop` reads.

pub mod adapter;
pub mod channel;
pub mod config;
pub mod control;
pub mod dataplane;
pub mod fips;
pub mod identity;
pub mod market;
pub mod mint;
pub mod node;
pub mod speedtest;
pub mod wallet;
pub mod wire;

pub use identity::Identity;
pub use node::{Node, NodeConfig, PeerConfig};
