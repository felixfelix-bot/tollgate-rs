//! The raw-TCP framing: a 2-byte little-endian length prefix per message.
//!
//! ```text
//! +------------+-----------------+------------+-----------------+----
//! |  len (LE)  |  CBOR message   |  len (LE)  |  CBOR message   | ...
//! +------------+-----------------+------------+-----------------+----
//!    2 bytes      <len> bytes       2 bytes      <len> bytes
//! ```
//!
//! Two bytes caps a message at 65535 bytes, which is the point as much as the
//! compactness is: a peer cannot announce a huge length and make the receiver
//! allocate for it. No TollGate message comes close to the cap.

use alloc::vec::Vec;

use crate::codec::{Error, encode};
use crate::message::Message;

/// Largest message the 2-byte prefix can describe.
pub const MAX_FRAME_LEN: usize = u16::MAX as usize;

/// Encode a message and append it to `out` with its length prefix.
pub fn encode_frame(msg: &Message, out: &mut Vec<u8>) -> Result<(), Error> {
    // Reserve the prefix, encode in place, then backfill the real length —
    // avoids encoding into a scratch buffer and copying.
    let prefix_at = out.len();
    out.extend_from_slice(&[0, 0]);
    encode(msg, out)?;

    let len = out.len() - prefix_at - 2;
    debug_assert!(len <= MAX_FRAME_LEN, "no message can exceed the 16-bit cap");
    out[prefix_at..prefix_at + 2].copy_from_slice(&(len as u16).to_le_bytes());
    Ok(())
}

/// Incremental reader that pulls whole frames out of a growing byte buffer.
///
/// Sans-IO: the caller feeds it whatever bytes arrived, whenever they arrived,
/// and pulls out complete messages. It never reads a socket itself.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// A reader with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-read bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Take the next complete message, or `None` if one has not fully arrived.
    ///
    /// Call in a loop until it returns `None` — a single read may carry several
    /// messages.
    pub fn next_message(&mut self) -> Option<Result<Message, Error>> {
        if self.buf.len() < 2 {
            return None;
        }
        let len = u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
        if self.buf.len() < 2 + len {
            return None;
        }

        let result = crate::codec::decode(&self.buf[2..2 + len]);
        // Consume the frame whether or not it decoded: the length prefix was
        // intact, so the stream stays in sync and one bad message does not
        // poison every message behind it.
        self.buf.drain(..2 + len);
        Some(result)
    }

    /// Bytes held but not yet forming a complete message. Useful for asserting
    /// a clean teardown.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}
