//! Encryption for provider keys stored through the admin API (BYOK).
//!
//! AES-256-GCM with a random 96-bit nonce per value. Stored form:
//! `v1:<base64 nonce>:<base64 ciphertext>`. The key comes from
//! `admin.encryption_key` and never touches the store.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::{rngs::OsRng, RngCore};

pub struct Cipher(Aes256Gcm);

impl Cipher {
    pub fn from_base64(key: &str) -> Result<Self> {
        let bytes = B64.decode(key.trim()).context("not valid base64")?;
        if bytes.len() != 32 {
            bail!("expected 32 bytes, got {}", bytes.len());
        }
        let cipher = Aes256Gcm::new_from_slice(&bytes).context("invalid key")?;
        Ok(Self(cipher))
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ct = self
            .0
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        Ok(format!("v1:{}:{}", B64.encode(nonce), B64.encode(ct)))
    }

    pub fn decrypt(&self, stored: &str) -> Result<String> {
        let mut parts = stored.splitn(3, ':');
        let (Some("v1"), Some(n), Some(c)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("unrecognised encrypted value");
        };
        let nonce = B64.decode(n).context("bad nonce")?;
        if nonce.len() != 12 {
            bail!("bad nonce length");
        }
        let ct = B64.decode(c).context("bad ciphertext")?;
        let pt = self
            .0
            .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
            .map_err(|_| anyhow::anyhow!("decryption failed (wrong admin.encryption_key?)"))?;
        String::from_utf8(pt).context("decrypted value is not UTF-8")
    }
}

/// A new random key for `admin.encryption_key`.
pub fn generate_encryption_key() -> String {
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    B64.encode(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        let c = Cipher::from_base64(&generate_encryption_key()).unwrap();
        let e = c.encrypt("sk-secret").unwrap();
        assert!(e.starts_with("v1:") && !e.contains("sk-secret"));
        assert_eq!(c.decrypt(&e).unwrap(), "sk-secret");
        assert_ne!(c.encrypt("sk-secret").unwrap(), e, "fresh nonce each time");
        let other = Cipher::from_base64(&generate_encryption_key()).unwrap();
        assert!(other.decrypt(&e).is_err());
        assert!(Cipher::from_base64("c2hvcnQ=").is_err());
    }
}
