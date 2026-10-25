//! Hex helpers shared by hub and agent — one implementation, two crates.

use crate::error::{ErrorCore, Result};

/// Decode an even-length hex string; odd length is a clean error, never a panic.
pub fn hex_bytes(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(ErrorCore::Hex(hex_len_error()));
    }
    s.as_bytes()
        .chunks_exact(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap_or("00"), 16).map_err(ErrorCore::from))
        .collect()
}

/// Encode bytes as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// ParseIntError has no public constructor; craft one via an empty parse.
fn hex_len_error() -> std::num::ParseIntError {
    String::new()
        .parse::<u8>()
        .expect_err("empty string is never a valid u8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_length_hex_is_error_not_panic() {
        assert!(hex_bytes("abc").is_err());
        assert!(hex_bytes("").is_ok());
        assert_eq!(hex_bytes("00ff").unwrap(), vec![0x00, 0xff]);
    }

    #[test]
    fn round_trip() {
        assert_eq!(hex_bytes(&hex(&[1, 2, 253])).unwrap(), vec![1, 2, 253]);
    }
}
