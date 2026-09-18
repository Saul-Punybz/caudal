//! Token claims shape: `sub` (stream name or `prefix*` pattern), `act`
//! (a single action or an array of them) and the standard `exp`/`nbf`.

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
