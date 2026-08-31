//! Registration: the ceremony that hands the server a public key.
//!
//! Two halves. The server issues options — who the site is, who the user is,
//! and a challenge — and the browser hands them to an authenticator, which
//! makes a key pair, keeps the private half and returns the public one inside
//! an attestation object.
//!
//! The checks in [`RelyingParty::finish_registration`] are not a formality.
//! Each one is there because skipping it is a known attack, and the doc
//! comment on each says which.

use crate::ceremony::{
    AttestedCredential, AuthenticatorAttachment, AuthenticatorData, AuthenticatorFlags, ClientData,
    RelyingParty, ResidentKey, UserEntity, UserVerification,
};
use crate::challenge::{Ceremony, Challenge, ChallengeStore};
use crate::cose::SignatureAlgorithm;
use crate::credential::{Credential, CredentialStore};
use crate::{Cbor, Error, Json, Result};
use rustlavel_auth::{base64, unix_now};

/// What the server sends to `navigator.credentials.create()`.
#[derive(Debug, Clone)]
pub struct PublicKeyCredentialCreationOptions {
    rp_id: String,
    rp_name: String,
    user: UserEntity,
    challenge: Vec<u8>,
    algorithms: Vec<SignatureAlgorithm>,
    timeout_ms: u64,
    user_verification: UserVerification,
    resident_key: ResidentKey,
    attachment: Option<AuthenticatorAttachment>,
    exclude: Vec<Vec<u8>>,
}

impl PublicKeyCredentialCreationOptions {
    /// The credentials the authenticator should refuse to duplicate.
    ///
    /// Not a security control — an authenticator is free to ignore it, and the
    /// duplicate would be refused by the store anyway. It exists so that a
    /// user who already registered this device is told so by the browser
    /// instead of being walked through a ceremony that fails at the end.
    pub fn excluding<I: IntoIterator<Item = Vec<u8>>>(
        mut self,
        credential_ids: I,
    ) -> PublicKeyCredentialCreationOptions {
        self.exclude = credential_ids.into_iter().collect();
        self
    }

    pub fn challenge(&self) -> &[u8] {
        &self.challenge
    }

    pub fn exclude_credentials(&self) -> &[Vec<u8>] {
        &self.exclude
    }

    /// The `publicKey` object, ready to be serialised into a response body.
    pub fn json(&self) -> Json {
        let parameters: Vec<Json> = self
            .algorithms
            .iter()
            .map(|algorithm| {
                Json::object([
                    ("type", Json::String("public-key".into())),
                    ("alg", Json::Number(algorithm.cose_id() as f64)),
                ])
            })
            .collect();

        let exclude: Vec<Json> = self
            .exclude
            .iter()
            .map(|id| {
                Json::object([
                    ("type", Json::String("public-key".into())),
                    ("id", Json::String(base64::encode_url(id))),
                ])
            })
            .collect();

        let mut selection = vec![
            ("residentKey", Json::String(self.resident_key.as_str().into())),
            // The old boolean, kept because Safari and some Android versions
            // still read it and ignore `residentKey`.
            ("requireResidentKey", Json::Bool(self.resident_key == ResidentKey::Required)),
            ("userVerification", Json::String(self.user_verification.as_str().into())),
        ];
        if let Some(attachment) = self.attachment {
            selection.push(("authenticatorAttachment", Json::String(attachment.as_str().into())));
        }

        Json::object([
            (
                "rp",
                Json::object([
                    ("id", Json::String(self.rp_id.clone())),
                    ("name", Json::String(self.rp_name.clone())),
                ]),
            ),
            ("user", self.user.json()),
            ("challenge", Json::String(base64::encode_url(&self.challenge))),
            ("pubKeyCredParams", Json::Array(parameters)),
            ("timeout", Json::Number(self.timeout_ms as f64)),
            // Nothing here verifies an attestation statement, so nothing here
            // asks for one. Requesting attestation you will not check collects
            // a device identifier for no benefit.
            ("attestation", Json::String("none".into())),
            ("authenticatorSelection", Json::object(selection)),
            ("excludeCredentials", Json::Array(exclude)),
        ])
    }
}

/// Compact JSON, so `format!("{options}")` is a response body.
impl std::fmt::Display for PublicKeyCredentialCreationOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.json())
    }
}

/// What the browser posts back after `navigator.credentials.create()`.
#[derive(Debug, Clone)]
pub struct RegistrationResponse {
    id: Vec<u8>,
    client_data_json: Vec<u8>,
    attestation_object: Vec<u8>,
}

impl RegistrationResponse {
    pub fn new(
        id: impl Into<Vec<u8>>,
        client_data_json: impl Into<Vec<u8>>,
        attestation_object: impl Into<Vec<u8>>,
    ) -> RegistrationResponse {
        RegistrationResponse {
            id: id.into(),
            client_data_json: client_data_json.into(),
            attestation_object: attestation_object.into(),
        }
    }

    /// Read the shape a browser produces from `PublicKeyCredential.toJSON()`.
    pub fn from_json(value: &Json) -> Result<RegistrationResponse> {
        Ok(RegistrationResponse {
            id: crate::ceremony::base64_field(value, "rawId")?,
            client_data_json: crate::ceremony::base64_field(value, "response.clientDataJSON")?,
            attestation_object: crate::ceremony::base64_field(value, "response.attestationObject")?,
        })
    }

    /// The same, from the request body directly.
    pub fn parse(body: &str) -> Result<RegistrationResponse> {
        RegistrationResponse::from_json(&Json::parse(body)?)
    }

    pub fn id(&self) -> &[u8] {
        &self.id
    }

    pub fn client_data_json(&self) -> &[u8] {
        &self.client_data_json
    }

    pub fn attestation_object(&self) -> &[u8] {
        &self.attestation_object
    }
}

/// What was learned about the authenticator's own claim of identity: nothing.
///
/// There is deliberately no `Verified` variant. This package parses
/// attestation statements and does not verify any of them — verifying `packed`
/// or `tpm` means a certificate chain, the FIDO Metadata Service, and a
/// revocation story, none of which is written here. A variant that could be
/// mistaken for a checked result would be a lie in the type system, which is
/// worse than no result at all.
///
/// Passkeys use `none`, so for the credentials this package is built for there
/// is nothing to verify in the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationStatement {
    /// `fmt` was `none`: the authenticator asserted nothing about itself, so
    /// there is nothing that could have been checked.
    None,
    /// A statement in this format was present, parsed, and **not verified**.
    /// Treat the authenticator as unidentified.
    Unverified { format: String },
}

impl AttestationStatement {
    pub fn format(&self) -> &str {
        match self {
            AttestationStatement::None => "none",
            AttestationStatement::Unverified { format } => format,
        }
    }

    /// Whether the statement was cryptographically checked. Always `false`.
    ///
    /// Not a placeholder. A caller with a policy of "only certified
    /// authenticators" needs to be able to ask, and the honest answer from
    /// this package is no — so such a policy must refuse every registration
    /// here rather than believe one was checked.
    pub fn was_verified(&self) -> bool {
        false
    }
}

/// A completed registration.
#[derive(Debug, Clone)]
pub struct Registration {
    credential: Credential,
    attestation: AttestationStatement,
    flags: AuthenticatorFlags,
}

impl Registration {
    /// The credential, as it was written to the store.
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    pub fn into_credential(self) -> Credential {
        self.credential
    }

    /// What the authenticator claimed about itself, and whether it was checked.
    pub fn attestation(&self) -> &AttestationStatement {
        &self.attestation
    }

    pub fn flags(&self) -> AuthenticatorFlags {
        self.flags
    }

    /// Whether the authenticator verified the user, whatever the policy asked.
    pub fn user_verified(&self) -> bool {
        self.flags.user_verified()
    }
}

impl RelyingParty {
    /// Build creation options around a challenge you have already issued.
    ///
    /// The challenge is not stored by this call. Use it when the application
    /// keeps challenges somewhere this crate does not know about — a session,
    /// say — and store it yourself, or call [`RelyingParty::start_registration`]
    /// and let it do both.
    pub fn creation_options(
        &self,
        user: &UserEntity,
        challenge: &Challenge,
    ) -> PublicKeyCredentialCreationOptions {
        PublicKeyCredentialCreationOptions {
            rp_id: self.id().to_string(),
            rp_name: self.name().to_string(),
            user: user.clone(),
            challenge: challenge.bytes().to_vec(),
            algorithms: SignatureAlgorithm::offered().to_vec(),
            timeout_ms: self.timeout_ms(),
            user_verification: self.user_verification(),
            resident_key: self.resident_key(),
            attachment: self.attachment(),
            exclude: Vec::new(),
        }
    }

    /// Issue and store a challenge, and return the options to send back.
    ///
    /// The challenge is bound to this user, so the response cannot be finished
    /// against a different account, and the user's existing credentials go
    /// into `excludeCredentials` so a second registration of the same device
    /// is refused by the browser rather than by the store.
    pub async fn start_registration(
        &self,
        user: &UserEntity,
        challenges: &dyn ChallengeStore,
        credentials: &dyn CredentialStore,
    ) -> Result<PublicKeyCredentialCreationOptions> {
        let challenge = Challenge::issue(Ceremony::Registration).bound_to(user.id().to_vec());
        let options = self.creation_options(user, &challenge);
        challenges.store(challenge).await?;

        let existing = credentials.find_for_user(user.id()).await?;
        Ok(options.excluding(existing.into_iter().map(|credential| credential.id().to_vec())))
    }

    /// Check the authenticator's response, and store the credential.
    ///
    /// `user` must be the user whose session started the ceremony — the same
    /// one passed to [`RelyingParty::start_registration`]. It is checked
    /// against the challenge, so getting it wrong is a refusal rather than a
    /// credential attached to the wrong account.
    ///
    /// On success the credential is in the store. On any error nothing was
    /// written, and the challenge has been spent either way.
    pub async fn finish_registration(
        &self,
        user: &UserEntity,
        response: &RegistrationResponse,
        challenges: &dyn ChallengeStore,
        credentials: &dyn CredentialStore,
    ) -> Result<Registration> {
        let client_data = ClientData::parse(response.client_data_json())?;

        // A response to the *other* ceremony is not evidence for this one.
        client_data.expect_kind(Ceremony::Registration.client_data_type())?;

        // Taking it out is what makes it single-use; the checks come after,
        // so a challenge refused here is still spent and cannot be retried.
        let key = base64::encode_url(client_data.challenge());
        let challenge = challenges.take(&key).await?.ok_or_else(|| {
            Error::msg(
                "that challenge is not one this server issued, or it has already been answered. \
                 A challenge that can be answered twice is a password.",
            )
        })?;
        challenge.accept(Ceremony::Registration, unix_now())?;

        if let Some(bound) = challenge.user_handle()
            && bound != user.id()
        {
            return Err(Error::msg(
                "that challenge was issued to a different user. Finishing one account's \
                 ceremony against another's session is how a credential ends up attached to \
                 the wrong person.",
            ));
        }

        // The check phishing has to get past.
        client_data.expect_origin(self)?;

        let (attestation, auth_data_bytes) =
            parse_attestation_object(response.attestation_object())?;
        let auth_data = AuthenticatorData::parse(&auth_data_bytes)?;

        // A credential registered to another site must not land in this one.
        auth_data.expect_rp_id(self)?;
        auth_data.expect_user_present()?;
        auth_data.expect_user_verification(self.user_verification())?;

        let attested = auth_data.attested_credential().ok_or_else(|| {
            Error::msg(
                "the authenticator returned no attested credential data, so this registration \
                 carries no public key and nothing could ever be verified against it",
            )
        })?;

        // The offer and the verifier have to agree, or this registers a
        // credential that can never log in again.
        let algorithm = attested.key().algorithm();
        if !SignatureAlgorithm::offered().contains(&algorithm) {
            return Err(Error::msg(format!(
                "the authenticator returned a {} key, which was not offered",
                algorithm.cose_id()
            )));
        }

        // The signed bytes name the credential; the JSON envelope also does.
        // They have to be the same one, or the id stored is not the id the
        // authenticator attested to.
        if response.id() != attested.id() {
            return Err(Error::msg(
                "the credential id in the response does not match the one inside the signed \
                 authenticator data",
            ));
        }

        if credentials.find(attested.id()).await?.is_some() {
            return Err(Error::msg(
                "that credential id is already registered. Letting it through would give one \
                 id two keys, and an assertion names only the id.",
            ));
        }

        let credential = credentials
            .create(new_credential(attested, auth_data.sign_count(), user))
            .await?;

        Ok(Registration { credential, attestation, flags: auth_data.flags() })
    }
}

fn new_credential(
    attested: &AttestedCredential,
    sign_count: u32,
    user: &UserEntity,
) -> Credential {
    Credential::new(
        attested.id().to_vec(),
        attested.key().clone(),
        sign_count,
        user.id().to_vec(),
        *attested.aaguid(),
        unix_now(),
    )
}

/// `{"fmt": .., "attStmt": {..}, "authData": ..}`.
fn parse_attestation_object(bytes: &[u8]) -> Result<(AttestationStatement, Vec<u8>)> {
    let object = Cbor::parse(bytes)?;

    let format = object
        .get("fmt")
        .and_then(Cbor::as_text)
        .ok_or_else(|| Error::msg("the attestation object has no `fmt`"))?
        .to_string();
    let auth_data = object
        .get("authData")
        .and_then(Cbor::as_bytes)
        .ok_or_else(|| Error::msg("the attestation object has no `authData`"))?
        .to_vec();
    let statement = object
        .get("attStmt")
        .ok_or_else(|| Error::msg("the attestation object has no `attStmt`"))?;

    let entries = match statement {
        Cbor::Map(map) => map.len(),
        _ => return Err(Error::msg("the attestation object's `attStmt` is not a map")),
    };

    if format == "none" {
        // The `none` format's statement is defined as the empty map. Anything
        // inside it is bytes that travelled under a label saying there is
        // nothing to look at, and nothing here would ever look.
        if entries != 0 {
            return Err(Error::msg(format!(
                "the attestation format is `none` and the statement has {entries} entries. \
                 `none` means an empty statement; anything else is riding along unread."
            )));
        }
        return Ok((AttestationStatement::None, auth_data));
    }

    Ok((AttestationStatement::Unverified { format }, auth_data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ceremony::fake::{self, ASSERTION_FLAGS, Authenticator, REGISTRATION_FLAGS};
    use crate::challenge::MemoryChallengeStore;
    use crate::credential::MemoryCredentialStore;

    const ORIGIN: &str = "https://example.com";

    fn relying_party() -> RelyingParty {
        RelyingParty::new("example.com", "Example")
    }

    fn user() -> UserEntity {
        UserEntity::new(b"user-41".to_vec(), "ada", "Ada Lovelace")
    }

    /// Everything one registration needs, with the pieces left separate so a
    /// test can corrupt exactly one of them.
    struct Ceremony {
        rp: RelyingParty,
        device: Authenticator,
        challenges: MemoryChallengeStore,
        credentials: MemoryCredentialStore,
        challenge: Vec<u8>,
    }

    impl Ceremony {
        async fn start(rp: RelyingParty, device: Authenticator) -> Ceremony {
            let challenges = MemoryChallengeStore::new();
            let credentials = MemoryCredentialStore::new();
            let options =
                rp.start_registration(&user(), &challenges, &credentials).await.unwrap();
            let challenge = options.challenge().to_vec();

            Ceremony { rp, device, challenges, credentials, challenge }
        }

        async fn fresh() -> Ceremony {
            Ceremony::start(relying_party(), Authenticator::new(1)).await
        }

        fn respond(&self, flags: u8, rp_id: &str, origin: &str) -> RegistrationResponse {
            let auth_data = self.device.authenticator_data(rp_id, flags, 0);
            let client_data =
                self.device.client_data("webauthn.create", &self.challenge, origin);

            RegistrationResponse::new(
                self.device.credential_id.clone(),
                client_data,
                self.device.attestation_object("none", 0, &auth_data),
            )
        }

        fn healthy(&self) -> RegistrationResponse {
            self.respond(REGISTRATION_FLAGS, self.rp.id(), ORIGIN)
        }

        async fn finish(&self, response: &RegistrationResponse) -> Result<Registration> {
            self.rp
                .finish_registration(&user(), response, &self.challenges, &self.credentials)
                .await
        }
    }

    #[tokio::test]
    async fn a_registration_round_trips_and_stores_the_credential() {
        let ceremony = Ceremony::fresh().await;

        let registration = ceremony.finish(&ceremony.healthy()).await.unwrap();

        assert_eq!(registration.credential().id(), ceremony.device.credential_id.as_slice());
        assert_eq!(registration.credential().user_handle(), b"user-41");
        assert_eq!(registration.credential().aaguid(), &[1u8; 16]);
        assert_eq!(registration.attestation(), &AttestationStatement::None);
        assert!(registration.user_verified());

        let stored = ceremony.credentials.find(&ceremony.device.credential_id).await.unwrap();
        assert!(stored.is_some(), "the credential was not written");
    }

    #[tokio::test]
    async fn a_replayed_challenge_is_refused_because_answering_burned_it() {
        let ceremony = Ceremony::fresh().await;
        let response = ceremony.healthy();

        ceremony.finish(&response).await.unwrap();

        // Byte for byte the message that just worked.
        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("already been answered"), "got {error}");
    }

    #[tokio::test]
    async fn an_expired_challenge_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let stale = Challenge::issue_lasting(crate::challenge::Ceremony::Registration, 0)
            .bound_to(user().id().to_vec());
        let bytes = stale.bytes().to_vec();
        ceremony.challenges.store(stale).await.unwrap();

        // Nothing purges on take, so the expired challenge is really found and
        // really refused rather than merely missing.
        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            ceremony.device.client_data("webauthn.create", &bytes, ORIGIN),
            ceremony.device.attestation_object(
                "none",
                0,
                &ceremony.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0),
            ),
        );

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("expired"), "got {error}");
    }

    #[tokio::test]
    async fn a_challenge_issued_for_authentication_cannot_be_spent_on_registration() {
        let ceremony = Ceremony::fresh().await;
        let other = Challenge::issue(crate::challenge::Ceremony::Authentication);
        let bytes = other.bytes().to_vec();
        ceremony.challenges.store(other).await.unwrap();

        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            ceremony.device.client_data("webauthn.create", &bytes, ORIGIN),
            ceremony.device.attestation_object(
                "none",
                0,
                &ceremony.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0),
            ),
        );

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("issued for authentication"), "got {error}");
    }

    #[tokio::test]
    async fn a_wrong_origin_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let response = ceremony.respond(REGISTRATION_FLAGS, "example.com", "https://evil.test");

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("evil.test"), "got {error}");
    }

    #[tokio::test]
    async fn an_origin_that_is_only_a_prefix_of_the_right_one_is_refused() {
        // `https://example.com.evil.test` starts with the expected origin, and
        // a prefix comparison anywhere in this file would hand it the account.
        let ceremony = Ceremony::fresh().await;
        let response =
            ceremony.respond(REGISTRATION_FLAGS, "example.com", "https://example.com.evil.test");

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("exact"), "got {error}");
        assert!(ceremony.credentials.is_empty());
    }

    #[tokio::test]
    async fn a_wrong_relying_party_id_hash_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let response = ceremony.respond(REGISTRATION_FLAGS, "evil.test", ORIGIN);

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("different relying party"), "got {error}");
    }

    #[tokio::test]
    async fn user_present_not_set_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let response = ceremony.respond(0x40, "example.com", ORIGIN);

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("User Present"), "got {error}");
    }

    #[tokio::test]
    async fn user_verification_is_refused_when_required_and_not_performed() {
        let rp = relying_party().with_user_verification(UserVerification::Required);
        let ceremony = Ceremony::start(rp, Authenticator::new(1)).await;
        let response = ceremony.respond(0x01 | 0x40, "example.com", ORIGIN);

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("user verification was required"), "got {error}");
    }

    #[tokio::test]
    async fn the_wrong_client_data_type_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let auth_data = ceremony.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0);
        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            // The type an assertion carries, posted to the registration route.
            ceremony.device.client_data("webauthn.get", &ceremony.challenge, ORIGIN),
            ceremony.device.attestation_object("none", 0, &auth_data),
        );

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("webauthn.get"), "got {error}");
    }

    #[tokio::test]
    async fn a_tampered_client_data_json_is_refused() {
        // One byte of the challenge, changed. The bytes no longer name a
        // challenge this server issued.
        let ceremony = Ceremony::fresh().await;
        let mut challenge = ceremony.challenge.clone();
        challenge[0] ^= 1;

        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            ceremony.device.client_data("webauthn.create", &challenge, ORIGIN),
            ceremony.device.attestation_object(
                "none",
                0,
                &ceremony.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0),
            ),
        );

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("not one this server issued"), "got {error}");
    }

    #[tokio::test]
    async fn a_credential_id_registered_twice_is_refused() {
        let ceremony = Ceremony::fresh().await;
        ceremony.finish(&ceremony.healthy()).await.unwrap();

        // A second, honest ceremony from a device that reuses the id.
        let second = Ceremony {
            rp: relying_party(),
            device: Authenticator::new(2)
                .with_credential_id(ceremony.device.credential_id.clone()),
            challenges: ceremony.challenges,
            credentials: ceremony.credentials,
            challenge: Vec::new(),
        };
        let challenge = Challenge::issue(crate::challenge::Ceremony::Registration)
            .bound_to(user().id().to_vec());
        let bytes = challenge.bytes().to_vec();
        second.challenges.store(challenge).await.unwrap();

        let response = RegistrationResponse::new(
            second.device.credential_id.clone(),
            second.device.client_data("webauthn.create", &bytes, ORIGIN),
            second.device.attestation_object(
                "none",
                0,
                &second.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0),
            ),
        );

        let error = second.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("already registered"), "got {error}");
    }

    #[tokio::test]
    async fn a_registration_finished_against_another_users_session_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let response = ceremony.healthy();

        let error = ceremony
            .rp
            .finish_registration(
                &UserEntity::new(b"user-99".to_vec(), "eve", "Eve"),
                &response,
                &ceremony.challenges,
                &ceremony.credentials,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("different user"), "got {error}");
    }

    #[tokio::test]
    async fn a_raw_id_that_disagrees_with_the_signed_data_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let mut response = ceremony.healthy();
        response.id = b"some other credential".to_vec();

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("does not match"), "got {error}");
    }

    #[tokio::test]
    async fn an_authenticator_that_returns_no_credential_is_refused() {
        // The AT flag clear: a well-formed message carrying no public key.
        let ceremony = Ceremony::fresh().await;
        let response = ceremony.respond(ASSERTION_FLAGS, "example.com", ORIGIN);

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("no attested credential data"), "got {error}");
    }

    #[tokio::test]
    async fn an_attestation_statement_is_reported_as_unverified_and_never_as_checked() {
        let ceremony = Ceremony::fresh().await;
        let auth_data = ceremony.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0);
        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            ceremony.device.client_data("webauthn.create", &ceremony.challenge, ORIGIN),
            ceremony.device.attestation_object("packed", 2, &auth_data),
        );

        let registration = ceremony.finish(&response).await.unwrap();

        assert_eq!(registration.attestation().format(), "packed");
        assert!(!registration.attestation().was_verified());
        assert!(matches!(
            registration.attestation(),
            AttestationStatement::Unverified { .. }
        ));
    }

    #[tokio::test]
    async fn a_statement_smuggled_under_fmt_none_is_refused() {
        let ceremony = Ceremony::fresh().await;
        let auth_data = ceremony.device.authenticator_data("example.com", REGISTRATION_FLAGS, 0);
        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            ceremony.device.client_data("webauthn.create", &ceremony.challenge, ORIGIN),
            ceremony.device.attestation_object("none", 1, &auth_data),
        );

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("riding along unread"), "got {error}");
    }

    #[tokio::test]
    async fn an_attestation_object_missing_a_field_is_an_error_and_never_a_panic() {
        let ceremony = Ceremony::fresh().await;

        // {"fmt": "none", "attStmt": {}} — no authData at all.
        let mut object = Vec::new();
        fake::map(2, &mut object);
        fake::text("fmt", &mut object);
        fake::text("none", &mut object);
        fake::text("attStmt", &mut object);
        fake::map(0, &mut object);

        let response = RegistrationResponse::new(
            ceremony.device.credential_id.clone(),
            ceremony.device.client_data("webauthn.create", &ceremony.challenge, ORIGIN),
            object,
        );

        let error = ceremony.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("no `authData`"), "got {error}");
    }

    #[tokio::test]
    async fn the_options_offer_only_algorithms_the_verifier_can_honour() {
        // An offer the verifier cannot check registers a credential that can
        // never log in again.
        let challenges = MemoryChallengeStore::new();
        let credentials = MemoryCredentialStore::new();
        let options = relying_party()
            .start_registration(&user(), &challenges, &credentials)
            .await
            .unwrap();

        let json = options.json();
        let offered = json.get("pubKeyCredParams").and_then(Json::as_array).unwrap();

        assert_eq!(offered.len(), SignatureAlgorithm::offered().len());
        for (entry, algorithm) in offered.iter().zip(SignatureAlgorithm::offered()) {
            assert_eq!(entry.get("alg").and_then(Json::as_i64), Some(algorithm.cose_id()));
            assert_eq!(entry.get("type").and_then(Json::as_str), Some("public-key"));
        }

        assert_eq!(json.get("rp.id").and_then(Json::as_str), Some("example.com"));
        assert_eq!(json.get("attestation").and_then(Json::as_str), Some("none"));
        assert_eq!(
            json.get("challenge").and_then(Json::as_str).map(str::len),
            Some(base64::encode_url(options.challenge()).len())
        );
        assert_eq!(challenges.len(), 1, "the challenge was not stored");
    }

    #[tokio::test]
    async fn the_options_exclude_the_credentials_the_user_already_has() {
        let ceremony = Ceremony::fresh().await;
        ceremony.finish(&ceremony.healthy()).await.unwrap();

        let options = ceremony
            .rp
            .start_registration(&user(), &ceremony.challenges, &ceremony.credentials)
            .await
            .unwrap();

        assert_eq!(
            options.exclude_credentials(),
            std::slice::from_ref(&ceremony.device.credential_id)
        );

        let json = options.json();
        let excluded = json.get("excludeCredentials").and_then(Json::as_array).unwrap();
        assert_eq!(
            excluded[0].get("id").and_then(Json::as_str),
            Some(base64::encode_url(&ceremony.device.credential_id).as_str())
        );
    }

    #[test]
    fn a_response_can_be_read_from_the_json_a_browser_posts() {
        let body = format!(
            "{{\"id\":\"{id}\",\"rawId\":\"{id}\",\"type\":\"public-key\",\"response\":\
             {{\"clientDataJSON\":\"{client}\",\"attestationObject\":\"{attestation}\"}}}}",
            id = base64::encode_url(b"credential"),
            client = base64::encode_url(b"{}"),
            attestation = base64::encode_url(b"\xa0"),
        );

        let response = RegistrationResponse::parse(&body).unwrap();

        assert_eq!(response.id(), b"credential");
        assert_eq!(response.client_data_json(), b"{}");
        assert_eq!(response.attestation_object(), b"\xa0");

        assert!(RegistrationResponse::parse("{}").is_err());
        assert!(RegistrationResponse::parse("not json").is_err());
    }
}
