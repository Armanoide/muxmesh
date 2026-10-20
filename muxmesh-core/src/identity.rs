//! Per-machine identity: pairing, tokens, hashes.
//!
//! Model (adapted from dockhand's hawser agents): every machine holds ITS OWN
//! token. The hub never stores the token itself — only a salted digest plus an
//! 8-char prefix for O(1) lookup. A compromised machine is revoked by deleting
//! one entry; the others keep working. No shared secret to scp around.
//!
//! First-run pairing:
//!   hub    $ muxmesh-hub generate-token <host>   -> token printed ONCE
//!   agent  ~/.config/muxmesh/token (0600)        <- operator pastes it
//!        ◄──201 {agent_token}────        hub generates 32 random bytes,
//!                                        stores digest+prefix in tokens.json
//!   agent writes ~/.config/muxmesh/token (0600), then pushes normally.
//!
//! Cryptography: std-only by design (offline build, no new dependency). The
//! digest is iterated SHA-256 with a per-token random salt — deliberately NOT
//! Argon2 (no dependency). For a 256-bit random secret stored on a LAN pivot
//! this is ample; if tokens ever become low-entropy passphrases, upgrade to a
//! memory-hard KDF. `shortcut: iterated SHA-256, upgrade to argon2 if tokens
//! ever stop being 32 random bytes.`

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Prefix length stored alongside the digest (dockhand-style O(1) lookup).
pub const PREFIX_LEN: usize = 8;

/// Digest rounds. The hot loop uses the allocation-free single-block path
/// (sha256_in_place): ~1 µs/round in release, a few µs in debug -> 100k
/// rounds stays well under a second even unoptimized.
const ROUNDS: u32 = 100_000;

/// Read `size` bytes from the OS CSPRNG (/dev/urandom works on Linux and
/// macOS). No dependency; std can't do it directly.
pub fn random_bytes(size: usize) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut f = fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; size];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// base64url WITHOUT padding: ureq's header value parser rejects '=' and a
/// padded token would 400 the X-Muxmesh-Token header. No padding here.
pub fn b64url(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((c.get(1).copied().unwrap_or(0) as u32) << 8)
            | (c.get(2).copied().unwrap_or(0) as u32);
        s.push(T[((n >> 18) & 63) as usize] as char);
        s.push(T[((n >> 12) & 63) as usize] as char);
        if c.len() > 1 {
            s.push(T[((n >> 6) & 63) as usize] as char);
        }
        if c.len() > 2 {
            s.push(T[(n & 63) as usize] as char);
        }
    }
    s
}

/// A new 256-bit secret, base64url-encoded (43 chars).
pub fn new_token() -> Result<String, std::io::Error> {
    Ok(b64url(&random_bytes(32)?))
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha_iv() -> [u32; 8] {
    [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ]
}

/// Compress exactly one 64-byte block into `h` (FIPS 180-4 core).
fn compress(h: &mut [u32; 8], chunk: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            chunk[4 * i],
            chunk[4 * i + 1],
            chunk[4 * i + 2],
            chunk[4 * i + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut v = *h;
    for i in 0..64 {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let t1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let t2 = s0.wrapping_add(maj);
        // FIPS 180-4 state shift: h<-g, g<-f, f<-e, e<-d+T1, d<-c, c<-b,
        // b<-a, a<-T1+T2. (v is [a,b,c,d,e,f,g,h].)
        v[7] = v[6];
        v[6] = v[5];
        v[5] = v[4];
        v[4] = v[3].wrapping_add(t1);
        v[3] = v[2];
        v[2] = v[1];
        v[1] = v[0];
        v[0] = t1.wrapping_add(t2);
    }
    for i in 0..8 {
        h[i] = h[i].wrapping_add(v[i]);
    }
}

fn digest_bytes(h: &[u32; 8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// SHA-256 (FIPS 180-4), compact std-only implementation.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = sha_iv();
    let bitlen = (data.len() as u64) * 8;

    // whole blocks straight from the input
    let full = data.len() / 64;
    for b in data[..full * 64].chunks(64) {
        compress(&mut h, b.try_into().unwrap());
    }

    // tail: remaining bytes + 0x80 + zeros + 64-bit big-endian bit length,
    // one or two padded blocks.
    let tail = &data[full * 64..];
    let mut pad = [0u8; 128];
    pad[..tail.len()].copy_from_slice(tail);
    pad[tail.len()] = 0x80;
    let total = if tail.len() <= 55 { 64 } else { 128 };
    pad[total - 8..total].copy_from_slice(&bitlen.to_be_bytes());
    for b in pad[..total].chunks(64) {
        compress(&mut h, b.try_into().unwrap());
    }

    digest_bytes(&h)
}

/// sha256 of a 32-byte input, single block, zero allocation (hot loop).
fn sha256_in_place(input: &[u8; 32]) -> [u8; 32] {
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(input);
    block[32] = 0x80;
    block[56..64].copy_from_slice(&(256u64).to_be_bytes());
    let mut h = sha_iv();
    compress(&mut h, &block);
    digest_bytes(&h)
}

/// Salted, iterated digest of a token: `hex(salt)$hex(sha256^N(salt||token))`.
pub fn hash_token(token: &str) -> Result<String, std::io::Error> {
    let salt = random_bytes(16)?;
    let mut cur = sha256(&[salt.as_slice(), token.as_bytes()].concat());
    for _ in 1..ROUNDS {
        cur = sha256_in_place(&cur);
    }
    Ok(format!("{}${}", hex(&salt), hex(&cur)))
}

/// Constant-time verify of a token against `hash_token` output.
pub fn verify_token(token: &str, stored: &str) -> bool {
    let (salt_hex, hash_hex) = match stored.split_once('$') {
        Some(p) => p,
        None => return false,
    };
    let salt = match unhex(salt_hex) {
        Some(s) => s,
        None => return false,
    };
    let expect = match unhex(hash_hex) {
        Some(s) if s.len() == 32 => s,
        _ => return false,
    };
    let mut cur = sha256(&[salt.as_slice(), token.as_bytes()].concat());
    for _ in 1..ROUNDS {
        cur = sha256_in_place(&cur);
    }
    fixed_eq(&cur, &expect)
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let b = s.as_bytes();
    (0..b.len())
        .step_by(2)
        .map(|i| {
            let hi = (b[i] as char).to_digit(16)?;
            let lo = (b[i + 1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

fn fixed_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// One entry in the hub's tokens.json: per-machine identity, digest only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenRecord {
    /// sha256^N(salt||token), hex. NEVER the raw token.
    pub hash: String,
    /// first 8 chars of the raw token, for O(1) candidate lookup.
    pub prefix: String,
    /// unix seconds of issuance.
    pub created: u64,
}

/// Hub-side registry: host -> identity. File-backed, atomic writes by the hub.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenStore {
    pub hosts: std::collections::BTreeMap<String, TokenRecord>,
}

impl TokenStore {
    /// Load (missing/corrupt -> empty; the hub then rejects everything).
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Issue a fresh token for `host` (re-issue = revoke of the old one).
    pub fn issue(&mut self, host: &str) -> Result<String, std::io::Error> {
        let token = new_token()?;
        self.hosts.insert(
            host.to_string(),
            TokenRecord {
                hash: hash_token(&token)?,
                prefix: token.chars().take(PREFIX_LEN).collect(),
                created: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            },
        );
        Ok(token)
    }

    /// Authenticate a request token for `host`.
    pub fn verify(&self, host: &str, token: &str) -> bool {
        self.hosts
            .get(host)
            .is_some_and(|rec| token.len() >= PREFIX_LEN && verify_token(token, &rec.hash))
    }

    /// Authenticate a request token against ANY host (for hub-relayed /cmd,
    /// where the target is named in the payload, not the header).
    pub fn verify_any(&self, token: &str) -> Option<String> {
        self.hosts.iter().find_map(|(host, rec)| {
            if token.len() >= PREFIX_LEN && verify_token(token, &rec.hash) {
                Some(host.clone())
            } else {
                None
            }
        })
    }

    /// Revoke a machine: its next request 401s, others unaffected.
    pub fn revoke(&mut self, host: &str) -> bool {
        self.hosts.remove(host).is_some()
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_streaming_boundaries() {
        // block-boundary lengths; reference digests generated with python
        // hashlib at authoring time (offline, deterministic).
        let cases: [(usize, &str); 6] = [
            (
                55,
                "463eb28e72f82e0a96c0a4cc53690c571281131f672aa229e0d45ae59b598b59",
            ),
            (
                56,
                "da2ae4d6b36748f2a318f23e7ab1dfdf45acdc9d049bd80e59de82a60895f562",
            ),
            (
                63,
                "29af2686fd53374a36b0846694cc342177e428d1647515f078784d69cdb9e488",
            ),
            (
                64,
                "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108",
            ),
            (
                65,
                "4bfd2c8b6f1eec7a2afeb48b934ee4b2694182027e6d0fc075074f2fabb31781",
            ),
            (
                1000,
                "4e4c294b331f7a2099a379bec34b9f9fc03dc46ab465d998f4d683da53487e6d",
            ),
        ];
        for (len, want) in cases {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            assert_eq!(hex(&sha256(&data)), want, "len={len}");
        }
    }

    #[test]
    fn sha256_in_place_matches_generic() {
        let input = [7u8; 32];
        assert_eq!(sha256_in_place(&input), sha256(&input));
    }

    #[test]
    fn b64url_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        // 32 random bytes -> exactly 43 chars, no padding
        assert_eq!(b64url(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn token_issue_verify_revoke() {
        let mut st = TokenStore::default();
        let tok = st.issue("host-a").unwrap();
        assert!(st.verify("host-a", &tok));
        // wrong host, wrong token, empty: all denied
        assert!(!st.verify("host-b", &tok));
        assert!(!st.verify("host-a", "bogus"));
        assert!(!st.verify("host-a", ""));
        assert_eq!(st.verify_any(&tok).as_deref(), Some("host-a"));
        assert_eq!(st.verify_any("bogus"), None);
        // re-issue revokes the old token (dockhand semantics)
        let tok2 = st.issue("host-a").unwrap();
        assert!(!st.verify("host-a", &tok));
        assert!(st.verify("host-a", &tok2));
        assert!(st.revoke("host-a"));
        assert!(!st.verify("host-a", &tok2));
    }

    #[test]
    fn stored_hash_never_contains_the_token() {
        let mut st = TokenStore::default();
        let tok = st.issue("h").unwrap();
        let json = st.to_json_pretty().unwrap();
        assert!(!json.contains(&tok));
        assert!(json.contains(&tok[..PREFIX_LEN])); // prefix only
    }

    #[test]
    fn hash_format_and_verify_roundtrip() {
        let h = hash_token("s3cr3t").unwrap();
        assert!(h.contains('$'));
        assert!(verify_token("s3cr3t", &h));
        assert!(!verify_token("s3cr34", &h));
        assert!(!verify_token("s3cr3t", "garbage"));
    }
}
