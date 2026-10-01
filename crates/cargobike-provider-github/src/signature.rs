//! The GitHub signature digest : HMAC-SHA256 keyed by the
//! webhook secret over the RAW delivery body, hex-comparable against
//! `X-Hub-Signature-256`. Production verifies via webhook's
//! verify_slice (the constant-time); this module's helper exists for
//! the tests' signature generation.

/// The delivery's digest, lowercase hex (the tests build signatures
/// with it); production verifies via webhook's verify_slice.
#[cfg(test)]
pub fn hmac_sha256_hex(secret_key: &[u8], body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret_key) {
        Ok(mac) => mac,
        Err(error) => unreachable!("HMAC accepts any key length: {error}"),
    };
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    hex::encode(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rfc_4231_case_2_vector() {
        // HMAC-SHA256(the key = "Jefe", data = "what do ya want for nothing?")
        let digest = hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            digest,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
