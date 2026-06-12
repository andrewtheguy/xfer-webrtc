//! Shared-secret authenticated encryption for online (Nostr) signaling payloads.
//!
//! The xfer code is a bearer capability shared out-of-band; it carries a random
//! pre-shared key (PSK). Both peers derive a per-session AEAD key from that PSK
//! and use it to seal the SDP/ICE signaling payloads they publish to relays.
//!
//! This provides two properties over the relay:
//! - **Confidentiality**: relays (and anyone who copies the code) never see the
//!   SDP or ICE candidates, so local network candidate addresses are not exposed.
//! - **Authentication (both directions)**: only a party holding the PSK can
//!   produce a payload that decrypts, so a third party who learns the transfer
//!   ID and pubkeys from relay traffic cannot forge a valid offer or answer.

use anyhow::{Result, anyhow};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

/// Length of the pre-shared key embedded in the xfer code (128-bit).
pub const PSK_LEN: usize = 16;

/// XChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 24;

/// HKDF context string binding derived keys to this protocol/version.
const HKDF_INFO: &[u8] = b"xfer-webrtc/v6/signaling";

/// Generate a fresh random pre-shared key for a new transfer.
pub fn generate_psk() -> [u8; PSK_LEN] {
    let mut psk = [0u8; PSK_LEN];
    rand::rng().fill_bytes(&mut psk);
    psk
}

/// Build the AEAD associated data binding a payload to its transfer and message type.
fn aad(transfer_id: &str, event_type: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(transfer_id.len() + 1 + event_type.len());
    v.extend_from_slice(transfer_id.as_bytes());
    v.push(b':');
    v.extend_from_slice(event_type.as_bytes());
    v
}

/// Derive the per-session AEAD key from the PSK, bound to the transfer id.
fn derive_cipher(psk: &[u8; PSK_LEN], transfer_id: &str) -> XChaCha20Poly1305 {
    let hk = Hkdf::<Sha256>::new(Some(transfer_id.as_bytes()), psk);
    let mut key = [0u8; 32];
    hk.expand(HKDF_INFO, &mut key)
        .expect("32 is a valid output length for HKDF-SHA256");
    XChaCha20Poly1305::new((&key).into())
}

/// Seal a signaling payload. Output layout: `nonce ‖ ciphertext‖tag`.
pub fn seal(
    psk: &[u8; PSK_LEN],
    transfer_id: &str,
    event_type: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = derive_cipher(psk, transfer_id);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: &aad(transfer_id, event_type),
            },
        )
        .map_err(|_| anyhow!("Failed to encrypt signaling payload"))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Open a signaling payload produced by [`seal`].
///
/// Returns `None` on any failure (truncated input, wrong key, tampered
/// ciphertext, or mismatched associated data) so callers can simply drop
/// unauthenticated events.
pub fn open(
    psk: &[u8; PSK_LEN],
    transfer_id: &str,
    event_type: &str,
    data: &[u8],
) -> Option<Vec<u8>> {
    if data.len() < NONCE_LEN {
        return None;
    }
    let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
    let cipher = derive_cipher(psk, transfer_id);
    let nonce = XNonce::from_slice(nonce_bytes);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: &aad(transfer_id, event_type),
            },
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let psk = generate_psk();
        let tid = "082962d8cde95b80a2813e002d79cc1d";
        let plaintext = b"some sdp payload";
        let sealed = seal(&psk, tid, "webrtc-offer", plaintext).unwrap();
        let opened = open(&psk, tid, "webrtc-offer", &sealed).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn wrong_psk_fails() {
        let tid = "082962d8cde95b80a2813e002d79cc1d";
        let sealed = seal(&generate_psk(), tid, "webrtc-offer", b"x").unwrap();
        assert!(open(&generate_psk(), tid, "webrtc-offer", &sealed).is_none());
    }

    #[test]
    fn wrong_transfer_id_fails() {
        let psk = generate_psk();
        let sealed = seal(&psk, "transfer-a", "webrtc-offer", b"x").unwrap();
        assert!(open(&psk, "transfer-b", "webrtc-offer", &sealed).is_none());
    }

    #[test]
    fn wrong_event_type_fails() {
        let psk = generate_psk();
        let tid = "082962d8cde95b80a2813e002d79cc1d";
        let sealed = seal(&psk, tid, "webrtc-offer", b"x").unwrap();
        // An answer-typed open must reject an offer-sealed payload (AAD binding).
        assert!(open(&psk, tid, "webrtc-answer", &sealed).is_none());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let psk = generate_psk();
        let tid = "082962d8cde95b80a2813e002d79cc1d";
        let mut sealed = seal(&psk, tid, "webrtc-offer", b"hello").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(open(&psk, tid, "webrtc-offer", &sealed).is_none());
    }

    #[test]
    fn truncated_input_fails() {
        let psk = generate_psk();
        assert!(open(&psk, "tid", "webrtc-offer", &[0u8; 4]).is_none());
    }
}
