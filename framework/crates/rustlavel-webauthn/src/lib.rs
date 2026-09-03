//! rustlavel-webauthn: passkeys, written on the W3C WebAuthn specification.
//!
//! A password is a shared secret: the server holds something that can be
//! stolen from it, and the user types something that can be phished out of
//! them. A passkey is neither. The authenticator keeps a private key that
//! never leaves it, the server stores only the public half, and the signature
//! covers the origin — so a convincing copy of your login page at a different
//! address gets nothing, because the browser will not sign for it.
//!
//! That last property is the one worth the work. Every other second factor can
//! be relayed by a good enough phishing page; this one cannot.
//!
//! Everything here is written from scratch except the signature verification.
//! CBOR, COSE framing, the ceremonies, the challenge handling — all ours. The
//! elliptic-curve maths is not, and should not be: a subtly wrong verifier
//! accepts forged assertions, which is the single failure this package exists
//! to prevent.

pub mod authentication;
pub mod cbor;
pub mod ceremony;
pub mod challenge;
pub mod cose;
pub mod credential;
pub mod registration;

/// The big-integer arithmetic RS256 needs. Public-key only — see the module.
pub(crate) mod rsa;

pub use cbor::Cbor;
pub use cose::{CoseKey, SignatureAlgorithm};

pub use ceremony::{
    AttestedCredential, AuthenticatorAttachment, AuthenticatorData, AuthenticatorFlags, ClientData,
    RelyingParty, ResidentKey, UserEntity, UserVerification,
};
pub use challenge::{
    Ceremony, Challenge, ChallengeStore, MemoryChallengeStore, SharedChallengeStore,
};
pub use credential::{
    Credential, CredentialStore, MemoryCredentialStore, SharedCredentialStore,
};

pub use authentication::{
    Authentication, AuthenticationResponse, PublicKeyCredentialRequestOptions,
};
pub use registration::{
    AttestationStatement, PublicKeyCredentialCreationOptions, Registration, RegistrationResponse,
};

pub use rustlavel_core::{Error, Json, Result};

/// What a controller finishing either ceremony imports.
pub mod prelude {
    pub use crate::{
        AttestationStatement, Authentication, AuthenticationResponse, Ceremony, Challenge,
        ChallengeStore, Credential, CredentialStore, MemoryChallengeStore, MemoryCredentialStore,
        Registration, RegistrationResponse, RelyingParty, ResidentKey, UserEntity,
        UserVerification,
    };
}
