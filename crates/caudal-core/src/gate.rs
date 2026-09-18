//! Who may publish or play. The core only asks; the server decides (JWT,
//! allow lists, anything) by installing a [`Gate`] on the [`crate::Registry`].

use std::future::Future;
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
    fn check<'a>(&'a self, access: Access, stream: &'a str, token: Option<&'a str>) -> GateFuture<'a>;
}
