//! Inter-node auth: an HS256 JWT signed with the cluster's shared secret.
//! Edges mint a fresh one per request; origins verify it on the locate
//! endpoint and on the MoQ session (`?jwt=`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// `aud` of every cluster token, so a cluster token is never mistaken for
/// a viewer's play token signed with the same key material, and back.
pub const AUDIENCE: &str = "caudal-cluster";

/// How long a minted token is valid.
const LIFETIME: Duration = Duration::from_secs(300);

/// Shortest accepted secret, in bytes.
pub const MIN_SECRET_LEN: usize = 16;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    iat: u64,
    exp: u64,
}

/// The shared secret, able to mint and verify cluster tokens.
#[derive(Clone)]
pub struct Secret {
    enc: EncodingKey,
    dec: DecodingKey,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SecretError {
    #[error("[cluster] secret must be at least {MIN_SECRET_LEN} bytes")]
    TooShort,
}

impl Secret {
    pub fn new(secret: &str) -> Result<Self, SecretError> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(SecretError::TooShort);
        }
        Ok(Self { enc: EncodingKey::from_secret(secret.as_bytes()), dec: DecodingKey::from_secret(secret.as_bytes()) })
    }

    /// A token for `node_id`, valid for five minutes.
    pub fn mint(&self, node_id: &str) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let claims =
            Claims { iss: node_id.to_owned(), aud: AUDIENCE.to_owned(), iat: now, exp: now + LIFETIME.as_secs() };
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.enc).expect("HS256 encoding cannot fail")
    }

    /// The node id (`iss`) of a valid cluster token.
    pub fn verify(&self, token: &str) -> Option<String> {
        let mut v = Validation::new(Algorithm::HS256);
        v.set_audience(&[AUDIENCE]);
        v.set_required_spec_claims(&["exp", "aud", "iss"]);
        v.leeway = 30;
        jsonwebtoken::decode::<Claims>(token, &self.dec, &v).ok().map(|d| d.claims.iss)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_refusals() {
        let s = Secret::new("0123456789abcdef-shared").unwrap();
        let t = s.mint("edge-1");
        assert_eq!(s.verify(&t).as_deref(), Some("edge-1"));

        let other = Secret::new("a-different-secret-entirely").unwrap();
        assert_eq!(other.verify(&t), None, "wrong secret");
        assert_eq!(s.verify("not.a.jwt"), None);
        assert_eq!(s.verify(""), None);

        // Right secret, wrong audience (e.g. a viewer play token).
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let claims = Claims { iss: "x".into(), aud: "play".into(), iat: now, exp: now + 60 };
        let t = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &s.enc).unwrap();
        assert_eq!(s.verify(&t), None);

        // Expired.
        let claims = Claims { iss: "x".into(), aud: AUDIENCE.into(), iat: now - 1000, exp: now - 600 };
        let t = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &s.enc).unwrap();
        assert_eq!(s.verify(&t), None);
    }

    #[test]
    fn short_secret() {
        assert_eq!(Secret::new("short").unwrap_err(), SecretError::TooShort);
    }
}
