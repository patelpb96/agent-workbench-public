//! Per-install secret and challenge-response. HMAC-SHA256 built on `sha2` (no extra
//! crate). Defence in depth against other users, sandboxed processes and accidents;
//! not a barrier against code already running as this user (see design section 13).
use crate::{caps::ct_eq, proto::ClientKind};
use sha2::{Digest, Sha256};

pub const SECRET_BYTES: usize = 32;
pub const NONCE_BYTES: usize = 32;

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Proof that the client holds the secret for this exact challenge, protocol version
/// and client kind (so a proof for one cannot be replayed as another).
pub fn proof(secret: &[u8], nonce: &[u8], version: u32, kind: ClientKind) -> [u8; 32] {
    let mut msg = b"wb-auth-v1".to_vec();
    msg.extend_from_slice(&version.to_le_bytes());
    msg.push(kind.tag());
    msg.extend_from_slice(nonce);
    hmac_sha256(secret, &msg)
}

pub fn verify(secret: &[u8], nonce: &[u8], version: u32, kind: ClientKind, claimed_hex: &str) -> bool {
    match unhex(claimed_hex) {
        Some(c) => ct_eq(&proof(secret, nonce, version, kind), &c),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_vectors() {
        // RFC 4231 test case 1 and 2.
        assert_eq!(hex(&hmac_sha256(&[0x0b; 20], b"Hi There")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // Key longer than the block size is hashed first (test case 6).
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn proofs_bind_secret_nonce_version_and_kind() {
        let secret = [9u8; SECRET_BYTES];
        let nonce = [3u8; NONCE_BYTES];
        let good = hex(&proof(&secret, &nonce, 1, ClientKind::Gui));
        assert!(verify(&secret, &nonce, 1, ClientKind::Gui, &good));
        assert!(!verify(&secret, &nonce, 1, ClientKind::Cli, &good), "kind is bound");
        assert!(!verify(&secret, &nonce, 2, ClientKind::Gui, &good), "version is bound");
        assert!(!verify(&secret, &[4u8; NONCE_BYTES], 1, ClientKind::Gui, &good), "nonce is bound (no replay)");
        assert!(!verify(&[8u8; SECRET_BYTES], &nonce, 1, ClientKind::Gui, &good));
        for bad in ["", "zz", "abc", &good[..good.len() - 2]] {
            assert!(!verify(&secret, &nonce, 1, ClientKind::Gui, bad));
        }
    }

    #[test]
    fn hex_round_trips_and_rejects_non_hex() {
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])).unwrap(), [0, 1, 254, 255]);
        assert!(unhex("0g").is_none() && unhex("0").is_none() && unhex("é1").is_none());
    }
}
