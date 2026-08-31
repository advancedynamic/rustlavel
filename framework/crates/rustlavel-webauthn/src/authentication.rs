//! Authentication: the ceremony that proves the private key is still there.
//!
//! The server issues a challenge; the authenticator signs
//! `authenticatorData || SHA-256(clientDataJSON)` with the key registered
//! earlier. That message is the whole design: the client data carries the
//! origin the browser was actually at, and the authenticator refuses to sign
//! for the wrong one — which is why a passkey cannot be phished the way a
//! password or a one-time code can.
//!
//! Everything [`RelyingParty::finish_authentication`] checks is a way that
//! guarantee gets lost if the check is missing.

use crate::ceremony::{
    AuthenticatorData, AuthenticatorFlags, ClientData, RelyingParty, UserVerification,
};
use crate::challenge::{Ceremony, Challenge, ChallengeStore};
use crate::credential::{Credential, CredentialStore};
use crate::{Error, Json, Result};
use rustlavel_auth::{base64, unix_now};

/// What the server sends to `navigator.credentials.get()`.
#[derive(Debug, Clone)]
pub struct PublicKeyCredentialRequestOptions {
    rp_id: String,
    challenge: Vec<u8>,
    timeout_ms: u64,
    user_verification: UserVerification,
    allow: Vec<Vec<u8>>,
}

impl PublicKeyCredentialRequestOptions {
    /// Name the credentials that may answer.
    ///
    /// An empty list is the passkey case: the authenticator offers whichever
    /// discoverable credential it holds for this site, and the user picks. A
    /// non-empty one is needed for credentials that are not discoverable,
    /// which cannot be found without being named.
    pub fn allowing<I: IntoIterator<Item = Vec<u8>>>(
        mut self,
        credential_ids: I,
    ) -> PublicKeyCredentialRequestOptions {
        self.allow = credential_ids.into_iter().collect();
        self
    }

    pub fn challenge(&self) -> &[u8] {
        &self.challenge
    }

    pub fn allow_credentials(&self) -> &[Vec<u8>] {
        &self.allow
    }

    /// The `publicKey` object, ready to be serialised into a response body.
    pub fn json(&self) -> Json {
        let allow: Vec<Json> = self
            .allow
            .iter()
            .map(|id| {
                Json::object([
                    ("type", Json::String("public-key".into())),
                    ("id", Json::String(base64::encode_url(id))),
                ])
            })
            .collect();

        Json::object([
            ("rpId", Json::String(self.rp_id.clone())),
            ("challenge", Json::String(base64::encode_url(&self.challenge))),
            ("timeout", Json::Number(self.timeout_ms as f64)),
            ("userVerification", Json::String(self.user_verification.as_str().into())),
            ("allowCredentials", Json::Array(allow)),
        ])
    }
}

/// Compact JSON, so `format!("{options}")` is a response body.
impl std::fmt::Display for PublicKeyCredentialRequestOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.json())
    }
}

/// What the browser posts back after `navigator.credentials.get()`.
#[derive(Debug, Clone)]
pub struct AuthenticationResponse {
    id: Vec<u8>,
    client_data_json: Vec<u8>,
    authenticator_data: Vec<u8>,
    signature: Vec<u8>,
    user_handle: Option<Vec<u8>>,
}

impl AuthenticationResponse {
    pub fn new(
        id: impl Into<Vec<u8>>,
        client_data_json: impl Into<Vec<u8>>,
        authenticator_data: impl Into<Vec<u8>>,
        signature: impl Into<Vec<u8>>,
    ) -> AuthenticationResponse {
        AuthenticationResponse {
            id: id.into(),
            client_data_json: client_data_json.into(),
            authenticator_data: authenticator_data.into(),
            signature: signature.into(),
            user_handle: None,
        }
    }

    /// The user handle a discoverable credential hands back.
    pub fn with_user_handle(mut self, user_handle: impl Into<Vec<u8>>) -> AuthenticationResponse {
        self.user_handle = Some(user_handle.into());
        self
    }

    /// Read the shape a browser produces from `PublicKeyCredential.toJSON()`.
    pub fn from_json(value: &Json) -> Result<AuthenticationResponse> {
        let response = AuthenticationResponse {
            id: crate::ceremony::base64_field(value, "rawId")?,
            client_data_json: crate::ceremony::base64_field(value, "response.clientDataJSON")?,
            authenticator_data: crate::ceremony::base64_field(
                value,
                "response.authenticatorData",
            )?,
            signature: crate::ceremony::base64_field(value, "response.signature")?,
            user_handle: None,
        };

        // Optional, and null for a credential that is not discoverable.
        match value.get("response.userHandle") {
            Some(handle) if !handle.is_null() => {
                Ok(response.with_user_handle(crate::ceremony::base64_field(
                    value,
                    "response.userHandle",
                )?))
            }
            _ => Ok(response),
        }
    }

    /// The same, from the request body directly.
    pub fn parse(body: &str) -> Result<AuthenticationResponse> {
        AuthenticationResponse::from_json(&Json::parse(body)?)
    }

    pub fn id(&self) -> &[u8] {
        &self.id
    }

    pub fn client_data_json(&self) -> &[u8] {
        &self.client_data_json
    }

    pub fn authenticator_data(&self) -> &[u8] {
        &self.authenticator_data
    }

    pub fn signature(&self) -> &[u8] {
        &self.signature
    }

    pub fn user_handle(&self) -> Option<&[u8]> {
        self.user_handle.as_deref()
    }
}

/// A completed authentication: who signed in, and what the device reported.
#[derive(Debug, Clone)]
pub struct Authentication {
    credential_id: Vec<u8>,
    user_handle: Vec<u8>,
    sign_count: u32,
    flags: AuthenticatorFlags,
}

impl Authentication {
    pub fn credential_id(&self) -> &[u8] {
        &self.credential_id
    }

    /// The user handle from the *stored* credential, never the one the client
    /// sent. The client's copy is checked against this, not trusted over it.
    pub fn user_handle(&self) -> &[u8] {
        &self.user_handle
    }

    pub fn sign_count(&self) -> u32 {
        self.sign_count
    }

    pub fn flags(&self) -> AuthenticatorFlags {
        self.flags
    }

    pub fn user_verified(&self) -> bool {
        self.flags.user_verified()
    }
}

impl RelyingParty {
    /// Build request options around a challenge you have already issued.
    pub fn request_options(&self, challenge: &Challenge) -> PublicKeyCredentialRequestOptions {
        PublicKeyCredentialRequestOptions {
            rp_id: self.id().to_string(),
            challenge: challenge.bytes().to_vec(),
            timeout_ms: self.timeout_ms(),
            user_verification: self.user_verification(),
            allow: Vec::new(),
        }
    }

    /// Issue and store a challenge for a login that names no user.
    ///
    /// The passkey case: `allowCredentials` is empty, the authenticator offers
    /// what it has, and the account is discovered from the user handle it
    /// returns. Nothing is revealed about which accounts exist, which is the
    /// other reason to prefer it.
    pub async fn start_authentication(
        &self,
        challenges: &dyn ChallengeStore,
    ) -> Result<PublicKeyCredentialRequestOptions> {
        let challenge = Challenge::issue(Ceremony::Authentication);
        let options = self.request_options(&challenge);
        challenges.store(challenge).await?;

        Ok(options)
    }

    /// Issue and store a challenge for one known user.
    ///
    /// Use this when the user has already typed a username. The challenge is
    /// bound to their handle, so an assertion from somebody else's credential
    /// cannot finish it, and `allowCredentials` lists what they registered so
    /// a non-discoverable credential can still answer.
    pub async fn start_authentication_for(
        &self,
        user_handle: &[u8],
        challenges: &dyn ChallengeStore,
        credentials: &dyn CredentialStore,
    ) -> Result<PublicKeyCredentialRequestOptions> {
        let challenge =
            Challenge::issue(Ceremony::Authentication).bound_to(user_handle.to_vec());
        let options = self.request_options(&challenge);
        challenges.store(challenge).await?;

        let theirs = credentials.find_for_user(user_handle).await?;
        Ok(options.allowing(theirs.into_iter().map(|credential| credential.id().to_vec())))
    }

    /// Check an assertion, and advance the credential's sign counter.
    ///
    /// On success the credential's `sign_count` and `last_used_at` have been
    /// written. On any error nothing was written, and the challenge has been
    /// spent either way.
    pub async fn finish_authentication(
        &self,
        response: &AuthenticationResponse,
        challenges: &dyn ChallengeStore,
        credentials: &dyn CredentialStore,
    ) -> Result<Authentication> {
        let credential = credentials.find(response.id()).await?.ok_or_else(|| {
            Error::msg(
                "no credential is registered under that id, so there is no key to check this \
                 assertion against",
            )
        })?;

        let client_data = ClientData::parse(response.client_data_json())?;

        // A registration response is not evidence of a login.
        client_data.expect_kind(Ceremony::Authentication.client_data_type())?;

        // Taking it out is what makes it single-use. Without this every
        // captured assertion is a reusable credential.
        let key = base64::encode_url(client_data.challenge());
        let challenge = challenges.take(&key).await?.ok_or_else(|| {
            Error::msg(
                "that challenge is not one this server issued, or it has already been answered. \
                 A challenge that can be answered twice is a password.",
            )
        })?;
        challenge.accept(Ceremony::Authentication, unix_now())?;

        if let Some(bound) = challenge.user_handle()
            && bound != credential.user_handle()
        {
            return Err(Error::msg(
                "that challenge was issued for a different user. An assertion from one account \
                 must not finish another account's login.",
            ));
        }

        // The check phishing has to get past.
        client_data.expect_origin(self)?;

        let auth_data = AuthenticatorData::parse(response.authenticator_data())?;
        auth_data.expect_rp_id(self)?;
        auth_data.expect_user_present()?;
        auth_data.expect_user_verification(self.user_verification())?;

        // A discoverable credential says whose it is. The stored row already
        // knows, so this is a check and not a lookup: believing the client
        // here would let an assertion name any account it liked.
        if let Some(claimed) = response.user_handle()
            && claimed != credential.user_handle()
        {
            return Err(Error::msg(
                "the assertion claims a user handle that does not belong to that credential",
            ));
        }

        // Everything above is bookkeeping; this is the proof.
        let mut message = response.authenticator_data().to_vec();
        message.extend_from_slice(&crate::ceremony::sha256(response.client_data_json()));

        if !credential.key().verify(&message, response.signature()) {
            return Err(Error::msg(
                "the signature does not verify under the registered public key. Either the \
                 signer was not this credential, or the authenticator data or client data was \
                 changed after it was signed.",
            ));
        }

        check_sign_count(&credential, auth_data.sign_count())?;

        let now = unix_now();
        credentials.record_use(credential.id(), auth_data.sign_count(), now).await?;

        Ok(Authentication {
            credential_id: credential.id().to_vec(),
            user_handle: credential.user_handle().to_vec(),
            sign_count: auth_data.sign_count(),
            flags: auth_data.flags(),
        })
    }
}

/// The one check that looks for a copy of the key rather than a forgery.
///
/// An authenticator that counts increments the counter on every assertion. If
/// a value arrives that is not greater than the last one stored, two things
/// have been signing with the same key — which is what a cloned authenticator
/// looks like from here, and it is the only signal there is.
///
/// A counter of zero on **both** sides is not a failure and must not be
/// treated as one: it means the authenticator does not keep a counter, which
/// is normal and expected for a passkey synced across devices, since there is
/// no single device left to count. Refusing that would refuse every modern
/// credential.
///
/// A credential that counted before and reports zero now *is* refused. The
/// clause above is about an authenticator that never counted, not about one
/// that stopped.
fn check_sign_count(credential: &Credential, reported: u32) -> Result<()> {
    let stored = credential.sign_count();

    if stored != 0 && reported <= stored {
        return Err(Error::msg(format!(
            "the sign counter went backwards: this credential last reported {stored} and now \
             reports {reported}. Two authenticators are signing with the same private key, \
             which means it has been copied off the device that was supposed to hold it."
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ceremony::fake::{ASSERTION_FLAGS, Authenticator, REGISTRATION_FLAGS};
    use crate::ceremony::UserEntity;
    use crate::challenge::MemoryChallengeStore;
    use crate::credential::MemoryCredentialStore;
    use crate::registration::RegistrationResponse;

    const ORIGIN: &str = "https://example.com";

    fn relying_party() -> RelyingParty {
        RelyingParty::new("example.com", "Example")
    }

    fn user() -> UserEntity {
        UserEntity::new(b"user-41".to_vec(), "ada", "Ada Lovelace")
    }

    /// A registered credential and a challenge waiting to be answered, with
    /// the pieces left separate so a test can corrupt exactly one of them.
    struct Session {
        rp: RelyingParty,
        device: Authenticator,
        challenges: MemoryChallengeStore,
        credentials: MemoryCredentialStore,
        challenge: Vec<u8>,
    }

    impl Session {
        async fn fresh() -> Session {
            Session::with(relying_party(), Authenticator::new(1)).await
        }

        async fn with(rp: RelyingParty, device: Authenticator) -> Session {
            let challenges = MemoryChallengeStore::new();
            let credentials = MemoryCredentialStore::new();

            // Register for real, so the key an assertion is checked against is
            // the key the authenticator actually holds.
            let options =
                rp.start_registration(&user(), &challenges, &credentials).await.unwrap();
            let auth_data = device.authenticator_data(rp.id(), REGISTRATION_FLAGS, 0);
            let registration = RegistrationResponse::new(
                device.credential_id.clone(),
                device.client_data("webauthn.create", options.challenge(), ORIGIN),
                device.attestation_object("none", 0, &auth_data),
            );
            rp.finish_registration(&user(), &registration, &challenges, &credentials)
                .await
                .unwrap();

            let request = rp.start_authentication(&challenges).await.unwrap();
            let challenge = request.challenge().to_vec();

            Session { rp, device, challenges, credentials, challenge }
        }

        fn assert_with(
            &self,
            flags: u8,
            rp_id: &str,
            origin: &str,
            sign_count: u32,
            challenge: &[u8],
        ) -> AuthenticationResponse {
            let auth_data = self.device.authenticator_data(rp_id, flags, sign_count);
            let client_data = self.device.client_data("webauthn.get", challenge, origin);
            let signature = self.device.sign(&auth_data, &client_data);

            AuthenticationResponse::new(
                self.device.credential_id.clone(),
                client_data,
                auth_data,
                signature,
            )
        }

        fn healthy(&self) -> AuthenticationResponse {
            self.assert_with(ASSERTION_FLAGS, self.rp.id(), ORIGIN, 0, &self.challenge)
        }

        async fn finish(&self, response: &AuthenticationResponse) -> Result<Authentication> {
            self.rp.finish_authentication(response, &self.challenges, &self.credentials).await
        }
    }

    #[tokio::test]
    async fn an_assertion_round_trips_and_records_the_login() {
        let session = Session::fresh().await;

        let authentication = session.finish(&session.healthy()).await.unwrap();

        assert_eq!(authentication.credential_id(), session.device.credential_id.as_slice());
        assert_eq!(authentication.user_handle(), b"user-41");
        assert!(authentication.user_verified());

        let stored =
            session.credentials.find(&session.device.credential_id).await.unwrap().unwrap();
        assert!(stored.last_used_at().is_some(), "the login was not recorded");
    }

    #[tokio::test]
    async fn a_signature_from_a_different_credential_is_refused() {
        // A genuine assertion, correctly formed, signed by another device.
        let session = Session::fresh().await;
        let intruder =
            Authenticator::new(2).with_credential_id(session.device.credential_id.clone());

        let auth_data = intruder.authenticator_data("example.com", ASSERTION_FLAGS, 0);
        let client_data = intruder.client_data("webauthn.get", &session.challenge, ORIGIN);
        let response = AuthenticationResponse::new(
            session.device.credential_id.clone(),
            client_data.clone(),
            auth_data.clone(),
            intruder.sign(&auth_data, &client_data),
        );

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("does not verify"), "got {error}");
    }

    #[tokio::test]
    async fn a_tampered_authenticator_data_is_refused() {
        // The counter, raised by one after signing. Nothing else changed.
        let session = Session::fresh().await;
        let mut response = session.healthy();
        response.authenticator_data[36] = 1;

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("does not verify"), "got {error}");
    }

    #[tokio::test]
    async fn a_tampered_client_data_json_is_refused() {
        // The origin rewritten to the expected one after the browser signed
        // for somewhere else: the check that catches it is the signature, not
        // the string comparison.
        let session = Session::fresh().await;
        let auth_data = session.device.authenticator_data("example.com", ASSERTION_FLAGS, 0);
        let signed =
            session.device.client_data("webauthn.get", &session.challenge, "https://evil.test");
        let signature = session.device.sign(&auth_data, &signed);
        let presented =
            session.device.client_data("webauthn.get", &session.challenge, ORIGIN);

        let response = AuthenticationResponse::new(
            session.device.credential_id.clone(),
            presented,
            auth_data,
            signature,
        );

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("does not verify"), "got {error}");
    }

    #[tokio::test]
    async fn a_regressed_sign_counter_is_refused_as_a_cloned_authenticator() {
        let session = Session::fresh().await;

        // Get the stored counter up to five with a genuine assertion.
        let response =
            session.assert_with(ASSERTION_FLAGS, "example.com", ORIGIN, 5, &session.challenge);
        session.finish(&response).await.unwrap();

        let stored =
            session.credentials.find(&session.device.credential_id).await.unwrap().unwrap();
        assert_eq!(stored.sign_count(), 5);

        // Now a second device, holding a copy of the key, at a lower count.
        let request = session.rp.start_authentication(&session.challenges).await.unwrap();
        let cloned =
            session.assert_with(ASSERTION_FLAGS, "example.com", ORIGIN, 3, request.challenge());

        let error = session.finish(&cloned).await.unwrap_err().to_string();
        assert!(error.contains("went backwards"), "got {error}");
        assert!(error.contains("copied off the device"), "got {error}");
    }

    #[tokio::test]
    async fn a_sign_counter_of_zero_on_both_sides_is_normal_and_not_a_clone() {
        // Synced passkeys keep no counter: there is no single device to count.
        // Refusing this would refuse every modern credential.
        let session = Session::fresh().await;

        session.finish(&session.healthy()).await.unwrap();

        let request = session.rp.start_authentication(&session.challenges).await.unwrap();
        let again =
            session.assert_with(ASSERTION_FLAGS, "example.com", ORIGIN, 0, request.challenge());

        assert!(session.finish(&again).await.is_ok(), "a passkey was mistaken for a clone");
    }

    #[tokio::test]
    async fn a_replayed_assertion_is_refused_because_answering_burned_the_challenge() {
        let session = Session::fresh().await;
        let response = session.healthy();

        session.finish(&response).await.unwrap();

        // Byte for byte the message that just worked.
        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("already been answered"), "got {error}");
    }

    #[tokio::test]
    async fn an_expired_challenge_is_refused() {
        let session = Session::fresh().await;
        let stale = Challenge::issue_lasting(Ceremony::Authentication, 0);
        let bytes = stale.bytes().to_vec();
        session.challenges.store(stale).await.unwrap();

        let response = session.assert_with(ASSERTION_FLAGS, "example.com", ORIGIN, 0, &bytes);

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("expired"), "got {error}");
    }

    #[tokio::test]
    async fn a_registration_challenge_cannot_be_spent_on_a_login() {
        let session = Session::fresh().await;
        let other = Challenge::issue(Ceremony::Registration);
        let bytes = other.bytes().to_vec();
        session.challenges.store(other).await.unwrap();

        let response = session.assert_with(ASSERTION_FLAGS, "example.com", ORIGIN, 0, &bytes);

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("issued for registration"), "got {error}");
    }

    #[tokio::test]
    async fn a_wrong_origin_is_refused() {
        let session = Session::fresh().await;
        let response = session.assert_with(
            ASSERTION_FLAGS,
            "example.com",
            "https://evil.test",
            0,
            &session.challenge,
        );

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("evil.test"), "got {error}");
    }

    #[tokio::test]
    async fn an_origin_that_is_only_a_prefix_of_the_right_one_is_refused() {
        // A genuine signature over a genuine challenge, collected at
        // `https://example.com.evil.test`. Only the exact origin check refuses
        // it, and this is the attack the whole protocol exists to stop.
        let session = Session::fresh().await;
        let response = session.assert_with(
            ASSERTION_FLAGS,
            "example.com",
            "https://example.com.evil.test",
            0,
            &session.challenge,
        );

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("exact"), "got {error}");
    }

    #[tokio::test]
    async fn a_wrong_relying_party_id_hash_is_refused() {
        let session = Session::fresh().await;
        let response =
            session.assert_with(ASSERTION_FLAGS, "evil.test", ORIGIN, 0, &session.challenge);

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("different relying party"), "got {error}");
    }

    #[tokio::test]
    async fn user_present_not_set_is_refused() {
        let session = Session::fresh().await;
        let response = session.assert_with(0x00, "example.com", ORIGIN, 0, &session.challenge);

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("User Present"), "got {error}");
    }

    #[tokio::test]
    async fn user_verification_is_refused_when_required_and_not_performed() {
        let rp = relying_party().with_user_verification(UserVerification::Required);
        let session = Session::with(rp, Authenticator::new(1)).await;
        let response = session.assert_with(0x01, "example.com", ORIGIN, 0, &session.challenge);

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("user verification was required"), "got {error}");
    }

    #[tokio::test]
    async fn a_user_handle_that_belongs_to_somebody_else_is_refused() {
        let session = Session::fresh().await;
        let response = session.healthy().with_user_handle(b"user-99".to_vec());

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("does not belong"), "got {error}");
    }

    #[tokio::test]
    async fn an_unknown_credential_id_is_refused() {
        let session = Session::fresh().await;
        let mut response = session.healthy();
        response.id = b"never registered".to_vec();

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("no credential is registered"), "got {error}");
    }

    #[tokio::test]
    async fn a_login_bound_to_one_user_cannot_be_finished_by_another() {
        let session = Session::fresh().await;
        let bound = session
            .rp
            .start_authentication_for(b"user-99", &session.challenges, &session.credentials)
            .await
            .unwrap();

        let response =
            session.assert_with(ASSERTION_FLAGS, "example.com", ORIGIN, 0, bound.challenge());

        let error = session.finish(&response).await.unwrap_err().to_string();
        assert!(error.contains("issued for a different user"), "got {error}");
    }

    #[tokio::test]
    async fn a_login_started_for_a_user_names_the_credentials_they_have() {
        let session = Session::fresh().await;
        let options = session
            .rp
            .start_authentication_for(b"user-41", &session.challenges, &session.credentials)
            .await
            .unwrap();

        assert_eq!(
            options.allow_credentials(),
            std::slice::from_ref(&session.device.credential_id)
        );

        let json = options.json();
        assert_eq!(json.get("rpId").and_then(Json::as_str), Some("example.com"));
        assert_eq!(
            json.get("allowCredentials.0.id").and_then(Json::as_str),
            Some(base64::encode_url(&session.device.credential_id).as_str())
        );

        // The usernameless form names nobody, so it leaks nothing about which
        // accounts exist.
        let anonymous = session.rp.start_authentication(&session.challenges).await.unwrap();
        assert!(anonymous.allow_credentials().is_empty());
        let json = anonymous.json();
        assert!(json.get("allowCredentials").and_then(Json::as_array).unwrap().is_empty());
    }

    #[test]
    fn a_response_can_be_read_from_the_json_a_browser_posts() {
        let body = format!(
            "{{\"id\":\"{id}\",\"rawId\":\"{id}\",\"type\":\"public-key\",\"response\":\
             {{\"clientDataJSON\":\"{client}\",\"authenticatorData\":\"{auth}\",\
             \"signature\":\"{signature}\",\"userHandle\":\"{handle}\"}}}}",
            id = base64::encode_url(b"credential"),
            client = base64::encode_url(b"{}"),
            auth = base64::encode_url(b"authdata"),
            signature = base64::encode_url(b"sig"),
            handle = base64::encode_url(b"user-41"),
        );

        let response = AuthenticationResponse::parse(&body).unwrap();

        assert_eq!(response.id(), b"credential");
        assert_eq!(response.authenticator_data(), b"authdata");
        assert_eq!(response.signature(), b"sig");
        assert_eq!(response.user_handle(), Some(b"user-41".as_slice()));

        // A credential that is not discoverable sends null, which is not a
        // handle of zero bytes.
        let without = body.replace(
            &format!("\"userHandle\":\"{}\"", base64::encode_url(b"user-41")),
            "\"userHandle\":null",
        );
        assert_eq!(AuthenticationResponse::parse(&without).unwrap().user_handle(), None);

        assert!(AuthenticationResponse::parse("{}").is_err());
    }
}
