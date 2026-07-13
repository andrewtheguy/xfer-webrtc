//! Rotating short PIN and PIN-root key derivation for secure-send-web's
//! Nostr "Auto Exchange" mode.
//!
//! The PIN is 10 Crockford-base32 characters (9 data + 1 checksum), displayed
//! as `XXXXX-XXXXX`. The sender mints a fresh PIN every [`PIN_ROTATION_MS`]
//! and honors the [`PIN_ACTIVE_GENERATIONS`] most recent ones, so any single
//! PIN is valid for at most [`PIN_TTL_MS`].
//!
//! Every PIN-scoped value is an HKDF derivation off a single PBKDF2-SHA-256
//! stretch of the PIN (the "PIN root"), domain-separated by info label:
//! `hint:<bucket>` (rendezvous event lookup tag), `auth` (claim/confirm
//! sealing key), `rendezvous` (rendezvous payload key), and `fingerprint`
//! (local-only visual check). The PIN derives no content-encryption keys —
//! those come from the ephemeral ECDH exchange the PIN authenticates (see
//! [`crate::crypto::ecdh`]). Mirrors secure-send-web's `src/lib/crypto/pin.ts`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use hkdf::Hkdf;
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;

use super::aes::AES_KEY_LEN;
use super::chunk::fill_random;

/// Total PIN length, including the trailing checksum character.
pub const PIN_LENGTH: usize = 10;
const PIN_CHECKSUM_LENGTH: usize = 1;
/// Display/entry grouping: `XXXXX-XXXXX`.
const PIN_GROUP_LENGTH: usize = 5;

/// Crockford base32 alphabet: digits + uppercase letters, excluding I, L, O
/// (mapped from look-alikes on input: I/L -> 1, O -> 0) and U. Matches
/// secure-send-web's `PIN_CHARSET`.
const PIN_CHARSET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How often the sender mints and publishes a fresh PIN.
pub const PIN_ROTATION_MS: u64 = 120_000;
/// Total time the sender keeps rotating/waiting before giving up. A resource
/// backstop, not a security control: rotation already caps any single PIN's
/// exposure at [`PIN_TTL_MS`], so waiting longer is not less safe. Mirrors
/// secure-send-web's `PIN_WAIT_TIMEOUT_MS`.
pub const PIN_WAIT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// How many recent PIN generations the sender honors when verifying a claim.
pub const PIN_ACTIVE_GENERATIONS: usize = 3;
/// Resulting validity of any single PIN: bounds rendezvous-event freshness on
/// the receiver and is the NIP-40 expiration the sender attaches.
pub const PIN_TTL_MS: u64 = PIN_ROTATION_MS * PIN_ACTIVE_GENERATIONS as u64;
/// How many earlier rotation buckets the receiver derives hints for. An event
/// of age exactly PIN_TTL_MS can sit PIN_ACTIVE_GENERATIONS buckets back, so
/// the look-back must equal PIN_ACTIVE_GENERATIONS to cover the whole
/// non-expired window.
pub const PIN_HINT_LOOKBACK_BUCKETS: u64 = PIN_ACTIVE_GENERATIONS as u64;

const PBKDF2_ITERATIONS: u32 = 600_000;
/// Domain-separation salt for the PBKDF2 PIN-root derivation (public).
const PIN_ROOT_SALT: &str = "secure-send:pin-root:v2";
/// HKDF salt shared by every derivation off the PIN root; each purpose is
/// domain-separated by its HKDF info label.
const PIN_HKDF_SALT: &str = "secure-send:pin:v2";

/// PIN hint length in hex characters (64 bits): the Nostr `#h` filter tag.
const PIN_HINT_LENGTH: usize = 16;
/// PIN fingerprint length in lowercase hex characters (48 bits, local-only).
const PIN_FINGERPRINT_LENGTH: usize = 12;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as u64
}

pub fn now_sec() -> u64 {
    now_ms() / 1000
}

/// Compute the checksum character using a position-weighted sum.
///
/// Weights are the odd numbers 1, 3, 5, ... — every weight is coprime with the
/// charset size (32), so any single-character substitution always changes the
/// checksum. Mirrors secure-send-web's `computeChecksum`.
fn compute_checksum(data: &[u8]) -> u8 {
    let mut sum = 0usize;
    for (i, byte) in data.iter().enumerate() {
        let Some(index) = PIN_CHARSET.iter().position(|c| c == byte) else {
            return PIN_CHARSET[0];
        };
        sum += index * (2 * i + 1);
    }
    PIN_CHARSET[sum % PIN_CHARSET.len()]
}

/// Generate a random PIN: 9 data characters drawn with rejection sampling
/// (no modulo bias) plus the checksum character.
pub fn generate_pin() -> Result<String> {
    let data_len = PIN_LENGTH - PIN_CHECKSUM_LENGTH;
    let charset_len = PIN_CHARSET.len();
    let max_multiple = (256 / charset_len) * charset_len;
    let mut data = Vec::with_capacity(PIN_LENGTH);
    let mut buf = vec![0u8; data_len * 2];

    while data.len() < data_len {
        fill_random(&mut buf)?;
        for byte in &buf {
            let n = *byte as usize;
            if n < max_multiple {
                data.push(PIN_CHARSET[n % charset_len]);
                if data.len() == data_len {
                    break;
                }
            }
        }
    }

    data.push(compute_checksum(&data));
    String::from_utf8(data).map_err(|e| anyhow::anyhow!("generated invalid PIN: {e}"))
}

/// Canonicalize typed PIN characters: uppercase and map the Crockford base32
/// look-alikes (O -> 0, I/L -> 1). Separators (whitespace, dashes) are
/// dropped. Characters outside the PIN charset are preserved so callers can
/// detect and surface invalid input. Mirrors secure-send-web's
/// `normalizePinInput`.
pub fn normalize_pin_input(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .filter_map(normalize_pin_char)
        .collect()
}

/// Canonical form of one typed PIN character, or `None` only for characters
/// that cannot be represented (non-ASCII).
fn normalize_pin_char(c: char) -> Option<char> {
    if !c.is_ascii() {
        return None;
    }
    Some(match c.to_ascii_uppercase() {
        'O' => '0',
        'I' | 'L' => '1',
        upper => upper,
    })
}

/// Canonicalize one typed character for interactive PIN entry: returns the
/// canonical charset character, or `None` for anything that can never be part
/// of a PIN (including separators, which the TUI simply ignores).
pub fn canonical_pin_char(c: char) -> Option<char> {
    let canonical = normalize_pin_char(c)?;
    PIN_CHARSET.contains(&(canonical as u8)).then_some(canonical)
}

/// Format a PIN for display as symmetric groups (`XXXXX-XXXXX`). Also groups
/// partial input, for live entry display.
pub fn format_pin(pin: &str) -> String {
    pin.as_bytes()
        .chunks(PIN_GROUP_LENGTH)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// Validate PIN format and checksum (expects normalized input).
pub fn is_valid_pin(pin: &str) -> bool {
    let bytes = pin.as_bytes();
    if bytes.len() != PIN_LENGTH {
        return false;
    }
    if !bytes.iter().all(|byte| PIN_CHARSET.contains(byte)) {
        return false;
    }

    let data = &bytes[..PIN_LENGTH - PIN_CHECKSUM_LENGTH];
    compute_checksum(data) == bytes[PIN_LENGTH - PIN_CHECKSUM_LENGTH]
}

pub fn generate_transfer_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    fill_random(&mut bytes)?;
    Ok(hex_lower(&bytes))
}

/// The PIN root: the PBKDF2-SHA-256 stretch of the PIN, ready for cheap HKDF
/// expansions. The expensive stretch runs exactly once per PIN; brute-forcing
/// any derived value still costs the full PBKDF2 work factor per PIN guess.
///
/// CPU-bound (~600k PBKDF2 iterations): call [`PinRoot::derive`] from
/// `spawn_blocking` in async contexts.
pub struct PinRoot {
    hkdf: Hkdf<Sha256>,
}

impl PinRoot {
    pub fn derive(pin: &str) -> Self {
        let mut root = [0u8; 32];
        pbkdf2_hmac::<Sha256>(
            pin.as_bytes(),
            PIN_ROOT_SALT.as_bytes(),
            PBKDF2_ITERATIONS,
            &mut root,
        );
        Self {
            hkdf: Hkdf::new(Some(PIN_HKDF_SALT.as_bytes()), &root),
        }
    }

    fn expand(&self, info: &str, out: &mut [u8]) {
        self.hkdf
            .expand(info.as_bytes(), out)
            .expect("HKDF output length is always valid here");
    }

    fn aes_key(&self, info: &str) -> [u8; AES_KEY_LEN] {
        let mut key = [0u8; AES_KEY_LEN];
        self.expand(info, &mut key);
        key
    }

    /// The PIN hint (16 hex chars) for an absolute rotation bucket. Published
    /// as the Nostr `#h` tag so the receiver can locate the rendezvous event
    /// without revealing the PIN; bucket scoping keeps the tag from being a
    /// stable cross-transfer correlator.
    pub fn hint_for_bucket(&self, bucket: u64) -> String {
        let mut bytes = [0u8; PIN_HINT_LENGTH / 2];
        self.expand(&format!("hint:{bucket}"), &mut bytes);
        hex_lower(&bytes)
    }

    /// The PIN hint for the rotation bucket `bucket_offset` buckets before the
    /// current one (0 = current bucket).
    pub fn hint(&self, bucket_offset: u64) -> String {
        self.hint_for_bucket(current_rotation_bucket().saturating_sub(bucket_offset))
    }

    /// The AES-GCM key that seals the claim/confirm handshake payloads. A
    /// payload that decrypts under this key proves the author knows the PIN.
    pub fn auth_key(&self) -> [u8; AES_KEY_LEN] {
        self.aes_key("auth")
    }

    /// The AES-GCM key for the rendezvous event payload (transfer id, sender
    /// ECDH public key, handshake nonce, file metadata).
    pub fn rendezvous_key(&self) -> [u8; AES_KEY_LEN] {
        self.aes_key("rendezvous")
    }

    /// The PIN fingerprint: a stable one-way derivation displayed to both
    /// sides so two humans can visually confirm they entered the same PIN.
    /// Never published to relays, so it carries no rotation-bucket scoping.
    ///
    /// Encoded as 12 lowercase hex chars, displayed as-is (no grouping).
    pub fn fingerprint(&self) -> String {
        let mut bytes = vec![0u8; PIN_FINGERPRINT_LENGTH.div_ceil(2)];
        self.expand("fingerprint", &mut bytes);
        hex_lower(&bytes)[..PIN_FINGERPRINT_LENGTH].to_string()
    }
}

/// The current PIN rotation bucket (`floor(now_ms / PIN_ROTATION_MS)`).
fn current_rotation_bucket() -> u64 {
    now_ms() / PIN_ROTATION_MS
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pin_validates() {
        let pin = generate_pin().unwrap();
        assert_eq!(pin.len(), PIN_LENGTH);
        assert!(is_valid_pin(&pin));
        assert!(pin.bytes().all(|b| PIN_CHARSET.contains(&b)));
    }

    #[test]
    fn checksum_rejects_typo_and_transposition() {
        // Fixed vector: checksum of "ABCDE0123" is 'Y' (weights 1,3,5,...),
        // verified against secure-send-web's computeChecksum.
        assert!(is_valid_pin("ABCDE0123Y"));
        assert!(!is_valid_pin("ABCDF0123Y")); // substitution
        assert!(!is_valid_pin("BACDE0123Y")); // transposition
        assert!(!is_valid_pin("ABCDE0123")); // too short
    }

    #[test]
    fn input_normalization_maps_lookalikes() {
        assert_eq!(normalize_pin_input(" abcde-0123y "), "ABCDE0123Y");
        assert_eq!(normalize_pin_input("oO-iI-lL"), "001111");
        // Invalid characters are preserved so validation can reject them.
        assert_eq!(normalize_pin_input("AB*U"), "AB*U");
    }

    #[test]
    fn canonical_char_accepts_only_charset() {
        assert_eq!(canonical_pin_char('a'), Some('A'));
        assert_eq!(canonical_pin_char('o'), Some('0'));
        assert_eq!(canonical_pin_char('L'), Some('1'));
        assert_eq!(canonical_pin_char('7'), Some('7'));
        assert_eq!(canonical_pin_char('U'), None);
        assert_eq!(canonical_pin_char('-'), None);
        assert_eq!(canonical_pin_char('*'), None);
    }

    #[test]
    fn pin_formats_in_groups_of_five() {
        assert_eq!(format_pin("ABCDE0123Y"), "ABCDE-0123Y");
        assert_eq!(format_pin("ABCDE01"), "ABCDE-01");
    }

    #[test]
    fn pin_root_matches_web_vectors() {
        // Parity with secure-send-web's importPinRoot + HKDF derivations,
        // verified against the Web Crypto API (PBKDF2-SHA-256, 600k
        // iterations, salt "secure-send:pin-root:v2"; HKDF-SHA-256, salt
        // "secure-send:pin:v2").
        let root = PinRoot::derive("ABCDE0123Y");
        assert_eq!(root.hint_for_bucket(14778858), "182f809c22f137e7");
        assert_eq!(
            hex_lower(&root.auth_key()),
            "2e05996ba3836f437372d98955398142ef9e1f2c477fb999ca8e93718c3970a5"
        );
        assert_eq!(
            hex_lower(&root.rendezvous_key()),
            "7f9f1f01b4db42c33120fc470ecc77100ce6015bcfa6aac7cc4abd346769d2f9"
        );
        assert_eq!(root.fingerprint(), "6fb4d8649db3");
    }
}
