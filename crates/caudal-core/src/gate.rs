//! Who may publish or play. The core only asks; the server decides (JWT,
//! allow lists, anything) by installing a [`Gate`] on the [`crate::Registry`].

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Publish,
    Play,
}

/// Why access was refused, for logs and HTTP status codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// No credentials were given but some are required (HTTP 401).
    Missing,
    /// Credentials were given but are wrong, expired, or not for this (HTTP 403).
    Refused(String),
}

pub type GateFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Denied>> + Send + 'a>>;

pub trait Gate: Send + Sync + 'static {
    /// `ip` is the caller's resolved address (the TCP/UDP/QUIC peer, or the
    /// client behind a trusted reverse proxy for HTTP protocols); `None`
    /// when a protocol has no way to learn it (see callers for which ones).
    fn check<'a>(
        &'a self,
        access: Access,
        stream: &'a str,
        token: Option<&'a str>,
        ip: Option<IpAddr>,
    ) -> GateFuture<'a>;
}

pub type RewriteFuture<'a> = Pin<Box<dyn Future<Output = Result<String, Denied>> + Send + 'a>>;

/// What a [`Rewrite`] is asked about: one publish or play request, before
/// any access check.
#[derive(Debug, Clone, Copy)]
pub struct RewriteRequest<'a> {
    pub access: Access,
    /// The protocol the request came in on (`"rtmp"`, `"srt"`, `"whip"`,
    /// `"whep"`, `"rtsp"`, `"hls"`, `"moq"`).
    pub protocol: &'static str,
    /// The name the client asked for.
    pub stream: &'a str,
    pub token: Option<&'a str>,
    pub ip: Option<IpAddr>,
}

/// Stream routing: maps the name a client asked for to the name it
/// actually publishes to or plays (MistServer's `PUSH_REWRITE` and
/// `PLAY_REWRITE`). The server installs one on the [`crate::Registry`];
/// protocols reach it through [`crate::Registry::resolve`].
pub trait Rewrite: Send + Sync + 'static {
    /// `Ok(name)` to go on under `name` (the requested name to keep it),
    /// `Err` to refuse the request.
    fn rewrite<'a>(&'a self, req: RewriteRequest<'a>) -> RewriteFuture<'a>;
}
