//! Application-level encryption at rest (ADR 0005): XChaCha20-Poly1305 with random nonces.

use std::fmt;
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;

use crate::error::{Error, ErrorKind, Result};

/// Length of a [`StoreKey`] in bytes.
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const VERSION: u8 = 1;
const HEADER_LEN: usize = 1 + NONCE_LEN;

/// A 256-bit per-account encryption key.
///
/// `Debug` never prints the key bytes. The bytes are overwritten when the key is dropped.
#[derive(Clone)]
pub struct StoreKey([u8; KEY_LEN]);

impl StoreKey {
    /// Wraps raw key bytes (for example read from the OS keychain).
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Generates a fresh random key from the OS-seeded CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Returns the key bytes, for storing them in the keychain.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Drop for StoreKey {
    fn drop(&mut self) {
        // Best effort; the compiler may elide this, but it costs nothing.
        self.0.fill(0);
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey(redacted)")
    }
}

/// Supplies the per-account store key (M-MOCKABLE-SYSCALLS).
///
/// The app implements this with the OS keychain: load the key for the account, or create,
/// persist and return a new one on first use. Tests use `FixedKeySource` (feature `test-util`).
/// Called once from [`Store::open`](crate::Store::open) on a blocking thread.
pub trait KeySource: Send + Sync + 'static {
    /// Returns the account's key, creating and persisting one if none exists yet.
    ///
    /// # Errors
    ///
    /// Any error (keychain locked, access denied) aborts opening the store with
    /// [`ErrorKind::Crypto`]. The error must not contain secrets.
    fn load_or_create(&self) -> std::result::Result<StoreKey, Box<dyn std::error::Error + Send + Sync + 'static>>;
}

/// A [`KeySource`] holding a fixed key in memory, for tests.
#[cfg(any(test, feature = "test-util"))]
#[derive(Debug, Clone)]
pub struct FixedKeySource(StoreKey);

#[cfg(any(test, feature = "test-util"))]
impl FixedKeySource {
    /// A source that always returns `key`.
    #[must_use]
    pub fn new(key: StoreKey) -> Self {
        Self(key)
    }

    /// A source with a fixed, well-known test key (all bytes `0x42`).
    #[must_use]
    pub fn test_key() -> Self {
        Self(StoreKey::from_bytes([0x42; KEY_LEN]))
    }
}

#[cfg(any(test, feature = "test-util"))]
impl KeySource for FixedKeySource {
    fn load_or_create(&self) -> std::result::Result<StoreKey, Box<dyn std::error::Error + Send + Sync + 'static>> {
        Ok(self.0.clone())
    }
}

/// Encrypts and decrypts values with the account key. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Cipher {
    aead: Arc<XChaCha20Poly1305>,
}

impl fmt::Debug for Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cipher(redacted)")
    }
}

impl Cipher {
    pub(crate) fn new(key: &StoreKey) -> Self {
        Self { aead: Arc::new(XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_bytes()))) }
    }

    /// Encrypts `plain`, binding it to `aad` (e.g. `"message_body:<id>"`) so ciphertexts cannot
    /// be swapped between rows. Output: version byte, 24-byte nonce, ciphertext and tag.
    pub(crate) fn seal(&self, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let ct = self
            .aead
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
            .map_err(|_| Error::new(ErrorKind::Crypto, "encryption failed"))?;
        let mut out = Vec::with_capacity(HEADER_LEN.saturating_add(ct.len()));
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypts a value produced by [`Cipher::seal`] with the same `aad`.
    pub(crate) fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        let (Some(&version), Some(nonce), Some(ct)) =
            (sealed.first(), sealed.get(1..HEADER_LEN), sealed.get(HEADER_LEN..))
        else {
            return Err(Error::new(ErrorKind::Corrupt, "ciphertext too short"));
        };
        if version != VERSION {
            return Err(Error::new(ErrorKind::Corrupt, "unknown ciphertext version"));
        }
        self.aead
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| Error::new(ErrorKind::Crypto, "decryption failed (wrong key or tampered data)"))
    }

    /// Decrypts to a UTF-8 string.
    pub(crate) fn open_string(&self, aad: &[u8], sealed: &[u8]) -> Result<String> {
        String::from_utf8(self.open(aad, sealed)?).map_err(|_| Error::corrupt("decrypted text is not utf-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_aad_binding() {
        let c = Cipher::new(&StoreKey::generate());
        let sealed = c.seal(b"a", b"hello").unwrap();
        assert_eq!(c.open(b"a", &sealed).unwrap(), b"hello");
        assert_eq!(c.open(b"b", &sealed).unwrap_err().kind(), ErrorKind::Crypto);
        assert_eq!(c.open(b"a", &sealed[..5]).unwrap_err().kind(), ErrorKind::Corrupt);
        let other = Cipher::new(&StoreKey::generate());
        assert_eq!(other.open(b"a", &sealed).unwrap_err().kind(), ErrorKind::Crypto);
    }

    #[test]
    fn key_debug_is_redacted() {
        let k = StoreKey::from_bytes([7; KEY_LEN]);
        assert_eq!(format!("{k:?}"), "StoreKey(redacted)");
    }
}
