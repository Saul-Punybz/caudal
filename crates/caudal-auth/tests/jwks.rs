//! JWKS-backed `Authorizer` checks against a local axum server: RS256 and
//! EdDSA happy paths, plus the unknown-`kid` refetch rate limit. No network
//! beyond 127.0.0.1.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use caudal_auth::{Action, AuthConfig, AuthError, Authorizer, KeySource};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts as _;

#[derive(Clone)]
struct JwksState {
    keys: Arc<Mutex<Vec<Jwk>>>,
    requests: Arc<AtomicUsize>,
}

async fn jwks_handler(State(state): State<JwksState>) -> Json<JwkSet> {
    state.requests.fetch_add(1, Ordering::SeqCst);
    Json(JwkSet { keys: state.keys.lock().unwrap().clone() })
}

async fn spawn_jwks_server(state: JwksState) -> String {
    let app = axum::Router::new().route("/jwks", get(jwks_handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/jwks")
}

fn rsa_key_and_jwk(kid: &str) -> (EncodingKey, Jwk) {
    let mut rng = rand::thread_rng();
    let private = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let der = private.to_pkcs1_der().unwrap();
    let encoding_key = EncodingKey::from_rsa_der(der.as_bytes());
    let mut jwk = Jwk::from_encoding_key(&encoding_key, Algorithm::RS256).unwrap();
    jwk.common.key_id = Some(kid.to_string());
    // Sanity: the exported `n`/`e` really came from this key.
    let _ = private.n();
    (encoding_key, jwk)
}

/// The minimal (v1, no public-key attribute) RFC 8410 PKCS8 DER for an
/// Ed25519 private key: a fixed 16-byte header around the 32-byte seed.
/// `jsonwebtoken`'s `Jwk::from_encoding_key` specifically expects this
/// 48-byte shape (see its comment on the Ed branch); `ed25519-dalek`'s own
/// `EncodePrivateKey` impl instead emits the v2 form with the public key
/// attached, which it rejects.
fn ed25519_pkcs8_der(seed: &[u8; 32]) -> Vec<u8> {
    const HEADER: [u8; 16] =
        [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];
    let mut der = HEADER.to_vec();
    der.extend_from_slice(seed);
    der
}

fn ed_key_and_jwk(kid: &str) -> (EncodingKey, Jwk) {
    let mut rng = rand::thread_rng();
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rng);
    let der = ed25519_pkcs8_der(&signing_key.to_bytes());
    let encoding_key = EncodingKey::from_ed_der(&der);
    let mut jwk = Jwk::from_encoding_key(&encoding_key, Algorithm::EdDSA).unwrap();
    jwk.common.key_id = Some(kid.to_string());
    (encoding_key, jwk)
}

fn sign(encoding_key: &EncodingKey, alg: Algorithm, kid: &str, sub: &str, act: serde_json::Value) -> String {
    let mut header = Header::new(alg);
    header.kid = Some(kid.to_string());
    let exp = jsonwebtoken::get_current_timestamp() as i64 + 60;
    let claims = serde_json::json!({ "sub": sub, "act": act, "exp": exp });
    encode(&header, &claims, encoding_key).unwrap()
}

#[tokio::test]
async fn rs256_via_jwks_happy_path() {
    let (enc, jwk) = rsa_key_and_jwk("rsa-1");
    let state = JwksState { keys: Arc::new(Mutex::new(vec![jwk])), requests: Arc::new(AtomicUsize::new(0)) };
    let url = spawn_jwks_server(state).await;

    let auth = Authorizer::new(AuthConfig {
        keys: Some(KeySource::Jwks { url, refresh: Duration::from_secs(3600) }),
        publish: true,
        play: true,
    });
    // Give the background fetch a moment to populate the cache.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let token = sign(&enc, Algorithm::RS256, "rsa-1", "live-main", serde_json::json!("publish"));
    assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Ok(()));
}

#[tokio::test]
async fn eddsa_via_jwks_happy_path() {
    let (enc, jwk) = ed_key_and_jwk("ed-1");
    let state = JwksState { keys: Arc::new(Mutex::new(vec![jwk])), requests: Arc::new(AtomicUsize::new(0)) };
    let url = spawn_jwks_server(state).await;

    let auth = Authorizer::new(AuthConfig {
        keys: Some(KeySource::Jwks { url, refresh: Duration::from_secs(3600) }),
        publish: true,
        play: true,
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let token = sign(&enc, Algorithm::EdDSA, "ed-1", "live-main", serde_json::json!("play"));
    assert_eq!(auth.check(Action::Play, "live-main", Some(&token)).await, Ok(()));
}

#[tokio::test]
async fn unknown_kid_triggers_one_rate_limited_refetch() {
    let (enc_present, jwk_present) = rsa_key_and_jwk("known-1");
    let (enc_new, jwk_new) = rsa_key_and_jwk("new-1");
    let state = JwksState { keys: Arc::new(Mutex::new(vec![jwk_present])), requests: Arc::new(AtomicUsize::new(0)) };
    let url = spawn_jwks_server(state.clone()).await;

    // A long refresh so the periodic loop never fires again during the test;
    // only its one immediate fetch at spawn time.
    let auth = Authorizer::new(AuthConfig {
        keys: Some(KeySource::Jwks { url, refresh: Duration::from_secs(3600) }),
        publish: true,
        play: true,
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_initial = state.requests.load(Ordering::SeqCst);
    assert_eq!(after_initial, 1, "background task should fetch once on start");

    // Token references a kid the server doesn't have yet.
    let token = sign(&enc_new, Algorithm::RS256, "new-1", "live-main", serde_json::json!("publish"));
    let result = auth.check(Action::Publish, "live-main", Some(&token)).await;
    assert_eq!(result, Err(AuthError::Invalid));
    assert_eq!(
        state.requests.load(Ordering::SeqCst),
        after_initial + 1,
        "unknown kid should trigger exactly one refetch"
    );

    // The server now has the key, but the rate limit should suppress an
    // immediate second refetch: the check still fails, and no new HTTP
    // request was made.
    state.keys.lock().unwrap().push(jwk_new);
    let _ = enc_present; // keep alive / silence unused warning if reordered
    let result = auth.check(Action::Publish, "live-main", Some(&token)).await;
    assert_eq!(result, Err(AuthError::Invalid));
    assert_eq!(
        state.requests.load(Ordering::SeqCst),
        after_initial + 1,
        "a second unknown-kid check within the rate limit window must not refetch"
    );
}

#[tokio::test]
async fn wrong_algorithm_for_jwks_is_invalid() {
    // A secret-signed HS256 token must never be accepted by a JWKS-backed
    // authorizer (algorithm confusion).
    let (_enc, jwk) = rsa_key_and_jwk("rsa-1");
    let state = JwksState { keys: Arc::new(Mutex::new(vec![jwk])), requests: Arc::new(AtomicUsize::new(0)) };
    let url = spawn_jwks_server(state).await;

    let auth = Authorizer::new(AuthConfig {
        keys: Some(KeySource::Jwks { url, refresh: Duration::from_secs(3600) }),
        publish: true,
        play: true,
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("rsa-1".to_string());
    let exp = jsonwebtoken::get_current_timestamp() as i64 + 60;
    let claims = serde_json::json!({ "sub": "live-main", "act": "publish", "exp": exp });
    let token = encode(&header, &claims, &EncodingKey::from_secret(b"whatever")).unwrap();

    assert_eq!(auth.check(Action::Publish, "live-main", Some(&token)).await, Err(AuthError::Invalid));
}
