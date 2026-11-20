//! Viewer sessions: who is watching what right now, over which protocol,
//! from where, and how much has been sent; and a way to end one.
//!
//! An output opens a [`Session`] when it starts serving a viewer and keeps
//! it for the connection's life; dropping it closes the session. The output
//! adds the bytes it sends and watches [`Session::kicked`] so an operator
//! can end the connection. A kick can also block the viewer's address from
//! that stream for a while ([`Sessions::kick`]), checked in
//! [`crate::Registry::authorize`] before any gate, so a kicked viewer does
//! not just reconnect.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::watch;

/// Blocks kept before expired ones are swept.
const SWEEP_AT: usize = 10_000;
/// Longest block a kick may ask for.
pub const MAX_BLOCK: Duration = Duration::from_secs(24 * 3600);

struct Entry {
    id: u64,
    stream: Arc<str>,
    protocol: &'static str,
    ip: Option<IpAddr>,
    started_unix: u64,
    started: Instant,
    bytes_out: AtomicU64,
    kick: watch::Sender<bool>,
}

/// One session as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: u64,
    pub stream: String,
    pub protocol: &'static str,
    pub ip: Option<IpAddr>,
    /// Unix seconds.
    pub started: u64,
    pub duration_secs: u64,
    pub bytes_out: u64,
}

/// What [`Sessions::kick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickResult {
    NotFound,
    /// Ended; `blocked` says whether its address is now blocked.
    Kicked {
        blocked: bool,
    },
}

/// Every open session plus the kick blocks. One per [`crate::Registry`].
#[derive(Default)]
pub struct Sessions {
    next_id: AtomicU64,
    open: Mutex<HashMap<u64, Arc<Entry>>>,
    /// `(stream, ip)` → blocked until.
    blocked: Mutex<HashMap<(Arc<str>, IpAddr), Instant>>,
}

impl Sessions {
    pub(crate) fn open(self: &Arc<Self>, stream: Arc<str>, protocol: &'static str, ip: Option<IpAddr>) -> Session {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (kick, rx) = watch::channel(false);
        let started_unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let entry = Arc::new(Entry {
            id,
            stream,
            protocol,
            ip,
            started_unix,
            started: Instant::now(),
            bytes_out: AtomicU64::new(0),
            kick,
        });
        self.open.lock().insert(id, entry.clone());
        Session { entry, sessions: self.clone(), kicked: rx }
    }

    /// Open sessions, oldest first; only `stream`'s when given.
    pub fn list(&self, stream: Option<&str>) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> = self
            .open
            .lock()
            .values()
            .filter(|e| stream.is_none_or(|s| &*e.stream == s))
            .map(|e| SessionInfo {
                id: e.id,
                stream: e.stream.to_string(),
                protocol: e.protocol,
                ip: e.ip,
                started: e.started_unix,
                duration_secs: e.started.elapsed().as_secs(),
                bytes_out: e.bytes_out.load(Ordering::Relaxed),
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// Ends session `id`. With `block`, its address may not play that
    /// stream again for that long (capped at [`MAX_BLOCK`]); a session
    /// without an address cannot be blocked.
    pub fn kick(&self, id: u64, block: Option<Duration>) -> KickResult {
        let Some(e) = self.open.lock().get(&id).cloned() else { return KickResult::NotFound };
        let blocked = match (block, e.ip) {
            (Some(d), Some(ip)) if !d.is_zero() => {
                let now = Instant::now();
                let mut b = self.blocked.lock();
                if b.len() >= SWEEP_AT {
                    b.retain(|_, until| *until > now);
                }
                b.insert((e.stream.clone(), ip), now + d.min(MAX_BLOCK));
                true
            }
            _ => false,
        };
        let _ = e.kick.send(true);
        tracing::info!(session = id, stream = %e.stream, protocol = e.protocol, ip = ?e.ip, blocked, "session kicked");
        KickResult::Kicked { blocked }
    }

    /// Whether `ip` is blocked from playing `stream` by a kick.
    pub fn is_blocked(&self, stream: &str, ip: IpAddr) -> bool {
        let mut b = self.blocked.lock();
        if b.is_empty() {
            return false;
        }
        let key = (Arc::<str>::from(stream), ip);
        match b.get(&key) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                b.remove(&key);
                false
            }
            None => false,
        }
    }

    fn close(&self, id: u64) {
        self.open.lock().remove(&id);
    }
}

/// One open viewer session. Dropping it closes the session.
pub struct Session {
    entry: Arc<Entry>,
    sessions: Arc<Sessions>,
    kicked: watch::Receiver<bool>,
}

impl Session {
    pub fn id(&self) -> u64 {
        self.entry.id
    }

    /// Counts `n` bytes sent to the viewer.
    pub fn add_bytes(&self, n: usize) {
        self.entry.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Whether an operator ended this session.
    pub fn is_kicked(&self) -> bool {
        *self.kicked.borrow()
    }

    /// Resolves when an operator ends this session. Cancel-safe: meant for
    /// a `select!` arm next to the output's own work.
    pub async fn kicked(&mut self) {
        let _ = self.kicked.wait_for(|k| *k).await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.sessions.close(self.entry.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_list_kick_and_close() {
        let s = Arc::new(Sessions::default());
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        let mut a = s.open("cam".into(), "srt", Some(ip));
        let b = s.open("other".into(), "rtsp", None);
        a.add_bytes(1316);
        a.add_bytes(1316);

        let all = s.list(None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].bytes_out, 2632);
        assert_eq!(s.list(Some("other")).iter().map(|x| x.id).collect::<Vec<_>>(), vec![b.id()]);

        assert_eq!(s.kick(999, None), KickResult::NotFound);
        assert_eq!(s.kick(a.id(), Some(Duration::from_secs(60))), KickResult::Kicked { blocked: true });
        tokio::time::timeout(Duration::from_secs(1), a.kicked()).await.expect("kick wakes the session");
        assert!(a.is_kicked());
        assert!(s.is_blocked("cam", ip));
        assert!(!s.is_blocked("other", ip), "blocks are per stream");

        assert_eq!(s.kick(b.id(), Some(Duration::from_secs(60))), KickResult::Kicked { blocked: false }, "no address");
        drop(a);
        drop(b);
        assert!(s.list(None).is_empty(), "dropping closes");
    }

    #[test]
    fn blocks_expire() {
        let s = Sessions::default();
        let ip: IpAddr = "198.51.100.1".parse().unwrap();
        s.blocked.lock().insert(("cam".into(), ip), Instant::now() - Duration::from_secs(1));
        assert!(!s.is_blocked("cam", ip));
        assert!(s.blocked.lock().is_empty(), "expired entry removed");
    }
}
