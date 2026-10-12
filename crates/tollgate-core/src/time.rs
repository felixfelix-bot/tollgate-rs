//! Time as a plain number the host supplies.
//!
//! Core never reads a clock. Every decision that depends on time takes a
//! `now_ms` argument, and the host is responsible for it being monotonic. The
//! newtype exists so a duration can never be passed where an instant belongs.

use core::ops::{Add, Sub};

/// A point in time, in milliseconds on the host's monotonic clock.
///
/// Only differences are meaningful — the epoch is whatever the host picked. The
/// protocol never puts a timestamp on the wire precisely so the two peers need
/// no clock agreement: a grant window is measured from receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Millis(pub u64);

impl Millis {
    /// The host's zero point.
    pub const ZERO: Self = Self(0);

    /// Milliseconds elapsed since `earlier`, saturating at zero if the clock
    /// went backwards.
    pub fn saturating_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<u64> for Millis {
    type Output = Self;

    fn add(self, ms: u64) -> Self {
        Self(self.0.saturating_add(ms))
    }
}

impl Sub<u64> for Millis {
    type Output = Self;

    fn sub(self, ms: u64) -> Self {
        Self(self.0.saturating_sub(ms))
    }
}
