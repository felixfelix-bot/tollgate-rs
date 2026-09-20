//! Spilman payment channels for the TollGate node.
//!
//! This module is the seam between TollGate and [`cdk-spilman`] (the Spilman /
//! streaming-Cashu channel protocol). `cdk-spilman` owns the protocol —
//! 2-of-2 NUT-11 funding, BIP-340 Schnorr balance updates, settlement, P2BK
//! privacy — and reaches back into the node through the `SpilmanHost` trait for
//! everything deployment-specific: storage, pricing, keyset caching, and the two
//! operations that need the node's own key.
//!
//! Enabled with the `spilman` cargo feature:
//!
//! ```bash
//! cargo check -p tollgate-net --features spilman
//! ```
//!
//! [`cdk-spilman`]: https://github.com/SatsAndSports/cashu_spilman_channels

pub mod host;

pub use host::{ChannelMeter, ChannelSettlement, TollGateHost};
