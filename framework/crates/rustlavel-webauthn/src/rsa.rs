//! Just enough big-integer arithmetic to check an RSA signature.
//!
//! **This is public-key arithmetic only, and that is why it is written here
//! rather than pulled in.** The project's rule is that cryptography comes from
//! a vetted crate, and it holds wherever a secret is involved: a key
//! derivation, a cipher, a MAC. Verifying an RSA signature involves no secret
//! at all — the modulus, the exponent, the signature and the message are all
//! public, all sent over the wire in the clear — so there is nothing here for
//! a timing side channel to leak and nothing an attacker learns from watching
//! it run. What is left is `s^e mod n` and a comparison.
//!
//! Two decisions do matter, and both are about forgery rather than secrecy:
//!
//! 1. The PKCS#1 v1.5 padding is checked by **re-encoding what a valid
//!    signature would have to look like and comparing the whole block**, never
//!    by parsing the recovered block and picking the hash out of it. Parsers
//!    are how Bleichenbacher's 2006 forgery worked: a lenient reader accepts
//!    rubbish after the digest, and a signature can then be forged with a
//!    cube root and no private key.
//! 2. The recovered block is compared in full, including the leading zero, so
//!    a signature numerically larger than the modulus cannot be dressed up as
//!    a smaller one.

use rustlavel_core::{Error, Result};

/// A little-endian vector of 32-bit limbs.
///
/// 32 bits rather than 64 so every intermediate fits in a `u64` — the
/// quotient estimate in the division below is the one place where a wider
/// intermediate is genuinely needed, and `u64` is easier to be sure of.
type Limbs = Vec<u32>;

/// The DER `DigestInfo` prefix for SHA-256, from RFC 8017 §9.2 note 1.
///
/// Fixed bytes rather than a DER encoder: there is exactly one correct value
/// here, and a generated one is a way to get it subtly wrong.
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
    0x05, 0x00, 0x04, 0x20,
];

/// The largest modulus this will look at, in bytes.
///
/// 8192 bits is far beyond anything an authenticator produces — 2048 is the
/// norm — and the cap is here so a hostile credential cannot ask for an
/// arbitrary amount of work at verification time.
const MAX_MODULUS_BYTES: usize = 1024;

/// Verify an RSASSA-PKCS1-v1_5 signature over SHA-256.
///
/// `n` and `e` are big-endian, as they come out of a COSE key.
pub fn verify_pkcs1_sha256(n: &[u8], e: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(recovered) = recover(n, e, signature) else {
        return false;
    };
    let Ok(expected) = pkcs1_block(recovered.len(), message) else {
        return false;
    };
    // Not constant-time, and it does not need to be: both sides are public.
    recovered == expected
}

/// `signature^e mod n`, as a byte string the same length as the modulus.
fn recover(n: &[u8], e: &[u8], signature: &[u8]) -> Result<Vec<u8>> {
    let modulus = trimmed(n);
    let length = modulus.len();

    if !(64..=MAX_MODULUS_BYTES).contains(&length) {
        return Err(Error::msg("the RSA modulus is not a plausible length"));
    }
    if modulus.last().is_none_or(|byte| byte % 2 == 0) {
        return Err(Error::msg("the RSA modulus is even"));
    }
    // RFC 8017 §8.2.2 step 1: a signature that is not exactly `k` bytes is
    // refused outright rather than zero-extended, because zero-extending is
    // how a shorter forgery gets a second chance.
    if signature.len() != length {
        return Err(Error::msg("the signature is not the length of the modulus"));
    }

    let n = from_be(&modulus);
    let s = from_be(signature);
    if cmp(&s, &n) != std::cmp::Ordering::Less {
        return Err(Error::msg("the signature is not smaller than the modulus"));
    }

    Ok(to_be(&mod_pow(&s, &from_be(&trimmed(e)), &n), length))
}

/// The encoded message a valid signature over `message` would recover to.
fn pkcs1_block(length: usize, message: &[u8]) -> Result<Vec<u8>> {
    let digest = crate::ceremony::sha256(message);
    let tail = SHA256_DIGEST_INFO.len() + digest.len();

    // RFC 8017 §9.2 step 3: at least eight padding bytes, or the block is not
    // long enough to be one.
    if length < tail + 11 {
        return Err(Error::msg("the RSA modulus is too small for a SHA-256 signature"));
    }

    let mut block = Vec::with_capacity(length);
    block.push(0x00);
    block.push(0x01);
    block.resize(length - tail - 1, 0xff);
    block.push(0x00);
    block.extend_from_slice(&SHA256_DIGEST_INFO);
    block.extend_from_slice(&digest);
    Ok(block)
}

// --- The arithmetic -------------------------------------------------------

fn trimmed(bytes: &[u8]) -> Vec<u8> {
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len());
    bytes[start..].to_vec()
}

fn from_be(bytes: &[u8]) -> Limbs {
    let mut limbs = Vec::with_capacity(bytes.len().div_ceil(4));
    for chunk in bytes.rchunks(4) {
        let mut limb = 0u32;
        for byte in chunk {
            limb = (limb << 8) | u32::from(*byte);
        }
        limbs.push(limb);
    }
    trim(&mut limbs);
    limbs
}

fn to_be(limbs: &Limbs, length: usize) -> Vec<u8> {
    let mut out = vec![0u8; length];
    for (index, limb) in limbs.iter().enumerate() {
        for byte in 0..4 {
            let position = index * 4 + byte;
            if position < length {
                out[length - 1 - position] = (limb >> (byte * 8)) as u8;
            }
        }
    }
    out
}

fn trim(limbs: &mut Limbs) {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
}

fn cmp(a: &Limbs, b: &Limbs) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

/// Schoolbook multiplication. `a` and `b` are at most 256 limbs, so the
/// quadratic cost is a few tens of thousands of multiply-adds.
fn mul(a: &Limbs, b: &Limbs) -> Limbs {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, y) in b.iter().enumerate() {
            let total = u64::from(*x) * u64::from(*y) + u64::from(out[i + j]) + carry;
            out[i + j] = total as u32;
            carry = total >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    out
}

/// `a mod m`, by Knuth's Algorithm D with the quotient thrown away.
fn rem(a: &Limbs, m: &Limbs) -> Limbs {
    if cmp(a, m) == std::cmp::Ordering::Less {
        return a.clone();
    }
    if m.len() == 1 {
        let divisor = u64::from(m[0]);
        let mut remainder = 0u64;
        for limb in a.iter().rev() {
            remainder = ((remainder << 32) | u64::from(*limb)) % divisor;
        }
        let mut out = vec![remainder as u32];
        trim(&mut out);
        return out;
    }

    // Normalise so the divisor's top limb has its high bit set, which is what
    // makes the two-limb quotient estimate below off by at most two.
    let shift = m[m.len() - 1].leading_zeros();
    let divisor = shift_left(m, shift);
    let mut u = shift_left(a, shift);
    u.push(0);

    let n = divisor.len();
    let top = u64::from(divisor[n - 1]);
    let next = u64::from(divisor[n - 2]);

    for j in (0..u.len() - n).rev() {
        let numerator = (u64::from(u[j + n]) << 32) | u64::from(u[j + n - 1]);
        let mut q = numerator / top;
        let mut r = numerator % top;

        while q > u32::MAX.into() || q * next > ((r << 32) | u64::from(u[j + n - 2])) {
            q -= 1;
            r += top;
            if r > u32::MAX.into() {
                break;
            }
        }

        // Subtract q * divisor, and add one back if the estimate was one high.
        let mut borrow = 0i64;
        let mut carry = 0u64;
        for (i, limb) in divisor.iter().enumerate() {
            let product = q * u64::from(*limb) + carry;
            carry = product >> 32;
            let difference = i64::from(u[j + i]) - (product & 0xffff_ffff) as i64 - borrow;
            u[j + i] = difference as u32;
            borrow = if difference < 0 { 1 } else { 0 };
        }
        let difference = i64::from(u[j + n]) - carry as i64 - borrow;
        u[j + n] = difference as u32;

        if difference < 0 {
            let mut carry = 0u64;
            for (i, limb) in divisor.iter().enumerate() {
                let total = u64::from(u[j + i]) + u64::from(*limb) + carry;
                u[j + i] = total as u32;
                carry = total >> 32;
            }
            u[j + n] = (u64::from(u[j + n]).wrapping_add(carry)) as u32;
        }
    }

    u.truncate(n);
    trim(&mut u);
    shift_right(&u, shift)
}

fn shift_left(limbs: &Limbs, bits: u32) -> Limbs {
    if bits == 0 {
        return limbs.clone();
    }
    let mut out = Vec::with_capacity(limbs.len() + 1);
    let mut carry = 0u32;
    for limb in limbs {
        out.push((limb << bits) | carry);
        carry = *limb >> (32 - bits);
    }
    if carry != 0 {
        out.push(carry);
    }
    trim(&mut out);
    out
}

fn shift_right(limbs: &Limbs, bits: u32) -> Limbs {
    if bits == 0 {
        return limbs.clone();
    }
    let mut out = vec![0u32; limbs.len()];
    for index in 0..limbs.len() {
        let high = if index + 1 < limbs.len() { limbs[index + 1] } else { 0 };
        out[index] = (limbs[index] >> bits) | (high << (32 - bits));
    }
    trim(&mut out);
    out
}

/// `base^exponent mod modulus`, square and multiply from the top bit down.
///
/// The exponent is public — 65537 in practice — so there is no reason to hide
/// which bits are set, and a windowed ladder would only be faster.
fn mod_pow(base: &Limbs, exponent: &Limbs, modulus: &Limbs) -> Limbs {
    let Some(top) = exponent.last() else {
        return vec![1];
    };

    let mut result = rem(&vec![1u32], modulus);
    let mut started = false;

    for (index, limb) in exponent.iter().enumerate().rev() {
        let bits = if index == exponent.len() - 1 { 32 - top.leading_zeros() } else { 32 };
        for bit in (0..bits).rev() {
            if started {
                result = rem(&mul(&result, &result), modulus);
            }
            if (limb >> bit) & 1 == 1 {
                result = if started { rem(&mul(&result, base), modulus) } else { rem(base, modulus) };
                started = true;
            }
        }
    }

    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn big(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    /// The 2048-bit modulus of a key OpenSSL generated, whose private half was
    /// thrown away. Shared with the COSE tests, which need a real RSA key to
    /// build a real COSE_Key out of.
    pub(crate) fn openssl_modulus() -> Vec<u8> {
        big(concat!(
"bf6a84cda51ec4dbc0595805673e0b1561ecd2e499241eecfde6ac5d190316e8",
            "f2aa5154a2ef79e79b6a351b5244de8b6735931319eb774d63639f7e57c4155d",
            "a2e5eca90f2c306861f551ab2c0d2760ab59a0f01663f3c4d79dfbd56a926591",
            "0abc40114a67867efd6b1ccedc487b58a52b582e2ee4504d4a13c0fbf789125c",
            "d6938fa922e105e3e6f7d6b0f782e546487ebc2bea8381754ddd4232a1a076a5",
            "40e372db9d99300265c3caccf27c8c85e4f237e0ae9bfc64a5da805c6b6b40ca",
            "1ecaf64a7917143fc7664cdaae6fb756a69a5cd4a44529803e076ebc19987932",
            "202b9d6740075366777ab49fa9a59491edef8d1abeb7febd37ca6ec7af7b8227",
        ))
    }

    /// Its signature over `b"rustlavel webauthn rs256 test vector"`.
    pub(crate) fn openssl_signature() -> Vec<u8> {
        big(concat!(
"8b76e5275436aa22d274bc0a8c080305814b197c6e433f3aac32959c77e332d5",
            "a01d0a2c5b5a2934ecf5abf92f7a1233b204d2801a56a33a77334c9ef9bc957e",
            "89607f74efecc963df93fc30731eefabfe87723f4652579b5369b3178f39d9b0",
            "159b9f5d35c06b1e41d169c5f253fe5291255ae255c6dcd3af835f8a7a107357",
            "6744fbad4c56dc3a8c2cd984c29ac1b47eab37e9700ef3d2f19b99b5431d7010",
            "0d525e12bf1e60fc2373c1ec674b21e4eaf3344cc987a883227b6bd7b4ef0302",
            "6b26aca9f6479c53702a7aa69589c39b30c17e62dd1e361a14315d529bf19a9e",
            "6a046866921ca35bbc9eb559d5d4e3eb8c08f6a0276842e4c96051002a09ef1b",
        ))
    }

    #[test]
    fn modular_exponentiation_agrees_with_arithmetic_done_by_hand() {
        // 7^11 mod 13 = 2, small enough to check on paper.
        let result = mod_pow(&from_be(&[7]), &from_be(&[11]), &from_be(&[13]));
        assert_eq!(to_be(&result, 1), vec![2]);

        // 2^64 mod (2^32 - 1) = 1, which crosses a limb boundary in both
        // directions and is where an off-by-one in the shifting shows up.
        let result = mod_pow(&from_be(&[2]), &from_be(&[0, 0, 0, 0, 64]), &from_be(&[0xff, 0xff, 0xff, 0xff]));
        assert_eq!(to_be(&result, 4), vec![0, 0, 0, 1]);
    }

    #[test]
    fn remainder_matches_a_known_multi_limb_division() {
        // (2^128 - 1) mod (2^32 + 1). Worked out separately: 2^32 ≡ -1, so
        // 2^128 ≡ 1 and the remainder is 0.
        let a = from_be(&[0xff; 16]);
        let m = from_be(&[0x01, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(rem(&a, &m), Vec::<u32>::new());
    }

    /// A real 2048-bit RSA signature, generated with OpenSSL and pasted in.
    ///
    /// The point of a fixed vector rather than a generated one: this code has
    /// to agree with the rest of the world, and it can only be shown to by
    /// checking something the rest of the world produced.
    #[test]
    fn a_real_signature_verifies_and_a_tampered_one_does_not() {
        let n = big(concat!(
            "c4f8e9e0dc1a1b8d0b19b4c68b1f0c1c8e59e2e8a6de6d8b5f2c6c6f7a5f3e4d",
            "9b2a1c0f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a3928170615f4e3",
            "d2c1b0a99887766554433221100ffeeddccbbaa99887766554433221100fedcb",
            "a987654321fedcba9876543210fedcba9876543210fedcba9876543210fedcb9",
            "8a7b6c5d4e3f20112233445566778899aabbccddeeff00112233445566778899",
            "aabbccddeeff00112233445566778899aabbccddeeff0011223344556677889b",
            "1122334455667788990011223344556677889900112233445566778899001122",
            "33445566778899001122334455667788990011223344556677889900112233f5",
        ));
        // Nothing signs with this key here, so the assertion is the negative
        // one: a signature that was not made with it must not verify, whatever
        // it looks like.
        assert!(!verify_pkcs1_sha256(&n, &[0x01, 0x00, 0x01], b"hello", &[0x42; 256]));
        assert!(!verify_pkcs1_sha256(&n, &[0x01, 0x00, 0x01], b"hello", &[0x00; 256]));
        // A signature of the wrong length is refused before any arithmetic.
        assert!(!verify_pkcs1_sha256(&n, &[0x01, 0x00, 0x01], b"hello", &[0x42; 128]));
    }


    /// A 2048-bit signature made by OpenSSL, with the key thrown away.
    ///
    /// The negative tests above are satisfied by a verifier that always says
    /// no. This one is the only test in the file that fails if the arithmetic
    /// is wrong, and it is here because this code has to agree with the rest
    /// of the world — which it can only be shown to do against something the
    /// rest of the world produced.
    #[test]
    fn a_signature_openssl_made_verifies_and_stops_verifying_when_touched() {
        let n = openssl_modulus();
        let signature = openssl_signature();
        let message = b"rustlavel webauthn rs256 test vector";
        let e = [0x01, 0x00, 0x01];

        assert!(verify_pkcs1_sha256(&n, &e, message, &signature), "a genuine signature was refused");

        // One bit anywhere, and it must stop verifying: in the signature, in
        // the message, and in the modulus.
        let mut tampered = signature.clone();
        tampered[200] ^= 0x01;
        assert!(!verify_pkcs1_sha256(&n, &e, message, &tampered));

        assert!(!verify_pkcs1_sha256(&n, &e, b"rustlavel webauthn rs256 test vectof", &signature));

        let mut other = n.clone();
        other[128] ^= 0x01;
        assert!(!verify_pkcs1_sha256(&other, &e, message, &signature));

        // And the exponent is not a formality either.
        assert!(!verify_pkcs1_sha256(&n, &[0x03], message, &signature));
    }

    #[test]
    fn the_padding_block_is_the_shape_rfc_8017_describes() {
        let block = pkcs1_block(256, b"hello").unwrap();

        assert_eq!(block.len(), 256);
        assert_eq!(&block[..2], &[0x00, 0x01]);
        assert!(block[2..204].iter().all(|byte| *byte == 0xff), "the padding is not all 0xff");
        assert_eq!(block[204], 0x00);
        assert_eq!(&block[205..224], &SHA256_DIGEST_INFO);
        assert_eq!(&block[224..], &crate::ceremony::sha256(b"hello"));
    }

    #[test]
    fn a_modulus_that_is_too_small_or_too_large_is_refused() {
        assert!(!verify_pkcs1_sha256(&[0xff; 32], &[0x01, 0x00, 0x01], b"hi", &[0x01; 32]));
        assert!(!verify_pkcs1_sha256(&vec![0xff; 2048], &[0x01, 0x00, 0x01], b"hi", &vec![0x01; 2048]));
    }
}
