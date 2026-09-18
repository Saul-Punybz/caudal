//! Admin login for Caudal's UI and management API.
//!
//! Three ways in, all optional, all under `[admin]`:
//! - local users with argon2id password hashes (`caudal hash-password`),
//! - single sign-on through an OpenID Connect provider (code + PKCE),
//! - API tokens for automation, sent as `Authorization: Bearer`.
//!
//! A browser login gets a server-side session: a random 256-bit id in an
//! `HttpOnly`, `SameSite=Lax` cookie (`Secure` over TLS). State-changing
//! requests made with that cookie must also carry the session's CSRF token
//! in `X-CSRF-Token` (synchronizer token pattern). Bearer requests carry no
//! ambient credential, so they need no CSRF token.
//!
//! [`protect`] wraps the whole HTTP app; [`needs_admin`] is the single
//! place that says which paths stay public (players, publishers, probes).

mod config;
mod http;
mod oidc;
mod password;
mod ratelimit;
mod session;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub use config::{AdminSection, ApiTokenEntry, Exposure, OIDC_CALLBACK_PATH, OidcSection, UserEntry, check_exposure};
pub use http::{AdminUser, CSRF_HEADER, LOGIN_HEADER, SESSION_COOKIE, ViaTls, needs_admin, protect, router};
pub use password::{hash_password, verify_password};

/// Login attempts (password posts and SSO starts/callbacks) per client
/// address per window.
const LOGIN_ATTEMPTS: u32 = 10;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// The admin gate, built from a validated `[admin]` section. Cheap to
/// clone.
#[derive(Clone)]
pub struct Admin(Arc<Inner>);

struct Inner {
    users: HashMap<String, String>,
    tokens: Vec<(String, [u8; 32])>,
    sessions: session::Sessions,
    limiter: ratelimit::RateLimiter,
    oidc: Option<oidc::Oidc>,
    public_metrics: bool,
    secure_cookies: bool,
}

/// Counts only: hashes and sessions never reach logs.
impl std::fmt::Debug for Admin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admin")
            .field("users", &self.0.users.len())
            .field("api_tokens", &self.0.tokens.len())
            .field("oidc", &self.0.oidc.is_some())
            .finish_non_exhaustive()
    }
}

impl Admin {
    /// `Ok(None)` when the section configures no way to log in.
    pub fn new(section: &AdminSection) -> Result<Option<Self>, String> {
        section.validate()?;
        if !section.login_enabled() {
            return Ok(None);
        }
        let tokens = section
            .api_tokens
            .iter()
            .map(|t| {
                let mut d = [0u8; 32];
                hex::decode_to_slice(&t.token_sha256, &mut d).map_err(|e| e.to_string())?;
                Ok((t.name.clone(), d))
            })
            .collect::<Result<_, String>>()?;
        Ok(Some(Self(Arc::new(Inner {
            users: section.users.iter().map(|u| (u.name.clone(), u.password_hash.clone())).collect(),
            tokens,
            sessions: session::Sessions::new(Duration::from_secs(section.session_ttl_secs)),
            limiter: ratelimit::RateLimiter::new(LOGIN_ATTEMPTS, LOGIN_WINDOW),
            oidc: section.oidc.clone().map(oidc::Oidc::new).transpose()?,
            public_metrics: section.public_metrics,
            secure_cookies: section.secure_cookies,
        }))))
    }
}
