use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, Rng, RngCore};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `prefix_` + 24 hex chars (96 random bits).
pub fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 12];
    OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}_{hex}")
}

/// `prefix_` + 256 random bits, URL-safe base64.
pub fn random_secret(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Secrets are stored and looked up by SHA-256, never in plain text.
pub fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// No 0/O/1/I so codes survive being read aloud or typed on a phone.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 8 chars from a 32-symbol alphabet = 40 bits. Codes are single use and
/// expire in minutes, so this is far beyond online guessing range.
pub fn pairing_code() -> String {
    let mut rng = OsRng;
    (0..8)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

pub fn format_code(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..])
}

pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip() {
        let code = pairing_code();
        assert_eq!(code.len(), 8);
        assert_eq!(normalize_code(&format_code(&code).to_lowercase()), code);
    }

    #[test]
    fn secrets_are_unique() {
        assert_ne!(random_secret("t"), random_secret("t"));
        assert_eq!(hash_secret("a"), hash_secret("a"));
    }
}
