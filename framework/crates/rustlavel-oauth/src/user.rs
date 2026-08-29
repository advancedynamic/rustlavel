//! One shape of user, out of five differently-shaped userinfo responses.
//!
//! There is no standard here worth relying on. OpenID Connect says `sub` and
//! `picture`; GitHub says `id` and `avatar_url` and sends the id as a number;
//! Microsoft Graph says `displayName` and often has no `mail` at all; Discord
//! sends an image hash rather than a URL. Normalising once, here, is what keeps
//! an application's sign-in handler from growing a match on the provider name.

use crate::provider::{Provider, UserMap};
use rustlavel_auth::Authenticatable;
use rustlavel_core::Json;

/// A user as one provider describes them.
///
/// Everything except the id is optional, and that is not defensiveness — it is
/// what these endpoints actually return. GitHub omits the email unless the user
/// made it public *and* the token carries `user:email`; Microsoft Graph has no
/// avatar URL to give. An application that requires an email has to ask for it
/// itself rather than assume one arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct SocialUser {
    /// Which provider said this. Part of the identity, not a label: id `1` at
    /// GitHub and id `1` at GitLab are different people.
    pub provider: String,
    /// The provider's own identifier, stable across name and email changes —
    /// which is why accounts are linked on this and never on the email.
    pub id: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar: Option<String>,
    /// The whole response, so a provider-specific field is available without
    /// this crate having to know about it.
    pub raw: Json,
}

impl SocialUser {
    /// Read a userinfo response through a provider's field map.
    ///
    /// `None` when there is no id: a profile that cannot be identified cannot
    /// be linked to an account, and guessing at the email instead is how two
    /// people end up sharing one login.
    pub fn from_json(provider: &str, map: &UserMap, raw: &Json) -> Option<SocialUser> {
        let id = first(raw, &map.id)?;
        let avatar = match (&map.avatar_template, first(raw, &map.avatar)) {
            (Some(template), Some(value)) => {
                Some(template.replace("{id}", &id).replace("{avatar}", &value))
            }
            (None, avatar) => avatar,
            // A template with nothing to put in it: the user has no picture,
            // and rendering the template with an empty hash would be a link to
            // a 404 rather than an honest absence.
            (Some(_), None) => None,
        };

        Some(SocialUser {
            provider: provider.to_string(),
            id,
            name: first(raw, &map.name),
            email: first(raw, &map.email),
            avatar,
            raw: raw.clone(),
        })
    }

    pub fn from_provider(provider: &Provider, raw: &Json) -> Option<SocialUser> {
        SocialUser::from_json(&provider.name, &provider.map, raw)
    }

    /// `github:12345` — what belongs in a `social_accounts` row, and what
    /// [`Authenticatable`] hands the session.
    ///
    /// Qualified by provider because the ids are only unique within one: an
    /// application that stored the bare id would let a GitLab account with the
    /// right number sign in as a GitHub user.
    pub fn qualified_id(&self) -> String {
        format!("{}:{}", self.provider, self.id)
    }
}

impl Authenticatable for SocialUser {
    fn auth_identifier(&self) -> String {
        self.qualified_id()
    }
}

/// The first of `paths` that holds something usable.
fn first(raw: &Json, paths: &[String]) -> Option<String> {
    paths.iter().find_map(|path| raw.get(path).and_then(as_text))
}

/// A JSON value as a profile field.
///
/// `null` and `""` are both absences: GitHub sends `"email": null` for a
/// private address, and an empty display name is not a name. Numbers are
/// accepted because GitHub and GitLab send the user id as one, and rendering
/// that as `12345.0` would produce an identifier that never matches the row it
/// was stored against.
fn as_text(value: &Json) -> Option<String> {
    match value {
        Json::String(text) if !text.is_empty() => Some(text.clone()),
        Json::Number(number) if number.fract() == 0.0 && number.abs() < 9e15 => {
            Some(format!("{}", *number as i64))
        }
        Json::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(provider: Provider, body: &str) -> SocialUser {
        SocialUser::from_provider(&provider, &Json::parse(body).unwrap())
            .expect("the fixture has an id")
    }

    #[test]
    fn a_numeric_id_becomes_the_integer_and_not_a_float() {
        // `12345.0` would never match the `github:12345` already in the
        // database, and the bug would only appear for existing users.
        let mapped = user(Provider::github(), r#"{"id":12345,"login":"ada"}"#);

        assert_eq!(mapped.id, "12345");
        assert_eq!(mapped.qualified_id(), "github:12345");
    }

    #[test]
    fn github_falls_back_to_the_handle_when_there_is_no_real_name() {
        let named = user(Provider::github(), r#"{"id":1,"name":"Ada Lovelace","login":"ada"}"#);
        assert_eq!(named.name.as_deref(), Some("Ada Lovelace"));

        let anonymous = user(Provider::github(), r#"{"id":1,"name":null,"login":"ada"}"#);
        assert_eq!(anonymous.name.as_deref(), Some("ada"));
    }

    #[test]
    fn a_null_email_is_an_absence_rather_than_the_string_null() {
        // GitHub sends this whenever the address is private, which is the
        // default. An application that requires an email must ask for it.
        let mapped = user(Provider::github(), r#"{"id":1,"login":"ada","email":null}"#);

        assert_eq!(mapped.email, None);
    }

    #[test]
    fn discords_avatar_hash_is_assembled_into_a_url() {
        let mapped = user(
            Provider::discord(),
            r#"{"id":"80351110","username":"nelly","global_name":"Nelly","avatar":"8342729096"}"#,
        );

        assert_eq!(mapped.name.as_deref(), Some("Nelly"));
        assert_eq!(
            mapped.avatar.as_deref(),
            Some("https://cdn.discordapp.com/avatars/80351110/8342729096.png")
        );
    }

    #[test]
    fn a_discord_user_with_no_avatar_gets_none_rather_than_a_broken_link() {
        let mapped = user(Provider::discord(), r#"{"id":"1","username":"nelly","avatar":null}"#);

        assert_eq!(mapped.avatar, None);
        assert_eq!(mapped.name.as_deref(), Some("nelly"), "no global_name, so the username");
    }

    #[test]
    fn microsoft_falls_back_to_the_principal_name_when_there_is_no_mailbox() {
        let with_mail = user(
            Provider::microsoft(),
            r#"{"id":"aad-1","displayName":"Ada","mail":"ada@corp.test",
                "userPrincipalName":"ada@corp.onmicrosoft.test"}"#,
        );
        assert_eq!(with_mail.email.as_deref(), Some("ada@corp.test"));

        let without = user(
            Provider::microsoft(),
            r#"{"id":"aad-1","displayName":"Ada","mail":null,
                "userPrincipalName":"ada@corp.onmicrosoft.test"}"#,
        );
        assert_eq!(without.email.as_deref(), Some("ada@corp.onmicrosoft.test"));
        assert_eq!(without.avatar, None, "Graph has no photo URL to map");
    }

    #[test]
    fn google_reads_the_openid_connect_names() {
        let mapped = user(
            Provider::google(),
            r#"{"sub":"1078","name":"Ada","email":"ada@x.test","picture":"https://x.test/a.png"}"#,
        );

        assert_eq!(mapped.id, "1078");
        assert_eq!(mapped.avatar.as_deref(), Some("https://x.test/a.png"));
        assert_eq!(mapped.provider, "google");
    }

    #[test]
    fn gitlab_reads_its_own_names() {
        let mapped = user(
            Provider::gitlab(),
            r#"{"id":42,"username":"ada","name":"Ada","avatar_url":"https://gl.test/a.png"}"#,
        );

        assert_eq!(mapped.qualified_id(), "gitlab:42");
        assert_eq!(mapped.avatar.as_deref(), Some("https://gl.test/a.png"));
    }

    #[test]
    fn the_same_id_at_two_providers_is_two_different_people() {
        let github = user(Provider::github(), r#"{"id":1,"login":"ada"}"#);
        let gitlab = user(Provider::gitlab(), r#"{"id":1,"username":"mallory"}"#);

        assert_eq!(github.id, gitlab.id);
        assert_ne!(github.auth_identifier(), gitlab.auth_identifier());
    }

    #[test]
    fn a_response_with_no_id_is_refused_rather_than_half_mapped() {
        assert!(
            SocialUser::from_provider(&Provider::github(), &Json::parse(r#"{"login":"ada"}"#).unwrap())
                .is_none()
        );
    }

    #[test]
    fn the_whole_response_is_kept_for_fields_this_crate_does_not_know() {
        let mapped = user(Provider::github(), r#"{"id":1,"login":"ada","company":"Analytical"}"#);

        assert_eq!(mapped.raw.get("company").and_then(Json::as_str), Some("Analytical"));
    }
}
