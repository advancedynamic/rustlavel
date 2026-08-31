//! COSE keys, and the one thing this package must never get wrong.
//!
//! An authenticator hands back its public key as a COSE_Key — a CBOR map with
//! integer labels — and every later assertion is checked against it. If the
//! parsing is loose or the verification is wrong, the package accepts forged
//! logins, and nothing else it does matters.

use crate::cbor::Cbor;
use rustlavel_core::{Error, Result};

/// COSE label 1: which family of key this is.
const LABEL_KTY: i64 = 1;
/// COSE label 3: which algorithm it signs with.
const LABEL_ALG: i64 = 3;
/// COSE label -1: the curve, for both EC2 and OKP keys.
const LABEL_CRV: i64 = -1;
/// COSE label -2: `x`, or the whole public key for an OKP.
const LABEL_X: i64 = -2;
/// COSE label -3: `y`, for EC2 only.
const LABEL_Y: i64 = -3;

const KTY_OKP: i64 = 1;
const KTY_EC2: i64 = 2;
const CRV_P256: i64 = 1;
const CRV_ED25519: i64 = 6;

/// The algorithms this package will verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    /// COSE -7. ECDSA on P-256 with SHA-256, and what essentially every
    /// passkey in the world uses: Apple, Google, Yubico, the password
    /// managers.
    Es256,
    /// COSE -8. Ed25519, which some security keys prefer.
    EdDsa,
}

impl SignatureAlgorithm {
    /// The number as it appears in a COSE key and in `pubKeyCredParams`.
    pub fn cose_id(self) -> i64 {
        match self {
            SignatureAlgorithm::Es256 => -7,
            SignatureAlgorithm::EdDsa => -8,
        }
    }

    pub fn from_cose_id(id: i64) -> Option<SignatureAlgorithm> {
        match id {
            -7 => Some(SignatureAlgorithm::Es256),
            -8 => Some(SignatureAlgorithm::EdDsa),
            _ => None,
        }
    }

    /// What a relying party offers during registration, best first.
    pub fn offered() -> [SignatureAlgorithm; 2] {
        [SignatureAlgorithm::Es256, SignatureAlgorithm::EdDsa]
    }
}

/// A public key an authenticator registered, in the only two shapes accepted.
///
/// Not `Debug`-derived: a public key is not a secret, but the credential it
/// belongs to identifies a person, and a login flow is exactly the code whose
/// logs get pasted into a bug report.
#[derive(Clone, PartialEq, Eq)]
pub enum CoseKey {
    /// Uncompressed P-256 coordinates, 32 bytes each.
    Es256 { x: [u8; 32], y: [u8; 32] },
    /// A compressed Ed25519 point.
    EdDsa { x: [u8; 32] },
}

impl CoseKey {
    pub fn algorithm(&self) -> SignatureAlgorithm {
        match self {
            CoseKey::Es256 { .. } => SignatureAlgorithm::Es256,
            CoseKey::EdDsa { .. } => SignatureAlgorithm::EdDsa,
        }
    }

    /// Read a COSE_Key out of decoded CBOR.
    ///
    /// The algorithm is checked against the key type rather than trusted on
    /// its own: a key claiming ES256 while carrying an Ed25519 point is not a
    /// combination any honest authenticator produces, and accepting it would
    /// mean verifying with something other than what was registered.
    pub fn parse(value: &Cbor) -> Result<CoseKey> {
        let kty = value
            .get_int(LABEL_KTY)
            .and_then(Cbor::as_i64)
            .ok_or_else(|| Error::msg("the COSE key has no key type (label 1)"))?;
        let alg = value
            .get_int(LABEL_ALG)
            .and_then(Cbor::as_i64)
            .ok_or_else(|| Error::msg("the COSE key has no algorithm (label 3)"))?;
        let crv = value.get_int(LABEL_CRV).and_then(Cbor::as_i64);

        let algorithm = SignatureAlgorithm::from_cose_id(alg).ok_or_else(|| {
            Error::msg(format!(
                "COSE algorithm {alg} is not one this package verifies. It speaks ES256 (-7) \
                 and EdDSA (-8); RS256 (-257), used by some TPM-backed Windows Hello \
                 credentials, is not implemented."
            ))
        })?;

        match (algorithm, kty, crv) {
            (SignatureAlgorithm::Es256, KTY_EC2, Some(CRV_P256)) => Ok(CoseKey::Es256 {
                x: coordinate(value, LABEL_X, "x")?,
                y: coordinate(value, LABEL_Y, "y")?,
            }),
            (SignatureAlgorithm::EdDsa, KTY_OKP, Some(CRV_ED25519)) => {
                Ok(CoseKey::EdDsa { x: coordinate(value, LABEL_X, "x")? })
            }
            (algorithm, kty, crv) => Err(Error::msg(format!(
                "the COSE key claims algorithm {} but has key type {kty} and curve {crv:?}, \
                 which do not go together",
                algorithm.cose_id()
            ))),
        }
    }

    /// Whether `signature` really covers `message` under this key.
    ///
    /// The whole package rests here. A `false` — or an error — must mean the
    /// signature is not good, and nothing that reaches this function may be
    /// able to make it say otherwise.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self {
            CoseKey::Es256 { x, y } => verify_es256(x, y, message, signature),
            CoseKey::EdDsa { x } => verify_eddsa(x, message, signature),
        }
    }
}

/// Names the algorithm, never the key material.
impl std::fmt::Debug for CoseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoseKey")
            .field("algorithm", &self.algorithm())
            .field("key", &"<public key>")
            .finish()
    }
}

fn coordinate(value: &Cbor, label: i64, name: &str) -> Result<[u8; 32]> {
    let bytes = value
        .get_int(label)
        .and_then(Cbor::as_bytes)
        .ok_or_else(|| Error::msg(format!("the COSE key has no `{name}` coordinate")))?;

    // Exactly 32, not "at most". A short coordinate left-padded by the reader
    // is a different point from the one that was registered.
    <[u8; 32]>::try_from(bytes).map_err(|_| {
        Error::msg(format!(
            "the COSE key's `{name}` coordinate is {} bytes, and must be exactly 32",
            bytes.len()
        ))
    })
}

fn verify_es256(x: &[u8; 32], y: &[u8; 32], message: &[u8], signature: &[u8]) -> bool {
    use p256::ecdsa::signature::Verifier;

    let point = p256::EncodedPoint::from_affine_coordinates(x.into(), y.into(), false);
    let Ok(key) = p256::ecdsa::VerifyingKey::from_encoded_point(&point) else {
        return false;
    };

    // WebAuthn signatures are DER, not the fixed-width form. `from_der` also
    // rejects the non-canonical encodings that let one signature be written
    // several ways.
    let Ok(parsed) = p256::ecdsa::Signature::from_der(signature) else {
        return false;
    };

    key.verify(message, &parsed).is_ok()
}

fn verify_eddsa(x: &[u8; 32], message: &[u8], signature: &[u8]) -> bool {
    use ed25519_dalek::Verifier;

    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(x) else {
        return false;
    };
    let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
        return false;
    };

    key.verify(message, &ed25519_dalek::Signature::from_bytes(&bytes)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor::Cbor;

    /// `{1: 2, 3: -7, -1: 1, -2: x, -3: y}` — the shape every passkey sends.
    fn es256_key(x: [u8; 32], y: [u8; 32]) -> Cbor {
        let mut bytes = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        bytes.extend_from_slice(&x);
        bytes.extend_from_slice(&[0x22, 0x58, 0x20]);
        bytes.extend_from_slice(&y);
        Cbor::parse(&bytes).expect("a well-formed COSE key")
    }

    /// A signing key from fixed bytes, so the test needs no randomness and
    /// gives the same answer on every machine.
    fn signing_key() -> p256::ecdsa::SigningKey {
        p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).expect("a valid scalar")
    }

    fn public_coordinates(key: &p256::ecdsa::SigningKey) -> ([u8; 32], [u8; 32]) {
        let point = key.verifying_key().to_encoded_point(false);
        (
            point.x().expect("an x coordinate").as_slice().try_into().unwrap(),
            point.y().expect("a y coordinate").as_slice().try_into().unwrap(),
        )
    }

    #[test]
    fn reads_the_key_a_passkey_registers() {
        let key = CoseKey::parse(&es256_key([1u8; 32], [2u8; 32])).unwrap();

        assert_eq!(key.algorithm(), SignatureAlgorithm::Es256);
        assert!(matches!(key, CoseKey::Es256 { x, y } if x == [1u8; 32] && y == [2u8; 32]));
    }

    #[test]
    fn a_real_signature_verifies_and_a_tampered_one_does_not() {
        use p256::ecdsa::signature::Signer;

        let signing = signing_key();
        let (x, y) = public_coordinates(&signing);
        let key = CoseKey::parse(&es256_key(x, y)).unwrap();

        let message = b"authenticator data || client data hash";
        let signature: p256::ecdsa::Signature = signing.sign(message);
        let der = signature.to_der();

        assert!(key.verify(message, der.as_bytes()), "a genuine signature must verify");
        assert!(!key.verify(b"a different message", der.as_bytes()), "wrong message");

        // One bit of the signature, flipped.
        let mut tampered = der.as_bytes().to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(!key.verify(message, &tampered), "a tampered signature must not verify");
    }

    #[test]
    fn a_signature_from_a_different_key_does_not_verify() {
        use p256::ecdsa::signature::Signer;

        let signing = signing_key();
        let other = p256::ecdsa::SigningKey::from_bytes(&[9u8; 32].into()).unwrap();
        let (x, y) = public_coordinates(&signing);
        let key = CoseKey::parse(&es256_key(x, y)).unwrap();

        let message = b"assertion";
        let signature: p256::ecdsa::Signature = other.sign(message);

        assert!(!key.verify(message, signature.to_der().as_bytes()));
    }

    #[test]
    fn garbage_in_the_signature_field_is_false_and_never_a_panic() {
        // Everything here is attacker-controlled, so every shape must return
        // false rather than unwinding out of a login handler.
        let signing = signing_key();
        let (x, y) = public_coordinates(&signing);
        let key = CoseKey::parse(&es256_key(x, y)).unwrap();

        for signature in [b"".as_slice(), b"\x00", &[0xff; 8], &[0x30; 72], &[0u8; 64]] {
            assert!(!key.verify(b"message", signature));
        }
    }

    #[test]
    fn an_algorithm_that_disagrees_with_the_key_type_is_refused() {
        // A key claiming ES256 while carrying an Ed25519 point is not something
        // an honest authenticator produces, and accepting it would mean
        // verifying with something other than what was registered.
        let mut bytes = vec![0xa4, 0x01, 0x01, 0x03, 0x26, 0x20, 0x06, 0x21, 0x58, 0x20];
        bytes.extend_from_slice(&[1u8; 32]);
        let error = CoseKey::parse(&Cbor::parse(&bytes).unwrap()).unwrap_err().to_string();

        assert!(error.contains("do not go together"), "got {error}");
    }

    #[test]
    fn a_short_coordinate_is_refused_rather_than_padded() {
        // Left-padding a 31-byte coordinate produces a different point from the
        // one that was registered, and would verify against nothing.
        let mut bytes = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x1f];
        bytes.extend_from_slice(&[1u8; 31]);
        bytes.extend_from_slice(&[0x22, 0x58, 0x20]);
        bytes.extend_from_slice(&[2u8; 32]);

        let error = CoseKey::parse(&Cbor::parse(&bytes).unwrap()).unwrap_err().to_string();
        assert!(error.contains("31 bytes"), "got {error}");
        assert!(error.contains("exactly 32"), "got {error}");
    }

    #[test]
    fn rs256_says_plainly_that_it_is_not_implemented() {
        // Windows Hello on a TPM can offer it, and "unknown algorithm -257"
        // would send somebody hunting for a parsing bug that is not there.
        let key = Cbor::parse(&[0xa2, 0x01, 0x03, 0x03, 0x39, 0x01, 0x00]).unwrap();
        let error = CoseKey::parse(&key).unwrap_err().to_string();

        assert!(error.contains("RS256"), "got {error}");
        assert!(error.contains("not implemented"), "got {error}");
    }

    #[test]
    fn debug_does_not_print_key_material() {
        let key = CoseKey::parse(&es256_key([0xab; 32], [0xcd; 32])).unwrap();
        let printed = format!("{key:?}");

        assert!(!printed.contains("171"), "coordinates reached the output: {printed}");
        assert!(printed.contains("Es256"));
    }

    #[test]
    fn the_offered_algorithms_are_the_ones_that_can_be_verified() {
        // An offer the verifier cannot honour registers a credential that can
        // never log in again.
        for algorithm in SignatureAlgorithm::offered() {
            assert_eq!(SignatureAlgorithm::from_cose_id(algorithm.cose_id()), Some(algorithm));
        }
    }
}
