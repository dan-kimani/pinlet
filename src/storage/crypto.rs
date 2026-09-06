//! Note encryption: XChaCha20-Poly1305 with an argon2id-derived key
//! (spec §3.10). Locked notes live in the git-ignored `locked/`
//! directory and never sync.

use argon2::Argon2;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// File magic identifying the blob format.
const MAGIC: &[u8] = b"PINLET\0";

/// Length of the salt (argon2id input).
const SALT_LEN: usize = 16;

/// Length of the XChaCha20-Poly1305 nonce.
const NONCE_LEN: usize = 24;

/// Encrypt `plaintext` with a key derived from `password`.
/// Returns a base64 blob: magic + salt + nonce + ciphertext.
pub fn encrypt(password: &str, plaintext: &str) -> AppResult<String> {
    let salt: [u8; SALT_LEN] = *Uuid::new_v4().as_bytes();
    let nonce = random_nonce();

    let key = derive_key(password, &salt)?;
    let key = Key::try_from(&key[..]).map_err(|err| AppError::Crypto(err.to_string()))?;
    let nonce = XNonce::try_from(&nonce[..]).map_err(|err| AppError::Crypto(err.to_string()))?;
    let cipher = XChaCha20Poly1305::new(&key);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| AppError::Crypto("encryption failed".to_owned()))?;

    let mut blob = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(MAGIC);
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(BASE64.encode(blob))
}

/// Decrypt a blob produced by [`encrypt`].
pub fn decrypt(password: &str, encoded: &str) -> AppResult<String> {
    let blob = BASE64
        .decode(encoded.trim())
        .map_err(|err| AppError::Crypto(err.to_string()))?;
    let payload_len = MAGIC.len() + SALT_LEN + NONCE_LEN;
    if !blob.starts_with(MAGIC) || blob.len() < payload_len {
        return Err(AppError::Crypto("not a Pinlet encrypted blob".to_owned()));
    }

    let salt = &blob[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce = XNonce::try_from(&blob[MAGIC.len() + SALT_LEN..payload_len])
        .map_err(|err| AppError::Crypto(err.to_string()))?;
    let ciphertext = &blob[payload_len..];

    let key = derive_key(password, salt)?;
    let key = Key::try_from(&key[..]).map_err(|err| AppError::Crypto(err.to_string()))?;
    let cipher = XChaCha20Poly1305::new(&key);
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| AppError::Crypto("wrong password".to_owned()))?;
    String::from_utf8(plaintext).map_err(|err| AppError::Crypto(err.to_string()))
}

/// Derive the 32-byte key from the password and salt (argon2id).
fn derive_key(password: &str, salt: &[u8]) -> AppResult<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|err| AppError::Crypto(err.to_string()))?;
    Ok(key)
}

/// 24 random bytes from the OS RNG (via two UUIDs).
fn random_nonce() -> [u8; NONCE_LEN] {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..16].copy_from_slice(first.as_bytes());
    nonce[16..].copy_from_slice(&second.as_bytes()[..8]);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_plaintext() {
        let blob = encrypt("hunter2", "secret title\nsecret body\n").unwrap();
        let plaintext = decrypt("hunter2", &blob).unwrap();
        assert_eq!(plaintext, "secret title\nsecret body\n");
    }

    #[test]
    fn wrong_password_fails() {
        let blob = encrypt("correct", "data").unwrap();
        assert!(decrypt("wrong", &blob).is_err());
    }

    #[test]
    fn garbage_input_fails() {
        assert!(decrypt("x", "not base64 at all!!").is_err());
    }
}
