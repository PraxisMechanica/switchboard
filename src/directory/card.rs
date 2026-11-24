//! Signed peer cards and deterministic canonical serialization.

use serde::{Deserialize, Serialize};

use crate::identity::{self, CanonicalBytes, PeerId, Signed, Verified};
use crate::primitives::Timestamp;

use super::DirectoryError;

/// Capability extension advertised on a peer's directory card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertisedExtension {
    pub name: String,
    pub description: String,
}

/// Self-signed contact card published by a node to the team phone book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerCard {
    pub handle: String,
    pub peer_id: PeerId,
    pub direct_addrs: Vec<String>,
    pub extensions: Vec<AdvertisedExtension>,
    pub updated_at: Timestamp,
}

impl CanonicalBytes for PeerCard {
    const DOMAIN_TAG: &'static [u8] = b"switchboard.directory.peer_card.v1";

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        write_str(&mut buf, &self.handle);
        buf.extend_from_slice(self.peer_id.as_bytes());

        let mut sorted_addrs: Vec<&str> = self.direct_addrs.iter().map(String::as_str).collect();
        sorted_addrs.sort_unstable();
        write_len(&mut buf, sorted_addrs.len());
        for addr in sorted_addrs {
            write_str(&mut buf, addr);
        }

        let mut sorted_exts: Vec<(&str, &str)> = self
            .extensions
            .iter()
            .map(|ext| (ext.name.as_str(), ext.description.as_str()))
            .collect();
        sorted_exts.sort_unstable();
        write_len(&mut buf, sorted_exts.len());
        for (name, description) in sorted_exts {
            write_str(&mut buf, name);
            write_str(&mut buf, description);
        }

        buf.extend_from_slice(&self.updated_at.as_millis().to_be_bytes());
        buf
    }
}

/// Verifies a signed peer card's signature, self-signing invariant, and handle format.
pub fn verify_peer_card(signed: Signed<PeerCard>) -> Result<Verified<PeerCard>, DirectoryError> {
    let verified = identity::verify(signed)?;

    if verified.signer() != verified.payload().peer_id {
        return Err(DirectoryError::SignerMismatch {
            signer: verified.signer(),
            card_peer_id: verified.payload().peer_id,
        });
    }

    identity::validate_handle(&verified.payload().handle)?;

    Ok(verified)
}

fn write_len(buf: &mut Vec<u8>, len: usize) {
    let len_u64 = u64::try_from(len).unwrap_or(u64::MAX);
    buf.extend_from_slice(&len_u64.to_be_bytes());
}

fn write_str(buf: &mut Vec<u8>, value: &str) {
    write_len(buf, value.len());
    buf.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use crate::identity::{IdentityError, LocalKeypair};

    use super::*;

    fn sample_card(peer_id: PeerId) -> PeerCard {
        PeerCard {
            handle: "alice".to_string(),
            peer_id,
            direct_addrs: vec!["10.0.0.2:4433".to_string(), "10.0.0.1:4433".to_string()],
            extensions: vec![
                AdvertisedExtension {
                    name: "zeta".to_string(),
                    description: "Zeta skill".to_string(),
                },
                AdvertisedExtension {
                    name: "alpha".to_string(),
                    description: "Alpha skill".to_string(),
                },
            ],
            updated_at: Timestamp::from_millis(1_700_000_000_000),
        }
    }

    #[test]
    fn canonical_bytes_are_order_independent_for_addrs_and_extensions() {
        let kp = LocalKeypair::generate();
        let card_a = sample_card(kp.peer_id());
        let mut card_b = card_a.clone();
        card_b.direct_addrs.reverse();
        card_b.extensions.reverse();

        assert_eq!(card_a.canonical_bytes(), card_b.canonical_bytes());

        let mut signed = identity::sign(&kp, card_a);
        signed.payload = card_b;
        let verified = verify_peer_card(signed).expect("reordered collections remain valid");
        assert_eq!(verified.signer(), kp.peer_id());
    }

    #[test]
    fn verify_peer_card_rejects_signer_mismatch() {
        let alice = LocalKeypair::generate();
        let mallory = LocalKeypair::generate();
        let card = sample_card(alice.peer_id());
        let signed = identity::sign(&mallory, card);

        assert!(matches!(
            verify_peer_card(signed),
            Err(DirectoryError::SignerMismatch { .. })
        ));
    }

    #[test]
    fn verify_peer_card_rejects_invalid_handle() {
        let kp = LocalKeypair::generate();
        let mut card = sample_card(kp.peer_id());
        card.handle = "../escape".to_string();
        let signed = identity::sign(&kp, card);

        assert!(matches!(
            verify_peer_card(signed),
            Err(DirectoryError::Identity(IdentityError::InvalidHandle(_)))
        ));
    }

    #[test]
    fn verify_peer_card_rejects_tampered_payload() {
        let kp = LocalKeypair::generate();
        let mut signed = identity::sign(&kp, sample_card(kp.peer_id()));
        signed
            .payload
            .direct_addrs
            .push("192.0.2.1:9999".to_string());

        assert!(matches!(
            verify_peer_card(signed),
            Err(DirectoryError::Identity(IdentityError::InvalidSignature))
        ));
    }
}
