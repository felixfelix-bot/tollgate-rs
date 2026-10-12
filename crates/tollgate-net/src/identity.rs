//! The node's keypair, and the signature over a channel update.
//!
//! Core holds no keys and verifies nothing — it decides *what* to sign and
//! trusts what it is handed. Everything cryptographic lives here, which is the
//! same split FIPS uses for Noise.

use anyhow::{Context, Result};
use secp256k1::{Keypair, SECP256K1, SecretKey, XOnlyPublicKey};
use sha2::{Digest, Sha256};
use tollgate_protocol::{ChannelId, PubKey, Signature};

/// This node's secp256k1 identity.
#[derive(Debug, Clone)]
pub struct Identity {
    keypair: Keypair,
    pubkey: PubKey,
}

impl Identity {
    /// Generate a fresh keypair.
    pub fn generate() -> Self {
        let (secret, _) = SECP256K1.generate_keypair(&mut secp256k1::rand::thread_rng());
        Self::from_secret(secret)
    }

    /// Load from a 32-byte secret key in hex.
    pub fn from_hex(hex_str: &str) -> Result<Self> {
        let bytes = hex::decode(hex_str.trim()).context("identity key is not valid hex")?;
        let secret = SecretKey::from_slice(&bytes)
            .context("identity key is not a valid secp256k1 secret")?;
        Ok(Self::from_secret(secret))
    }

    fn from_secret(secret: SecretKey) -> Self {
        let keypair = Keypair::from_secret_key(SECP256K1, &secret);
        let pubkey = PubKey(keypair.public_key().serialize());
        Self { keypair, pubkey }
    }

    /// The compressed public key this node is known by.
    pub fn pubkey(&self) -> PubKey {
        self.pubkey
    }

    /// The secret key in hex, for writing to an identity file.
    pub fn secret_hex(&self) -> String {
        hex::encode(self.keypair.secret_key().secret_bytes())
    }

    /// Sign a channel update.
    pub fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Signature {
        let digest = update_digest(channel_id, cumulative);
        let sig = SECP256K1.sign_schnorr(&digest, &self.keypair);
        Signature(sig.to_byte_array())
    }
}

/// Check a channel update signature against the peer that should have made it.
///
/// The host does this **before** handing the message to core, so core can treat
/// every message it sees as authentic.
pub fn verify_update(
    peer: PubKey,
    channel_id: ChannelId,
    cumulative: u64,
    signature: Signature,
) -> bool {
    // A compressed key's first byte is the parity prefix; Schnorr verification
    // works on the 32-byte x-only form underneath it.
    let Ok(xonly) = XOnlyPublicKey::from_slice(&peer.0[1..]) else {
        return false;
    };
    let Ok(sig) = secp256k1::schnorr::Signature::from_slice(&signature.0) else {
        return false;
    };
    let digest = update_digest(channel_id, cumulative);
    SECP256K1.verify_schnorr(&sig, &digest, &xonly).is_ok()
}

/// What a channel update signature commits to: the channel and the cumulative
/// total, and nothing else.
///
/// The window is deliberately outside the signature. It is measured from
/// receipt rather than against a timestamp, so it is the provider's own clock
/// that bounds it and there is nothing for the payer to commit to.
fn update_digest(channel_id: ChannelId, cumulative: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(channel_id.0);
    hasher.update(cumulative.to_be_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_verifies_against_the_signer() {
        let id = Identity::generate();
        let channel = ChannelId([7; 32]);
        let sig = id.sign_update(channel, 5_000);
        assert!(verify_update(id.pubkey(), channel, 5_000, sig));
    }

    #[test]
    fn a_signature_does_not_verify_against_a_different_total() {
        // Otherwise a peer could replay one signature at any cumulative it
        // liked, and the ratchet would mean nothing.
        let id = Identity::generate();
        let channel = ChannelId([7; 32]);
        let sig = id.sign_update(channel, 5_000);
        assert!(!verify_update(id.pubkey(), channel, 5_001, sig));
    }

    #[test]
    fn a_signature_does_not_carry_across_channels() {
        let id = Identity::generate();
        let sig = id.sign_update(ChannelId([7; 32]), 5_000);
        assert!(!verify_update(id.pubkey(), ChannelId([8; 32]), 5_000, sig));
    }

    #[test]
    fn another_nodes_signature_is_rejected() {
        let (mine, theirs) = (Identity::generate(), Identity::generate());
        let channel = ChannelId([7; 32]);
        let sig = theirs.sign_update(channel, 5_000);
        assert!(!verify_update(mine.pubkey(), channel, 5_000, sig));
    }

    #[test]
    fn a_key_round_trips_through_hex() {
        let id = Identity::generate();
        let back = Identity::from_hex(&id.secret_hex()).expect("reload");
        assert_eq!(back.pubkey(), id.pubkey());
    }
}
