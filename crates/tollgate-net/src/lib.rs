//! Shared library for the TollGate node binaries.
//!
//! Most of the node lives in the `tollgate` binary ([`main.rs`](../main.rs)).
//! This library holds only the pieces the monitoring tool (`tolltop`) also needs:
//! the serializable [`status`] snapshot and the [`control`] socket client. Both
//! binaries depend on the same types, so the wire format can't drift between the
//! node that serves status and the tool that reads it.

pub mod control;
pub mod status;

/// Spilman payment channels (streaming Cashu) — the `SpilmanHost` adapter that
/// lets this node accept channel payments. Enabled with the `spilman` feature.
#[cfg(feature = "spilman")]
pub mod spilman;
