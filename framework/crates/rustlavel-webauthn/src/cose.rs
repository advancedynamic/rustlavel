//! COSE keys, and the one thing this package must never get wrong.
//!
//! An authenticator hands back its public key as a COSE_Key — a CBOR map with
//! integer labels — and every later assertion is checked against it. If the
//! parsing is loose or the verification is wrong, the package accepts forged
//! logins, and nothing else it does matters.

use crate::cbor::{Cbor, CborKey};
use rustlavel_core::{Error, Result};
use std::collections::BTreeMap;

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

    /// The COSE_Key map this key came from.
    ///
    /// The labels and the values are the ones `parse` insists on, and no
    /// others: kty, alg, crv, x and y for an EC2 key, and the same without y
    /// for an OKP. Anything more would be a field this package never read and
    /// so could not have checked.
    ///
    /// The pair `parse` / `to_cbor` is what makes a credential storable. A
    /// store that keeps its credentials anywhere but memory needs bytes for a
    /// column, and the bytes have to read back as the same key.
    pub fn to_cbor(&self) -> Cbor {
        let mut map = BTreeMap::new();
        map.insert(CborKey::Int(LABEL_ALG), Cbor::Negative(self.algorithm().cose_id()));

        match self {
            CoseKey::Es256 { x, y } => {
                map.insert(CborKey::Int(LABEL_KTY), Cbor::Unsigned(KTY_EC2 as u64));
                map.insert(CborKey::Int(LABEL_CRV), Cbor::Unsigned(CRV_P256 as u64));
                map.insert(CborKey::Int(LABEL_X), Cbor::Bytes(x.to_vec()));
                map.insert(CborKey::Int(LABEL_Y), Cbor::Bytes(y.to_vec()));
            }
            CoseKey::EdDsa { x } => {
                map.insert(CborKey::Int(LABEL_KTY), Cbor::Unsigned(KTY_OKP as u64));
                map.insert(CborKey::Int(LABEL_CRV), Cbor::Unsigned(CRV_ED25519 as u64));
                map.insert(CborKey::Int(LABEL_X), Cbor::Bytes(x.to_vec()));
            }
        }

        Cbor::Map(map)
    }

    /// The key as canonical CTAP2 CBOR: what to put in a database column.
    ///
    /// Because both ends of this are canonical, storing what an authenticator
    /// registered and reading it back gives the same bytes it sent — which is
    /// worth having, because it means a stored credential can be compared with
    /// the attestation it came from without parsing either.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_cbor().to_bytes()
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

    /// `{1: 1, 3: -8, -1: 6, -2: x}` — the OKP shape, in canonical CTAP2
    /// order, written out by hand.
    ///
    /// Unlike the EC2 case there is no fixture in this crate to borrow: the
    /// fake authenticator the ceremony tests use only speaks ES256. These are
    /// the bytes a security key that prefers Ed25519 puts on the wire, and the
    /// point of writing them here rather than building them with the encoder
    /// is that the encoder is what they are checking.
    fn eddsa_key_bytes(x: &[u8; 32]) -> Vec<u8> {
        let mut bytes = vec![0xa4, 0x01, 0x01, 0x03, 0x27, 0x20, 0x06, 0x21, 0x58, 0x20];
        bytes.extend_from_slice(x);
        bytes
    }

    #[test]
    fn a_real_es256_key_re_encodes_to_the_bytes_the_authenticator_sent() {
        // Byte-for-byte, not merely equivalent. A real authenticator already
        // sends canonical CBOR, so anything less than identical bytes would
        // mean this encoder and the specification disagree somewhere.
        for seed in 1..=4 {
            let device = crate::ceremony::fake::Authenticator::new(seed);
            let sent = device.cose_key();
            let key = CoseKey::parse(&Cbor::parse(&sent).unwrap()).unwrap();

            assert_eq!(key.to_bytes(), sent, "seed {seed}");
            assert_eq!(
                CoseKey::parse(&Cbor::parse(&key.to_bytes()).unwrap()).unwrap(),
                key,
                "seed {seed} did not read back as itself"
            );
        }
    }

    #[test]
    fn an_ed25519_key_re_encodes_to_the_bytes_a_security_key_sends() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let sent = eddsa_key_bytes(&signing.verifying_key().to_bytes());

        let key = CoseKey::parse(&Cbor::parse(&sent).unwrap()).unwrap();
        assert_eq!(key.algorithm(), SignatureAlgorithm::EdDsa);
        assert_eq!(key.to_bytes(), sent);
        assert_eq!(CoseKey::parse(&Cbor::parse(&key.to_bytes()).unwrap()).unwrap(), key);
    }

    #[test]
    fn a_stored_es256_key_still_verifies_a_real_assertion() {
        // The one that matters. Self-consistency proves nothing on its own:
        // an encoder that swapped x and y would still round-trip through its
        // own parser. Only a genuine signature, made by the device that
        // registered, says the stored bytes are the key that was registered.
        let device = crate::ceremony::fake::Authenticator::new(4);
        let auth_data =
            device.authenticator_data("example.test", crate::ceremony::fake::ASSERTION_FLAGS, 9);
        let client_data =
            device.client_data("webauthn.get", b"a challenge", "https://example.test");
        let signature = device.sign(&auth_data, &client_data);

        let mut message = auth_data.clone();
        message.extend_from_slice(&crate::ceremony::sha256(&client_data));

        // Registered, written to bytes the way a database-backed store would,
        // and read back out of them.
        let registered = CoseKey::parse(&Cbor::parse(&device.cose_key()).unwrap()).unwrap();
        let stored = CoseKey::parse(&Cbor::parse(&registered.to_bytes()).unwrap()).unwrap();

        assert!(registered.verify(&message, &signature), "the fixture itself must verify");
        assert!(stored.verify(&message, &signature), "the key survived storage in name only");
    }

    #[test]
    fn a_stored_ed25519_key_still_verifies_a_real_signature() {
        use ed25519_dalek::Signer;

        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let sent = eddsa_key_bytes(&signing.verifying_key().to_bytes());
        let registered = CoseKey::parse(&Cbor::parse(&sent).unwrap()).unwrap();
        let stored = CoseKey::parse(&Cbor::parse(&registered.to_bytes()).unwrap()).unwrap();

        let message = b"authenticator data || client data hash";
        let signature = signing.sign(message).to_bytes();

        assert!(stored.verify(message, &signature));
        assert!(!stored.verify(b"a different message", &signature));
    }

    #[test]
    fn the_written_key_carries_the_labels_parse_reads_and_nothing_else() {
        // A label this package never read is a label it never checked, so
        // writing one out would be claiming more than was verified.
        let key = CoseKey::parse(&es256_key([1u8; 32], [2u8; 32])).unwrap();
        let Cbor::Map(map) = key.to_cbor() else { unreachable!("a COSE key is a map") };

        assert_eq!(map.len(), 5);
        for label in [LABEL_KTY, LABEL_ALG, LABEL_CRV, LABEL_X, LABEL_Y] {
            assert!(map.contains_key(&CborKey::Int(label)), "label {label} is missing");
        }

        let key = CoseKey::EdDsa { x: [3u8; 32] };
        let Cbor::Map(map) = key.to_cbor() else { unreachable!("a COSE key is a map") };

        assert_eq!(map.len(), 4);
        assert!(!map.contains_key(&CborKey::Int(LABEL_Y)), "an OKP key has no y");
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
