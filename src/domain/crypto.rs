use hmac::{Hmac, Mac};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

type HmacSha256 = Hmac<Sha256>;

pub(crate) fn sha256(input: &[u8]) -> [u8; 32] {
    Sha256::digest(input).into()
}

pub(crate) fn sha256_domain(domain: &[u8], input: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(input);
    digest.finalize().into()
}

pub(crate) fn hmac_sha256_domain(key: &[u8], domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(domain);
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_nist_vector() {
        assert_eq!(
            sha256(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_vector() {
        assert_eq!(
            hmac_sha256_domain(&[0x0b; 20], b"", &[b"Hi There"]),
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
                0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
                0x2e, 0x32, 0xcf, 0xf7,
            ]
        );
    }

    #[test]
    fn domain_hash_and_constant_time_comparison_are_exact() {
        let digest = sha256_domain(b"domain\0", b"payload");
        assert_eq!(digest, sha256(b"domain\0payload"));
        assert!(constant_time_eq(&digest, &digest));
        assert!(!constant_time_eq(&digest, &[0; 32]));
        assert!(!constant_time_eq(&digest, &digest[..31]));
    }
}
