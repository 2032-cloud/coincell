//! The one hash the engine cares about: the backend's `content_hash`.
//!
//! 2032 computes it as `crypto.subtle.digest("SHA-256", bytes)` rendered as
//! lowercase hex (`src/worker/crypto.ts`). Match that byte for byte or the
//! `(game_instance_id, content_hash)` dedupe silently stops working.

use sha2::{Digest, Sha256};

/// Lowercase-hex SHA-256 of `bytes`, 64 chars, no prefix.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 0xf) as u32, 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn is_lowercase_hex_of_len_64() {
        let h = sha256_hex(b"some raw save bytes");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }
}
