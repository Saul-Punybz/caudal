//! Who may publish and play which stream, and telling other systems what
//! happened. Entry points fixed by the orchestrator.
//!
//! Tokens are JWTs. Claims: `sub` = stream name or a pattern ending in `*`
//! (`live/*`), `act` = `"publish"` or `"play"` (or both, as an array),
//! `exp` required. Keys: a shared HS256 secret, or a JWKS URL (RS256/ES256/
//! EdDSA), cached and refreshed.

mod claims;
mod hooks_delivery;
mod jwks;

use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Publish,
    Play,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Publish => "publish",
            Action::Play => "play",
        }
    }
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

/// The algorithms accepted for a JWKS-backed key: asymmetric only, so a
/// token can never be forged with a symmetric secret the server never
/// handed out (algorithm confusion).
const JWKS_ALGORITHMS: [Algorithm; 3] = [Algorithm::RS256, Algorithm::ES256, Algorithm::EdDSA];

enum Keys {
    /// HS256 only; rejects `alg: none` and any other algorithm by
    /// construction (`Validation::algorithms` never contains them).
    Secret(Box<DecodingKey>),
    Jwks(Arc<jwks::JwksCache>),
}

/// Checks tokens. Cheap to clone and share across tasks.
#[derive(Clone)]
pub struct Authorizer {
    cfg: Arc<AuthConfig>,
    keys: Option<Arc<Keys>>,
}

impl Authorizer {
    pub fn new(cfg: AuthConfig) -> Self {
        let keys = cfg.keys.as_ref().map(|source| {
            Arc::new(match source {
                KeySource::Secret(secret) => Keys::Secret(Box::new(DecodingKey::from_secret(secret.as_bytes()))),
                KeySource::Jwks { url, refresh } => Keys::Jwks(jwks::JwksCache::spawn(url.clone(), *refresh)),
            })
        });
        Self { cfg: Arc::new(cfg), keys }
    }

    /// `token` comes from the ingest URL (RTMP `?token=`, SRT stream id
    /// `publish/<name>?token=` or `#!::r=<name>,m=publish,t=<token>`) or the
    /// player request (`?token=` or `Authorization: Bearer`).
    pub async fn check(&self, action: Action, stream: &str, token: Option<&str>) -> Result<(), AuthError> {
        let required = match action {
            Action::Publish => self.cfg.publish,
            Action::Play => self.cfg.play,
        };
        let Some(keys) = &self.keys else {
            return Ok(());
        };
        if !required {
            return Ok(());
        }

        let token = token.ok_or(AuthError::Missing)?;
        let header = decode_header(token).map_err(|_| AuthError::Invalid)?;

        let claims = match keys.as_ref() {
            Keys::Secret(key) => {
                if header.alg != Algorithm::HS256 {
                    return Err(AuthError::Invalid);
                }
                let validation = base_validation(Algorithm::HS256);
                decode::<claims::Claims>(token, key, &validation).map_err(|_| AuthError::Invalid)?.claims
            }
            Keys::Jwks(cache) => {
                if !JWKS_ALGORITHMS.contains(&header.alg) {
                    return Err(AuthError::Invalid);
                }
                let kid = header.kid.as_deref().ok_or(AuthError::Invalid)?;
                let jwk = cache.get(kid).await.ok_or(AuthError::Invalid)?;
                let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthError::Invalid)?;
                let validation = base_validation(header.alg);
                decode::<claims::Claims>(token, &key, &validation).map_err(|_| AuthError::Invalid)?.claims
            }
        };

        if !claims::sub_matches(&claims.sub, stream) {
            return Err(AuthError::Forbidden);
        }
        if !claims.act.allows(action.as_str()) {
            return Err(AuthError::Forbidden);
        }
        Ok(())
    }
}

/// Shared `Validation`: 30s leeway on `exp`/`nbf`, `nbf` honored only when
/// present, no `aud` check (we never issue or expect one).
fn base_validation(alg: Algorithm) -> Validation {
    let mut validation = Validation::new(alg);
    validation.leeway = 30;
    validation.validate_nbf = true;
    validation.validate_aud = false;
    validation
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
    inner: Arc<Option<hooks_delivery::HooksInner>>,
}

impl Hooks {
    pub fn new(cfg: Option<HooksConfig>) -> Self {
        let inner = cfg.and_then(|cfg| match hooks_delivery::HooksInner::build(&cfg.secret) {
            Ok(webhook) => Some(hooks_delivery::HooksInner {
                urls: cfg.urls,
                webhook,
                http: hooks_delivery::HooksInner::http_client(),
            }),
            Err(err) => {
                tracing::warn!(error = %err, "invalid webhook secret; hooks disabled");
                None
            }
        });
        Self { inner: Arc::new(inner) }
    }

    pub fn emit(&self, event: HookEvent) {
        if self.inner.is_none() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Some(inner) = inner.as_ref() {
                        hooks_delivery::deliver_all(inner, event).await;
                    }
                });
            }
            Err(_) => {
                tracing::warn!("no tokio runtime available; dropping webhook event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    fn hs256_config(secret: &str, publish: bool, play: bool) -> AuthConfig {
        AuthConfig { keys: Some(KeySource::Secret(secret.to_string())), publish, play }
    }

    fn token_with(secret: &str, alg: Algorithm, claims: serde_json::Value) -> String {
        encode(&Header::new(alg), &claims, &EncodingKey::from_secret(secret.as_bytes())).unwrap()
    }

    fn hs256_token(secret: &str, sub: &str, act: serde_json::Value, exp_offset: i64) -> String {
        let exp = jsonwebtoken::get_current_timestamp() as i64 + exp_offset;
        token_with(secret, Algorithm::HS256, json!({ "sub": sub, "act": act, "exp": exp }))
    }

    #[tokio::test]
    async fn no_keys_allows_everything() {
        let auth = Authorizer::new(AuthConfig { keys: None, publish: true, play: true });
        assert_eq!(auth.check(Action::Publish, "any", None).await, Ok(()));
    }

    #[tokio::test]
    async fn action_not_required_allows_everything() {
        let auth = Authorizer::new(hs256_config("s3cret", false, true));
        assert_eq!(auth.check(Action::Publish, "any", None).await, Ok(()));
    }

    #[tokio::test]
    async fn missing_token_when_required() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        assert_eq!(auth.check(Action::Publish, "live-main", None).await, Err(AuthError::Missing));
    }

    #[tokio::test]
    async fn hs256_happy_path() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-main", json!("publish"), 60);
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Ok(()));
    }

    #[tokio::test]
    async fn wrong_secret_is_invalid() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("other-secret", "live-main", json!("publish"), 60);
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Err(AuthError::Invalid));
    }

    #[tokio::test]
    async fn expired_is_invalid() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-main", json!("publish"), -120);
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Err(AuthError::Invalid));
    }

    #[tokio::test]
    async fn not_yet_valid_is_invalid() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let now = jsonwebtoken::get_current_timestamp() as i64;
        let token = token_with(
            "s3cret",
            Algorithm::HS256,
            json!({ "sub": "live-main", "act": "publish", "exp": now + 3600, "nbf": now + 120 }),
        );
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Err(AuthError::Invalid));
    }

    #[tokio::test]
    async fn alg_none_is_invalid() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let now = jsonwebtoken::get_current_timestamp() as i64;
        let header = general_purpose_b64(&json!({ "alg": "none", "typ": "JWT" }));
        let payload = general_purpose_b64(&json!({ "sub": "live-main", "act": "publish", "exp": now + 60 }));
        let token = format!("{header}.{payload}.");
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Err(AuthError::Invalid));
    }

    #[tokio::test]
    async fn wrong_stream_is_forbidden() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-main", json!("publish"), 60);
        assert_eq!(auth.check(Action::Publish, "live-other", Some(&token)).await, Err(AuthError::Forbidden));
    }

    #[tokio::test]
    async fn wrong_action_is_forbidden() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-main", json!("play"), 60);
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Err(AuthError::Forbidden));
    }

    #[tokio::test]
    async fn prefix_pattern_matches() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-*", json!("publish"), 60);
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Ok(()));
        assert_eq!(auth.check(Action::Publish, "other", Some(&token)).await, Err(AuthError::Forbidden));
    }

    #[tokio::test]
    async fn act_as_array() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-main", json!(["publish", "play"]), 60);
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Ok(()));
        assert_eq!(auth.check(Action::Play, "live-main", Some(&token)).await, Ok(()));
    }

    #[tokio::test]
    async fn malformed_token_is_invalid() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        assert_eq!(auth.check(Action::Publish, "live-main", Some("not-a-jwt")).await, Err(AuthError::Invalid));
    }

    #[tokio::test]
    async fn hooks_new_none_is_a_noop() {
        let hooks = Hooks::new(None);
        // Must not panic and must not block; nothing to assert beyond that.
        hooks.emit(HookEvent::StreamStarted { stream: "live-main".to_string() });
    }

    fn general_purpose_b64(value: &serde_json::Value) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
    }
}
