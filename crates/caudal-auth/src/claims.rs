//! Token claims shape: `sub` (stream name or `prefix*` pattern), `act`
//! (a single action or an array of them), the standard `exp`/`nbf`, and
//! the optional `ipa` (MistServer's claim: the client IP or CIDR the token
//! is bound to).

use std::net::IpAddr;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct Claims {
    pub sub: String,
    pub act: ActClaim,
    // Present only so `exp`/`nbf` participate in `serde`'s required-field
    // check the way the rest of the claim set does; validity is enforced by
    // `jsonwebtoken::Validation`, not by this struct.
    #[allow(dead_code)]
    pub exp: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub nbf: Option<i64>,
    /// Absent: the token is good from any address (as before `ipa`).
    #[serde(default)]
    pub ipa: Option<OneOrMany>,
}

/// A claim that is a bare string or an array of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn items(&self) -> &[String] {
        match self {
            OneOrMany::One(s) => std::slice::from_ref(s),
            OneOrMany::Many(v) => v,
        }
    }
}

/// Whether `ip` is inside one of the `ipa` entries (an address or a CIDR).
/// `Err` when an entry does not parse: the token is malformed. An
/// IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is judged as IPv4.
pub(crate) fn ipa_allows(ipa: &OneOrMany, ip: IpAddr) -> Result<bool, ()> {
    let ip = ip.to_canonical();
    let mut any = false;
    for entry in ipa.items() {
        let cidr = caudal_core::Cidr::parse(entry).map_err(|_| ())?;
        any |= cidr.contains(ip);
    }
    Ok(any)
}

/// `act` is either a bare string or an array of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ActClaim {
    One(String),
    Many(Vec<String>),
}

impl ActClaim {
    pub(crate) fn allows(&self, action: &str) -> bool {
        match self {
            ActClaim::One(a) => a == action,
            ActClaim::Many(list) => list.iter().any(|a| a == action),
        }
    }
}

/// `sub` matches `stream` exactly, or as a prefix when it ends in `*`
/// (`live-*` matches `live-main`; a bare `*` matches everything).
pub(crate) fn sub_matches(pattern: &str, stream: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => stream.starts_with(prefix),
        None => pattern == stream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(sub_matches("live-main", "live-main"));
        assert!(!sub_matches("live-main", "live-other"));
    }

    #[test]
    fn prefix_match() {
        assert!(sub_matches("live-*", "live-main"));
        assert!(sub_matches("live-*", "live-"));
        assert!(!sub_matches("live-*", "other"));
    }

    #[test]
    fn wildcard_match() {
        assert!(sub_matches("*", "anything"));
        assert!(sub_matches("*", ""));
    }

    #[test]
    fn ipa_entries() {
        let one = OneOrMany::One("203.0.113.7".into());
        assert_eq!(ipa_allows(&one, "203.0.113.7".parse().unwrap()), Ok(true));
        assert_eq!(ipa_allows(&one, "::ffff:203.0.113.7".parse().unwrap()), Ok(true), "mapped v4");
        assert_eq!(ipa_allows(&one, "203.0.113.8".parse().unwrap()), Ok(false));
        let many = OneOrMany::Many(vec!["10.0.0.0/8".into(), "2001:db8::/32".into()]);
        assert_eq!(ipa_allows(&many, "10.1.2.3".parse().unwrap()), Ok(true));
        assert_eq!(ipa_allows(&many, "2001:db8::1".parse().unwrap()), Ok(true));
        assert_eq!(ipa_allows(&many, "192.0.2.1".parse().unwrap()), Ok(false));
        assert_eq!(ipa_allows(&OneOrMany::One("nope".into()), "10.0.0.1".parse().unwrap()), Err(()));
    }

    #[test]
    fn act_allows_single() {
        let a = ActClaim::One("publish".to_string());
        assert!(a.allows("publish"));
        assert!(!a.allows("play"));
    }

    #[test]
    fn act_allows_array() {
        let a = ActClaim::Many(vec!["publish".to_string(), "play".to_string()]);
        assert!(a.allows("publish"));
        assert!(a.allows("play"));
        assert!(!a.allows("delete"));
    }
}
