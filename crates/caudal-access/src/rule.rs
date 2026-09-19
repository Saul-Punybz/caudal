//! One `[[access]]` rule: which streams it covers and the allow/deny
//! entries for playing and publishing.

use std::net::IpAddr;

use caudal_core::Cidr;

use crate::AccessDenied;
use crate::geo::GeoDb;

/// One allow/deny list item: a CIDR (bare address = host route), or
/// `country:XX` (ISO 3166-1 alpha-2, case-insensitive on input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Cidr(Cidr),
    Country(String),
}

impl Entry {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.strip_prefix("country:") {
            Some(code) => {
                let code = code.trim();
                if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                    return Err(format!(
                        "`{s}` is not a valid country code (expected `country:XX`, e.g. `country:PR`)"
                    ));
                }
                Ok(Entry::Country(code.to_ascii_uppercase()))
            }
            None => Cidr::parse(s).map(Entry::Cidr),
        }
    }

    fn is_country(&self) -> bool {
        matches!(self, Entry::Country(_))
    }

    fn matches(&self, ip: IpAddr, country: Option<&str>) -> bool {
        match self {
            Entry::Cidr(c) => c.contains(ip),
            Entry::Country(code) => country.is_some_and(|c| c == code),
        }
    }
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Entry::Cidr(c) => write!(f, "{c}"),
            Entry::Country(code) => write!(f, "country:{code}"),
        }
    }
}

/// `streams = ["glob*"]` plus the four allow/deny lists. Empty
/// allow-and-deny on a given action means that action is unrestricted by
/// this rule.
#[derive(Debug, Clone, Default)]
pub struct Rule {
    pub streams: Vec<String>,
    pub play_allow: Vec<Entry>,
    pub play_deny: Vec<Entry>,
    pub publish_allow: Vec<Entry>,
    pub publish_deny: Vec<Entry>,
}

impl Rule {
    pub fn matches_stream(&self, name: &str) -> bool {
        self.streams.iter().any(|p| glob_matches(p, name))
    }

    pub(crate) fn needs_geo(&self) -> bool {
        [&self.play_allow, &self.play_deny, &self.publish_allow, &self.publish_deny]
            .into_iter()
            .any(|list| list.iter().any(Entry::is_country))
    }
}

/// `prefix*` matches by prefix (a bare `*` matches every stream); anything
/// else must match the stream name exactly. Same convention as
/// `caudal-auth`'s token `sub` claim (`crates/caudal-auth/src/claims.rs`).
fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Deny beats allow. An empty allow list is not a whitelist (nothing to
/// narrow to); a non-empty one is (an address matching neither list loses).
pub(crate) fn evaluate(
    allow: &[Entry],
    deny: &[Entry],
    ip: Option<IpAddr>,
    geo: Option<&GeoDb>,
) -> Result<(), AccessDenied> {
    if allow.is_empty() && deny.is_empty() {
        return Ok(());
    }
    let Some(ip) = ip else {
        return Err(AccessDenied {
            reason: "no_ip",
            detail: "no client address known, but this stream has IP/country rules".to_owned(),
        });
    };
    let needs_geo = allow.iter().chain(deny).any(Entry::is_country);
    let country = if needs_geo { geo.and_then(|g| g.country(ip)) } else { None };

    if let Some(e) = deny.iter().find(|e| e.matches(ip, country.as_deref())) {
        let reason = if e.is_country() { "country_denied" } else { "ip_denied" };
        return Err(AccessDenied { reason, detail: format!("{ip} matched deny entry {e}") });
    }
    if allow.is_empty() || allow.iter().any(|e| e.matches(ip, country.as_deref())) {
        return Ok(());
    }
    Err(AccessDenied { reason: "not_allowlisted", detail: format!("{ip} matched no allow entry") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Entry {
        Entry::parse(s).unwrap()
    }

    #[test]
    fn glob_prefix_and_exact() {
        let r = Rule { streams: vec!["live-*".into()], ..Default::default() };
        assert!(r.matches_stream("live-main"));
        assert!(!r.matches_stream("other"));
        let r = Rule { streams: vec!["exact".into()], ..Default::default() };
        assert!(r.matches_stream("exact"));
        assert!(!r.matches_stream("exact-other"));
        let r = Rule { streams: vec!["*".into()], ..Default::default() };
        assert!(r.matches_stream("anything"));
    }

    #[test]
    fn entry_parses_cidr_and_country() {
        assert_eq!(Entry::parse("10.0.0.0/8").unwrap(), Entry::Cidr(Cidr::parse("10.0.0.0/8").unwrap()));
        assert_eq!(Entry::parse("country:pr").unwrap(), Entry::Country("PR".into()));
        assert!(Entry::parse("country:puerto-rico").is_err());
        assert!(Entry::parse("country:p").is_err());
        assert!(Entry::parse("not-an-ip-or-country").is_err());
    }

    #[test]
    fn empty_lists_are_unrestricted() {
        assert_eq!(evaluate(&[], &[], None, None), Ok(()));
        assert_eq!(evaluate(&[], &[], Some("1.2.3.4".parse().unwrap()), None), Ok(()));
    }

    #[test]
    fn deny_wins_over_allow() {
        let allow = [cidr("1.2.3.0/24")];
        let deny = [cidr("1.2.3.4")];
        let err = evaluate(&allow, &deny, Some("1.2.3.4".parse().unwrap()), None).unwrap_err();
        assert_eq!(err.reason, "ip_denied");
    }

    #[test]
    fn deny_only_list_is_a_blocklist_not_a_whitelist() {
        let deny = [cidr("1.2.3.4")];
        assert_eq!(evaluate(&[], &deny, Some("9.9.9.9".parse().unwrap()), None), Ok(()));
        assert!(evaluate(&[], &deny, Some("1.2.3.4".parse().unwrap()), None).is_err());
    }

    #[test]
    fn allow_list_makes_it_a_whitelist() {
        let allow = [cidr("1.2.3.0/24")];
        assert_eq!(evaluate(&allow, &[], Some("1.2.3.9".parse().unwrap()), None), Ok(()));
        let err = evaluate(&allow, &[], Some("9.9.9.9".parse().unwrap()), None).unwrap_err();
        assert_eq!(err.reason, "not_allowlisted");
    }

    #[test]
    fn no_ip_with_rules_present_is_denied() {
        let allow = [cidr("1.2.3.0/24")];
        let err = evaluate(&allow, &[], None, None).unwrap_err();
        assert_eq!(err.reason, "no_ip");
    }

    #[test]
    fn ipv6_cidr_matching() {
        let deny = [cidr("2001:db8::/32")];
        assert!(evaluate(&[], &deny, Some("2001:db8::1".parse().unwrap()), None).is_err());
        assert_eq!(evaluate(&[], &deny, Some("2001:db9::1".parse().unwrap()), None), Ok(()));
    }
}
