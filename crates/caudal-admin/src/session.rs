//! Server-side sessions and their CSRF tokens.
//!
//! The cookie carries a random 256-bit id; the store keys sessions by the
//! id's SHA-256, so a lookup's timing says nothing about real ids and a
//! memory dump holds no usable cookie.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use subtle::ConstantTimeEq;

use crate::password::{random_token, token_digest};

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub user: String,
    /// Synchronizer token: the UI reads it from `/api/v1/auth/session` and
    /// sends it back as `X-CSRF-Token` on every state-changing request.
    pub csrf: String,
    pub expires: Instant,
}

impl Session {
    /// Constant-time check of a presented CSRF token.
    pub fn csrf_ok(&self, presented: Option<&str>) -> bool {
        presented.is_some_and(|p| bool::from(p.as_bytes().ct_eq(self.csrf.as_bytes())))
    }
}

pub(crate) struct Sessions {
    ttl: Duration,
    map: Mutex<HashMap<[u8; 32], Session>>,
}

impl Sessions {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, map: Mutex::new(HashMap::new()) }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Starts a session; returns the cookie value and the session.
    pub fn create(&self, user: &str, now: Instant) -> (String, Session) {
        let id = random_token(32);
        let session = Session { user: user.to_owned(), csrf: random_token(32), expires: now + self.ttl };
        let mut map = self.map.lock();
        map.retain(|_, s| s.expires > now);
        map.insert(token_digest(&id), session.clone());
        (id, session)
    }

    /// The live session for a cookie value; expired ones are dropped.
    pub fn get(&self, id: &str, now: Instant) -> Option<Session> {
        let key = token_digest(id);
        let mut map = self.map.lock();
        match map.get(&key) {
            Some(s) if s.expires > now => Some(s.clone()),
            Some(_) => {
                map.remove(&key);
                None
            }
            None => None,
        }
    }

    pub fn remove(&self, id: &str) {
        self.map.lock().remove(&token_digest(id));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_expire_after_the_ttl() {
        let store = Sessions::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let (id, s) = store.create("ana", t0);
        assert_eq!(s.user, "ana");
        assert!(store.get(&id, t0 + Duration::from_secs(59)).is_some());
        assert!(store.get(&id, t0 + Duration::from_secs(60)).is_none(), "expiry is exclusive");
        assert_eq!(store.len(), 0, "expired session dropped on lookup");
        assert!(store.get("not-a-session", t0).is_none());
    }

    #[test]
    fn logout_removes_and_new_logins_prune() {
        let store = Sessions::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let (a, _) = store.create("ana", t0);
        let (b, _) = store.create("ben", t0);
        assert_ne!(a, b);
        store.remove(&a);
        assert!(store.get(&a, t0).is_none());
        assert!(store.get(&b, t0).is_some());
        // A login after b's expiry prunes it.
        store.create("cy", t0 + Duration::from_secs(61));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn csrf_token_must_match_exactly() {
        let store = Sessions::new(Duration::from_secs(60));
        let (_, s) = store.create("ana", Instant::now());
        assert!(s.csrf_ok(Some(&s.csrf.clone())));
        assert!(!s.csrf_ok(None));
        assert!(!s.csrf_ok(Some("")));
        assert!(!s.csrf_ok(Some(&s.csrf[..s.csrf.len() - 1])));
        let (_, other) = store.create("ana", Instant::now());
        assert!(!s.csrf_ok(Some(&other.csrf)), "another session's token is refused");
    }
}
