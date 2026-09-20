//! What Couch keeps for a paired bridge, and nothing else.
//!
//! A Hue bridge is addressed by a settings field the owner typed (`host`), and
//! opened with two things the owner never sees: the application key the bridge
//! issued when the link button was pressed, and the exact certificate that
//! bridge presented while it was being paired. Both live in the
//! [`Credential`](couch_sdk::Credential) Couch stores privately for the
//! connection and hands back on the next `connect_with`. They are never in the
//! settings, never in the environment, never on disk anywhere this package can
//! read, and never in the text of an error.
//!
//! The certificate travels as base64 because a credential is JSON and DER is
//! not text. The encoder is here rather than in a dependency: it is sixteen
//! lines, and a package that cross-compiles to a static ARM binary pays for
//! every crate it adds.

use couch_sdk::Credential;
use serde_json::{json, Value};

use crate::{Error, Result};

/// The key and the pinned certificate for one bridge.
pub struct HueCredential {
    /// The `hue-application-key` header value the bridge issued.
    pub application_key: String,
    /// The bridge id, which is also the common name on its certificate. Shown
    /// (last six characters) in the summary a pairing produces.
    pub bridge_id: String,
    /// The exact certificate seen while pairing, in DER.
    pub certificate: Vec<u8>,
}

impl std::fmt::Debug for HueCredential {
    /// Never the key, never the certificate. Anything that formats one of
    /// these is a log line or a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HueCredential").finish_non_exhaustive()
    }
}

impl HueCredential {
    pub fn new(
        application_key: impl Into<String>,
        bridge_id: impl Into<String>,
        certificate: Vec<u8>,
    ) -> Self {
        Self {
            application_key: application_key.into(),
            bridge_id: bridge_id.into(),
            certificate,
        }
    }

    /// Read one back. [`Error::Configuration`] if it is not this package's own
    /// shape or cannot address a bridge; the error never repeats what it read.
    pub fn parse(credential: &Credential) -> Result<Self> {
        let map = credential.get();
        let text = |key: &str| map.get(key).and_then(Value::as_str).unwrap_or_default();
        let application_key = text("application_key").to_string();
        let bridge_id = text("bridge_id").to_string();
        let certificate = decode(text("certificate")).ok_or(Error::Configuration)?;
        if application_key.is_empty()
            || application_key.len() > 128
            || !application_key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || bridge_id.is_empty()
            || bridge_id.len() > 64
            || !bridge_id.bytes().all(|b| b.is_ascii_alphanumeric())
            || certificate.is_empty()
        {
            return Err(Error::Configuration);
        }
        Ok(Self {
            application_key,
            bridge_id,
            certificate,
        })
    }

    /// What the pairing flow hands Couch to keep, and what a test builds from
    /// a bridge whose key and certificate it already knows.
    pub fn to_credential(&self) -> Credential {
        Credential::new(json!({
            "application_key": self.application_key,
            "bridge_id": self.bridge_id,
            "certificate": encode(&self.certificate),
        }))
        .expect("a Hue credential is a small JSON object")
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(ALPHABET[(packed >> shift) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The inverse. `None` for anything that is not standard base64, including an
/// empty string: an empty certificate is not a pin.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || text.len() % 4 != 0 {
        return None;
    }
    let value = |b: u8| ALPHABET.iter().position(|a| *a == b).map(|v| v as u32);
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let bytes = text.as_bytes();
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let padding = if last {
            chunk.iter().filter(|b| **b == b'=').count()
        } else {
            0
        };
        if padding > 2 || (padding > 0 && chunk[4 - padding..].iter().any(|b| *b != b'=')) {
            return None;
        }
        let mut packed = 0u32;
        for (position, byte) in chunk.iter().enumerate() {
            packed |= if position < 4 - padding {
                value(*byte)?
            } else {
                0
            } << (18 - 6 * position);
        }
        for shift in [16, 8, 0].into_iter().take(3 - padding) {
            out.push((packed >> shift) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_every_length_and_refuses_what_is_not_base64() {
        for length in 0..64usize {
            let bytes: Vec<u8> = (0..length).map(|n| (n * 7 + 13) as u8).collect();
            let text = encode(&bytes);
            assert_eq!(text.len() % 4, 0, "{length}");
            if length > 0 {
                assert_eq!(decode(&text).as_deref(), Some(bytes.as_slice()), "{length}");
            }
        }
        assert_eq!(encode(b"Hue"), "SHVl");
        assert_eq!(encode(b"Hu"), "SHU=");
        assert_eq!(encode(b"H"), "SA==");
        assert_eq!(decode("SHVl").unwrap(), b"Hue");
        for broken in ["", "A", "AB", "ABC", "A===", "A=AA", "****", "SHV!"] {
            assert!(decode(broken).is_none(), "{broken:?}");
        }
    }

    #[test]
    fn a_credential_round_trips_and_a_broken_one_says_nothing_about_itself() {
        let credential = HueCredential::new("abc-123", "001788FFFE0A1B2C", vec![1, 2, 3, 4]);
        let stored = credential.to_credential();
        let read = HueCredential::parse(&stored).unwrap();
        assert_eq!(read.application_key, "abc-123");
        assert_eq!(read.bridge_id, "001788FFFE0A1B2C");
        assert_eq!(read.certificate, vec![1, 2, 3, 4]);
        assert_eq!(format!("{read:?}"), "HueCredential { .. }");
        assert_eq!(format!("{stored:?}"), "Credential(..)");
        for broken in [
            json!({}),
            json!({"application_key": "", "bridge_id": "a", "certificate": "AQ=="}),
            json!({"application_key": "a b", "bridge_id": "a", "certificate": "AQ=="}),
            json!({"application_key": "a", "bridge_id": "", "certificate": "AQ=="}),
            json!({"application_key": "a", "bridge_id": "a", "certificate": ""}),
            json!({"application_key": "a", "bridge_id": "a", "certificate": "not base64"}),
            json!({"application_key": "a", "bridge_id": "a"}),
        ] {
            let credential = Credential::new(broken.clone()).unwrap();
            assert_eq!(
                HueCredential::parse(&credential).unwrap_err(),
                Error::Configuration,
                "{broken}"
            );
        }
    }
}
