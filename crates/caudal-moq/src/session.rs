//! Viewer sessions: accept, authorize, count.
//!
//! Auth follows moq-relay's convention: the token rides the connect URL as
//! `?jwt=<token>`. moq-native hands us the URL path and query before the
//! session is accepted, so the check happens up front:
//!
//! - `https://host:port/<stream>?jwt=…`: `authorize(Play, stream, token)`;
//!   the session then sees only that broadcast (names stay absolute, so the
//!   player still subscribes to `<stream>`).
//! - `https://host:port/?jwt=…` (or no token): a token the gate accepts for
//!   any stream (probed with the empty name, which no real stream has; with
//!   no gate everything passes) sees every broadcast, including ones that
//!   start later. A narrower token sees the streams it may play that are
//!   live when it connects; for a stream that starts later, connect with
//!   the `/<stream>` path.
//!
//! Refusals close the session with Unauthorized, which players surface as
//! an error instead of waiting.

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Access, Denied, Registry};

/// Accepts sessions until the server stops (Ctrl-C or a fatal listener
/// error). Each session runs in its own task; a misbehaving client only
/// ends its own session.
pub(crate) fn spawn_accept(
    mut server: moq_native::Server,
    registry: Arc<Registry>,
    origin: moq_net::origin::Producer,
    stats: moq_net::stats::Registry,
) {
    let tier = stats.tier(moq_net::stats::Tier::default());
    tokio::spawn(async move {
        while let Some(request) = server.accept().await {
            let (registry, origin, tier) = (registry.clone(), origin.clone(), tier.clone());
            tokio::spawn(async move { serve(request, registry, origin, tier).await });
        }
        tracing::info!("moq: server stopped accepting sessions");
    });
}

async fn serve(
    request: moq_native::Request,
    registry: Arc<Registry>,
    origin: moq_net::origin::Producer,
    tier: moq_net::stats::Handle,
) {
    let token = request.query().and_then(|q| query_param(q, "jwt")).map(str::to_owned);
    let path = request.path().trim_matches('/').to_owned();
    let transport = request.transport();

    let consumer = match allowed(&registry, &origin, &path, token.as_deref()).await {
        Ok(c) => c,
        Err(code) => {
            tracing::debug!(path = %path, code, %transport, "moq: session refused");
            let _ = request.close(code).await;
            return;
        }
    };
    let session = match request.with_publisher(consumer).with_stats(tier.session(path.as_str())).ok().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, %transport, "moq: session handshake failed");
            return;
        }
    };
    tracing::debug!(path = %path, %transport, "moq: viewer session open");
    // Dropping the session closes it: hold it until the peer goes away.
    let err = session.closed().await;
    tracing::debug!(path = %path, reason = %err, "moq: viewer session closed");
}

/// What a session may see, or the HTTP-style status to close it with.
///
/// `ip` is always `None` here: `moq_native::Request` (0.19.19) doesn't
/// expose the transport's peer address for any of its backends (quinn,
/// quiche, iroh, the `noq`/websocket in-process transports), only the MoQ
/// SETUP's URL/path/authority and, for mTLS, the peer's certificate
/// identity. A stream matched by an IP/CIDR `[[access.rules]]` entry is
/// therefore denied here with `reason: "no_ip"` (fail closed, see
/// `caudal_access::rule::evaluate`) rather than silently let through;
/// `country:`-only or token-only rules are unaffected. Revisit if a future
/// `moq-native` release adds a `Request::remote_addr()`.
async fn allowed(
    registry: &Registry,
    origin: &moq_net::origin::Producer,
    path: &str,
    token: Option<&str>,
) -> Result<moq_net::origin::Consumer, u16> {
    let ip = None;
    let all = origin.consume();
    if !path.is_empty() {
        if !caudal_core::media::valid_stream_name(path) {
            return Err(404);
        }
        registry.authorize(Access::Play, path, token, ip).await.map_err(status)?;
        return all.scope(&[moq_net::Path::new(path)]).ok_or(403);
    }
    if registry.authorize(Access::Play, "", token, ip).await.is_ok() {
        return Ok(all);
    }
    let mut names = Vec::new();
    let mut refusal = Denied::Missing;
    for stream in registry.list() {
        match registry.authorize(Access::Play, stream.name(), token, ip).await {
            Ok(()) => names.push(stream.name().to_owned()),
            Err(d) => refusal = d,
        }
    }
    if names.is_empty() {
        return Err(status(refusal));
    }
    let prefixes: Vec<moq_net::Path> = names.iter().map(|n| moq_net::Path::new(n)).collect();
    all.scope(&prefixes).ok_or(403)
}

fn status(d: Denied) -> u16 {
    match d {
        Denied::Missing => 401,
        Denied::Refused(_) => 403,
    }
}

/// The raw value of `key` in a URL query (`a=1&jwt=x.y.z`). JWTs are
/// base64url and dots, never percent-encoded.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// Once a second, reports how many sessions are subscribed to each stream's
/// broadcast (moq-net's egress stats: distinct sessions with at least one
/// open subscription) as the stream's `"moq"` viewers.
pub(crate) fn spawn_viewer_counts(registry: Arc<Registry>, stats: moq_net::stats::Registry) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let report = stats.report();
            for stream in registry.list() {
                let n: u64 = report
                    .traffic
                    .iter()
                    .filter(|e| e.path.as_str() == stream.name())
                    .map(|e| e.publisher.active_broadcasts())
                    .sum();
                stream.set_output_viewers("moq", n as usize);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query() {
        assert_eq!(query_param("jwt=a.b.c", "jwt"), Some("a.b.c"));
        assert_eq!(query_param("x=1&jwt=t", "jwt"), Some("t"));
        assert_eq!(query_param("jwtx=1", "jwt"), None);
        assert_eq!(query_param("", "jwt"), None);
    }

    struct OnlyCam1;
    impl caudal_core::Gate for OnlyCam1 {
        fn check<'a>(
            &'a self,
            _: Access,
            stream: &'a str,
            token: Option<&'a str>,
            _ip: Option<std::net::IpAddr>,
        ) -> caudal_core::GateFuture<'a> {
            Box::pin(async move {
                match token {
                    None => Err(Denied::Missing),
                    Some("all") => Ok(()),
                    Some("cam1") if stream == "cam1" => Ok(()),
                    Some(_) => Err(Denied::Refused("no".into())),
                }
            })
        }
    }

    #[tokio::test]
    async fn authorization() {
        let registry = Registry::new();
        let origin = moq_net::Origin::random().produce();
        // No gate: everything goes.
        assert!(allowed(&registry, &origin, "", None).await.is_ok());
        assert!(allowed(&registry, &origin, "cam1", None).await.is_ok());
        assert_eq!(allowed(&registry, &origin, "../x", None).await.err(), Some(404));

        registry.set_gate(Arc::new(OnlyCam1));
        let _p = registry.publish("cam1", Default::default()).unwrap();
        let _q = registry.publish("cam2", Default::default()).unwrap();
        assert_eq!(allowed(&registry, &origin, "cam1", None).await.err(), Some(401));
        assert_eq!(allowed(&registry, &origin, "cam2", Some("cam1")).await.err(), Some(403));
        assert!(allowed(&registry, &origin, "cam1", Some("cam1")).await.is_ok());
        assert!(allowed(&registry, &origin, "", Some("all")).await.is_ok());
        assert_eq!(allowed(&registry, &origin, "", None).await.err(), Some(401));
        assert!(allowed(&registry, &origin, "", Some("cam1")).await.is_ok());
        assert_eq!(allowed(&registry, &origin, "", Some("bad")).await.err(), Some(403));
    }
}
