//! PKCE (RFC 7636, S256) and random `state`/`nonce` values.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};

use crate::secret::PkceVerifier;

/// Returns `bytes` random bytes from a CSPRNG, base64url-encoded without padding.
pub(crate) fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

impl PkceVerifier {
    /// Generates a fresh verifier: 32 random bytes, base64url (43 characters, RFC 7636 §4.1).
    #[must_use]
    pub fn generate() -> Self {
        Self::new(random_urlsafe(32))
    }

    /// The S256 code challenge: `BASE64URL(SHA256(verifier))` (RFC 7636 §4.2).
    #[must_use]
    pub fn challenge_s256(&self) -> String {
        let digest = Sha256::digest(self.secret().as_bytes());
        URL_SAFE_NO_PAD.encode(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b_vector() {
        let v = PkceVerifier::new("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(v.challenge_s256(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_is_valid_and_unique() {
        let a = PkceVerifier::generate();
        let b = PkceVerifier::generate();
        assert_ne!(a.secret(), b.secret());
        // RFC 7636: 43..=128 chars from the unreserved set.
        assert!((43..=128).contains(&a.secret().len()));
        assert!(a.secret().chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }
}
