//! `caudal import-mist`: reads a MistServer `config.json` (also commonly
//! named `mistserver.conf`; it is JSON either way, MistServer never writes
//! an INI-style file) and produces a Caudal `caudal.toml` plus a report of
//! every setting that was translated, approximated, or has no Caudal
//! equivalent. `docs/IMPORT-MIST.md` has the full mapping table.
//!
//! MistServer's controller config has no published JSON Schema, so the
//! shape below is read from its own source
//! (github.com/DDVTECH/mistserver, Unlicense), not from documentation; see
//! `docs/research/MISTSERVER-REVERSE.md` §2.2, §3, §4.2 and §4.3:
//!
//! - `config.protocols`: the connector list. `CheckProtocols` in
//!   `src/controller/controller_connectors.cpp` reads `(*ait)["connector"]`
//!   plus whatever other keys that connector's `capa["required"]` /
//!   `capa["optional"]` need (typically `port`, `interface`). Connector
//!   names come from each output's own `capa["name"]`, e.g. `"RTMP"`
//!   (`src/output/output_rtmp.cpp:503`), `"HLS"` (`output_hls.cpp:193`,
//!   which sets `capa["deps"] = "HTTP"` — HLS rides the HTTP connector's
//!   port, it has none of its own), `"TSSRT"` (`output_tssrt.cpp:410`),
//!   `"RTSP"` (`output_rtsp.cpp:63`), `"WebRTC"` (`output_webrtc.cpp:547`),
//!   `"HTTPTS"` (`output_httpts.cpp:60`), `"CMAF"` (`output_cmaf.cpp:128`),
//!   `"HTTPS"` (`output_https.cpp:60`).
//! - `streams.<name>.source` / `.DVR`: `CheckStreams`'s doc comment in
//!   `src/controller/controller_streams.cpp` (~line 260-300). `source` is
//!   a `push://<host>[@password]` (host empty = any publisher), a pull URL,
//!   or a local file/folder path. Pull schemes are each input's
//!   `source_match`: `srt://` (`input_tssrt.cpp:50-159`), `tsudp://`,
//!   `http-ts://` and http(s) `.ts` (`input_ts.cpp:177-252`), `.m3u8` over
//!   http(s) and `http-hls://` (`input_hls.cpp:551-597`), `rtsp://`
//!   (`input_rtsp.cpp:42-64`), `rtmp://`. `DVR` is the requested DVR buffer
//!   size **in milliseconds**.
//! - `auto_push` (and the legacy `autopushes` array, upgraded to
//!   `auto_push` the first time a MistServer instance with them loads —
//!   see `controller_storage.cpp` ~line 650): each entry has `stream`,
//!   `target`, and optional scheduling (`scheduletime`, `completetime`,
//!   both unix seconds; `start_rule`, `end_rule`, `inhibit`) —
//!   `controller_push.cpp`'s `makePushObject` (~line 50-97, 530-570).
//!   Network targets use the TS output's `tsudp://` / `tsrtp://`, SRT's
//!   `srt://` and RTMP's `rtmp(s)://` push URLs (`lib/stream.cpp:914-990`).
//! - `config.triggers`: an object keyed by trigger name (`STREAM_PUSH`,
//!   `PUSH_REWRITE`, `USER_NEW`, ...), each value one entry per registered
//!   handler, either `[handler, sync, streams, params, default]` or
//!   `{handler, sync, streams, params, default}`
//!   (`controller_storage.cpp` ~line 979-1031). A failing or non-blocking
//!   handler answers its `default`, `"true"` when absent
//!   (`lib/triggers.cpp:289-297`).
//! - `account.<user>.password`: an MD5 hash (`Secure::md5`,
//!   `src/controller/controller.cpp`'s `createAccount`) — a different
//!   format from Caudal's argon2id, so users are never migrated.
//!
//! The output TOML is validated the same way `caudal check` validates any
//! config file (parse + [`crate::config::Config::validate`]) before it is
//! written; [`run`] fails loudly and writes nothing if that fails. Each
//! pull and push is also checked on its own (the same URL parsers the
//! server uses) before it goes in, so one URL Caudal cannot use becomes a
//! line in the report instead of failing the whole import.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;

use serde::Deserialize;
use serde_json::Value as Json;

use crate::config;

/// How well one MistServer setting carries over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// A direct, lossless mapping.
    Translated,
    /// Mapped, but something had to change: units, per-stream folded into
    /// a global setting, a URL path or a payload format that differs, ...
    Approximated,
    /// Nothing in Caudal does this; the setting is not in the output.
    NoEquivalent,
}

#[derive(Debug, Clone)]
pub struct Note {
    pub kind: NoteKind,
    /// Where this came from in the Mist config, e.g. `streams.cam1.DVR`.
    pub mist: String,
    pub detail: String,
}

fn note(kind: NoteKind, mist: impl Into<String>, detail: impl Into<String>) -> Note {
    Note { kind, mist: mist.into(), detail: detail.into() }
}

#[derive(Debug)]
pub struct ImportResult {
    pub toml: String,
    pub notes: Vec<Note>,
}

impl ImportResult {
    /// A human-readable report, grouped by [`NoteKind`], for stdout.
    pub fn report(&self) -> String {
        let mut out = String::new();
        let counts = (
            self.notes.iter().filter(|n| n.kind == NoteKind::Translated).count(),
            self.notes.iter().filter(|n| n.kind == NoteKind::Approximated).count(),
            self.notes.iter().filter(|n| n.kind == NoteKind::NoEquivalent).count(),
        );
        let _ = writeln!(out, "{} translated, {} approximated, {} not imported", counts.0, counts.1, counts.2);
        for (kind, heading) in [
            (NoteKind::Translated, "\nTranslated"),
            (NoteKind::Approximated, "\nApproximated (imported, read the note before going live)"),
            (NoteKind::NoEquivalent, "\nNot imported (no Caudal equivalent, or a different meaning)"),
        ] {
            let matching: Vec<&Note> = self.notes.iter().filter(|n| n.kind == kind).collect();
            if matching.is_empty() {
                continue;
            }
            let _ = writeln!(out, "{heading}:");
            for n in matching {
                let _ = writeln!(out, "  {}: {}", n.mist, n.detail);
            }
        }
        out
    }
}

// ---- MistServer config.json shape (see module docs for source refs) ----

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct MistFile {
    config: MistConfigSection,
    streams: BTreeMap<String, MistStream>,
    auto_push: Json,
    autopushes: Json,
    account: BTreeMap<String, Json>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct MistConfigSection {
    protocols: Vec<MistProtocol>,
    triggers: BTreeMap<String, Json>,
}

#[derive(Debug, Deserialize)]
struct MistProtocol {
    connector: String,
    #[serde(flatten)]
    rest: serde_json::Map<String, Json>,
}

#[derive(Debug, Deserialize)]
struct MistStream {
    #[serde(default)]
    source: Option<String>,
    #[serde(rename = "DVR", default)]
    dvr_ms: Option<u64>,
    #[serde(flatten)]
    rest: serde_json::Map<String, Json>,
}

fn protocol_port(p: &MistProtocol) -> Option<u16> {
    match p.rest.get("port") {
        Some(Json::Number(n)) => n.as_u64().and_then(|v| u16::try_from(v).ok()),
        Some(Json::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The connector's `interface` (the address MistServer binds), when it is
/// an IP address. Empty or absent: all interfaces.
fn protocol_interface(p: &MistProtocol) -> Option<IpAddr> {
    p.rest.get("interface").and_then(Json::as_str).and_then(|s| s.trim().parse().ok())
}

fn protocol_str<'a>(p: &'a MistProtocol, key: &str) -> Option<&'a str> {
    p.rest.get(key).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// `auto_push`/`autopushes` entries: MistServer's own `JSON::Value::append`
/// turns an initially-null value into an array, so `auto_push` is an array
/// in practice; accept an object too (keyed by id), which is what newer
/// versions write.
fn push_entries(v: &Json) -> Vec<&Json> {
    match v {
        Json::Array(a) => a.iter().collect(),
        Json::Object(o) => o.values().collect(),
        _ => Vec::new(),
    }
}

/// A TOML basic string literal for `s`, escaped.
fn q(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn q_list<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    let items: Vec<String> = items.into_iter().map(|s| q(s)).collect();
    format!("[{}]", items.join(", "))
}

/// Unix seconds as an RFC 3339 UTC instant (`2026-01-01T00:00:00Z`).
fn rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", tod / 3600, tod % 3600 / 60, tod % 60)
}

/// A fresh Standard Webhooks secret (`whsec_` + base64 of 32 random
/// bytes). MistServer triggers are unsigned, so there is none to carry
/// over; Caudal signs every hook call and needs one. The bytes come from
/// `RandomState`, whose keys the standard library seeds from the OS RNG.
fn random_webhook_secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let mut bytes = Vec::with_capacity(32);
    for i in 0..4u64 {
        let mut h = std::hash::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        bytes.extend_from_slice(&h.finish().to_le_bytes());
    }
    format!("whsec_{}", base64_std(&bytes))
}

fn base64_std(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> shift) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// MistServer's `stringToBool` reading of a trigger answer or `default`.
fn mist_false(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off")
}

/// Target variables MistServer expands (`lib/stream.cpp:164-222`) that a
/// `[[push.auto]]` URL does not: Caudal knows only `$stream` / `$basename`.
const MIST_ONLY_VARIABLES: [&str; 16] = [
    "wildcard",
    "pluswildcard",
    "datetime",
    "year",
    "month",
    "day",
    "hour",
    "minute",
    "second",
    "wday",
    "yday",
    "week",
    "uuid",
    "source",
    "streamname",
    "basename_wildcard",
];

fn mist_variables_in(target: &str) -> Vec<&'static str> {
    MIST_ONLY_VARIABLES
        .iter()
        .copied()
        .filter(|v| {
            target.contains(&format!("${{{v}}}"))
                || target.match_indices(&format!("${v}")).any(|(i, m)| {
                    // `$year` but not `$yearly`: the name ends at a non-identifier byte.
                    !target[i + m.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                })
        })
        .collect()
}

// ---- triggers ----

/// One registered handler of a trigger.
#[derive(Debug)]
struct Handler {
    url: String,
    sync: bool,
    streams: Vec<String>,
    params: Option<String>,
    default: Option<String>,
}

fn json_truthy(v: &Json) -> bool {
    match v {
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Json::String(s) => !mist_false(s) && !s.trim().is_empty(),
        _ => false,
    }
}

fn json_strings(v: Option<&Json>) -> Vec<String> {
    match v {
        Some(Json::String(s)) if !s.trim().is_empty() => vec![s.trim().to_owned()],
        Some(Json::Array(a)) => {
            a.iter().filter_map(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    }
}

fn json_opt_string(v: Option<&Json>) -> Option<String> {
    match v {
        Some(Json::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Json::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn parse_handlers(defs: &Json) -> Vec<Handler> {
    let entries: Vec<&Json> = match defs {
        Json::Array(a) => a.iter().collect(),
        Json::Object(_) => vec![defs],
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .filter_map(|e| match e {
            Json::Array(a) => Some(Handler {
                url: a.first().and_then(Json::as_str).unwrap_or("").trim().to_owned(),
                sync: a.get(1).is_some_and(json_truthy),
                streams: json_strings(a.get(2)),
                params: json_opt_string(a.get(3)),
                default: json_opt_string(a.get(4)),
            }),
            Json::Object(o) => Some(Handler {
                url: o.get("handler").and_then(Json::as_str).unwrap_or("").trim().to_owned(),
                sync: o.get("sync").is_some_and(json_truthy),
                streams: json_strings(o.get("streams")),
                params: json_opt_string(o.get("params")),
                default: json_opt_string(o.get("default")),
            }),
            Json::String(s) => Some(Handler {
                url: s.trim().to_owned(),
                sync: false,
                streams: Vec::new(),
                params: None,
                default: None,
            }),
            _ => None,
        })
        .collect()
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// A blocking hook built from one or more triggers that point at the same
/// URL: `[auth.hook]` or `[auth.rewrite]`.
#[derive(Debug, Default)]
struct HookOut {
    url: String,
    secret: String,
    publish: bool,
    play: bool,
    on_error: &'static str,
}

/// Maps the triggers in `wanted` (`(name, is_publish)`) to one Caudal
/// blocking hook. Each trigger it consumes is added to `handled`.
fn map_blocking_triggers(
    triggers: &BTreeMap<String, Json>,
    wanted: &[(&str, bool)],
    section: &str,
    handled: &mut BTreeSet<String>,
    notes: &mut Vec<Note>,
    describe: impl Fn(&str, &Handler) -> (NoteKind, String),
) -> Option<HookOut> {
    let mut hook: Option<HookOut> = None;
    let mut any_default_false = false;
    for &(name, is_publish) in wanted {
        let Some(defs) = triggers.get(name) else { continue };
        handled.insert(name.to_owned());
        let key = format!("config.triggers.{name}");
        for (i, h) in parse_handlers(defs).iter().enumerate() {
            if !is_http_url(&h.url) {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    key.clone(),
                    format!(
                        "handler `{}` is an executable, not an http(s) URL: Caudal's `{section}` only calls \
                         HTTP endpoints; put the script behind a small HTTP service",
                        h.url
                    ),
                ));
                continue;
            }
            if !h.sync {
                let tail = if h.default.as_deref().is_some_and(mist_false) {
                    " Its `default` is false, so in MistServer it denies every request; Caudal has no \
                     \"deny everything\" switch for this, use `[[access.rules]]` or `[auth]` tokens"
                } else {
                    ""
                };
                notes.push(note(
                    NoteKind::NoEquivalent,
                    key.clone(),
                    format!(
                        "handler `{}` is non-blocking: MistServer does not wait for it and uses its \
                         `default`, so it only notifies; Caudal's `[hooks]` notifies only on \
                         stream.started / stream.ended.{tail}",
                        h.url
                    ),
                ));
                continue;
            }
            if i > 0 {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    key.clone(),
                    format!(
                        "extra handler `{}`: MistServer asks every handler (any false denies), Caudal's \
                         `{section}` has one `url`; only the first handler was imported",
                        h.url
                    ),
                ));
                continue;
            }
            if let Some(existing) = &hook
                && existing.url != h.url
            {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    key.clone(),
                    format!(
                        "handler `{}` differs from `{}` (already imported for another trigger): Caudal's \
                         `{section}` has one `url` for publish and play; serve both from one endpoint \
                         (`type` in the request tells them apart)",
                        h.url, existing.url
                    ),
                ));
                continue;
            }
            let out = hook.get_or_insert_with(|| HookOut {
                url: h.url.clone(),
                secret: random_webhook_secret(),
                on_error: "deny",
                ..HookOut::default()
            });
            if is_publish {
                out.publish = true;
            } else {
                out.play = true;
            }
            any_default_false |= h.default.as_deref().is_some_and(mist_false);
            let (kind, mut detail) = describe(name, h);
            if !h.streams.is_empty() {
                let _ = write!(
                    detail,
                    "; MistServer asked it only for streams {:?}, Caudal asks for every stream (answer 2xx \
                     for the others)",
                    h.streams
                );
            }
            if let Some(p) = &h.params {
                let _ = write!(detail, "; `params` {p:?} not imported: Caudal sends no per-handler parameters");
            }
            notes.push(note(kind, key.clone(), detail));
        }
    }
    if let Some(h) = &mut hook {
        h.on_error = if any_default_false { "deny" } else { "allow" };
    }
    hook
}

// ---- stream sources ----

/// What a MistServer stream `source` pull URL becomes.
enum Pull {
    Srt(String),
    Udp(String),
    Http(String),
    Hls(String),
    Rtmp(String),
    Rtsp(String),
}

impl Pull {
    fn section(&self) -> &'static str {
        match self {
            Pull::Srt(_) => "[[srt.pull]]",
            Pull::Udp(_) => "[[udp.pull]]",
            Pull::Http(_) => "[[http.pull]]",
            Pull::Hls(_) => "[[hls.pull]]",
            Pull::Rtmp(_) => "[[rtmp.pull]]",
            Pull::Rtsp(_) => "[[rtsp.pull]]",
        }
    }

    fn url(&self) -> &str {
        match self {
            Pull::Srt(u) | Pull::Udp(u) | Pull::Http(u) | Pull::Hls(u) | Pull::Rtmp(u) | Pull::Rtsp(u) => u,
        }
    }
}

/// `Some(Ok((pull, caveat)))` for a source URL Caudal can pull,
/// `Some(Err(reason))` for a pull scheme it knows but cannot use as given,
/// `None` for a scheme no Caudal input reads.
fn classify_pull(src: &str, used_udp_ports: &mut HashSet<u16>) -> Option<Result<(Pull, Option<String>), String>> {
    let lower = src.to_ascii_lowercase();
    let (path_part, _) = lower.split_once('?').unwrap_or((&lower, ""));
    if lower.starts_with("srt://") {
        return Some(classify_srt(src));
    }
    if let Some(rest) = strip_scheme(src, &["tsudp://", "udp://"]) {
        let url = format!("udp://{rest}");
        let (url, caveat) = match url.split_once('?') {
            Some((base, query)) => (
                base.to_owned(),
                Some(format!("query `?{query}` dropped: `[[udp.pull]]` takes only the address (and `interface`)")),
            ),
            None => (url, None),
        };
        let target = match caudal_udp::parse_url(&url) {
            Ok(t) => t,
            Err(e) => return Some(Err(format!("`[[udp.pull]]` cannot use it: {e}"))),
        };
        if !target.is_multicast() && !used_udp_ports.insert(target.addr.port()) {
            return Some(Err(format!(
                "UDP port {} is already used by another imported unicast pull; two would split its datagrams",
                target.addr.port()
            )));
        }
        return Some(Ok((Pull::Udp(url), caveat)));
    }
    if let Some(rest) = strip_scheme(src, &["http-ts://"]) {
        let url = format!("http://{rest}");
        return Some(caudal_http_pull::parse_url(&url).map(|_| (Pull::Http(url), None)).map_err(|e| e.to_string()));
    }
    if let Some(rest) = strip_scheme(src, &["https-ts://"]) {
        let url = format!("https://{rest}");
        return Some(caudal_http_pull::parse_url(&url).map(|_| (Pull::Http(url), None)).map_err(|e| e.to_string()));
    }
    if let Some(rest) = strip_scheme(src, &["http-hls://"]) {
        let url = format!("http://{rest}");
        return Some(caudal_hls_pull::parse_url(&url).map(|_| (Pull::Hls(url), None)).map_err(|e| e.to_string()));
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        if path_part.ends_with(".m3u8") {
            return Some(
                caudal_hls_pull::parse_url(src)
                    .map(|_| {
                        (
                            Pull::Hls(src.to_owned()),
                            Some(
                                "MPEG-TS segments only for now (fMP4/CMAF segments are refused at run time), \
                                 no AES-128 / SAMPLE-AES"
                                    .to_owned(),
                            ),
                        )
                    })
                    .map_err(|e| e.to_string()),
            );
        }
        if path_part.ends_with(".ts") || path_part.ends_with(".m2ts") {
            return Some(
                caudal_http_pull::parse_url(src).map(|_| (Pull::Http(src.to_owned()), None)).map_err(|e| e.to_string()),
            );
        }
        return Some(Err(
            "an http(s) file MistServer plays as VoD (MP4, MKV, FLV, ...): Caudal pulls only live MPEG-TS \
             (`[[http.pull]]`) and HLS playlists (`[[hls.pull]]`) over HTTP; download the file and use a \
             `[[channel]]`"
                .to_owned(),
        ));
    }
    if lower.starts_with("rtmp://") || lower.starts_with("rtmps://") {
        return Some(caudal_rtmp::pull::parse_url(src).map(|_| (Pull::Rtmp(src.to_owned()), None)));
    }
    if lower.starts_with("rtsp://") || lower.starts_with("rtsps://") {
        return Some(Ok((Pull::Rtsp(src.to_owned()), None)));
    }
    None
}

fn strip_scheme<'a>(src: &'a str, schemes: &[&str]) -> Option<&'a str> {
    schemes.iter().find_map(|s| src.get(..s.len()).filter(|head| head.eq_ignore_ascii_case(s)).map(|_| &src[s.len()..]))
}

/// `srt://host:port[?opts]`: Caudal's `[[srt.pull]]` is a caller. A
/// MistServer listener source (`srt://:port`, `srt://0.0.0.0:port`,
/// `mode=listener`) or a rendezvous one has no equivalent.
fn classify_srt(src: &str) -> Result<(Pull, Option<String>), String> {
    let rest = &src["srt://".len()..];
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = authority.trim_end_matches('/');
    let host = match authority.rsplit_once(':') {
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => authority,
    };
    let mut unknown = Vec::new();
    let mut mode = None;
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k {
            "streamid" | "passphrase" | "latency" => {}
            "mode" => mode = Some(v.to_ascii_lowercase()),
            other => unknown.push(other.to_owned()),
        }
    }
    let listener =
        host.is_empty() || host == "0.0.0.0" || host == "::" || matches!(mode.as_deref(), Some("listener" | "server"));
    if listener {
        return Err("a listener source (MistServer waits for a caller on its own port): Caudal's SRT listener \
                    is the shared `[srt] bind`; point the encoder at it with streamid `publish/<stream>` instead"
            .to_owned());
    }
    if mode.as_deref() == Some("rendezvous") {
        return Err("SRT rendezvous mode: Caudal's `[[srt.pull]]` is a caller only".to_owned());
    }
    if authority.rsplit_once(':').is_none_or(|(_, p)| p.parse::<u16>().is_err()) {
        return Err("needs host:port".to_owned());
    }
    let caveat = (!unknown.is_empty() || mode.is_some()).then(|| {
        let mut ignored: Vec<String> = unknown.clone();
        if mode.is_some() {
            ignored.push("mode".to_owned());
        }
        format!("URL options {ignored:?} are ignored by Caudal (it reads `streamid`, `passphrase`, `latency`)")
    });
    Ok((Pull::Srt(src.to_owned()), caveat))
}

// ---- auto_push targets ----

/// A `[[push.auto]]` rule to write, with its MistServer time window if any.
struct AutoPushOut {
    stream: String,
    url: String,
    /// `Some((start, stop))` (RFC 3339) when MistServer had a schedule.
    schedule: Option<(Option<String>, Option<String>)>,
}

struct ScheduleOut {
    stream: String,
    start: String,
    duration_secs: u32,
}

fn json_unix(v: Option<&Json>) -> Option<i64> {
    match v? {
        Json::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|&t| t > 0)
}

/// MistServer push target -> the URL `[[push.auto]]` takes, or why not.
fn auto_push_url(target: &str) -> Result<(String, Option<String>), String> {
    let lower = target.to_ascii_lowercase();
    let (url, caveat) = if lower.starts_with("rtmp://") || lower.starts_with("rtmps://") || lower.starts_with("srt://")
    {
        (target.to_owned(), None)
    } else if let Some(rest) = strip_scheme(target, &["tsudp://", "udp://"]) {
        split_udp_query(&format!("udp://{rest}"))
    } else if let Some(rest) = strip_scheme(target, &["tsrtp://", "rtp://"]) {
        split_udp_query(&format!("rtp://{rest}"))
    } else {
        let scheme = target.split_once("://").map_or(target, |(s, _)| s);
        return Err(format!(
            "`{scheme}://` targets have no Caudal push engine (it pushes rtmp(s)://, srt://, and MPEG-TS over \
             udp:// / rtp://; `whip://`, `tstcp://`, `rist://`, `cmaf://` and HTTP targets are not built)"
        ));
    };
    let vars = mist_variables_in(&url);
    if !vars.is_empty() {
        return Err(format!(
            "uses MistServer target variables {vars:?}; `[[push.auto]]` expands only `$stream` and `$basename`"
        ));
    }
    Ok((url, caveat))
}

fn split_udp_query(url: &str) -> (String, Option<String>) {
    match url.split_once('?') {
        Some((base, query)) => (
            base.trim_end_matches('/').to_owned(),
            Some(format!(
                "query `?{query}` dropped: set `ttl` / `interface` / `pacing` on the rule by hand if it carried them"
            )),
        ),
        None => (url.trim_end_matches('/').to_owned(), None),
    }
}

/// Parses `mist_json` and builds the Caudal TOML plus the report. Does not
/// touch the filesystem.
pub fn import(mist_json: &str) -> Result<ImportResult, String> {
    let file: MistFile = serde_json::from_str(mist_json).map_err(|e| format!("parsing MistServer config: {e}"))?;
    let mut notes = Vec::new();
    let mut b = BuiltConfig::default();

    import_protocols(&file, &mut b, &mut notes);
    import_streams(&file, &mut b, &mut notes);
    import_auto_push(&file, &mut b, &mut notes);
    import_triggers(&file, &mut b, &mut notes);

    // --- account: never migrated, different hash format ---
    if !file.account.is_empty() {
        let names = file.account.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
        notes.push(note(
            NoteKind::NoEquivalent,
            "account".to_string(),
            format!(
                "{} user(s) ({names}) not migrated: MistServer hashes passwords with MD5 \
                 (controller.cpp's createAccount), Caudal uses argon2id. Run `caudal hash-password` \
                 for each user and fill in the commented `[admin]` block below.",
                file.account.len()
            ),
        ));
    }
    notes.push(note(
        NoteKind::NoEquivalent,
        "(none — always noted)".to_string(),
        "a commented-out `[admin]` block is included below: without `[admin]`, Caudal refuses to \
         listen on anything but loopback (see caudal.example.toml)"
            .to_string(),
    ));

    Ok(ImportResult { toml: build_toml(&b), notes })
}

/// A bind address: the connector's `interface` when it gave an IP,
/// otherwise all interfaces (MistServer's default).
fn bind_addr(iface: Option<IpAddr>, port: u16) -> SocketAddr {
    SocketAddr::new(iface.unwrap_or(IpAddr::from([0, 0, 0, 0])), port)
}

/// HTTP-side binds go to loopback unless MistServer itself bound one:
/// without `[admin]` Caudal refuses to start on a non-loopback address.
fn loopback_bind(iface: Option<IpAddr>, port: u16) -> (SocketAddr, bool) {
    match iface {
        Some(ip) if ip.is_loopback() => (SocketAddr::new(ip, port), false),
        _ => (SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port), true),
    }
}

const LOOPBACK_CAVEAT: &str = "bound to 127.0.0.1, not MistServer's address: without `[admin]` Caudal \
     refuses to start on a non-loopback address — set up `[admin]` (commented in below), then change this \
     to 0.0.0.0 to match MistServer's original reach";

fn import_protocols(file: &MistFile, b: &mut BuiltConfig, notes: &mut Vec<Note>) {
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    let mut has_hls = false;

    for p in &file.config.protocols {
        let n = seen.entry(p.connector.clone()).or_insert(0);
        *n += 1;
        let first = *n == 1;
        let mist_key = format!("config.protocols[connector={}]", p.connector);
        let port = protocol_port(p);
        let iface = protocol_interface(p);

        // One listener per Caudal section: a second connector of the same
        // kind has nowhere to go.
        let extra = |section: &str, key: &str| {
            note(
                NoteKind::NoEquivalent,
                mist_key.clone(),
                format!("extra `{}` listener not imported: Caudal has one `[{section}] {key}` bind", p.connector),
            )
        };
        let bound = |section: &str, key: &str, addr: SocketAddr, default: u16, caveat: Option<&str>| {
            let kind = if port.is_some() && caveat.is_none() { NoteKind::Translated } else { NoteKind::Approximated };
            let mut detail = match port {
                Some(_) => format!("-> `[{section}] {key} = \"{addr}\"`"),
                None => {
                    format!("no `port` given; kept Caudal's default port {default}: `[{section}] {key} = \"{addr}\"`")
                }
            };
            if let Some(c) = caveat {
                detail.push_str("; ");
                detail.push_str(c);
            }
            note(kind, mist_key.clone(), detail)
        };

        match p.connector.as_str() {
            "HTTP" => {
                if !first {
                    notes.push(extra("server", "http_bind"));
                    continue;
                }
                let (addr, forced) = loopback_bind(iface, port.unwrap_or(8080));
                b.http_bind = Some(addr);
                notes.push(bound("server", "http_bind", addr, 8080, forced.then_some(LOOPBACK_CAVEAT)));
            }
            "HTTPS" => {
                if !first {
                    notes.push(extra("tls", "bind"));
                    continue;
                }
                match (protocol_str(p, "cert"), protocol_str(p, "key")) {
                    (Some(cert), Some(key)) => {
                        let (addr, forced) = loopback_bind(iface, port.unwrap_or(4433));
                        b.tls = Some((addr, cert.to_owned(), key.to_owned()));
                        let mut n = bound("tls", "bind", addr, 4433, forced.then_some(LOOPBACK_CAVEAT));
                        let _ = write!(
                            n.detail,
                            ", cert = {cert:?}, key = {key:?} (same PEM files; Caudal also offers ACME via \
                             `acme_domains`)"
                        );
                        notes.push(n);
                    }
                    _ => notes.push(note(
                        NoteKind::NoEquivalent,
                        mist_key,
                        "HTTPS connector without both `cert` and `key` paths: set `[tls]` by hand \
                         (`cert` + `key`, or `acme_domains` for Let's Encrypt)"
                            .to_string(),
                    )),
                }
            }
            "HLS" => {
                has_hls = true;
                notes.push(note(
                    NoteKind::Approximated,
                    mist_key,
                    "TS-HLS is served on `[server] http_bind` (`[hls] ts = true`, Caudal's default): \
                     MistServer's `/hls/{name}/index.m3u8` is `/hls/{name}/ts.m3u8` in Caudal, where \
                     `index.m3u8` is the LL-HLS playlist (docs/HLS.md); SAMPLE-AES is not carried over"
                        .to_string(),
                ));
            }
            "CMAF" => notes.push(note(
                NoteKind::Approximated,
                mist_key,
                "LL-HLS (CMAF) is always on in Caudal at `/hls/{name}/master.m3u8` (MistServer: \
                 `/cmaf/{name}/index.m3u8`); MistServer's DASH (`/cmaf/{name}/index.mpd`) and Smooth \
                 manifests from this connector have no Caudal output yet"
                    .to_string(),
            )),
            "HTTPTS" => notes.push(note(
                NoteKind::Approximated,
                mist_key,
                "HTTP-TS is always on in Caudal at `/ts/{name}.ts` on `[server] http_bind` (MistServer: \
                 `/{name}.ts`); any `port` here is not used"
                    .to_string(),
            )),
            "AAC" | "MP3" => notes.push(note(
                NoteKind::Approximated,
                mist_key,
                format!(
                    "audio-only output is always on in Caudal at `/audio/{{name}}.{}` (MistServer: \
                     `/{{name}}.{}`), passthrough only: a stream whose audio is another codec gets 415 \
                     (docs/AUDIO.md)",
                    p.connector.to_ascii_lowercase(),
                    p.connector.to_ascii_lowercase()
                ),
            )),
            "JPG" => notes.push(note(
                NoteKind::Approximated,
                mist_key,
                "thumbnails are always on in Caudal at `/api/v1/streams/{name}/thumbnail.jpg` \
                 (MistServer: `/{name}.jpg`); no `.mjpg` motion JPEG"
                    .to_string(),
            )),
            "RTMP" => {
                if !first {
                    notes.push(extra("rtmp", "bind"));
                    continue;
                }
                let addr = bind_addr(iface, port.unwrap_or(1935));
                b.rtmp_bind = Some(addr);
                notes.push(bound(
                    "rtmp",
                    "bind",
                    addr,
                    1935,
                    Some(
                        "publish URLs are `rtmp://host/live/<stream>` (`[rtmp] app = \"live\"`); MistServer \
                         accepts any application name",
                    ),
                ));
            }
            "TSSRT" | "SRT" => {
                if !first {
                    notes.push(extra("srt", "bind"));
                    continue;
                }
                let addr = bind_addr(iface, port.unwrap_or(9000));
                b.srt_bind = Some(addr);
                b.srt_passphrase = protocol_str(p, "passphrase").map(str::to_owned);
                let mut n = bound("srt", "bind", addr, 9000, None);
                if b.srt_passphrase.is_some() {
                    n.detail.push_str(", with its `passphrase`");
                }
                notes.push(n);
            }
            "RTSP" => {
                if !first {
                    notes.push(extra("rtsp", "bind"));
                    continue;
                }
                let addr = bind_addr(iface, port.unwrap_or(8554));
                b.rtsp_bind = Some(addr);
                notes.push(bound("rtsp", "bind", addr, 8554, None));
            }
            "WebRTC" => {
                if !first {
                    notes.push(extra("webrtc", "udp_bind"));
                    continue;
                }
                let addr = bind_addr(iface, port.unwrap_or(8189));
                b.webrtc_bind = Some(addr);
                notes.push(bound(
                    "webrtc",
                    "udp_bind",
                    addr,
                    8189,
                    Some("WHIP / WHEP only: MistServer's WebSocket signalling (`/webrtc/{name}`) is not ported"),
                ));
            }
            other => notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!(
                    "connector `{other}` has no Caudal output (raw TS over TCP, DTSC, RIST, progressive \
                     MP4/MKV, WAV/FLAC/OGG and SubRip are not built; HDS, Flash and Smooth Streaming are \
                     never ported, see PLAN.md)"
                ),
            )),
        }
    }

    // MistServer serves only the connectors in its list, so a list without
    // HLS means TS-HLS was off.
    if !file.config.protocols.is_empty() && !has_hls {
        b.hls_ts_off = true;
        notes.push(note(
            NoteKind::Translated,
            "config.protocols (no HLS connector)",
            "TS-HLS was off in MistServer -> `[hls] ts = false`",
        ));
    }
}

fn import_streams(file: &MistFile, b: &mut BuiltConfig, notes: &mut Vec<Note>) {
    let mut max_dvr_ms: Option<u64> = None;
    let mut used_udp_ports: HashSet<u16> = HashSet::new();

    for (name, s) in &file.streams {
        let src_key = format!("streams.{name}.source");
        if !caudal_core::media::valid_stream_name(name) {
            notes.push(note(
                NoteKind::NoEquivalent,
                format!("streams.{name}"),
                "not a valid Caudal stream name (A-Z a-z 0-9 _ - . +, at most 128); rename it and import again",
            ));
            continue;
        }
        if let Some(dvr) = s.dvr_ms {
            max_dvr_ms = Some(max_dvr_ms.map_or(dvr, |m| m.max(dvr)));
            notes.push(note(
                NoteKind::Approximated,
                format!("streams.{name}.DVR"),
                format!(
                    "-> `[buffer] window_secs` ({dvr} ms asked here); Caudal's buffer window is one \
                     global setting, not per stream, so the largest DVR across all streams wins"
                ),
            ));
        }

        let mut pulled = false;
        match s.source.as_deref().map(str::trim) {
            None | Some("") => notes.push(note(
                NoteKind::Translated,
                src_key,
                "no source (or empty `push://`): publish is open to anyone under this stream name, \
                 same as Caudal's default (no `[auth]`/`[[access.rules]]` restriction)",
            )),
            Some(src) if src.starts_with("push://") => import_push_source(name, src, b, notes),
            Some(src) if !src.contains("://") => {
                b.channels.push((name.clone(), src.to_string()));
                notes.push(note(
                    NoteKind::Approximated,
                    src_key,
                    format!(
                        "file/folder `{src}` -> `[[channel]]` name=\"{name}\" items=[\"{src}\"]; MistServer \
                         serves this on demand as VoD, Caudal's `[[channel]]` plays it as a looping 24/7 \
                         live stream (loop = true) — the closest equivalent, not identical semantics"
                    ),
                ));
            }
            Some(src) => match classify_pull(src, &mut used_udp_ports) {
                Some(Ok((pull, caveat))) => {
                    pulled = true;
                    let detail = format!("-> `{}` stream = \"{name}\", url = {}", pull.section(), q(pull.url()));
                    let rewritten = pull.url() != src;
                    let n = match (caveat, rewritten) {
                        (Some(c), _) => note(NoteKind::Approximated, src_key, format!("{detail}; {c}")),
                        (None, true) => note(
                            NoteKind::Translated,
                            src_key,
                            format!("{detail} (MistServer's `{src}` spelling, same transport)"),
                        ),
                        (None, false) => note(NoteKind::Translated, src_key, detail),
                    };
                    notes.push(n);
                    b.pulls.push((name.clone(), pull));
                }
                Some(Err(reason)) => {
                    notes.push(note(NoteKind::NoEquivalent, src_key, format!("`{src}`: {reason}")));
                }
                None => notes.push(note(
                    NoteKind::NoEquivalent,
                    src_key,
                    format!(
                        "`{src}`: no Caudal input for this source (RIST, DTSC, SDP, `*-exec:`, V4L2, \
                         `balance:` and S3 sources are not built)"
                    ),
                )),
            },
        }

        for k in s.rest.keys() {
            if k == "name" {
                continue;
            }
            if k == "always_on" && pulled {
                notes.push(note(
                    NoteKind::Translated,
                    format!("streams.{name}.always_on"),
                    "Caudal's pulls are always on (they start with the server, not with the first viewer)",
                ));
                continue;
            }
            notes.push(note(
                NoteKind::NoEquivalent,
                format!("streams.{name}.{k}"),
                "no per-stream Caudal equivalent".to_string(),
            ));
        }
    }
    b.buffer_window_secs = max_dvr_ms.map(|ms| ms.div_ceil(1000).max(1));
}

fn import_push_source(name: &str, src: &str, b: &mut BuiltConfig, notes: &mut Vec<Note>) {
    let src_key = format!("streams.{name}.source");
    let rest = src["push://".len()..].trim();
    let (host, password) = match rest.split_once('@') {
        Some((h, p)) => (h.trim(), Some(p)),
        None => (rest, None),
    };
    if password.is_some() {
        notes.push(note(
            NoteKind::NoEquivalent,
            src_key.clone(),
            "`push://...@password`: Caudal has no per-stream publish password; use `[auth]` tokens \
             (a JWT whose `sub` is the stream) or `[auth.hook]`",
        ));
    }
    if host.is_empty() {
        if password.is_none() {
            notes.push(note(
                NoteKind::Translated,
                src_key,
                "`push://` with no host: publish is open to anyone, same as Caudal's default",
            ));
        }
    } else if caudal_core::Cidr::parse(host).is_ok() {
        b.access_rules.push((name.to_owned(), host.to_string()));
        let kind = if password.is_some() { NoteKind::Approximated } else { NoteKind::Translated };
        let mut detail =
            format!("`push://{host}` -> `[[access.rules]]` streams=[\"{name}\"] publish_allow=[\"{host}\"]");
        if password.is_some() {
            detail.push_str("; MistServer let the password skip the IP check, Caudal always checks the IP");
        }
        notes.push(note(kind, src_key, detail));
    } else {
        notes.push(note(
            NoteKind::NoEquivalent,
            src_key,
            format!(
                "`push://{host}`: `{host}` is a hostname, not an IP/CIDR; Caudal's \
                 `[[access.rules]]` only takes IP/CIDR or `country:XX` — resolve it to an \
                 address and add the rule by hand"
            ),
        ));
    }
}

fn import_auto_push(file: &MistFile, b: &mut BuiltConfig, notes: &mut Vec<Note>) {
    let push_source = if !push_entries(&file.auto_push).is_empty() {
        Some(("auto_push", &file.auto_push))
    } else if !push_entries(&file.autopushes).is_empty() {
        notes.push(note(
            NoteKind::NoEquivalent,
            "autopushes".to_string(),
            "legacy positional format: MistServer upgrades this to `auto_push` the first time it \
             loads the config (controller_storage.cpp); start that MistServer instance once and \
             re-export, or convert the entries to `auto_push` objects by hand before importing",
        ));
        None
    } else {
        None
    };
    let Some((key, val)) = push_source else { return };

    let mut udp_dests: HashSet<SocketAddr> = HashSet::new();
    for entry in push_entries(val) {
        let Some(obj) = entry.as_object() else { continue };
        let stream = obj.get("stream").and_then(Json::as_str).map(str::trim);
        let target = obj.get("target").and_then(Json::as_str).map(str::trim);
        let (Some(stream), Some(target)) = (stream, target) else {
            notes.push(note(NoteKind::NoEquivalent, key, "push entry missing `stream` or `target`, skipped"));
            continue;
        };
        let mist_key = format!("{key}[stream={stream}]");

        if stream.starts_with('#') || stream.ends_with('+') || stream.contains('+') {
            notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!(
                    "stream `{stream}`: tag (`#tag`) and wildcard (`name+`) selectors have no Caudal \
                     equivalent; `[[push.auto]]` matches exact names, `prefix*` or `*`"
                ),
            ));
            continue;
        }
        if !caudal_core::media::valid_stream_name(stream) {
            notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!("stream `{stream}` is not a valid Caudal stream name"),
            ));
            continue;
        }

        let schedule = json_unix(obj.get("scheduletime"));
        let complete = json_unix(obj.get("completetime"));
        let rules: Vec<&str> = ["start_rule", "end_rule", "inhibit"]
            .into_iter()
            .filter(|k| obj.get(*k).is_some_and(|v| !v.is_null() && v != &Json::String(String::new())))
            .collect();
        let scheduled = schedule.is_some() || complete.is_some();
        let sched_text = || {
            let mut parts = Vec::new();
            if let Some(t) = schedule {
                parts.push(format!("scheduletime {t} ({})", rfc3339_utc(t)));
            }
            if let Some(t) = complete {
                parts.push(format!("completetime {t} ({})", rfc3339_utc(t)));
            }
            parts.join(", ")
        };

        if !target.contains("://") {
            import_record_target(stream, target, &mist_key, schedule, complete, &rules, b, notes);
            continue;
        }

        let (url, caveat) = match auto_push_url(target) {
            Ok(v) => v,
            Err(reason) => {
                notes.push(note(NoteKind::NoEquivalent, mist_key, format!("target `{target}`: {reason}")));
                continue;
            }
        };
        let start = schedule.map(rfc3339_utc);
        let stop = complete.map(rfc3339_utc);
        // Built through serde so fields added to `[[push.auto]]` later keep
        // their defaults here instead of breaking the importer.
        let rule: config::AutoPushEntry = match serde_json::from_value(serde_json::json!({
            "streams": [stream],
            "url": url,
            "start": start,
            "stop": stop,
        })) {
            Ok(r) => r,
            Err(e) => {
                notes.push(note(NoteKind::NoEquivalent, mist_key, format!("target `{target}`: {e}")));
                continue;
            }
        };
        if let Err(e) = rule.validate() {
            notes.push(note(NoteKind::NoEquivalent, mist_key, format!("target `{target}`: {e}")));
            continue;
        }
        if rule.kind() == Ok(config::AutoPushKind::Udp)
            && let Ok(t) = rule.udp_target(stream)
            && !udp_dests.insert(t.group)
        {
            notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!(
                    "UDP destination {} is already used by another imported push: one stream per destination",
                    t.group
                ),
            ));
            continue;
        }

        if !rules.is_empty() {
            // Conditions decide whether the push runs at all: an always-on
            // rule would push when MistServer did not.
            notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!(
                    "{}: `[[push.auto]]` on this Caudal has no variable conditions or inhibit, and pushing \
                     without them would push when MistServer did not; not imported. `inhibit` is not mapped to \
                     `[[push.auto]] inhibit` until its semantics are checked against a real MistServer; \
                     `start_rule`/`end_rule` have no equivalent",
                    rules.join(", ")
                ),
            ));
            continue;
        }

        if scheduled {
            notes.push(note(
                NoteKind::Approximated,
                mist_key,
                format!(
                    "{} -> `[[push.auto]]` streams = [\"{stream}\"], url = {}, with `start`/`stop` (UTC). \
                     MistServer's `doAutoPush` compares `scheduletime` the wrong way round (a future time \
                     starts at once; docs/research/MISTSERVER-REVERSE.md §8): the rule uses the documented \
                     meaning, so it pushes only inside the window",
                    sched_text(),
                    q(&url)
                ),
            ));
            b.auto_pushes.push(AutoPushOut { stream: stream.to_owned(), url, schedule: Some((start, stop)) });
            continue;
        }

        let mut detail = format!("-> `[[push.auto]]` streams = [\"{stream}\"], url = {}", q(&url));
        let kind = if let Some(c) = caveat {
            detail.push_str("; ");
            detail.push_str(&c);
            NoteKind::Approximated
        } else {
            NoteKind::Translated
        };
        notes.push(note(kind, mist_key, detail));
        b.auto_pushes.push(AutoPushOut { stream: stream.to_owned(), url, schedule: None });
    }
}

#[allow(clippy::too_many_arguments)]
fn import_record_target(
    stream: &str,
    target: &str,
    mist_key: &str,
    schedule: Option<i64>,
    complete: Option<i64>,
    rules: &[&str],
    b: &mut BuiltConfig,
    notes: &mut Vec<Note>,
) {
    if !rules.is_empty() {
        notes.push(note(
            NoteKind::NoEquivalent,
            mist_key,
            format!(
                "file target `{target}` with {}: `[record]` has no variable conditions or inhibit; \
                 recording without them would record when MistServer did not; not imported",
                rules.join(", ")
            ),
        ));
        return;
    }
    let window = match (schedule, complete) {
        (None, None) => None,
        (Some(s), Some(c)) if c > s && u32::try_from(c - s).is_ok() => Some((s, (c - s) as u32)),
        _ => {
            notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!(
                    "file target `{target}`: a schedule without both `scheduletime` and a later \
                     `completetime` has no `[[record.schedule]]` equivalent (it needs a start and a \
                     duration); not imported"
                ),
            ));
            return;
        }
    };
    if b.record_dir.is_none()
        && let Some(parent) = Path::new(target).parent().filter(|p| !p.as_os_str().is_empty())
    {
        b.record_dir = Some(parent.display().to_string());
    }
    let pattern_caveat = "MistServer's per-push path pattern (e.g. `$datetime`) is not reproduced — Caudal \
                          always writes to `<dir>/<stream>/<timestamp>/`";
    match window {
        None => {
            b.record_streams.insert(stream.to_string());
            notes.push(note(
                NoteKind::Approximated,
                mist_key,
                format!("file target `{target}` -> `[record]` streams += [\"{stream}\"]; {pattern_caveat}"),
            ));
        }
        Some((start, duration_secs)) => {
            let start = rfc3339_utc(start);
            notes.push(note(
                NoteKind::Approximated,
                mist_key,
                format!(
                    "file target `{target}`, scheduletime/completetime -> `[[record.schedule]]` streams = \
                     [\"{stream}\"], start = \"{start}\", duration_secs = {duration_secs}; {pattern_caveat}"
                ),
            ));
            b.record_schedules.push(ScheduleOut { stream: stream.to_owned(), start, duration_secs });
        }
    }
}

const PAYLOAD_CAVEAT: &str = "the request is a signed JSON POST, not MistServer's newline-separated \
     text/plain lines: port the handler (docs/IMPORT-MIST.md)";

fn import_triggers(file: &MistFile, b: &mut BuiltConfig, notes: &mut Vec<Note>) {
    let triggers = &file.config.triggers;
    let mut handled: BTreeSet<String> = BTreeSet::new();

    b.auth_hook = map_blocking_triggers(
        triggers,
        &[("STREAM_PUSH", true), ("USER_NEW", false), ("CONN_PLAY", false)],
        "[auth.hook]",
        &mut handled,
        notes,
        |name, h| {
            let (action, when) = match name {
                "STREAM_PUSH" => ("publish = true", "before a publish"),
                "USER_NEW" => (
                    "play = true",
                    "before every play, cached `allow_cache_secs` (MistServer: once per new session, and \
                     only when no valid JWT was presented; Caudal asks after the token check, with or \
                     without a token)",
                ),
                _ => ("play = true", "before a play"),
            };
            let fallback = if h.default.as_deref().is_some_and(mist_false) {
                "`on_error = \"deny\"` (its `default` was false)"
            } else {
                "`on_error = \"allow\"` (MistServer lets the request through when a handler fails and has no \
                 false `default`)"
            };
            (
                NoteKind::Approximated,
                format!(
                    "-> `[auth.hook]` url = {}, {action}: asked {when}; {fallback}. `reply = \"mistserver\"`: \
                     a 2xx allows only when the body reads as true (MistServer's rule), so a handler \
                     answering 200 `false` still denies; {PAYLOAD_CAVEAT}; a new `whsec_` secret was \
                     generated, verify signatures with it",
                    q(&h.url)
                ),
            )
        },
    );

    b.auth_rewrite = map_blocking_triggers(
        triggers,
        &[("PUSH_REWRITE", true), ("PLAY_REWRITE", false)],
        "[auth.rewrite]",
        &mut handled,
        notes,
        |name, h| {
            let action = if name == "PUSH_REWRITE" { "publish = true" } else { "play = true" };
            (
                NoteKind::Approximated,
                format!(
                    "-> `[auth.rewrite]` url = {}, {action}: the answer is read as MistServer's is (plain \
                     text stream name, empty denies) or as JSON `{{\"stream\": ...}}`; {PAYLOAD_CAVEAT} \
                     (`data.stream`, `data.protocol`, `data.ip`, `data.token`; no full URL or host); a new \
                     `whsec_` secret was generated",
                    q(&h.url)
                ),
            )
        },
    );
    // A rewrite that fails keeps MistServer's request only if its handler
    // said so; Caudal's safer default is to refuse.
    if let Some(r) = &mut b.auth_rewrite {
        r.on_error = "deny";
    }

    for (name, defs) in triggers {
        if handled.contains(name) {
            continue;
        }
        let count = match defs {
            Json::Array(a) => a.len(),
            Json::Null => 0,
            _ => 1,
        };
        if count == 0 {
            continue;
        }
        let why = match name.as_str() {
            "RTMP_PUSH_REWRITE" => {
                "rewrites the whole RTMP URL; Caudal's `[auth.rewrite]` (PUSH_REWRITE) rewrites the stream \
                 name, which is what most handlers use this for — port it there by hand"
            }
            "STREAM_SOURCE" => {
                "Caudal starts no input on demand, so there is no source to override \
                 (docs/REWRITE-HOOK.md, \"STREAM_SOURCE: not done\")"
            }
            "STREAM_BUFFER" | "LIVE_BANDWIDTH" => {
                "closest is `[health]` (signed alerts on keyframe gaps, low bitrate, lost publisher), which \
                 notifies but cannot act on the stream"
            }
            "STREAM_END" | "PUSH_END" | "RECORDING_END" | "USER_END" | "CONN_CLOSE" | "INPUT_ABORT"
            | "PUSH_INPUT_CLOSE" | "LIVE_TRACK_LIST" | "OUTPUT_END" => {
                "a notification; Caudal's `[hooks]` notifies only on stream.started / stream.ended (signed \
                 webhooks), so configure `[hooks]` by hand if those cover it"
            }
            _ => {
                "Caudal's blocking hooks cover STREAM_PUSH / USER_NEW / CONN_PLAY (`[auth.hook]`) and \
                 PUSH_REWRITE / PLAY_REWRITE (`[auth.rewrite]`); this trigger has no equivalent. \
                 `[hooks]` (stream.started / stream.ended) and `[health]` only notify"
            }
        };
        notes.push(note(
            NoteKind::NoEquivalent,
            format!("config.triggers.{name}"),
            format!("{count} handler(s): {why}"),
        ));
    }
}

#[derive(Default)]
struct BuiltConfig {
    http_bind: Option<SocketAddr>,
    tls: Option<(SocketAddr, String, String)>,
    rtmp_bind: Option<SocketAddr>,
    srt_bind: Option<SocketAddr>,
    srt_passphrase: Option<String>,
    rtsp_bind: Option<SocketAddr>,
    webrtc_bind: Option<SocketAddr>,
    hls_ts_off: bool,
    buffer_window_secs: Option<u64>,
    channels: Vec<(String, String)>,
    access_rules: Vec<(String, String)>,
    pulls: Vec<(String, Pull)>,
    auto_pushes: Vec<AutoPushOut>,
    record_streams: BTreeSet<String>,
    record_schedules: Vec<ScheduleOut>,
    record_dir: Option<String>,
    auth_hook: Option<HookOut>,
    auth_rewrite: Option<HookOut>,
}

fn build_toml(c: &BuiltConfig) -> String {
    let mut out = String::new();
    out.push_str(
        "# Generated by `caudal import-mist` from a MistServer config. Validated against\n\
         # Caudal's own rules already (same checks as `caudal check`); read the import\n\
         # report printed alongside this file for what was approximated or dropped.\n\
         # `caudal doctor --config <this file>` checks the setup it describes next.\n",
    );
    let pulls = |section: &'static str| c.pulls.iter().filter(move |(_, p)| p.section() == section);

    if let Some(addr) = c.http_bind {
        // Loopback unless MistServer bound loopback itself: without
        // `[admin]`, Caudal refuses to start on a non-loopback address.
        let _ = write!(out, "\n[server]\nhttp_bind = \"{addr}\"\n");
    }
    if let Some((addr, cert, key)) = &c.tls {
        let _ = write!(out, "\n[tls]\nbind = \"{addr}\"\ncert = {}\nkey = {}\n", q(cert), q(key));
    }
    if let Some(addr) = c.rtmp_bind {
        let _ = write!(out, "\n[rtmp]\nbind = \"{addr}\"\napp = \"live\"\n");
    }
    for (stream, p) in pulls("[[rtmp.pull]]") {
        let _ = write!(out, "\n[[rtmp.pull]]\nstream = {}\nurl = {}\n", q(stream), q(p.url()));
    }
    if let Some(addr) = c.srt_bind {
        let _ = write!(out, "\n[srt]\nbind = \"{addr}\"\n");
        if let Some(pass) = &c.srt_passphrase {
            let _ = writeln!(out, "passphrase = {}", q(pass));
        }
    }
    for (stream, p) in pulls("[[srt.pull]]") {
        let _ = write!(out, "\n[[srt.pull]]\nstream = {}\nurl = {}\n", q(stream), q(p.url()));
    }
    if let Some(secs) = c.buffer_window_secs {
        let _ = write!(out, "\n[buffer]\nwindow_secs = {secs}\n");
    }
    if c.hls_ts_off {
        out.push_str("\n[hls]\nts = false\n");
    }
    for (stream, p) in pulls("[[hls.pull]]") {
        let _ = write!(out, "\n[[hls.pull]]\nstream = {}\nurl = {}\n", q(stream), q(p.url()));
    }
    if let Some(addr) = c.rtsp_bind {
        let _ = write!(out, "\n[rtsp]\nbind = \"{addr}\"\n");
    }
    for (stream, p) in pulls("[[rtsp.pull]]") {
        let _ = write!(out, "\n[[rtsp.pull]]\nstream = {}\nurl = {}\n", q(stream), q(p.url()));
    }
    if let Some(addr) = c.webrtc_bind {
        let _ = write!(out, "\n[webrtc]\nudp_bind = \"{addr}\"\n");
    }
    for (stream, p) in pulls("[[udp.pull]]") {
        let _ = write!(out, "\n[[udp.pull]]\nstream = {}\nurl = {}\n", q(stream), q(p.url()));
    }
    for (stream, p) in pulls("[[http.pull]]") {
        let _ = write!(out, "\n[[http.pull]]\nstream = {}\nurl = {}\n", q(stream), q(p.url()));
    }
    for (name, item) in &c.channels {
        let _ = write!(out, "\n[[channel]]\nname = {}\nitems = [{}]\n", q(name), q(item));
    }
    for p in &c.auto_pushes {
        let _ = write!(out, "\n[[push.auto]]\nstreams = [{}]\nurl = {}\n", q(&p.stream), q(&p.url));
        if let Some((start, stop)) = &p.schedule {
            if let Some(s) = start {
                let _ = writeln!(out, "start = \"{s}\"");
            }
            if let Some(s) = stop {
                let _ = writeln!(out, "stop = \"{s}\"");
            }
        }
    }
    if !c.record_streams.is_empty() || !c.record_schedules.is_empty() {
        out.push_str("\n[record]\nenabled = true\n");
        if let Some(dir) = &c.record_dir {
            let _ = writeln!(out, "dir = {}", q(dir));
        }
        // Always written: the default (`["*"]`) would record every stream.
        let _ = writeln!(out, "streams = {}", q_list(&c.record_streams));
        for s in &c.record_schedules {
            let _ = write!(
                out,
                "\n[[record.schedule]]\nstreams = [{}]\nstart = \"{}\"\nduration_secs = {}\n",
                q(&s.stream),
                s.start,
                s.duration_secs
            );
        }
    }
    for (stream, host) in &c.access_rules {
        let _ = write!(out, "\n[[access.rules]]\nstreams = [{}]\npublish_allow = [{}]\n", q(stream), q(host));
    }
    if let Some(h) = &c.auth_hook {
        let _ = write!(
            out,
            "\n# From MistServer's blocking triggers. `reply = \"mistserver\"` reads the body the\n\
             # way MistServer does: a 2xx allows only when it says true. docs/AUTH-HOOK.md\n\
             [auth.hook]\nurl = {}\nsecret = {}\nreply = \"mistserver\"\non_error = {}\npublish = {}\nplay = {}\n",
            q(&h.url),
            q(&h.secret),
            q(h.on_error),
            h.publish,
            h.play
        );
    }
    if let Some(h) = &c.auth_rewrite {
        let _ = write!(
            out,
            "\n# From MistServer's PUSH_REWRITE / PLAY_REWRITE. The request is signed JSON,\n\
             # not text lines; plain-text answers are accepted. docs/REWRITE-HOOK.md\n\
             [auth.rewrite]\nurl = {}\nsecret = {}\non_error = {}\npublish = {}\nplay = {}\n",
            q(&h.url),
            q(&h.secret),
            q(h.on_error),
            h.publish,
            h.play
        );
    }

    out.push_str(
        "\n# Admin login: without it, Caudal refuses to listen on anything but loopback.\n\
         # Hash each MistServer user's new password: printf '%s\\n' 'pass' | caudal hash-password\n\
         # [admin]\n\
         # session_ttl_secs = 43200\n\
         # [[admin.users]]\n\
         # name = \"ana\"\n\
         # password_hash = \"$argon2id$...\"\n",
    );

    out
}

/// Validates and writes the imported config, printing the report. Writes
/// nothing if the generated TOML doesn't itself pass validation — the same
/// checks `caudal check` runs.
pub fn run(input: &Path, output: &Path) -> ExitCode {
    let text = match std::fs::read_to_string(input) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("reading {}: {e}", input.display());
            return ExitCode::FAILURE;
        }
    };
    let result = match import(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = validate_generated(&result.toml) {
        eprintln!("generated config failed its own validation (this is a bug in `import-mist`): {e}");
        eprintln!("--- generated caudal.toml ---\n{}", result.toml);
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(output, &result.toml) {
        eprintln!("writing {}: {e}", output.display());
        return ExitCode::FAILURE;
    }
    println!("wrote {}", output.display());
    println!("{}", result.report());
    ExitCode::SUCCESS
}

/// The same two steps `config::load` runs on a file (parse, then
/// cross-key validate), run here on a string still in memory.
fn validate_generated(toml_text: &str) -> Result<config::Config, String> {
    let cfg: config::Config = toml::from_str(toml_text).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported(mist: &str) -> (ImportResult, config::Config) {
        let result = import(mist).unwrap();
        let cfg = validate_generated(&result.toml).unwrap_or_else(|e| panic!("{e}\n---\n{}", result.toml));
        (result, cfg)
    }

    fn has(result: &ImportResult, kind: NoteKind, mist: &str, detail: &str) -> bool {
        result.notes.iter().any(|n| n.kind == kind && n.mist.contains(mist) && n.detail.contains(detail))
    }

    #[test]
    fn empty_mist_config_imports_to_a_valid_empty_toml() {
        imported("{}");
    }

    #[test]
    fn rtmp_and_http_connectors_translate() {
        let mist = r#"{"config":{"protocols":[
            {"connector":"RTMP","port":1935},
            {"connector":"HTTP","port":8080},
            {"connector":"HLS"}
        ]}}"#;
        let (result, cfg) = imported(mist);
        assert!(result.toml.contains("[rtmp]\nbind = \"0.0.0.0:1935\""), "{}", result.toml);
        assert!(result.toml.contains("[server]\nhttp_bind = \"127.0.0.1:8080\""), "{}", result.toml);
        assert!(has(&result, NoteKind::Approximated, "HLS", "ts.m3u8"));
        assert!(cfg.hls.ts, "an HLS connector keeps TS-HLS on");
    }

    #[test]
    fn connector_interface_becomes_the_bind_address() {
        let mist = r#"{"config":{"protocols":[
            {"connector":"RTMP","port":1936,"interface":"10.1.2.3"},
            {"connector":"HTTP","port":8081,"interface":"127.0.0.1"}
        ]}}"#;
        let (_, cfg) = imported(mist);
        assert_eq!(cfg.rtmp.bind, "10.1.2.3:1936".parse().unwrap());
        assert_eq!(cfg.server.http_bind, "127.0.0.1:8081".parse().unwrap());
    }

    #[test]
    fn no_hls_connector_turns_ts_hls_off() {
        let (result, cfg) = imported(r#"{"config":{"protocols":[{"connector":"HTTP","port":8080}]}}"#);
        assert!(!cfg.hls.ts, "{}", result.toml);
        assert!(has(&result, NoteKind::Translated, "no HLS connector", "ts = false"));
    }

    #[test]
    fn https_connector_with_cert_and_key_becomes_tls() {
        let mist = r#"{"config":{"protocols":[
            {"connector":"HTTP","port":8080},
            {"connector":"HTTPS","port":4433,"cert":"/etc/ssl/fullchain.pem","key":"/etc/ssl/privkey.pem"}
        ]}}"#;
        let (_, cfg) = imported(mist);
        assert_eq!(cfg.tls.bind, Some("127.0.0.1:4433".parse().unwrap()));
        assert_eq!(cfg.tls.cert.as_deref(), Some(Path::new("/etc/ssl/fullchain.pem")));
        let (result, cfg) = imported(r#"{"config":{"protocols":[{"connector":"HTTPS","port":4433}]}}"#);
        assert!(cfg.tls.bind.is_none());
        assert!(has(&result, NoteKind::NoEquivalent, "HTTPS", "cert"));
    }

    #[test]
    fn always_on_outputs_are_reported_with_their_caudal_paths() {
        let mist = r#"{"config":{"protocols":[
            {"connector":"HTTPTS"},{"connector":"AAC"},{"connector":"MP3"},{"connector":"CMAF"},
            {"connector":"JPG"},{"connector":"DTSC","port":4200},{"connector":"HLS"}
        ]}}"#;
        let (result, _) = imported(mist);
        assert!(has(&result, NoteKind::Approximated, "HTTPTS", "/ts/{name}.ts"));
        assert!(has(&result, NoteKind::Approximated, "AAC", "/audio/{name}.aac"));
        assert!(has(&result, NoteKind::Approximated, "MP3", "/audio/{name}.mp3"));
        assert!(has(&result, NoteKind::Approximated, "CMAF", "DASH"));
        assert!(has(&result, NoteKind::Approximated, "JPG", "thumbnail.jpg"));
        assert!(has(&result, NoteKind::NoEquivalent, "DTSC", "no Caudal output"));
    }

    #[test]
    fn srt_connector_keeps_its_passphrase() {
        let mist = r#"{"config":{"protocols":[{"connector":"TSSRT","port":8889,"passphrase":"0123456789abcdef"}]}}"#;
        let (_, cfg) = imported(mist);
        assert_eq!(cfg.srt.bind, "0.0.0.0:8889".parse().unwrap());
        assert_eq!(cfg.srt.passphrase.as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn push_stream_with_ip_becomes_access_rule() {
        let mist = r#"{"streams":{"cam1":{"source":"push://10.0.0.5","DVR":30000}}}"#;
        let (result, _) = imported(mist);
        assert!(result.toml.contains("[[access.rules]]"), "{}", result.toml);
        assert!(result.toml.contains("publish_allow = [\"10.0.0.5\"]"), "{}", result.toml);
        assert!(result.toml.contains("[buffer]\nwindow_secs = 30"), "{}", result.toml);
    }

    #[test]
    fn push_stream_password_is_reported() {
        let (result, cfg) = imported(r#"{"streams":{"cam1":{"source":"push://10.0.0.5@hunter2"}}}"#);
        assert_eq!(cfg.access.rules.len(), 1);
        assert!(has(&result, NoteKind::NoEquivalent, "cam1.source", "password"));
        assert!(!result.toml.contains("hunter2"));
    }

    #[test]
    fn push_stream_with_hostname_has_no_equivalent() {
        let (result, _) = imported(r#"{"streams":{"cam1":{"source":"push://encoder.example.com"}}}"#);
        assert!(has(&result, NoteKind::NoEquivalent, "cam1", "hostname"));
        assert!(!result.toml.contains("access.rules"));
    }

    #[test]
    fn rtsp_pull_source_translates() {
        let (_, cfg) = imported(r#"{"streams":{"cam1":{"source":"rtsp://user:pass@192.0.2.10/stream1"}}}"#);
        assert_eq!(cfg.rtsp.pull[0].stream, "cam1");
        assert_eq!(cfg.rtsp.pull[0].url, "rtsp://user:pass@192.0.2.10/stream1");
    }

    #[test]
    fn srt_caller_source_becomes_srt_pull() {
        let mist = r#"{"streams":{"venue":{"source":"srt://203.0.113.7:9000?streamid=play/main&latency=200"}}}"#;
        let (result, cfg) = imported(mist);
        assert_eq!(cfg.srt.pull[0].stream, "venue");
        assert_eq!(cfg.srt.pull[0].url, "srt://203.0.113.7:9000?streamid=play/main&latency=200");
        assert!(has(&result, NoteKind::Translated, "venue.source", "[[srt.pull]]"));
    }

    #[test]
    fn srt_listener_source_is_not_a_pull() {
        for src in ["srt://:9001", "srt://0.0.0.0:9001", "srt://198.51.100.1:9001?mode=listener"] {
            let (result, cfg) = imported(&format!(r#"{{"streams":{{"in":{{"source":"{src}"}}}}}}"#));
            assert!(cfg.srt.pull.is_empty(), "{src}");
            assert!(has(&result, NoteKind::NoEquivalent, "in.source", "listener"), "{src}");
        }
        let (result, _) = imported(r#"{"streams":{"in":{"source":"srt://198.51.100.1:9001?mode=rendezvous"}}}"#);
        assert!(has(&result, NoteKind::NoEquivalent, "in.source", "rendezvous"));
    }

    #[test]
    fn srt_source_with_unknown_options_is_approximated() {
        let (result, cfg) = imported(r#"{"streams":{"v":{"source":"srt://203.0.113.7:9000?conntimeo=3000"}}}"#);
        assert_eq!(cfg.srt.pull.len(), 1);
        assert!(has(&result, NoteKind::Approximated, "v.source", "conntimeo"));
    }

    #[test]
    fn tsudp_source_becomes_udp_pull() {
        let mist = r#"{"streams":{
            "tv":{"source":"tsudp://239.1.1.1:5000"},
            "enc":{"source":"tsudp://:5002"},
            "raw":{"source":"udp://0.0.0.0:5004"}
        }}"#;
        let (result, cfg) = imported(mist);
        let urls: Vec<(&str, &str)> = cfg.udp.pull.iter().map(|p| (p.stream.as_str(), p.url.as_str())).collect();
        assert!(urls.contains(&("tv", "udp://239.1.1.1:5000")), "{urls:?}");
        assert!(urls.contains(&("enc", "udp://:5002")), "{urls:?}");
        assert!(urls.contains(&("raw", "udp://0.0.0.0:5004")), "{urls:?}");
        assert!(has(&result, NoteKind::Translated, "tv.source", "tsudp://"));
    }

    #[test]
    fn two_udp_pulls_on_one_unicast_port_import_only_the_first() {
        let mist = r#"{"streams":{"a":{"source":"tsudp://:5000"},"b":{"source":"tsudp://0.0.0.0:5000"}}}"#;
        let (result, cfg) = imported(mist);
        assert_eq!(cfg.udp.pull.len(), 1);
        assert!(has(&result, NoteKind::NoEquivalent, "b.source", "already used"));
    }

    #[test]
    fn udp_source_with_a_hostname_is_reported_not_fatal() {
        let (result, cfg) = imported(r#"{"streams":{"a":{"source":"tsudp://encoder.local:5000"}}}"#);
        assert!(cfg.udp.pull.is_empty());
        assert!(has(&result, NoteKind::NoEquivalent, "a.source", "[[udp.pull]]"));
    }

    #[test]
    fn http_ts_sources_become_http_pulls() {
        let mist = r#"{"streams":{
            "a":{"source":"http-ts://10.0.0.5:8080/stream"},
            "b":{"source":"https://irdbox.example/live/feed.ts"}
        }}"#;
        let (_, cfg) = imported(mist);
        let urls: Vec<&str> = cfg.http.pull.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["http://10.0.0.5:8080/stream", "https://irdbox.example/live/feed.ts"]);
    }

    #[test]
    fn m3u8_sources_become_hls_pulls() {
        let mist = r#"{"streams":{
            "tv":{"source":"https://cdn.example/live/master.m3u8"},
            "tv2":{"source":"http-hls://cdn.example/live/index.m3u8"}
        }}"#;
        let (result, cfg) = imported(mist);
        let urls: Vec<&str> = cfg.hls.pull.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["https://cdn.example/live/master.m3u8", "http://cdn.example/live/index.m3u8"]);
        assert!(has(&result, NoteKind::Approximated, "tv.source", "MPEG-TS segments"));
    }

    #[test]
    fn http_vod_file_is_not_a_pull() {
        let (result, cfg) = imported(r#"{"streams":{"m":{"source":"https://media.example/movie.mp4"}}}"#);
        assert!(cfg.http.pull.is_empty() && cfg.hls.pull.is_empty());
        assert!(has(&result, NoteKind::NoEquivalent, "m.source", "VoD"));
    }

    #[test]
    fn rtmp_pull_source_becomes_rtmp_pull() {
        let mist = r#"{"streams":{"cam1":{"source":"rtmp://origin.example.com/live/cam1","always_on":true}}}"#;
        let (result, cfg) = imported(mist);
        assert_eq!(cfg.rtmp.pull[0].url, "rtmp://origin.example.com/live/cam1");
        assert!(has(&result, NoteKind::Translated, "cam1.always_on", "always on"));
    }

    #[test]
    fn invalid_stream_name_is_skipped_with_a_note() {
        let (result, cfg) = imported(r#"{"streams":{"bad name":{"source":"rtmp://o.example/live/x"}}}"#);
        assert!(cfg.rtmp.pull.is_empty());
        assert!(has(&result, NoteKind::NoEquivalent, "bad name", "valid Caudal stream name"));
    }

    #[test]
    fn file_source_becomes_a_channel() {
        let (result, _) = imported(r#"{"streams":{"tv":{"source":"/media/intro.mp4"}}}"#);
        assert!(result.toml.contains("[[channel]]"), "{}", result.toml);
        assert!(result.toml.contains("name = \"tv\""), "{}", result.toml);
        assert!(result.toml.contains("items = [\"/media/intro.mp4\"]"), "{}", result.toml);
    }

    #[test]
    fn auto_push_network_targets_become_push_auto() {
        let mist = r#"{"auto_push":{
            "1":{"stream":"main","target":"rtmp://a.rtmp.youtube.com/live2/xxxx"},
            "2":{"stream":"main","target":"srt://203.0.113.9:9000?streamid=publish/$stream"},
            "3":{"stream":"main","target":"tsudp://10.0.0.20:5000"},
            "4":{"stream":"main","target":"tsrtp://239.1.1.9:5004"}
        }}"#;
        let (_, cfg) = imported(mist);
        let urls: Vec<&str> = cfg.push.auto.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "rtmp://a.rtmp.youtube.com/live2/xxxx",
                "srt://203.0.113.9:9000?streamid=publish/$stream",
                "udp://10.0.0.20:5000",
                "rtp://239.1.1.9:5004"
            ]
        );
        assert!(cfg.push.auto.iter().all(|r| r.streams == ["main"]));
    }

    #[test]
    fn auto_push_unsupported_targets_and_selectors_are_reported() {
        let mist = r##"{"auto_push":[
            {"stream":"main","target":"whip://edge.example/whip/main"},
            {"stream":"main","target":"rtmp://x.example/live/$datetime"},
            {"stream":"#cams","target":"rtmp://x.example/live/$stream"},
            {"stream":"live+","target":"rtmp://x.example/live/$stream"},
            {"stream":"main","target":"srt://203.0.113.9"}
        ]}"##;
        let (result, cfg) = imported(mist);
        assert!(cfg.push.auto.is_empty(), "{}", result.toml);
        assert!(has(&result, NoteKind::NoEquivalent, "main", "whip://"));
        assert!(has(&result, NoteKind::NoEquivalent, "main", "datetime"));
        assert!(has(&result, NoteKind::NoEquivalent, "#cams", "tag"));
        assert!(has(&result, NoteKind::NoEquivalent, "live+", "wildcard"));
        assert!(has(&result, NoteKind::NoEquivalent, "main", "port"));
    }

    #[test]
    fn auto_push_file_target_becomes_record() {
        let (result, cfg) = imported(r#"{"auto_push":[{"stream":"main","target":"/records/main.mp4"}]}"#);
        assert!(cfg.record.enabled, "{}", result.toml);
        assert_eq!(cfg.record.streams, ["main"]);
        assert_eq!(cfg.record.dir, Path::new("/records"));
    }

    #[test]
    fn scheduled_file_target_becomes_record_schedule() {
        let mist = r#"{"auto_push":[{"stream":"show","target":"/rec/show.mkv",
            "scheduletime":1767225600,"completetime":1767229200}]}"#;
        let (result, cfg) = imported(mist);
        assert!(cfg.record.streams.is_empty(), "only the window records: {}", result.toml);
        let s = &cfg.record.schedule[0];
        assert_eq!(s.streams, ["show"]);
        assert_eq!(s.start.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(s.duration_secs, 3600);
    }

    #[test]
    fn scheduled_network_push_gets_its_window() {
        let mist = r#"{"auto_push":[{"stream":"backup","target":"rtmp://live.twitch.tv/app/key",
            "scheduletime":1767225600,"completetime":1767229200}]}"#;
        let (result, cfg) = imported(mist);
        let r = &cfg.push.auto[0];
        assert_eq!(r.streams, ["backup"]);
        assert_eq!(r.start.as_deref(), Some("2026-01-01T00:00:00Z"), "{}", result.toml);
        assert_eq!(r.stop.as_deref(), Some("2026-01-01T01:00:00Z"));
        assert!(has(&result, NoteKind::Approximated, "backup", "only inside the window"));
    }

    #[test]
    fn auto_push_with_conditions_is_not_imported() {
        let mist = r#"{"auto_push":[{"stream":"main","target":"rtmp://x.example/live","inhibit":"backup"},
            {"stream":"main","target":"/rec/main.mkv","start_rule":["viewers",10,"5"]}]}"#;
        let (result, cfg) = imported(mist);
        assert!(cfg.push.auto.is_empty() && !cfg.record.enabled, "{}", result.toml);
        assert!(has(&result, NoteKind::NoEquivalent, "main", "inhibit"));
        assert!(has(&result, NoteKind::NoEquivalent, "main", "start_rule"));
    }

    #[test]
    fn legacy_autopushes_array_is_not_guessed_at() {
        let (result, _) = imported(r#"{"autopushes":[["main","rtmp://x/live"]]}"#);
        assert!(result.notes.iter().any(|n| n.mist == "autopushes"));
        assert!(!result.toml.contains("push.auto"));
    }

    #[test]
    fn blocking_user_new_and_stream_push_become_auth_hook() {
        let mist = r#"{"config":{"triggers":{
            "USER_NEW":[["https://auth.example.com/mist",true,[]]],
            "STREAM_PUSH":[{"handler":"https://auth.example.com/mist","sync":true,"streams":[],"default":"false"}]
        }}}"#;
        let (result, cfg) = imported(mist);
        let hook = cfg.auth.hook.as_ref().expect("[auth.hook]");
        assert_eq!(hook.url, "https://auth.example.com/mist");
        assert!(hook.publish && hook.play);
        assert!(hook.secret.starts_with("whsec_") && hook.secret.len() > 40);
        assert_eq!(hook.on_error, "deny", "a false default fails closed");
        assert_eq!(hook.reply, "mistserver", "the body decides, as in MistServer");
        assert!(has(&result, NoteKind::Approximated, "USER_NEW", "200 `false` still denies"));
        assert!(has(&result, NoteKind::Approximated, "STREAM_PUSH", "publish = true"));
        // The secret is a valid Standard Webhooks key.
        caudal_auth::GateHook::new(hook.to_config().unwrap(), Default::default()).unwrap();
    }

    #[test]
    fn auth_hook_without_a_false_default_fails_open_like_mistserver() {
        let mist = r#"{"config":{"triggers":{"CONN_PLAY":[["http://127.0.0.1:9000/play",true,["cam1"]]]}}}"#;
        let (result, cfg) = imported(mist);
        let hook = cfg.auth.hook.as_ref().unwrap();
        assert!(hook.play && !hook.publish);
        assert_eq!(hook.on_error, "allow");
        assert!(has(&result, NoteKind::Approximated, "CONN_PLAY", "only for streams"));
    }

    #[test]
    fn non_blocking_executable_and_extra_gate_handlers_are_reported() {
        let mist = r#"{"config":{"triggers":{
            "USER_NEW":[["https://a.example/1",false,[]]],
            "STREAM_PUSH":[["/usr/local/bin/check.sh",true,[]]],
            "CONN_PLAY":[["https://a.example/2",true,[]],["https://a.example/3",true,[]]]
        }}}"#;
        let (result, cfg) = imported(mist);
        let hook = cfg.auth.hook.as_ref().unwrap();
        assert_eq!(hook.url, "https://a.example/2");
        assert!(has(&result, NoteKind::NoEquivalent, "USER_NEW", "non-blocking"));
        assert!(has(&result, NoteKind::NoEquivalent, "STREAM_PUSH", "executable"));
        assert!(has(&result, NoteKind::NoEquivalent, "CONN_PLAY", "extra handler"));
    }

    #[test]
    fn rewrite_triggers_become_auth_rewrite() {
        let mist = r#"{"config":{"triggers":{
            "PUSH_REWRITE":[["https://route.example.com/mist",true,"",""]],
            "PLAY_REWRITE":[["https://other.example.com/mist",true,[]]],
            "RTMP_PUSH_REWRITE":[["https://route.example.com/rtmp",true,[]]]
        }}}"#;
        let (result, cfg) = imported(mist);
        let r = cfg.auth.rewrite.as_ref().expect("[auth.rewrite]");
        assert_eq!(r.url, "https://route.example.com/mist");
        assert!(r.publish && !r.play, "PLAY_REWRITE has another URL");
        assert_eq!(r.on_error, "deny");
        assert!(has(&result, NoteKind::Approximated, "PUSH_REWRITE", "plain"));
        assert!(has(&result, NoteKind::NoEquivalent, "PLAY_REWRITE", "one `url`"));
        assert!(has(&result, NoteKind::NoEquivalent, "RTMP_PUSH_REWRITE", "whole RTMP URL"));
        caudal_auth::RewriteHook::new(r.to_config().unwrap(), Default::default()).unwrap();
    }

    #[test]
    fn other_triggers_are_never_auto_mapped() {
        let mist = r#"{"config":{"triggers":{"STREAM_BUFFER":[["http://example/hook",false,"",""]],
            "STREAM_SOURCE":[["http://example/src",true,[]]]}}}"#;
        let (result, cfg) = imported(mist);
        assert!(has(&result, NoteKind::NoEquivalent, "STREAM_BUFFER", "[health]"));
        assert!(has(&result, NoteKind::NoEquivalent, "STREAM_SOURCE", "on demand"));
        assert!(cfg.auth.hook.is_none() && cfg.auth.rewrite.is_none());
        assert!(!result.toml.contains("[hooks]"));
    }

    #[test]
    fn accounts_are_never_migrated() {
        let (result, _) = imported(r#"{"account":{"ana":{"password":"5f4dcc3b5aa765d61d8327deb882cf99"}}}"#);
        assert!(result.notes.iter().any(|n| n.mist == "account" && n.detail.contains("hash-password")));
        assert!(!result.toml.contains("password_hash = \"5f4dcc3b"));
    }

    #[test]
    fn admin_block_is_always_commented_out() {
        let result = import("{}").unwrap();
        assert!(result.toml.contains("# [admin]"));
        assert!(!result.toml.contains("\n[admin]\n"));
    }

    #[test]
    fn strings_are_escaped_in_the_toml() {
        let (_, cfg) = imported(r#"{"streams":{"tv":{"source":"/media/a \"b\" \\c.mp4"}}}"#);
        assert_eq!(cfg.channel[0].items[0], Path::new("/media/a \"b\" \\c.mp4"));
    }

    #[test]
    fn rfc3339_from_unix() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_798_761_599), "2026-12-31T23:59:59Z");
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(b"foobar"), "Zm9vYmFy");
        assert_ne!(random_webhook_secret(), random_webhook_secret());
    }

    #[test]
    fn invalid_json_is_a_clean_error() {
        let err = import("not json").unwrap_err();
        assert!(err.contains("parsing"), "{err}");
    }
}
