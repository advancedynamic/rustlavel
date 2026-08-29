//! One provider's endpoints and quirks.
//!
//! Every provider in this file implements the same RFC, and every one of them
//! differs somewhere: where the credentials go, what the userinfo fields are
//! called, whether the token endpoint answers in JSON at all. A `Provider` is
//! the place those differences are written down once, so the client above it
//! stays a single implementation of the spec rather than a pile of `if
//! provider == "github"`.

use crate::scope::Scopes;

/// How the client proves who it is at the token endpoint, RFC 6749 §2.3.1.
///
/// The RFC gives two ways and says a server MUST support the first, but plenty
/// only implement the second — and a provider handed credentials the way it
/// does not expect answers `invalid_client`, which reads exactly like a wrong
/// secret. That is why this is a property of the provider and not a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientAuth {
    /// HTTP Basic, with the id and secret percent-encoded before they are
    /// joined and base64'd — §2.3.1 is explicit about the encoding, and a
    /// secret containing `:` or `+` is silently wrong without it.
    #[default]
    Basic,
    /// `client_id` and `client_secret` as ordinary form fields.
    Body,
}

/// Where a provider keeps each field of a user profile.
///
/// Candidates are tried in order, so `["name", "login"]` means "the real name
/// if there is one, otherwise the handle" — which is what GitHub needs, because
/// `name` is optional there and frequently null.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserMap {
    pub id: Vec<String>,
    pub name: Vec<String>,
    pub email: Vec<String>,
    pub avatar: Vec<String>,
    /// How to turn the avatar field into a URL, when it is not one already.
    ///
    /// Discord sends an image *hash*; the URL is assembled from it and the
    /// user id. `{id}` and `{avatar}` are substituted.
    pub avatar_template: Option<String>,
}

impl UserMap {
    pub fn new() -> UserMap {
        UserMap::default()
    }

    pub fn id<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> UserMap {
        self.id = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn name<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> UserMap {
        self.name = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn email<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> UserMap {
        self.email = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn avatar<I: IntoIterator<Item = S>, S: Into<String>>(mut self, paths: I) -> UserMap {
        self.avatar = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn avatar_from(mut self, template: impl Into<String>) -> UserMap {
        self.avatar_template = Some(template.into());
        self
    }
}

/// Everything the client needs to know about one provider.
#[derive(Debug, Clone)]
pub struct Provider {
    /// The name used in `/auth/{provider}/redirect` and stored on a
    /// [`crate::SocialUser`], so two providers' user ids cannot be confused.
    pub name: String,
    pub authorize_url: String,
    pub token_url: String,
    pub userinfo_url: Option<String>,
    /// RFC 7009. Absent for the providers that never implemented it.
    pub revoke_url: Option<String>,
    /// What is asked for when the application does not say.
    pub scopes: Scopes,
    pub client_auth: ClientAuth,
    /// Sent with every request this client makes to the provider.
    ///
    /// This exists for GitHub, which answers its token endpoint in
    /// `application/x-www-form-urlencoded` unless asked for JSON.
    pub headers: Vec<(String, String)>,
    /// Added to every authorisation URL, before the application's own.
    pub authorize_params: Vec<(String, String)>,
    pub map: UserMap,
}

impl Provider {
    /// A provider the presets do not cover — a self-hosted GitLab, Keycloak,
    /// an internal identity server.
    pub fn custom(
        name: impl Into<String>,
        authorize_url: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Provider {
        Provider {
            name: name.into(),
            authorize_url: authorize_url.into(),
            token_url: token_url.into(),
            userinfo_url: None,
            revoke_url: None,
            scopes: Scopes::new(),
            client_auth: ClientAuth::Basic,
            headers: Vec::new(),
            authorize_params: Vec::new(),
            // The OpenID Connect spelling, which is what a standards-compliant
            // provider will use.
            map: UserMap::new()
                .id(["sub"])
                .name(["name", "preferred_username"])
                .email(["email"])
                .avatar(["picture"]),
        }
    }

    pub fn userinfo(mut self, url: impl Into<String>) -> Provider {
        self.userinfo_url = Some(url.into());
        self
    }

    pub fn revoke(mut self, url: impl Into<String>) -> Provider {
        self.revoke_url = Some(url.into());
        self
    }

    pub fn scopes(mut self, scopes: impl Into<Scopes>) -> Provider {
        self.scopes = scopes.into();
        self
    }

    pub fn client_auth(mut self, how: ClientAuth) -> Provider {
        self.client_auth = how;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Provider {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Clear the headers a preset sends. Only useful for showing what a
    /// provider does without them.
    pub fn without_headers(mut self) -> Provider {
        self.headers.clear();
        self
    }

    pub fn authorize_param(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Provider {
        self.authorize_params.push((name.into(), value.into()));
        self
    }

    pub fn map(mut self, map: UserMap) -> Provider {
        self.map = map;
        self
    }

    /// Google, over OpenID Connect.
    ///
    /// A refresh token only ever arrives with `access_type=offline` *and*
    /// `prompt=consent`, and only on the first grant — add them with
    /// `OAuthClient::with` when the application needs offline access.
    pub fn google() -> Provider {
        Provider::custom(
            "google",
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
        )
        .userinfo("https://openidconnect.googleapis.com/v1/userinfo")
        .revoke("https://oauth2.googleapis.com/revoke")
        .scopes("openid email profile")
        .client_auth(ClientAuth::Basic)
    }

    /// GitHub, which is the awkward one.
    ///
    /// Its token endpoint answers `application/x-www-form-urlencoded` unless
    /// asked for JSON, and it reports failures as HTTP 200 with an `error`
    /// field. The header below fixes the first; the second is handled by
    /// [`crate::TokenResponse::from_json`], which reads `error` whatever the
    /// status says. GitHub also authenticates in the body rather than by Basic.
    pub fn github() -> Provider {
        Provider::custom(
            "github",
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
        )
        .userinfo("https://api.github.com/user")
        .scopes("read:user user:email")
        .client_auth(ClientAuth::Body)
        .header("accept", "application/json")
        .map(
            UserMap::new()
                .id(["id"])
                // `name` is optional on GitHub and is null more often than not.
                .name(["name", "login"])
                .email(["email"])
                .avatar(["avatar_url"]),
        )
    }

    /// GitLab.com. Point the URLs elsewhere for a self-hosted instance.
    pub fn gitlab() -> Provider {
        Provider::custom(
            "gitlab",
            "https://gitlab.com/oauth/authorize",
            "https://gitlab.com/oauth/token",
        )
        .userinfo("https://gitlab.com/api/v4/user")
        .revoke("https://gitlab.com/oauth/revoke")
        .scopes("read_user")
        .client_auth(ClientAuth::Body)
        .map(
            UserMap::new()
                .id(["id"])
                .name(["name", "username"])
                .email(["email"])
                .avatar(["avatar_url"]),
        )
    }

    /// Microsoft identity platform, against the `common` tenant.
    ///
    /// Graph has no photo *URL* — `/me/photo/$value` returns image bytes — so
    /// there is no avatar to map, and inventing one would hand every caller a
    /// link that 404s.
    pub fn microsoft() -> Provider {
        Provider::custom(
            "microsoft",
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        )
        .userinfo("https://graph.microsoft.com/v1.0/me")
        .scopes("openid email profile User.Read")
        .client_auth(ClientAuth::Body)
        .map(
            UserMap::new()
                .id(["id"])
                .name(["displayName"])
                // A work account often has no `mail`, only the principal name.
                .email(["mail", "userPrincipalName"])
                .avatar(Vec::<String>::new()),
        )
    }

    /// Discord.
    pub fn discord() -> Provider {
        Provider::custom(
            "discord",
            "https://discord.com/oauth2/authorize",
            "https://discord.com/api/oauth2/token",
        )
        .userinfo("https://discord.com/api/users/@me")
        .revoke("https://discord.com/api/oauth2/token/revoke")
        .scopes("identify email")
        .client_auth(ClientAuth::Basic)
        .map(
            UserMap::new()
                .id(["id"])
                .name(["global_name", "username"])
                .email(["email"])
                .avatar(["avatar"])
                .avatar_from("https://cdn.discordapp.com/avatars/{id}/{avatar}.png"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_the_two_endpoints_the_flow_cannot_run_without() {
        for provider in [
            Provider::google(),
            Provider::github(),
            Provider::gitlab(),
            Provider::microsoft(),
            Provider::discord(),
        ] {
            assert!(provider.authorize_url.starts_with("https://"), "{}", provider.name);
            assert!(provider.token_url.starts_with("https://"), "{}", provider.name);
            assert!(!provider.scopes.is_empty(), "{} asks for nothing", provider.name);
        }
    }

    #[test]
    fn github_asks_for_json_because_it_would_otherwise_send_a_form() {
        assert_eq!(
            Provider::github().headers,
            vec![("accept".to_string(), "application/json".to_string())]
        );
        // And nobody else needs the workaround.
        assert!(Provider::google().headers.is_empty());
    }

    #[test]
    fn providers_disagree_about_where_the_credentials_go() {
        // The whole reason ClientAuth exists: sending Basic to a body-only
        // endpoint is an `invalid_client` that looks like a wrong secret.
        assert_eq!(Provider::google().client_auth, ClientAuth::Basic);
        assert_eq!(Provider::github().client_auth, ClientAuth::Body);
        assert_eq!(Provider::microsoft().client_auth, ClientAuth::Body);
    }

    #[test]
    fn a_custom_provider_defaults_to_the_openid_connect_field_names() {
        let provider = Provider::custom("keycloak", "https://id.test/auth", "https://id.test/token");

        assert_eq!(provider.map.id, vec!["sub".to_string()]);
        assert_eq!(provider.userinfo_url, None);
        assert!(provider.scopes.is_empty(), "a custom provider asks for nothing by default");
    }

    #[test]
    fn only_discord_needs_its_avatar_assembled() {
        assert!(Provider::discord().map.avatar_template.is_some());
        assert!(Provider::github().map.avatar_template.is_none());
        assert!(Provider::microsoft().map.avatar.is_empty(), "Graph has no photo URL");
    }
}
