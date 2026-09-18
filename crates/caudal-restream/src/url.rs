//! Parsing and redacting `rtmp://`/`rtmps://` target URLs.
//!
//! A target URL is `scheme://host[:port]/app[/more-app-segments]/stream_key`.
//! The stream key is always the last path segment; everything before it is
//! the RTMP "app" name passed to `connect`. Redaction keeps the app path and
//! replaces the key so it never reaches a log line or the status API.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// The RTMP application name, e.g. `live2`. May be empty.
    pub app: String,
    /// The stream key: never logged.
    pub stream_key: String,
    /// `scheme://host[:port]/app`, used as `tcUrl`. Never includes the key.
    pub tc_url: String,
}

pub(crate) fn parse_target_url(url: &str) -> Result<ParsedUrl, String> {
    let (scheme, rest) = url.split_once("://").ok_or("missing scheme (expected rtmp:// or rtmps://)")?;
    let tls = match scheme {
        "rtmp" => false,
        "rtmps" => true,
        other => return Err(format!("unsupported scheme {other:?} (expected rtmp or rtmps)")),
    };
    let (authority, path) = rest.split_once('/').ok_or("missing path (expected /app/stream_key)")?;
    if authority.is_empty() {
        return Err("missing host".to_owned());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse::<u16>().map_err(|_| "invalid port".to_owned())?),
        None => (authority.to_owned(), if tls { 443 } else { 1935 }),
    };
    let (app, stream_key) = match path.rsplit_once('/') {
        Some((a, k)) => (a.to_owned(), k.to_owned()),
        None => (String::new(), path.to_owned()),
    };
    if stream_key.is_empty() {
        return Err("missing stream key".to_owned());
    }
    let tc_url =
        if app.is_empty() { format!("{scheme}://{authority}") } else { format!("{scheme}://{authority}/{app}") };
    Ok(ParsedUrl { tls, host, port, app, stream_key, tc_url })
}

/// `scheme://host/app/****`: the key is never included, whatever it is.
pub(crate) fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "****".to_owned();
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return format!("{scheme}://{rest}/****");
    };
    match path.rsplit_once('/') {
        Some((app, _key)) if !app.is_empty() => format!("{scheme}://{authority}/{app}/****"),
        _ => format!("{scheme}://{authority}/****"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtmp_and_rtmps() {
        let p = parse_target_url("rtmp://a.example.com/live/secretkey").unwrap();
        assert!(!p.tls);
        assert_eq!(p.host, "a.example.com");
        assert_eq!(p.port, 1935);
        assert_eq!(p.app, "live");
        assert_eq!(p.stream_key, "secretkey");
        assert_eq!(p.tc_url, "rtmp://a.example.com/live");

        let p = parse_target_url("rtmps://a.rtmps.youtube.com:443/live2/xxxx-yyyy-zzzz").unwrap();
        assert!(p.tls);
        assert_eq!(p.port, 443);
        assert_eq!(p.app, "live2");
        assert_eq!(p.stream_key, "xxxx-yyyy-zzzz");
    }

    #[test]
    fn defaults_the_port() {
        assert_eq!(parse_target_url("rtmps://host/app/key").unwrap().port, 443);
        assert_eq!(parse_target_url("rtmp://host/app/key").unwrap().port, 1935);
    }

    #[test]
    fn rejects_malformed_urls() {
        assert!(parse_target_url("http://host/app/key").is_err());
        assert!(parse_target_url("rtmp://").is_err());
        assert!(parse_target_url("rtmp://host").is_err());
        assert!(parse_target_url("rtmp://host/").is_err());
    }

    #[test]
    fn redacts_the_stream_key() {
        assert_eq!(redact_url("rtmp://a.example.com/live/super-secret-key"), "rtmp://a.example.com/live/****");
        assert_eq!(
            redact_url("rtmps://a.rtmps.youtube.com:443/live2/xxxx-yyyy-zzzz-wwww"),
            "rtmps://a.rtmps.youtube.com:443/live2/****"
        );
        assert!(!redact_url("rtmp://host/app/super-secret-key").contains("super-secret-key"));
    }
}
