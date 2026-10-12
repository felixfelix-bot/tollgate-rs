//! A channel backend that keeps the protocol's shape without its cryptography.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use tollgate_protocol::{ChannelId, PubKey, Signature};

use super::{ChannelBackend, FundedChannel, VerifiedChannel};
use crate::identity::{Identity, verify_update};

/// A channel backend that keeps the protocol's shape without its cryptography.
///
/// It produces real channel identifiers, enforces real capacities, and rolls
/// over for real — everything the state machine above it depends on. What it
/// does **not** do is lock funds in a 2-of-2 multisig or move any ecash: there
/// are no proofs, so nothing is actually at stake and settlement is a no-op.
///
/// That makes it right for tests and for demonstrating the protocol, and wrong
/// for anything holding value. A Cashu Spilman backend implementing the same
/// trait replaces it without the layers above noticing.
#[derive(Debug)]
pub struct LocalChannels {
    /// Signs and checks ratchet turns. A real Cashu backend would use the
    /// channel's own key material instead.
    identity: Identity,
    /// Distinguishes channels opened between the same pair against the same
    /// mint, so a rollover gets a genuinely new id rather than reopening the
    /// one it is replacing.
    nonce: AtomicU64,
    /// Channels we have funded or verified, so settlement can tell a channel we
    /// know from one we have never seen.
    known: Mutex<Vec<ChannelId>>,
}

impl LocalChannels {
    /// A backend with no channels open.
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            nonce: AtomicU64::new(0),
            known: Mutex::new(Vec::new()),
        }
    }
}

/// Wire form of the funding blob: who funded it, against which mint, for how
/// much, with which nonce. The receiver recomputes the channel id from exactly
/// these bytes, so the two sides cannot disagree about which channel they mean.
fn channel_id_of(funder: PubKey, mint_url: &str, capacity: u64, nonce: u64) -> ChannelId {
    let mut hasher = Sha256::new();
    hasher.update(funder.0);
    hasher.update((mint_url.len() as u32).to_be_bytes());
    hasher.update(mint_url.as_bytes());
    hasher.update(capacity.to_be_bytes());
    hasher.update(nonce.to_be_bytes());
    ChannelId(hasher.finalize().into())
}

fn encode_funding(funder: PubKey, mint_url: &str, capacity: u64, nonce: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(33 + 4 + mint_url.len() + 16);
    out.extend_from_slice(&funder.0);
    out.extend_from_slice(&(mint_url.len() as u32).to_be_bytes());
    out.extend_from_slice(mint_url.as_bytes());
    out.extend_from_slice(&capacity.to_be_bytes());
    out.extend_from_slice(&nonce.to_be_bytes());
    out
}

fn decode_funding(blob: &[u8]) -> Result<(PubKey, String, u64, u64)> {
    if blob.len() < 33 + 4 {
        bail!("funding blob is too short to hold a key and a length");
    }
    let mut funder = [0u8; 33];
    funder.copy_from_slice(&blob[..33]);

    let url_len = u32::from_be_bytes(blob[33..37].try_into().expect("4 bytes")) as usize;
    let rest = &blob[37..];
    if rest.len() != url_len + 16 {
        bail!("funding blob length does not match its declared mint URL length");
    }
    let mint_url = String::from_utf8(rest[..url_len].to_vec())?;
    let capacity = u64::from_be_bytes(rest[url_len..url_len + 8].try_into().expect("8 bytes"));
    let nonce = u64::from_be_bytes(rest[url_len + 8..].try_into().expect("8 bytes"));

    Ok((PubKey(funder), mint_url, capacity, nonce))
}

impl ChannelBackend for LocalChannels {
    fn fund(&self, _peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel> {
        // A real backend funds against the peer's own mint here, which is
        // always reachable: it is the peer we are already talking to.
        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed);
        let funder = PubKey([0; 33]);
        let channel_id = channel_id_of(funder, mint_url, capacity, nonce);
        self.known.lock().expect("not poisoned").push(channel_id);

        Ok(FundedChannel {
            channel_id,
            capacity,
            funding: encode_funding(funder, mint_url, capacity, nonce),
        })
    }

    fn verify(&self, _peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel> {
        let (funder, mint_url, capacity, nonce) = decode_funding(funding)?;
        if capacity == 0 {
            bail!("a channel with no capacity buys nothing");
        }
        let channel_id = channel_id_of(funder, &mint_url, capacity, nonce);
        self.known.lock().expect("not poisoned").push(channel_id);

        Ok(VerifiedChannel {
            channel_id,
            capacity,
        })
    }

    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature> {
        Ok(self.identity.sign_update(channel_id, cumulative))
    }

    fn verify_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool {
        verify_update(peer, channel_id, cumulative, signature)
    }

    fn settle(&self, channel_id: ChannelId) -> Result<()> {
        // Settling our own vouchers costs nothing but the service we already
        // sold: it cancels our own claim. There is no mint round-trip to make.
        if !self
            .known
            .lock()
            .expect("not poisoned")
            .contains(&channel_id)
        {
            bail!("asked to settle a channel we have never seen");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(seed: u8) -> PubKey {
        let mut b = [seed; 33];
        b[0] = 0x02;
        PubKey(b)
    }

    fn backend() -> LocalChannels {
        LocalChannels::new(Identity::generate())
    }

    #[test]
    fn both_sides_derive_the_same_channel_id() {
        // The funder and the verifier compute it independently from the same
        // bytes, so there is nothing to agree on and nothing to get wrong.
        let funder = backend();
        let verifier = backend();

        let funded = funder
            .fund(peer(2), "https://b.example/mint", 1_000_000)
            .expect("fund");
        let verified = verifier.verify(peer(1), &funded.funding).expect("verify");

        assert_eq!(verified.channel_id, funded.channel_id);
        assert_eq!(verified.capacity, funded.capacity);
    }

    #[test]
    fn a_rollover_opens_a_genuinely_new_channel() {
        let backend = backend();
        let first = backend
            .fund(peer(2), "https://b.example/mint", 1_000)
            .expect("fund");
        let second = backend
            .fund(peer(2), "https://b.example/mint", 1_000)
            .expect("fund");
        assert_ne!(
            first.channel_id, second.channel_id,
            "a replacement channel must not collide with the one it replaces"
        );
    }

    #[test]
    fn a_truncated_funding_blob_is_rejected() {
        let backend = backend();
        let funded = backend
            .fund(peer(2), "https://b.example/mint", 1_000)
            .expect("fund");
        for cut in [0, 10, 33, funded.funding.len() - 1] {
            assert!(
                backend.verify(peer(1), &funded.funding[..cut]).is_err(),
                "a blob cut to {cut} bytes should not verify"
            );
        }
    }

    #[test]
    fn a_zero_capacity_channel_is_rejected() {
        let backend = backend();
        let funded = backend
            .fund(peer(2), "https://b.example/mint", 0)
            .expect("fund");
        assert!(backend.verify(peer(1), &funded.funding).is_err());
    }

    #[test]
    fn settling_a_channel_we_never_saw_is_an_error() {
        let backend = backend();
        assert!(backend.settle(ChannelId([9; 32])).is_err());
    }
}
