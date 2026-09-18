//! The `[admin]` config section and the "may this server start?" rule.

use std::net::SocketAddr;

use serde::Deserialize;

fn default_session_ttl_secs() -> u64 {
    12 * 3600
}

/// `[admin]`: who may use the UI and the management API.
///
/// Login is on when at least one of `users`, `oidc` or `api_tokens` is set.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct AdminSection {
    /// Local accounts: `{ name, password_hash }`, the hash an argon2id PHC
    /// string from `caudal hash-password`.
    pub users: Vec<UserEntry>,
    /// Single sign-on through an OpenID Connect provider.
    pub oidc: Option<OidcSection>,
    /// How long a login lasts, in seconds (absolute, not sliding).
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// Tokens for scripts and scrapers, sent as `Authorization: Bearer`.
    /// Only their SHA-256 (hex) is stored.
    pub api_tokens: Vec<ApiTokenEntry>,
    /// Run with no login on a non-loopback address anyway. Must be set on
    /// purpose; contradicts `users`/`oidc`/`api_tokens`.
    pub allow_unauthenticated: bool,
    /// Serve `/metrics` without login (for a scraper on a private network).
    /// Default: `/metrics` needs an API token like the rest of the API.
    pub public_metrics: bool,
    /// Mark the session cookie `Secure` on plain HTTP too: set this when a
    /// reverse proxy terminates TLS in front of Caudal. Requests that
    /// arrive on Caudal's own HTTPS listener always get `Secure`.
    pub secure_cookies: bool,
}

impl Default for AdminSection {
    fn default() -> Self {
        Self {
            users: Vec::new(),
            oidc: None,
            session_ttl_secs: default_session_ttl_secs(),
            api_tokens: Vec::new(),
            allow_unauthenticated: false,
            public_metrics: false,
            secure_cookies: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UserEntry {
    pub name: String,
    pub password_hash: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApiTokenEntry {
    pub name: String,
    /// Lowercase or uppercase hex SHA-256 of the token.
    pub token_sha256: String,
}

/// `[admin.oidc]`: authorization code flow with PKCE.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OidcSection {
    /// e.g. `https://accounts.google.com`; discovery reads
    /// `{issuer}/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    /// Absent for public clients (PKCE only).
    pub client_secret: Option<String>,
    /// Must end in `/api/v1/auth/oidc/callback` on this server's public URL.
    pub redirect_url: String,
    /// Exact emails (case-insensitive) let in; the ID token must say
    /// `email_verified: true`.
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// Anyone whose ID token `groups` claim contains one of these.
    #[serde(default)]
    pub allowed_groups: Vec<String>,
    /// Scopes asked for besides `openid email profile` (e.g. `groups`).
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Path the OIDC provider redirects back to.
pub const OIDC_CALLBACK_PATH: &str = "/api/v1/auth/oidc/callback";

impl AdminSection {
    /// True when some way to log in is configured.
    pub fn login_enabled(&self) -> bool {
        !self.users.is_empty() || self.oidc.is_some() || !self.api_tokens.is_empty()
    }

    /// Checks every key; run by `caudal check` and at startup.
    pub fn validate(&self) -> Result<(), String> {
        if self.allow_unauthenticated && self.login_enabled() {
            return Err("[admin] `allow_unauthenticated = true` contradicts users/oidc/api_tokens; pick one".into());
        }
        if self.session_ttl_secs < 60 {
            return Err("[admin] `session_ttl_secs` must be at least 60".into());
        }
        let mut names = std::collections::HashSet::new();
        for u in &self.users {
            if u.name.is_empty() || u.name.len() > 128 {
                return Err("[admin] user `name` must be 1-128 characters".into());
            }
            if !names.insert(u.name.as_str()) {
                return Err(format!("[admin] user `{}` is listed twice", u.name));
            }
            crate::password::check_phc(&u.password_hash)
                .map_err(|e| format!("[admin] user `{}`: password_hash {e}", u.name))?;
        }
        for t in &self.api_tokens {
            if t.token_sha256.len() != 64 || hex::decode(&t.token_sha256).is_err() {
                return Err(format!("[admin] api token `{}`: token_sha256 must be 64 hex characters", t.name));
            }
        }
        if let Some(o) = &self.oidc {
            o.validate()?;
        }
        Ok(())
    }
}

impl OidcSection {
    fn validate(&self) -> Result<(), String> {
        if self.allowed_emails.is_empty() && self.allowed_groups.is_empty() {
            return Err(
                "[admin.oidc] set `allowed_emails` or `allowed_groups`; otherwise anyone with an account at the provider gets in"
                    .into(),
            );
        }
        if self.client_id.is_empty() {
            return Err("[admin.oidc] `client_id` is empty".into());
        }
        openidconnect::IssuerUrl::new(self.issuer.clone())
            .map_err(|e| format!("[admin.oidc] `issuer` is not a URL: {e}"))?;
        let redirect = openidconnect::url::Url::parse(&self.redirect_url)
            .map_err(|e| format!("[admin.oidc] `redirect_url` is not a URL: {e}"))?;
        if redirect.path() != OIDC_CALLBACK_PATH {
            return Err(format!("[admin.oidc] `redirect_url` must end in {OIDC_CALLBACK_PATH}"));
        }
        let loopback = matches!(redirect.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if redirect.scheme() != "https" && !loopback {
            return Err("[admin.oidc] `redirect_url` must be https (http only for localhost)".into());
        }
        Ok(())
    }
}

/// What startup decided about the admin surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Login required for the UI's API and every management route.
    Protected,
    /// No login; only loopback listeners (or explicitly allowed).
    Open,
}

/// The refuse-to-start rule: without login, every HTTP listener must be on
/// loopback, unless `[admin] allow_unauthenticated = true`. `binds` are the
/// HTTP and HTTPS listen addresses.
pub fn check_exposure(admin: Option<&AdminSection>, binds: &[SocketAddr]) -> Result<Exposure, String> {
    if admin.is_some_and(AdminSection::login_enabled) {
        return Ok(Exposure::Protected);
    }
    let allowed = admin.is_some_and(|a| a.allow_unauthenticated);
    if let Some(open) = binds.iter().find(|b| !b.ip().is_loopback())
        && !allowed
    {
        return Err(format!(
            "refusing to start: the UI and management API would be open to anyone on {open} with no login. \
             Add [admin] users/oidc/api_tokens (see `caudal hash-password`), bind to 127.0.0.1, \
             or set [admin] allow_unauthenticated = true to accept the risk"
        ));
    }
    Ok(Exposure::Open)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn refuses_to_start_open_on_a_public_address() {
        let public = [addr("0.0.0.0:8080")];
        assert!(check_exposure(None, &public).unwrap_err().contains("refusing to start"));
        assert_eq!(check_exposure(None, &[addr("127.0.0.1:8080"), addr("[::1]:8443")]), Ok(Exposure::Open));
        // An HTTPS listener on a public address counts too.
        assert!(check_exposure(None, &[addr("127.0.0.1:8080"), addr("0.0.0.0:8443")]).is_err());
        // An empty [admin] is not a login.
        assert!(check_exposure(Some(&AdminSection::default()), &public).is_err());
        let allow = AdminSection { allow_unauthenticated: true, ..Default::default() };
        assert_eq!(check_exposure(Some(&allow), &public), Ok(Exposure::Open));
        let tokens = AdminSection {
            api_tokens: vec![ApiTokenEntry { name: "ci".into(), token_sha256: "0".repeat(64) }],
            ..Default::default()
        };
        assert_eq!(check_exposure(Some(&tokens), &public), Ok(Exposure::Protected));
    }

    #[test]
    fn validation_names_the_problem() {
        let bad = |s: AdminSection| s.validate().unwrap_err();
        let token = ApiTokenEntry { name: "ci".into(), token_sha256: "ab".into() };
        assert!(bad(AdminSection { api_tokens: vec![token], ..Default::default() }).contains("64 hex"));
        let user = UserEntry { name: "a".into(), password_hash: "plaintext".into() };
        assert!(bad(AdminSection { users: vec![user], ..Default::default() }).contains("password_hash"));
        let both = AdminSection {
            allow_unauthenticated: true,
            api_tokens: vec![ApiTokenEntry { name: "ci".into(), token_sha256: "0".repeat(64) }],
            ..Default::default()
        };
        assert!(bad(both).contains("contradicts"));
        let oidc = OidcSection {
            issuer: "https://idp.example".into(),
            client_id: "caudal".into(),
            client_secret: None,
            redirect_url: "https://caudal.example/api/v1/auth/oidc/callback".into(),
            allowed_emails: vec![],
            allowed_groups: vec![],
            scopes: vec![],
        };
        assert!(bad(AdminSection { oidc: Some(oidc.clone()), ..Default::default() }).contains("allowed_"));
        let open_http = OidcSection {
            allowed_groups: vec!["ops".into()],
            redirect_url: "http://caudal.example/api/v1/auth/oidc/callback".into(),
            ..oidc.clone()
        };
        assert!(bad(AdminSection { oidc: Some(open_http), ..Default::default() }).contains("https"));
        let wrong_path = OidcSection {
            allowed_groups: vec!["ops".into()],
            redirect_url: "https://caudal.example/cb".into(),
            ..oidc
        };
        assert!(bad(AdminSection { oidc: Some(wrong_path), ..Default::default() }).contains("callback"));
    }
}
