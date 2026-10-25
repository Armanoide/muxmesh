use sha2::{Digest, Sha256};
use std::fmt;
use subtle::ConstantTimeEq;

pub struct TokenHash(pub [u8; 32]);

impl TokenHash {
    pub fn of(token: &str) -> Self {
        TokenHash(Sha256::digest(token.as_bytes()).into())
    }

    pub fn matches(&self, token: &str) -> bool {
        *self == Self::of(token)
    }
}

impl PartialEq for TokenHash {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash(<redacted>)") // never the bytes in a log
    }
}

/// Mint a machine token: 32 random bytes, hex.
/// This is the ONLY time the token exists in plaintext — the hub shows it
/// once at enrollment, then keeps only TokenHash::of(token).
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_token_matches_its_own_hash() {
        let token = mint_token();
        let hash = TokenHash::of(&token);
        assert!(hash.matches(&token));
    }

    #[test]
    fn different_token_does_not_match() {
        let hash = TokenHash::of(&mint_token());
        assert!(!hash.matches("0000000000000000000000000000000000000000000000000000000000000000"));
    }

    #[test]
    fn minted_tokens_are_unique_and_64_hex() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn debug_never_leaks_the_hash() {
        let hash = TokenHash::of(&mint_token());
        assert_eq!(format!("{hash:?}"), "TokenHash(<redacted>)");
    }
}
