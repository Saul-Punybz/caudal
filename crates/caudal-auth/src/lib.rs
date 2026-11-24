//! Who may publish and play which stream, and telling other systems what
//! happened. Entry points fixed by the orchestrator.
//!
//! Tokens are JWTs. Claims: `sub` = stream name or a pattern ending in `*`
//! (`live/*`), `act` = `"publish"` or `"play"` (or both, as an array),
//! `exp` required, `ipa` optional (the client IP or CIDR, or an array of
//! them, the token is bound to). Keys: a shared HS256 secret, or a JWKS
//! URL (RS256/ES256/EdDSA), cached and refreshed. Each signing key can be
//! limited in what its tokens may grant ([`KeyPermissions`], MistServer's
//! per-key `{input, output, stream}`).
//!
//! Stream keys (a secret sent in place of the stream name) live in
//! [`StreamKeyTable`].

mod blocking;
mod claims;
mod gate_hook;
mod hooks_delivery;
mod jwks;
mod rewrite_hook;
mod stream_keys;

pub use gate_hook::{GateHook, GateHookConfig, GateHookMetric, GateHookStats, OnError, ReplyMode, mist_bool};
pub use rewrite_hook::{RewriteHook, RewriteHookConfig, RewriteHookMetric, RewriteHookStats};
pub use stream_keys::{
    MAX_KEY_LEN, MIN_KEY_LEN, StreamKey, StreamKeyMetric, StreamKeyStats, StreamKeyTable, validate_key,
};

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// `None`: everything is allowed (today's behavior, fine on localhost).
    pub keys: Option<KeySource>,
    /// Require a token to publish.
    pub publish: bool,
    /// Require a token to play.
    pub play: bool,
    /// What tokens signed with the HS256 secret may grant. `None`: anything
    /// the token's own claims say.
    pub secret_permissions: Option<KeyPermissions>,
    /// What tokens signed with a JWKS key may grant, by `kid`. A key not
    /// listed is not limited beyond the token's own claims.
    pub kid_permissions: HashMap<String, KeyPermissions>,
}

/// The most a signing key's tokens may grant, whatever their claims say
/// (MistServer's per-key `input`/`output`/`stream`). A token must pass both
/// its own `sub`/`act` and its key's permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPermissions {
    pub publish: bool,
    pub play: bool,
    /// Stream patterns (exact, or `prefix*`) the key's tokens may be used
    /// for. Empty means any stream.
    pub streams: Vec<String>,
}

impl KeyPermissions {
    fn allows(&self, action: Action, stream: &str) -> bool {
        let action_ok = match action {
            Action::Publish => self.publish,
            Action::Play => self.play,
        };
        action_ok && (self.streams.is_empty() || self.streams.iter().any(|p| claims::sub_matches(p, stream)))
    }
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
                KeySource::Secret(secret) => Keys::Secret(Box::new(
                    DecodingKey::from_secret(secret.as_bytes()).expect("[auth] secret is validated to 32+ characters"),
                )),
                KeySource::Jwks { url, refresh } => Keys::Jwks(jwks::JwksCache::spawn(url.clone(), *refresh)),
            })
        });
        Self { cfg: Arc::new(cfg), keys }
    }

    /// [`Authorizer::check_request`] without a client address: a token
    /// with an `ipa` claim is refused.
    pub async fn check(&self, action: Action, stream: &str, token: Option<&str>) -> Result<(), AuthError> {
        self.check_request(action, stream, token, None).await
    }

    /// `token` comes from the ingest URL (RTMP `?token=`, SRT stream id
    /// `publish/<name>?token=` or `#!::r=<name>,m=publish,t=<token>`) or the
    /// player request (`?token=` or `Authorization: Bearer`). `ip` is the
    /// client's resolved address (see [`caudal_core::Gate::check`]); a token
    /// with an `ipa` claim is refused when it is `None` or outside `ipa`.
    pub async fn check_request(
        &self,
        action: Action,
        stream: &str,
        token: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
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

        let (claims, permissions) = match keys.as_ref() {
            Keys::Secret(key) => {
                if header.alg != Algorithm::HS256 {
                    return Err(AuthError::Invalid);
                }
                let validation = base_validation(Algorithm::HS256);
                let claims = decode::<claims::Claims>(token, key, &validation).map_err(|_| AuthError::Invalid)?.claims;
                (claims, self.cfg.secret_permissions.as_ref())
            }
            Keys::Jwks(cache) => {
                if !JWKS_ALGORITHMS.contains(&header.alg) {
                    return Err(AuthError::Invalid);
                }
                let kid = header.kid.as_deref().ok_or(AuthError::Invalid)?;
                let jwk = cache.get(kid).await.ok_or(AuthError::Invalid)?;
                let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthError::Invalid)?;
                let validation = base_validation(header.alg);
                let claims = decode::<claims::Claims>(token, &key, &validation).map_err(|_| AuthError::Invalid)?.claims;
                (claims, self.cfg.kid_permissions.get(kid))
            }
        };

        if !claims::sub_matches(&claims.sub, stream) {
            return Err(AuthError::Forbidden);
        }
        if !claims.act.allows(action.as_str()) {
            return Err(AuthError::Forbidden);
        }
        if permissions.is_some_and(|p| !p.allows(action, stream)) {
            return Err(AuthError::Forbidden);
        }
        if let Some(ipa) = &claims.ipa {
            let ip = ip.ok_or(AuthError::Forbidden)?;
            if !claims::ipa_allows(ipa, ip).map_err(|_| AuthError::Invalid)? {
                return Err(AuthError::Forbidden);
            }
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

#[derive(Clone)]
pub struct HooksConfig {
    pub urls: Vec<String>,
    /// Standard Webhooks secret (`whsec_...`).
    pub secret: String,
}

/// Hand-written so `{:?}` never prints `secret` or `urls` in full.
impl std::fmt::Debug for HooksConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HooksConfig")
            .field("urls", &self.urls.iter().map(|u| caudal_core::net::redact_url_host(u)).collect::<Vec<_>>())
            .field("secret", &"****")
            .finish()
    }
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
        AuthConfig { keys: Some(KeySource::Secret(secret.to_string())), publish, play, ..Default::default() }
    }

    fn token_with(secret: &str, alg: Algorithm, claims: serde_json::Value) -> String {
        encode(&Header::new(alg), &claims, &EncodingKey::from_secret(secret.as_bytes()).unwrap()).unwrap()
    }

    fn hs256_token(secret: &str, sub: &str, act: serde_json::Value, exp_offset: i64) -> String {
        let exp = jsonwebtoken::get_current_timestamp() as i64 + exp_offset;
        token_with(secret, Algorithm::HS256, json!({ "sub": sub, "act": act, "exp": exp }))
    }

    #[tokio::test]
    async fn no_keys_allows_everything() {
        let auth = Authorizer::new(AuthConfig { keys: None, publish: true, play: true, ..Default::default() });
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

    fn ipa_token(secret: &str, ipa: serde_json::Value) -> String {
        let exp = jsonwebtoken::get_current_timestamp() as i64 + 60;
        token_with(secret, Algorithm::HS256, json!({ "sub": "live-main", "act": "play", "exp": exp, "ipa": ipa }))
    }

    #[tokio::test]
    async fn ipa_binds_the_token_to_the_client_address() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = ipa_token("s3cret", json!("203.0.113.7"));
        let check = |ip: Option<&str>| {
            let auth = auth.clone();
            let token = token.clone();
            let ip = ip.map(|s| s.parse::<IpAddr>().unwrap());
            async move { auth.check_request(Action::Play, "live-main", Some(&token), ip).await }
        };
        assert_eq!(check(Some("203.0.113.7")).await, Ok(()));
        assert_eq!(check(Some("::ffff:203.0.113.7")).await, Ok(()));
        assert_eq!(check(Some("203.0.113.8")).await, Err(AuthError::Forbidden));
        assert_eq!(check(None).await, Err(AuthError::Forbidden), "no address to judge: refused");

        let cidrs = ipa_token("s3cret", json!(["10.0.0.0/8", "2001:db8::/32"]));
        assert_eq!(auth.check_request(Action::Play, "live-main", Some(&cidrs), "10.9.8.7".parse().ok()).await, Ok(()));
        assert_eq!(
            auth.check_request(Action::Play, "live-main", Some(&cidrs), "192.0.2.1".parse().ok()).await,
            Err(AuthError::Forbidden)
        );
        let bad = ipa_token("s3cret", json!("not-an-ip"));
        assert_eq!(
            auth.check_request(Action::Play, "live-main", Some(&bad), "10.0.0.1".parse().ok()).await,
            Err(AuthError::Invalid)
        );
    }

    #[tokio::test]
    async fn tokens_without_new_claims_behave_as_before() {
        let auth = Authorizer::new(hs256_config("s3cret", true, true));
        let token = hs256_token("s3cret", "live-main", json!("play"), 60);
        for ip in [None, "198.51.100.1".parse().ok()] {
            assert_eq!(auth.check_request(Action::Play, "live-main", Some(&token), ip).await, Ok(()));
        }
    }

    #[tokio::test]
    async fn secret_permissions_bound_what_tokens_grant() {
        let auth = Authorizer::new(AuthConfig {
            secret_permissions: Some(KeyPermissions { publish: false, play: true, streams: vec!["live-*".into()] }),
            ..hs256_config("s3cret", true, true)
        });
        let both = hs256_token("s3cret", "*", json!(["publish", "play"]), 60);
        assert_eq!(auth.check(Action::Play, "live-main", Some(&both)).await, Ok(()));
        assert_eq!(auth.check(Action::Publish, "live-main", Some(&both)).await, Err(AuthError::Forbidden));
        assert_eq!(auth.check(Action::Play, "other", Some(&both)).await, Err(AuthError::Forbidden));
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
