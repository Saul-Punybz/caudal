//! Who may publish and play which stream, and telling other systems what
//! happened. Entry points fixed by the orchestrator.
//!
//! Tokens are JWTs. Claims: `sub` = stream name or a pattern ending in `*`
//! (`live/*`), `act` = `"publish"` or `"play"` (or both, as an array),
//! `exp` required. Keys: a shared HS256 secret, or a JWKS URL (RS256/ES256/
//! EdDSA), cached and refreshed.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Publish,
    Play,
}

#[derive(Debug, Clone)]
pub enum KeySource {
    /// Shared HS256 secret.
    Secret(String),
    /// JWKS endpoint; keys are cached and refreshed every `refresh`.
    Jwks { url: String, refresh: Duration },
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// `None`: everything is allowed (today's behavior, fine on localhost).
    pub keys: Option<KeySource>,
    /// Require a token to publish.
    pub publish: bool,
    /// Require a token to play.
    pub play: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("a token is required")]
    Missing,
    #[error("the token is invalid or expired")]
    Invalid,
    #[error("the token does not allow this stream or action")]
    Forbidden,
}

/// Checks tokens. Cheap to clone and share across tasks.
#[derive(Clone)]
pub struct Authorizer {
    cfg: std::sync::Arc<AuthConfig>,
}

impl Authorizer {
    pub fn new(cfg: AuthConfig) -> Self {
        Self { cfg: std::sync::Arc::new(cfg) }
    }

    /// `token` comes from the ingest URL (RTMP `?token=`, SRT stream id
    /// `publish/<name>?token=` or `#!::r=<name>,m=publish,t=<token>`) or the
    /// player request (`?token=` or `Authorization: Bearer`).
    pub async fn check(&self, action: Action, stream: &str, token: Option<&str>) -> Result<(), AuthError> {
        let required = match action {
            Action::Publish => self.cfg.publish,
            Action::Play => self.cfg.play,
        };
        if self.cfg.keys.is_none() || !required {
            return Ok(());
        }
        let _ = (stream, token);
        Err(AuthError::Invalid)
    }
}

/// A stream event delivered to webhook endpoints, signed per the Standard
/// Webhooks spec (`webhook-id`, `webhook-timestamp`, `webhook-signature`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    StreamStarted { stream: String },
    StreamEnded { stream: String },
}

#[derive(Debug, Clone)]
pub struct HooksConfig {
    pub urls: Vec<String>,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
}

/// Fire-and-forget delivery with retries; never blocks ingest.
#[derive(Clone)]
pub struct Hooks {
    _cfg: std::sync::Arc<Option<HooksConfig>>,
}

impl Hooks {
    pub fn new(cfg: Option<HooksConfig>) -> Self {
        Self { _cfg: std::sync::Arc::new(cfg) }
    }

    pub fn emit(&self, event: HookEvent) {
        let _ = event;
    }
}
