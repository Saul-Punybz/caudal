//! OpenID Connect login: authorization code flow with PKCE, through the
//! `openidconnect` crate. It checks the ID token's signature (keys from the
//! provider's JWKS), issuer, audience (`client_id`), expiry and nonce; we
//! add the state/cookie binding and the allow-list.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use openidconnect::core::{CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, reqwest,
};
use parking_lot::Mutex;
use subtle::ConstantTimeEq;

use crate::config::OidcSection;

/// How long the user has at the provider between start and callback.
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(600);
/// Logins in flight at once; bounds memory if `/oidc/start` is hammered.
const MAX_PENDING: usize = 1024;
/// Discovery (and with it the JWKS) is refetched this often.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);

type Client =
    CoreClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointMaybeSet, EndpointMaybeSet>;

struct Pending {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    created: Instant,
}

pub(crate) struct Oidc {
    cfg: OidcSection,
    http: reqwest::Client,
    client: tokio::sync::Mutex<Option<(Instant, Client)>>,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Oidc {
    pub fn new(cfg: OidcSection) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            // Following redirects from the provider invites SSRF.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("[admin.oidc] http client: {e}"))?;
        Ok(Self { cfg, http, client: tokio::sync::Mutex::new(None), pending: Mutex::new(HashMap::new()) })
    }

    /// The discovered client, fetched on first use and refreshed hourly. A
    /// provider that is down at startup does not stop Caudal starting.
    async fn client(&self) -> Result<Client, String> {
        let mut slot = self.client.lock().await;
        if let Some((at, c)) = slot.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(c.clone());
        }
        let issuer = IssuerUrl::new(self.cfg.issuer.clone()).map_err(|e| e.to_string())?;
        // Discovery refuses metadata whose `issuer` differs from ours.
        let meta = CoreProviderMetadata::discover_async(issuer, &self.http)
            .await
            .map_err(|e| format!("oidc discovery failed: {}", error_chain(&e)))?;
        let client = CoreClient::from_provider_metadata(
            meta,
            ClientId::new(self.cfg.client_id.clone()),
            self.cfg.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(RedirectUrl::new(self.cfg.redirect_url.clone()).map_err(|e| e.to_string())?);
        *slot = Some((Instant::now(), client.clone()));
        Ok(client)
    }

    /// Begins a login: returns the provider URL to send the browser to and
    /// the `state`, which the caller also pins in a cookie.
    pub async fn start(&self, now: Instant) -> Result<(String, String), String> {
        let client = self.client().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = client
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("email".into()))
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge);
        for s in &self.cfg.scopes {
            req = req.add_scope(Scope::new(s.clone()));
        }
        let (url, state, nonce) = req.url();
        let mut pending = self.pending.lock();
        pending.retain(|_, p| now.duration_since(p.created) < PENDING_TTL);
        if pending.len() >= MAX_PENDING {
            return Err("too many sign-ins in progress".into());
        }
        pending.insert(state.secret().clone(), Pending { verifier, nonce, created: now });
        Ok((url.to_string(), state.secret().clone()))
    }

    /// Finishes a login. `cookie_state` is the state pinned in this
    /// browser's cookie at start: a callback carrying someone else's code
    /// (login CSRF) has no matching cookie. Returns the user's name.
    pub async fn finish(
        &self,
        code: &str,
        state: &str,
        cookie_state: Option<&str>,
        now: Instant,
    ) -> Result<String, String> {
        if !cookie_state.is_some_and(|c| bool::from(c.as_bytes().ct_eq(state.as_bytes()))) {
            return Err("state does not match this browser's sign-in".into());
        }
        let pending = self.pending.lock().remove(state).ok_or("unknown or used state")?;
        if now.duration_since(pending.created) >= PENDING_TTL {
            return Err("sign-in took too long".into());
        }
        let client = self.client().await?;
        let token = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|e| e.to_string())?
            .set_pkce_verifier(pending.verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| format!("token exchange failed: {}", error_chain(&e)))?;
        let id_token = token.extra_fields().id_token().ok_or("the provider returned no ID token")?;
        let verifier = client.id_token_verifier().set_allowed_algs([
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            CoreJwsSigningAlgorithm::RsaSsaPssSha256,
            CoreJwsSigningAlgorithm::EcdsaP256Sha256,
            CoreJwsSigningAlgorithm::EdDsa,
        ]);
        let claims = id_token.claims(&verifier, &pending.nonce).map_err(|e| format!("ID token rejected: {e}"))?;

        let email = claims.email().map(|e| e.as_str().to_owned());
        let verified = claims.email_verified() == Some(true);
        if let Some(email) = &email
            && verified
            && self.cfg.allowed_emails.iter().any(|a| a.eq_ignore_ascii_case(email))
        {
            return Ok(email.clone());
        }
        // `groups` is not a standard claim, so it is read from the payload
        // of the token whose signature and claims were just verified.
        let groups = groups_claim(&id_token.to_string());
        if groups.iter().any(|g| self.cfg.allowed_groups.contains(g)) {
            return Ok(email.unwrap_or_else(|| claims.subject().to_string()));
        }
        Err(format!(
            "{} is not allowed (email_verified: {verified}, groups: {groups:?})",
            email.as_deref().unwrap_or(claims.subject().as_str())
        ))
    }
}

fn groups_claim(jwt: &str) -> Vec<String> {
    use base64::Engine;
    let Some(payload) = jwt.split('.').nth(1) else { return Vec::new() };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return Vec::new() };
    match v.get("groups") {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|g| g.as_str().map(str::to_owned)).collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}
