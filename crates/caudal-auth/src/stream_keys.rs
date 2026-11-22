//! Stream keys: a secret an encoder publishes with in place of the stream
//! name (MistServer's `streamkeys {key: stream}`), so the name is never
//! the secret. Implements [`caudal_core::StreamKeys`].
//!
//! Keys are kept only as SHA-256 digests. A lookup hashes what the client
//! sent and compares it with every digest in constant time, without an
//! early exit, so neither the timing nor the length of the answer says
//! which key (if any) came close.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use caudal_core::Access;
use sha2::{Digest, Sha256};
use subtle::{ConditionallySelectable, ConstantTimeEq};

/// Shortest key accepted, so a key is never a guessable word.
pub const MIN_KEY_LEN: usize = 16;
/// Longest key accepted (the longest stream name).
pub const MAX_KEY_LEN: usize = 128;

/// One key: what it publishes or plays as.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamKey {
    pub key: String,
    pub stream: String,
    pub publish: bool,
    pub play: bool,
}

impl std::fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamKey")
            .field("key", &"****")
            .field("stream", &self.stream)
            .field("publish", &self.publish)
            .field("play", &self.play)
            .finish()
    }
}

/// Why a key is not usable. Never contains the key itself.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.len() < MIN_KEY_LEN || key.len() > MAX_KEY_LEN {
        return Err(format!("a stream key must be {MIN_KEY_LEN} to {MAX_KEY_LEN} characters"));
    }
    if !key.bytes().all(is_key_byte) {
        return Err("a stream key may only contain letters, digits, `-` and `_`".into());
    }
    Ok(())
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// `caudal_stream_key_requests_total`: by action, hit (the request came
/// with a key) or miss (its name was not a key). Never labelled with a key.
#[derive(Debug, Default)]
pub struct StreamKeyStats {
    hits: [AtomicU64; 2],
    misses: [AtomicU64; 2],
    /// Publishes refused because the stream takes its key only.
    refused: [AtomicU64; 2],
}

/// One row of [`StreamKeyStats::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamKeyMetric {
    pub action: &'static str,
    pub result: &'static str,
    pub count: u64,
}

fn idx(a: Access) -> usize {
    match a {
        Access::Publish => 0,
        Access::Play => 1,
    }
}

impl StreamKeyStats {
    pub fn snapshot(&self) -> Vec<StreamKeyMetric> {
        let mut out = Vec::with_capacity(6);
        for (i, action) in ["publish", "play"].into_iter().enumerate() {
            for (result, counter) in [("hit", &self.hits), ("miss", &self.misses), ("refused", &self.refused)] {
                out.push(StreamKeyMetric { action, result, count: counter[i].load(Ordering::Relaxed) });
            }
        }
        out
    }
}

struct Entry {
    digest: [u8; 32],
    stream: String,
    publish: bool,
    play: bool,
}

/// The installed table. Built from config on every reload.
pub struct StreamKeyTable {
    entries: Vec<Entry>,
    /// `stream_keys_only`: a stream with a publish key refuses a publish
    /// that came without one.
    keys_only: bool,
    stats: Arc<StreamKeyStats>,
}

impl StreamKeyTable {
    /// `keys` must already be validated (see [`validate_key`]); an
    /// invalid one is still stored, it just can never match a valid name.
    pub fn new(keys: &[StreamKey], keys_only: bool, stats: Arc<StreamKeyStats>) -> Self {
        let entries = keys
            .iter()
            .map(|k| Entry { digest: digest(&k.key), stream: k.stream.clone(), publish: k.publish, play: k.play })
            .collect();
        Self { entries, keys_only, stats }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The index of the entry `presented` is the key of, compared against
    /// every entry in constant time.
    fn find(&self, presented: &str) -> Option<&Entry> {
        // Shape is not secret: anything that cannot be a key skips hashing.
        if presented.len() < MIN_KEY_LEN || presented.len() > MAX_KEY_LEN || !presented.bytes().all(is_key_byte) {
            return None;
        }
        let d = digest(presented);
        let mut found = subtle::Choice::from(0);
        let mut index = 0u64;
        for (i, e) in self.entries.iter().enumerate() {
            let hit = e.digest.ct_eq(&d);
            index.conditional_assign(&(i as u64), hit & !found);
            found |= hit;
        }
        if bool::from(found) { self.entries.get(index as usize) } else { None }
    }
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn allows(e: &Entry, access: Access) -> bool {
    match access {
        Access::Publish => e.publish,
        Access::Play => e.play,
    }
}

impl caudal_core::StreamKeys for StreamKeyTable {
    fn by_name(&self, access: Access, presented: &str) -> Option<String> {
        match self.find(presented).filter(|e| allows(e, access)) {
            Some(e) => {
                self.stats.hits[idx(access)].fetch_add(1, Ordering::Relaxed);
                Some(e.stream.clone())
            }
            None => {
                self.stats.misses[idx(access)].fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    fn by_token(&self, access: Access, token: &str, stream: &str) -> bool {
        let ok = self.find(token).is_some_and(|e| allows(e, access) && e.stream == stream);
        if ok {
            self.stats.hits[idx(access)].fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    fn requires_key(&self, access: Access, stream: &str) -> bool {
        let required =
            self.keys_only && access == Access::Publish && self.entries.iter().any(|e| e.publish && e.stream == stream);
        if required {
            self.stats.refused[idx(access)].fetch_add(1, Ordering::Relaxed);
        }
        required
    }

    fn is_key(&self, s: &str) -> bool {
        self.find(s).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudal_core::StreamKeys as _;

    const KEY: &str = "sk_0123456789abcdefXYZ";

    fn key(k: &str, stream: &str, publish: bool, play: bool) -> StreamKey {
        StreamKey { key: k.into(), stream: stream.into(), publish, play }
    }

    fn table(keys_only: bool) -> (StreamKeyTable, Arc<StreamKeyStats>) {
        let stats = Arc::<StreamKeyStats>::default();
        let t = StreamKeyTable::new(
            &[
                key(KEY, "live-main", true, false),
                key("watch-only-key-0000000", "live-main", false, true),
                key("second-key-for-backup1", "live-backup", true, true),
            ],
            keys_only,
            stats.clone(),
        );
        (t, stats)
    }

    #[test]
    fn key_maps_to_its_stream_for_its_actions_only() {
        let (t, stats) = table(false);
        assert_eq!(t.by_name(Access::Publish, KEY), Some("live-main".into()));
        assert_eq!(t.by_name(Access::Play, KEY), None, "publish-only key");
        assert_eq!(t.by_name(Access::Play, "watch-only-key-0000000"), Some("live-main".into()));
        assert_eq!(t.by_name(Access::Publish, "second-key-for-backup1"), Some("live-backup".into()));
        assert_eq!(t.by_name(Access::Publish, "sk_0123456789abcdefXYz"), None, "one character off");
        assert_eq!(t.by_name(Access::Publish, "live-main"), None, "the name is not the key");
        let rows = stats.snapshot();
        let get = |a, r| rows.iter().find(|m| m.action == a && m.result == r).unwrap().count;
        assert_eq!((get("publish", "hit"), get("publish", "miss")), (2, 2));
        assert_eq!((get("play", "hit"), get("play", "miss")), (1, 1));
    }

    #[test]
    fn token_slot_only_for_its_own_stream() {
        let (t, _) = table(false);
        assert!(t.by_token(Access::Publish, KEY, "live-main"));
        assert!(!t.by_token(Access::Publish, KEY, "live-backup"));
        assert!(!t.by_token(Access::Play, KEY, "live-main"));
        assert!(!t.by_token(Access::Publish, "eyJ.a.b", "live-main"));
    }

    #[test]
    fn keys_only_refuses_publishes_without_the_key() {
        assert!(!table(false).0.requires_key(Access::Publish, "live-main"));
        let (t, stats) = table(true);
        assert!(t.requires_key(Access::Publish, "live-main"));
        assert!(!t.requires_key(Access::Play, "live-main"));
        assert!(!t.requires_key(Access::Publish, "other"));
        assert!(stats.snapshot().iter().any(|m| m.action == "publish" && m.result == "refused" && m.count == 1));
    }

    #[test]
    fn validation_and_debug_never_show_the_key() {
        assert!(validate_key(KEY).is_ok());
        assert!(validate_key("short").is_err());
        assert!(validate_key("has a space in it ok?").is_err());
        assert!(validate_key("dots.are.not.allowed.x").is_err());
        assert!(validate_key(&"x".repeat(129)).is_err());
        let dbg = format!("{:?}", key(KEY, "live-main", true, false));
        assert!(!dbg.contains(KEY) && dbg.contains("****"), "{dbg}");
        assert!(table(false).0.is_key(KEY));
        assert!(!table(false).0.is_key("live-main"));
    }
}
