//! How FIPS names a TollGate key.
//!
//! TollGate carries a 33-byte compressed secp256k1 key. FIPS names the same key
//! three ways, all derived from the 32-byte x-only key — which is the
//! compressed key without its parity byte — so nothing here has to ask the mesh
//! who anybody is:
//!
//! - the **npub** (NIP-19 bech32 of the x-only key) addresses a policy,
//! - the **node address** (`SHA-256(x-only)[..16]`) keys a report, because a
//!   node address is a hash and cannot be turned back into an npub,
//! - the **FIPS address** (`0xfd` then the first 15 bytes of the node address)
//!   is the IPv6 address the node holds on `fips0`.
//!
//! That last one is what makes a peer's claim checkable. A FIPS address is a
//! commitment to a key: the mesh routes to it only for the node that completed
//! the Noise IK handshake for that key, so a connection arriving from it could
//! only have come from the holder. Deriving it here turns "who is this?" into a
//! comparison rather than a question.

use std::net::Ipv6Addr;

use sha2::{Digest, Sha256};
use tollgate_protocol::PubKey;

/// The prefix every FIPS address carries: IPv6 unique-local space, so a mesh
/// address can never be confused with a routable one.
const ADDRESS_PREFIX: u8 = 0xfd;

/// The npub and node address FIPS knows a key by.
pub fn names(peer: PubKey) -> (String, String) {
    let x_only = &peer.0[1..];

    let npub = bech32::encode::<bech32::Bech32>(bech32::Hrp::parse_unchecked("npub"), x_only)
        .expect("an x-only key is always encodable as bech32");

    let node_addr = hex::encode(&Sha256::digest(x_only)[..16]);
    (npub, node_addr)
}

/// The address a node holding this key has on `fips0`.
///
/// One byte of the node address is spent on the prefix, leaving 120 bits — the
/// same truncation FIPS itself does, and enough that finding a second key for
/// an address is not a thing an attacker does.
pub fn address(peer: PubKey) -> Ipv6Addr {
    let node_addr = Sha256::digest(&peer.0[1..]);
    let mut bytes = [0u8; 16];
    bytes[0] = ADDRESS_PREFIX;
    bytes[1..].copy_from_slice(&node_addr[..15]);
    Ipv6Addr::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key whose npub and node address are known, so the derivation is
    /// checked against FIPS's own encoding rather than against itself.
    ///
    /// Taken from `PeerIdentity::from_npub` in the FIPS tree: the npub below
    /// decodes to this x-only key, and FIPS derives the node address as
    /// `SHA-256(x-only)[..16]`.
    const NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
    const X_ONLY_HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

    fn key() -> PubKey {
        let mut bytes = [0u8; 33];
        // The parity byte is not part of the identity FIPS names, so either
        // prefix must produce the same names.
        bytes[0] = 0x02;
        bytes[1..].copy_from_slice(&hex::decode(X_ONLY_HEX).unwrap());
        PubKey(bytes)
    }

    #[test]
    fn a_key_maps_to_the_npub_fips_names_it_by() {
        let (npub, _) = names(key());
        assert_eq!(npub, NPUB);
    }

    #[test]
    fn the_parity_byte_does_not_change_the_identity() {
        let mut odd = key();
        odd.0[0] = 0x03;
        assert_eq!(names(key()), names(odd));
        assert_eq!(address(key()), address(odd));
    }

    #[test]
    fn a_key_maps_to_the_node_address_fips_reports_it_by() {
        let (_, node_addr) = names(key());
        let expected = hex::encode(&Sha256::digest(hex::decode(X_ONLY_HEX).unwrap())[..16]);
        assert_eq!(node_addr, expected);
        assert_eq!(node_addr.len(), 32, "16 bytes, hex-encoded");
    }

    /// `FipsAddress::from_node_addr` in the FIPS tree: the prefix byte, then
    /// the first 15 bytes of the node address.
    #[test]
    fn a_key_maps_to_the_address_it_holds_on_the_mesh() {
        let node_addr = Sha256::digest(hex::decode(X_ONLY_HEX).unwrap());
        let mut expected = [0u8; 16];
        expected[0] = 0xfd;
        expected[1..].copy_from_slice(&node_addr[..15]);

        assert_eq!(address(key()), Ipv6Addr::from(expected));
        assert_eq!(
            address(key()).octets()[0],
            0xfd,
            "a mesh address is unique-local"
        );
    }

    #[test]
    fn different_keys_get_different_addresses() {
        let mut other = key();
        other.0[32] ^= 1;
        assert_ne!(address(key()), address(other));
    }
}
