//! Editing the config file from the API: add and remove pulls and pushes.
//!
//! The file stays the source of truth. An edit is applied to the TOML text
//! with `toml_edit` (comments, order and formatting the operator wrote are
//! kept), the result is parsed and validated like any config, and only then
//! written: to a temporary file in the same directory, flushed to disk and
//! renamed over the original, so a crash or a power cut leaves the old file
//! or the new one, never half of either. The running server then reloads
//! from the parsed result, the same way `POST /api/v1/config/reload` does.
//!
//! This is the part MistServer gets wrong (`docs/research/MISTSERVER-REVERSE.md`
//! §2): it writes its config lazily and in place with `ofstream`.

use std::io::Write as _;
use std::path::Path;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::config::Config;

/// Where an entry lives in the file. Every one is `{ stream, url }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `[[rtsp.pull]]`: a camera or server Caudal pulls from.
    RtspPull,
    /// `[[srt.push]]`: Caudal pushes the stream to a remote SRT listener.
    SrtPush,
    /// `[[srt.pull]]`: Caudal calls a remote SRT listener and publishes it.
    SrtPull,
    /// `[[udp.pull]]`: Caudal listens for MPEG-TS over UDP (or RTP).
    UdpPull,
    /// `[[http.pull]]`: Caudal GETs MPEG-TS served over HTTP(S).
    HttpPull,
    /// `[[rtmp.pull]]`: Caudal plays a stream from a remote RTMP(S) server.
    RtmpPull,
    /// `[[hls.pull]]`: Caudal polls an HLS playlist and fetches its segments.
    HlsPull,
    /// `[[udp.push]]`: Caudal sends MPEG-TS over UDP (or RTP) to a receiver.
    UdpPush,
    /// `[[restream]]`: Caudal pushes the stream over RTMP(S).
    Restream,
    /// `[[webrtc.push]]`: Caudal pushes the stream to a WHIP endpoint.
    WhipPush,
}

impl Kind {
    fn path(self) -> &'static [&'static str] {
        match self {
            Kind::RtspPull => &["rtsp", "pull"],
            Kind::SrtPush => &["srt", "push"],
            Kind::SrtPull => &["srt", "pull"],
            Kind::UdpPull => &["udp", "pull"],
            Kind::HttpPull => &["http", "pull"],
            Kind::RtmpPull => &["rtmp", "pull"],
            Kind::HlsPull => &["hls", "pull"],
            Kind::UdpPush => &["udp", "push"],
            Kind::Restream => &["restream"],
            Kind::WhipPush => &["webrtc", "push"],
        }
    }

    /// The section's name as it appears in the file, for messages.
    pub fn section(self) -> &'static str {
        match self {
            Kind::RtspPull => "[[rtsp.pull]]",
            Kind::SrtPush => "[[srt.push]]",
            Kind::SrtPull => "[[srt.pull]]",
            Kind::UdpPull => "[[udp.pull]]",
            Kind::HttpPull => "[[http.pull]]",
            Kind::RtmpPull => "[[rtmp.pull]]",
            Kind::HlsPull => "[[hls.pull]]",
            Kind::UdpPush => "[[udp.push]]",
            Kind::Restream => "[[restream]]",
            Kind::WhipPush => "[[webrtc.push]]",
        }
    }

    /// The pull kind for a source URL.
    pub fn for_pull(url: &str) -> Result<Kind, String> {
        match scheme(url) {
            Some("rtsp" | "rtsps") => Ok(Kind::RtspPull),
            Some("srt") => Ok(Kind::SrtPull),
            Some("udp" | "rtp") => Ok(Kind::UdpPull),
            Some("http" | "https") if caudal_hls_pull::is_playlist_url(url) => Ok(Kind::HlsPull),
            Some("http" | "https") => Ok(Kind::HttpPull),
            Some("rtmp" | "rtmps") => Ok(Kind::RtmpPull),
            _ => Err(format!(
                "pull url {url:?}: expected rtsp://, rtsps://, srt://, udp://, rtp://, http://, https://, rtmp:// \
                 or rtmps://"
            )),
        }
    }

    /// The push kind for a target URL.
    pub fn for_push(url: &str) -> Result<Kind, String> {
        match scheme(url) {
            Some("rtmp" | "rtmps") => Ok(Kind::Restream),
            Some("srt") => Ok(Kind::SrtPush),
            Some("udp" | "rtp") => Ok(Kind::UdpPush),
            // Plain https:// is ambiguous here; `[[webrtc.push]]` in the
            // file takes it.
            Some("whip" | "whip+http") => Ok(Kind::WhipPush),
            _ => Err(format!(
                "push url {:?}: expected rtmp://, rtmps://, srt://, udp://, rtp://, whip:// or whip+http://",
                redact(url)
            )),
        }
    }

    pub const PULLS: [Kind; 6] =
        [Kind::RtspPull, Kind::SrtPull, Kind::UdpPull, Kind::HttpPull, Kind::HlsPull, Kind::RtmpPull];
    pub const PUSHES: [Kind; 4] = [Kind::SrtPush, Kind::UdpPush, Kind::Restream, Kind::WhipPush];
}

fn scheme(url: &str) -> Option<&str> {
    url.split_once("://").map(|(s, _)| s)
}

/// One `{ stream, url }` entry as read from the running config.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EntryJson {
    pub stream: String,
    pub url: String,
    pub section: &'static str,
}

/// `url` with its secrets masked, for listing: an RTMP stream key (the last
/// path segment, as `caudal-restream` shows it), SRT `passphrase` /
/// `streamid` values, the password of an RTSP camera's `user:pass@`, and
/// for a WHIP URL its user info, query values and any path segment long
/// enough to be a key (see `caudal_webrtc::redact_whip_url`), and for an
/// HTTP(S) pull (`[[http.pull]]`, `[[hls.pull]]`) its password and query
/// values (a CDN's signed token); any other URL its password and query
/// values.
pub fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some(("whip" | "whip+http", _)) => caudal_webrtc::redact_whip_url(url),
        Some(("http" | "https", _)) => caudal_core::net::redact_url(url),
        Some(("rtmp" | "rtmps", rest)) => match rest.rsplit_once('/') {
            Some((head, key)) if !key.is_empty() && head.contains('/') => {
                format!("{}://{head}/****", url.split_once("://").unwrap().0)
            }
            _ => url.to_owned(),
        },
        Some(("srt", rest)) => match rest.split_once('?') {
            Some((addr, query)) => {
                let q: Vec<String> = query
                    .split('&')
                    .map(|kv| match kv.split_once('=') {
                        Some((k, _)) if matches!(k, "passphrase" | "streamid") => format!("{k}=****"),
                        _ => kv.to_owned(),
                    })
                    .collect();
                format!("srt://{addr}?{}", q.join("&"))
            }
            None => url.to_owned(),
        },
        Some((scheme @ ("rtsp" | "rtsps"), rest)) => {
            let (authority, path) = rest.split_once('/').map_or((rest, None), |(a, p)| (a, Some(p)));
            match authority.rsplit_once('@') {
                Some((userinfo, host)) => {
                    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
                    let path = path.map(|p| format!("/{p}")).unwrap_or_default();
                    format!("{scheme}://{user}:****@{host}{path}")
                }
                None => url.to_owned(),
            }
        }
        // Anything else (a mistyped scheme in a config error, `udp://`):
        // a password and query values are still masked.
        _ => caudal_core::net::redact_url(url),
    }
}

/// Every entry of `kinds` in `cfg`, in file order, secrets masked.
pub fn list(cfg: &Config, kinds: &[Kind]) -> Vec<EntryJson> {
    let mut v = Vec::new();
    for &k in kinds {
        let pairs: Vec<(String, String)> = match k {
            Kind::RtspPull => cfg.rtsp.pull.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::SrtPush => cfg.srt.push.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::SrtPull => cfg.srt.pull.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::UdpPull => cfg.udp.pull.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::HttpPull => cfg.http.pull.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::RtmpPull => cfg.rtmp.pull.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::HlsPull => cfg.hls.pull.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::UdpPush => cfg.udp.push.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::Restream => cfg.restream.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
            Kind::WhipPush => cfg.webrtc.push.iter().map(|e| (e.stream.clone(), e.url.clone())).collect(),
        };
        // A WHIP endpoint may be https://, which `redact` leaves alone.
        let mask = |u: &str| if k == Kind::WhipPush { caudal_webrtc::redact_whip_url(u) } else { redact(u) };
        v.extend(pairs.into_iter().map(|(stream, url)| EntryJson { stream, url: mask(&url), section: k.section() }));
    }
    v
}

/// `[[push.auto]]` rules in the same row shape as [`list`]: one row per
/// stream pattern, `url` the template (secrets masked). Read-only here:
/// `DELETE /api/v1/pushes/{stream}` does not remove them; edit the file.
pub fn list_auto_pushes(cfg: &Config) -> Vec<EntryJson> {
    cfg.push
        .auto
        .iter()
        .flat_map(|r| {
            r.streams.iter().map(|p| EntryJson { stream: p.clone(), url: redact(&r.url), section: "[[push.auto]]" })
        })
        .collect()
}

fn array(doc: &mut DocumentMut, kind: Kind, create: bool) -> Result<Option<&mut ArrayOfTables>, String> {
    let path = kind.path();
    let mut table: &mut Table = doc.as_table_mut();
    for key in &path[..path.len() - 1] {
        if !table.contains_key(key) {
            if !create {
                return Ok(None);
            }
            let mut t = Table::new();
            t.set_implicit(true);
            table.insert(key, Item::Table(t));
        }
        table = table
            .get_mut(key)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| format!("[{key}] in the config file is not a table; edit it by hand"))?;
    }
    let last = path[path.len() - 1];
    if !table.contains_key(last) {
        if !create {
            return Ok(None);
        }
        table.insert(last, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    table
        .get_mut(last)
        .and_then(Item::as_array_of_tables_mut)
        .map(Some)
        .ok_or_else(|| format!("{} in the config file is not an array of tables; edit it by hand", kind.section()))
}

fn field<'a>(t: &'a Table, key: &str) -> Option<&'a str> {
    t.get(key).and_then(Item::as_str)
}

/// `text` with `{ stream, url }` appended to `kind`. An identical entry is
/// a conflict, not a second copy.
pub fn add(text: &str, kind: Kind, stream: &str, url: &str) -> Result<String, EditError> {
    add_with(text, kind, stream, url, None)
}

/// [`add`], plus a `token` key (`[[webrtc.push]]` only; refused elsewhere).
pub fn add_with(text: &str, kind: Kind, stream: &str, url: &str, token: Option<&str>) -> Result<String, EditError> {
    if token.is_some() && kind != Kind::WhipPush {
        return Err(EditError::Invalid(format!("`token` only applies to {}", Kind::WhipPush.section())));
    }
    let mut doc: DocumentMut = text.parse().map_err(|e| EditError::Invalid(format!("config file: {e}")))?;
    let arr = array(&mut doc, kind, true).map_err(EditError::Invalid)?.expect("created");
    if arr.iter().any(|t| field(t, "stream") == Some(stream) && field(t, "url") == Some(url)) {
        return Err(EditError::Exists);
    }
    let mut t = Table::new();
    t.insert("stream", value(stream));
    t.insert("url", value(url));
    if let Some(tok) = token {
        t.insert("token", value(tok));
    }
    arr.push(t);
    Ok(doc.to_string())
}

/// `text` without the `kind` entries of `stream` (only the one with `url`
/// when given). `NotFound` when nothing matched.
pub fn remove(text: &str, kind: Kind, stream: &str, url: Option<&str>) -> Result<(String, usize), EditError> {
    let mut doc: DocumentMut = text.parse().map_err(|e| EditError::Invalid(format!("config file: {e}")))?;
    let Some(arr) = array(&mut doc, kind, false).map_err(EditError::Invalid)? else { return Err(EditError::NotFound) };
    let before = arr.len();
    arr.retain(|t| !(field(t, "stream") == Some(stream) && url.is_none_or(|u| field(t, "url") == Some(u))));
    let removed = before - arr.len();
    if removed == 0 {
        return Err(EditError::NotFound);
    }
    Ok((doc.to_string(), removed))
}

#[derive(Debug, PartialEq, Eq)]
pub enum EditError {
    /// The edit would make an invalid file, or the file cannot be edited.
    Invalid(String),
    Exists,
    NotFound,
}

/// Parses and validates `text` as a whole config.
pub fn check(text: &str) -> Result<Config, String> {
    let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

/// Replaces `path` with `contents` atomically: a temporary file next to it,
/// flushed to disk, renamed over it, then the directory flushed so the
/// rename itself survives a power cut. Keeps the original's permissions.
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new().prefix(".caudal-config-").suffix(".tmp").tempfile_in(dir)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    if let Ok(meta) = std::fs::metadata(path) {
        tmp.as_file().set_permissions(meta.permissions())?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# my server\n[server]\nhttp_bind = \"127.0.0.1:8080\"\n\n# cameras\n[[rtsp.pull]]\nstream = \"cam1\" # lobby\nurl = \"rtsp://10.0.0.5/live\"\n";

    #[test]
    fn add_keeps_comments_and_appends() {
        let out = add(FILE, Kind::RtspPull, "cam2", "rtsp://10.0.0.6/live").unwrap();
        assert!(out.starts_with("# my server\n"), "{out}");
        assert!(out.contains("stream = \"cam1\" # lobby"), "inline comment kept: {out}");
        let cfg = check(&out).unwrap();
        assert_eq!(cfg.rtsp.pull.iter().map(|p| p.stream.as_str()).collect::<Vec<_>>(), ["cam1", "cam2"]);
        assert_eq!(add(&out, Kind::RtspPull, "cam2", "rtsp://10.0.0.6/live"), Err(EditError::Exists));
    }

    #[test]
    fn add_creates_missing_sections() {
        let out = add("", Kind::SrtPush, "show", "srt://203.0.113.4:9000").unwrap();
        let out = add(&out, Kind::Restream, "show", "rtmps://live.example/app/KEY").unwrap();
        let cfg = check(&out).unwrap();
        assert_eq!(cfg.srt.push[0].url, "srt://203.0.113.4:9000");
        assert_eq!(cfg.restream[0].url, "rtmps://live.example/app/KEY");
    }

    #[test]
    fn an_hls_pull_sits_beside_the_hls_output_settings() {
        let out = add("[hls]\npart_ms = 100\n", Kind::HlsPull, "tv", "https://cdn.example/live/index.m3u8").unwrap();
        let cfg = check(&out).unwrap();
        assert_eq!(cfg.hls.part_ms, 100, "{out}");
        assert_eq!(cfg.hls.pull[0].url, "https://cdn.example/live/index.m3u8");
        let (gone, n) = remove(&out, Kind::HlsPull, "tv", None).unwrap();
        assert_eq!(n, 1);
        assert!(check(&gone).unwrap().hls.pull.is_empty());
    }

    #[test]
    fn remove_by_stream_or_by_stream_and_url() {
        let two = add(FILE, Kind::RtspPull, "cam1", "rtsp://10.0.0.9/backup").unwrap();
        let (one, n) = remove(&two, Kind::RtspPull, "cam1", Some("rtsp://10.0.0.9/backup")).unwrap();
        assert_eq!(n, 1);
        assert_eq!(check(&one).unwrap().rtsp.pull.len(), 1);
        let (none, n) = remove(&two, Kind::RtspPull, "cam1", None).unwrap();
        assert_eq!(n, 2);
        assert!(check(&none).unwrap().rtsp.pull.is_empty());
        assert!(none.starts_with("# my server\n"));
        assert_eq!(remove(FILE, Kind::RtspPull, "nope", None), Err(EditError::NotFound));
        assert_eq!(remove(FILE, Kind::Restream, "cam1", None), Err(EditError::NotFound), "section absent");
    }

    #[test]
    fn listing_masks_secrets() {
        assert_eq!(redact("rtmps://live.example/app/SECRETKEY"), "rtmps://live.example/app/****");
        assert_eq!(redact("rtmp://h:1935/live/x/KEY"), "rtmp://h:1935/live/x/****");
        assert_eq!(redact("srt://h:9000?passphrase=hunter22&latency=200"), "srt://h:9000?passphrase=****&latency=200");
        assert_eq!(redact("srt://h:9000"), "srt://h:9000");
        assert_eq!(redact("rtsp://admin:s3cret@10.0.0.5:554/live"), "rtsp://admin:****@10.0.0.5:554/live");
        assert_eq!(redact("rtsp://10.0.0.5/live"), "rtsp://10.0.0.5/live");
        assert_eq!(
            redact("https://cdn.example/live/index.m3u8?token=SIGNED&exp=9"),
            "https://cdn.example/live/index.m3u8?token=****&exp=****"
        );
        assert_eq!(redact("http://u:pw@10.0.0.5:8080/stream.ts"), "http://u:****@10.0.0.5:8080/stream.ts");
        assert_eq!(
            redact("whip://customer-a.cloudflarestream.com/0123456789abcdef0123456789abcdef/webRTC/publish"),
            "whip://customer-a.cloudflarestream.com/****/webRTC/publish"
        );
        assert_eq!(redact("srtt://h:9000?passphrase=hunter22"), "srtt://h:9000?passphrase=****", "a typo still masks");
        assert_eq!(redact("udp://239.1.1.1:5000"), "udp://239.1.1.1:5000");
    }

    #[test]
    fn config_errors_mask_secrets() {
        let e = check("[[srt.pull]]\nstream = \"a\"\nurl = \"srtt://h:9000?passphrase=hunter22\"\n").unwrap_err();
        assert!(e.contains("[[srt.pull]]") && !e.contains("hunter22"), "{e}");
        let e = check("[[push.auto]]\nstreams = [\"cam*\"]\nurl = \"rtmp://h/live/SECRETKEY\"\nstart = \"never\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("[[push.auto]]") && !e.contains("SECRETKEY"), "{e}");
    }

    #[test]
    fn whip_pushes_keep_their_token_out_of_listings() {
        let out =
            add_with("", Kind::WhipPush, "show", "https://h.example/whip/0123456789abcdef0123", Some("tok3n")).unwrap();
        let cfg = check(&out).unwrap();
        assert_eq!(cfg.webrtc.push[0].token.as_deref(), Some("tok3n"));
        let rows = list(&cfg, &Kind::PUSHES);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, "https://h.example/whip/****");
        assert_eq!(rows[0].section, "[[webrtc.push]]");
        assert!(!serde_json::to_string(&rows).unwrap().contains("tok3n"));
        assert!(matches!(add_with("", Kind::SrtPush, "a", "srt://h:1", Some("x")), Err(EditError::Invalid(_))));
        let (gone, _) = remove(&out, Kind::WhipPush, "show", None).unwrap();
        assert!(check(&gone).unwrap().webrtc.push.is_empty());
    }

    #[test]
    fn kinds_from_urls() {
        assert_eq!(Kind::for_pull("rtsp://h/x"), Ok(Kind::RtspPull));
        assert!(Kind::for_pull("ftp://h/x").is_err());
        assert_eq!(Kind::for_pull("srt://h:9000?streamid=play/x"), Ok(Kind::SrtPull));
        assert_eq!(Kind::for_pull("udp://239.1.1.1:5000"), Ok(Kind::UdpPull));
        assert_eq!(Kind::for_pull("rtp://:5004"), Ok(Kind::UdpPull));
        assert_eq!(Kind::for_pull("http://10.0.0.5:8080/stream.ts"), Ok(Kind::HttpPull));
        assert_eq!(Kind::for_pull("https://encoder.example/live"), Ok(Kind::HttpPull));
        assert_eq!(Kind::for_pull("rtmp://10.0.0.5/live/cam1"), Ok(Kind::RtmpPull));
        assert_eq!(Kind::for_pull("rtmps://live.example/app/KEY"), Ok(Kind::RtmpPull));
        assert_eq!(Kind::for_pull("https://cdn.example/live/index.m3u8"), Ok(Kind::HlsPull));
        assert_eq!(Kind::for_pull("http://h/Master.M3U8?token=x"), Ok(Kind::HlsPull));
        assert_eq!(Kind::for_pull("http://h/stream.ts?list=a.m3u8"), Ok(Kind::HttpPull), "the path decides");
        assert_eq!(Kind::for_push("rtmp://h/a/k"), Ok(Kind::Restream));
        assert_eq!(Kind::for_push("srt://h:1"), Ok(Kind::SrtPush));
        assert_eq!(Kind::for_push("udp://10.0.0.20:5000"), Ok(Kind::UdpPush));
        assert_eq!(Kind::for_push("rtp://10.0.0.20:5004"), Ok(Kind::UdpPush));
        assert_eq!(Kind::for_push("whip://h/whip/a"), Ok(Kind::WhipPush));
        assert_eq!(Kind::for_push("whip+http://h:8080/whip/a"), Ok(Kind::WhipPush));
        assert!(Kind::for_push("http://h:1").is_err());
    }

    #[test]
    fn a_field_that_is_not_a_table_is_refused() {
        assert!(matches!(add("rtsp = 3\n", Kind::RtspPull, "a", "rtsp://h/x"), Err(EditError::Invalid(_))));
    }

    #[test]
    fn atomic_write_replaces_and_keeps_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("caudal.toml");
        std::fs::write(&p, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        write_atomic(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "secrets stay private");
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().filter_map(Result::ok).collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left behind");
    }
}
