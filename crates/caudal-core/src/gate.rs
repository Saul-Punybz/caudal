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
