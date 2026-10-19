//! PKCE (RFC 7636) helpers + random state, mirroring codex-rs pkce.rs.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a fresh verifier/challenge pair (S256).
pub fn generate() -> Pkce {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = s256(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

pub fn s256(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Random URL-safe state token (CSRF protection).
pub fn random_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_roundtrip() {
        let pk = generate();
        assert_eq!(pk.verifier.len(), 86);
        assert_eq!(pk.challenge.len(), 43);
        assert_eq!(s256(&pk.verifier), pk.challenge);
    }

    #[test]
    fn pkce_unique() {
        let a = generate();
        let b = generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn state_unique_and_urlsafe() {
        let s = random_state();
        assert_eq!(s.len(), 43);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(s, random_state());
    }
}
