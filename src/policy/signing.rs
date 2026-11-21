//! Optional HMAC-SHA256 request signing and constant-time verification for `GET /load`.

use std::fmt;
use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use url::form_urlencoded;

use crate::policy::error::PolicyError;

type HmacSha256 = Hmac<Sha256>;

/// Signs and verifies canonicalised `GET /load` query strings using HMAC-SHA256.
#[derive(Clone)]
pub struct RequestSigner {
    secret: Vec<u8>,
}

impl fmt::Debug for RequestSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestSigner")
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl RequestSigner {
    /// Creates a new signer from a non-empty secret key.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::Validation`] if `secret` is empty.
    pub fn new(secret: impl Into<Vec<u8>>) -> Result<Self, PolicyError> {
        let secret = secret.into();
        if secret.is_empty() {
            return Err(PolicyError::Validation(
                "server.signing_secret must not be empty".to_owned(),
            ));
        }
        Ok(Self { secret })
    }

    /// Computes a 64-character lowercase hexadecimal HMAC-SHA256 signature over
    /// the canonicalised query string (excluding `sig`).
    #[must_use]
    pub fn sign_raw_query(&self, raw_query: &str) -> String {
        let canonical = canonicalise_query_for_signing(raw_query);
        self.sign_canonical(&canonical)
    }

    /// Signs a full URL, `/load?...` path, or bare query string, stripping any
    /// existing `sig` parameter and appending the freshly computed `sig=<hex>`.
    #[must_use]
    pub fn sign_load_url(&self, input: &str) -> String {
        let trimmed = input.trim();
        let (prefix, raw_query) = match trimmed.split_once('?') {
            Some((head, query)) => (Some(head), query),
            None => (None, trimmed),
        };

        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
            if key != "sig" {
                serializer.append_pair(&key, &value);
            }
        }
        let sig = self.sign_raw_query(raw_query);
        serializer.append_pair("sig", &sig);
        let signed_query = serializer.finish();

        match prefix {
            Some(head) => format!("{head}?{signed_query}"),
            None => signed_query,
        }
    }

    /// Computes a 64-character lowercase hexadecimal HMAC-SHA256 signature over
    /// an already canonicalised query string.
    #[must_use]
    pub fn sign_canonical(&self, canonical_query: &str) -> String {
        let mut mac = self.new_mac();
        mac.update(canonical_query.as_bytes());
        encode_hex(&mac.finalize().into_bytes())
    }

    /// Extracts `sig` from `raw_query` and verifies it in constant time against
    /// the canonicalised remainder of the query string.
    ///
    /// Returns `false` if `sig` is missing, duplicated, malformed hex, or mismatched.
    #[must_use]
    pub fn verify_raw_query(&self, raw_query: &str) -> bool {
        let mut supplied_sig: Option<String> = None;

        for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
            if key == "sig" {
                if supplied_sig.is_some() {
                    // Reject ambiguous duplicate `sig` parameters.
                    return false;
                }
                supplied_sig = Some(value.into_owned());
            }
        }

        let Some(sig_hex) = supplied_sig else {
            return false;
        };

        let Some(sig_bytes) = decode_hex_32(&sig_hex) else {
            return false;
        };

        let canonical = canonicalise_query_for_signing(raw_query);
        let mut mac = self.new_mac();
        mac.update(canonical.as_bytes());
        mac.verify_slice(&sig_bytes).is_ok()
    }

    fn new_mac(&self) -> HmacSha256 {
        // HMAC-SHA256 accepts keys of any length; `new_from_slice` cannot fail for `Hmac<Sha256>`.
        #[allow(clippy::expect_used)]
        HmacSha256::new_from_slice(&self.secret).expect("HMAC-SHA256 accepts keys of any length")
    }
}

/// Canonicalises a raw URL query string for signing by stripping `sig`, sorting
/// `(key, value)` pairs lexicographically, and re-encoding via standard
/// `application/x-www-form-urlencoded`.
#[must_use]
pub fn canonicalise_query_for_signing(raw_query: &str) -> String {
    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(raw_query.as_bytes())
        .filter(|(key, _)| key != "sig")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    pairs.sort();

    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in &pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    let raw = hex.as_bytes();
    if raw.len() != 64 {
        return None;
    }

    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(raw.chunks_exact(2)) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_secret_and_redacts_debug_output() {
        assert!(matches!(
            RequestSigner::new(Vec::new()),
            Err(PolicyError::Validation(_))
        ));

        let signer = RequestSigner::new(b"top-secret-key".to_vec()).unwrap();
        let debug_str = format!("{signer:?}");
        assert!(!debug_str.contains("top-secret-key"));
        assert!(debug_str.contains("[REDACTED]"));
    }

    #[test]
    fn signs_and_verifies_regardless_of_query_parameter_order() {
        let signer = RequestSigner::new(b"murklet-signing-secret".to_vec()).unwrap();

        let query_a = "url=products%2Fhero.png&w=800&q=85&f=webp";
        let sig = signer.sign_raw_query(query_a);
        assert_eq!(sig.len(), 64);

        let signed_a = format!("{query_a}&sig={sig}");
        let signed_reordered = format!("sig={sig}&f=webp&q=85&w=800&url=products%2Fhero.png");

        assert!(signer.verify_raw_query(&signed_a));
        assert!(signer.verify_raw_query(&signed_reordered));
    }

    #[test]
    fn rejects_tampered_missing_duplicate_or_malformed_signatures() {
        let signer = RequestSigner::new(b"murklet-signing-secret".to_vec()).unwrap();

        let base = "url=hero.png&w=800";
        let sig = signer.sign_raw_query(base);

        // Tampered width fails.
        assert!(!signer.verify_raw_query(&format!("url=hero.png&w=8000&sig={sig}")));
        // Added parameter fails.
        assert!(!signer.verify_raw_query(&format!("url=hero.png&w=800&f=png&sig={sig}")));
        // Missing signature fails.
        assert!(!signer.verify_raw_query(base));
        // Duplicate sig parameter fails.
        assert!(!signer.verify_raw_query(&format!("{base}&sig={sig}&sig={sig}")));
        // Malformed hex fails.
        assert!(!signer.verify_raw_query(&format!("{base}&sig=not-valid-hex")));
    }

    #[test]
    fn sign_load_url_preserves_prefix_and_replaces_existing_signature() {
        let signer = RequestSigner::new(b"cli-secret".to_vec()).unwrap();

        let signed_path = signer.sign_load_url("/load?url=sample.png&w=400&sig=olddeadbeef");
        assert!(signed_path.starts_with("/load?"));
        let query = signed_path.split_once('?').unwrap().1;
        assert!(signer.verify_raw_query(query));
        assert!(!query.contains("olddeadbeef"));

        let signed_bare = signer.sign_load_url("url=sample.png&w=400");
        assert!(!signed_bare.contains('?'));
        assert!(signer.verify_raw_query(&signed_bare));
    }
}
