//! `caudal import-mist`: reads a MistServer `config.json` (also commonly
//! named `mistserver.conf`; it is JSON either way, MistServer never writes
//! an INI-style file) and produces a Caudal `caudal.toml` plus a report of
//! every setting that was translated, approximated, or has no Caudal
//! equivalent.
//!
//! MistServer's controller config has no published JSON Schema, so the
//! shape below is read from its own source
//! (github.com/DDVTECH/mistserver, Unlicense), not from documentation:
//!
//! - `config.protocols`: the connector list. `CheckProtocols` in
//!   `src/controller/controller_connectors.cpp` reads `(*ait)["connector"]`
//!   plus whatever other keys that connector's `capa["required"]` /
//!   `capa["optional"]` need (typically `port`, `interface`). Connector
//!   names come from each output's own `capa["name"]`, e.g. `"RTMP"`
//!   (`src/output/output_rtmp.cpp:503`), `"HLS"` (`output_hls.cpp:193`,
//!   which sets `capa["deps"] = "HTTP"` — HLS rides the HTTP connector's
//!   port, it has none of its own), `"TSSRT"` (`output_tssrt.cpp:410`),
//!   `"RTSP"` (`output_rtsp.cpp:63`), `"WebRTC"` (`output_webrtc.cpp:547`).
//! - `streams.<name>.source` / `.DVR`: `CheckStreams`'s doc comment in
//!   `src/controller/controller_streams.cpp` (~line 260-300). `source` is
//!   a `push://<host>` (host empty = any publisher), a pull URL
//!   (`rtsp://`, `rtmp://`, `srt://`, ...), or a local file/folder path.
//!   `DVR` is the requested DVR buffer size **in milliseconds**.
//! - `auto_push` (and the legacy `autopushes` array, upgraded to
//!   `auto_push` the first time a MistServer instance with them loads —
//!   see `controller_storage.cpp` ~line 650): each entry has `stream`,
//!   `target`, and optional scheduling (`scheduletime`, `completetime`,
//!   `start_rule`, `end_rule`, `inhibit`) — `controller_push.cpp`'s
//!   `makePushObject`.
//! - `config.triggers`: an object keyed by trigger name (`STREAM_BUFFER`,
//!   `PUSH_REWRITE`, `USER_NEW`, ...), each value one array per registered
//!   handler, `[url, sync, streams, params, default]`
//!   (`controller_storage.cpp` ~line 930-970).
//! - `account.<user>.password`: an MD5 hash (`Secure::md5`,
//!   `src/controller/controller.cpp`'s `createAccount`) — a different
//!   format from Caudal's argon2id, so users are never migrated.
//!
//! The output TOML is validated the same way `caudal check` validates any
//! config file (parse + [`crate::config::Config::validate`]) before it is
//! written; [`run`] fails loudly and writes nothing if that fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
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
    /// a global setting, a pattern that can't be reproduced exactly, ...
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
        let _ =
            writeln!(out, "{} translated, {} approximated, {} with no Caudal equivalent", counts.0, counts.1, counts.2);
        for (kind, heading) in [
            (NoteKind::Translated, "\nTranslated"),
            (NoteKind::Approximated, "\nApproximated"),
            (NoteKind::NoEquivalent, "\nNo Caudal equivalent"),
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

/// `auto_push`/`autopushes` entries: MistServer's own `JSON::Value::append`
/// turns an initially-null value into an array, so `auto_push` is an array
/// in practice; accept an object too (keyed by id) in case a hand-edited
/// file uses one.
fn push_entries(v: &Json) -> Vec<&Json> {
    match v {
        Json::Array(a) => a.iter().collect(),
        Json::Object(o) => o.values().collect(),
        _ => Vec::new(),
    }
}

const PUSH_SCHEDULING_KEYS: [&str; 5] = ["scheduletime", "completetime", "start_rule", "end_rule", "inhibit"];

/// Parses `mist_json` and builds the Caudal TOML plus the report. Does not
/// touch the filesystem.
pub fn import(mist_json: &str) -> Result<ImportResult, String> {
    let file: MistFile = serde_json::from_str(mist_json).map_err(|e| format!("parsing MistServer config: {e}"))?;
    let mut notes = Vec::new();

    // --- connectors -> [server]/[rtmp]/[srt]/[rtsp]/[webrtc] ---
    let mut http_port: Option<u16> = None;
    let mut rtmp_port: Option<u16> = None;
    let mut srt_port: Option<u16> = None;
    let mut rtsp_port: Option<u16> = None;
    let mut webrtc_port: Option<u16> = None;
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();

    for p in &file.config.protocols {
        let n = seen.entry(p.connector.clone()).or_insert(0);
        *n += 1;
        let first = *n == 1;
        let mist_key = format!("config.protocols[connector={}]", p.connector);
        let port = protocol_port(p);

        let mut bind_default = |slot: &mut Option<u16>,
                                section: &str,
                                key: &str,
                                default: u16,
                                caveat: Option<&str>| {
            if !first {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    mist_key.clone(),
                    format!("extra `{}` listener not imported: Caudal has one `[{section}] {key}` bind", p.connector),
                ));
                return;
            }
            let bound = port.unwrap_or(default);
            *slot = Some(bound);
            let kind = if port.is_some() && caveat.is_none() { NoteKind::Translated } else { NoteKind::Approximated };
            let mut detail = match port {
                Some(port) => format!("-> `[{section}] {key}` on port {port}"),
                None => format!("no `port` given; kept Caudal's default `[{section}] {key}` port {default}"),
            };
            if let Some(c) = caveat {
                detail.push_str("; ");
                detail.push_str(c);
            }
            notes.push(note(kind, mist_key.clone(), detail));
        };

        match p.connector.as_str() {
            "HTTP" | "HTTPS" => bind_default(
                &mut http_port,
                "server",
                "http_bind",
                8080,
                Some(
                    "bound to 127.0.0.1, not MistServer's usual 0.0.0.0: without `[admin]` Caudal \
                     refuses to start on a non-loopback address — set up `[admin]` (commented in below), \
                     then change this to 0.0.0.0 to match MistServer's original reach",
                ),
            ),
            "HLS" => notes.push(note(
                NoteKind::Translated,
                mist_key,
                "served on `[server] http_bind` already: MistServer's HLS output has no port of its own \
                 either (`capa[\"deps\"] = \"HTTP\"` in output_hls.cpp), it rides the HTTP connector"
                    .to_string(),
            )),
            "RTMP" => bind_default(&mut rtmp_port, "rtmp", "bind", 1935, None),
            "TSSRT" | "SRT" => bind_default(&mut srt_port, "srt", "bind", 9000, None),
            "RTSP" => bind_default(&mut rtsp_port, "rtsp", "bind", 8554, None),
            "WebRTC" => bind_default(&mut webrtc_port, "webrtc", "udp_bind", 8189, None),
            other => notes.push(note(
                NoteKind::NoEquivalent,
                mist_key,
                format!(
                    "connector `{other}` has no Caudal output (not built, or one of MistServer's \
                     legacy outputs — HDS, Flash, Smooth Streaming — that Caudal never ports, see PLAN.md)"
                ),
            )),
        }
    }

    // --- streams -> [buffer]/[[channel]]/[[access.rules]]/[[rtsp.pull]] ---
    let mut max_dvr_ms: Option<u64> = None;
    let mut channels: Vec<(String, String)> = Vec::new();
    let mut access_rules: Vec<(String, String)> = Vec::new();
    let mut rtsp_pulls: Vec<(String, String)> = Vec::new();

    for (name, s) in &file.streams {
        if let Some(dvr) = s.dvr_ms {
            max_dvr_ms = Some(max_dvr_ms.map_or(dvr, |m| m.max(dvr)));
            notes.push(note(
                NoteKind::Approximated,
                format!("streams.{name}.DVR"),
                format!(
                    "-> `[buffer] window_secs` ({} ms asked here); Caudal's buffer window is one \
                     global setting, not per stream, so the largest DVR across all streams wins",
                    dvr
                ),
            ));
        }

        match s.source.as_deref().map(str::trim) {
            None | Some("") => notes.push(note(
                NoteKind::Translated,
                format!("streams.{name}.source"),
                "no source (or empty `push://`): publish is open to anyone under this stream name, \
                 same as Caudal's default (no `[auth]`/`[[access.rules]]` restriction)"
                    .to_string(),
            )),
            Some(src) if src.starts_with("push://") => {
                let host = src["push://".len()..].trim();
                if host.is_empty() {
                    notes.push(note(
                        NoteKind::Translated,
                        format!("streams.{name}.source"),
                        "`push://` with no host: publish is open to anyone, same as Caudal's default".to_string(),
                    ));
                } else if caudal_core::Cidr::parse(host).is_ok() {
                    access_rules.push((name.clone(), host.to_string()));
                    notes.push(note(
                        NoteKind::Translated,
                        format!("streams.{name}.source"),
                        format!(
                            "`push://{host}` -> `[[access.rules]]` streams=[\"{name}\"] publish_allow=[\"{host}\"]"
                        ),
                    ));
                } else {
                    notes.push(note(
                        NoteKind::NoEquivalent,
                        format!("streams.{name}.source"),
                        format!(
                            "`push://{host}`: `{host}` is a hostname, not an IP/CIDR; Caudal's \
                             `[[access.rules]]` only takes IP/CIDR or `country:XX` — resolve it to an \
                             address and add the rule by hand"
                        ),
                    ));
                }
            }
            Some(src) if src.starts_with("rtsp://") => {
                rtsp_pulls.push((name.clone(), src.to_string()));
                notes.push(note(
                    NoteKind::Translated,
                    format!("streams.{name}.source"),
                    format!("-> `[[rtsp.pull]]` stream=\"{name}\" url=\"{src}\""),
                ));
            }
            Some(src) if src.starts_with("rtmp://") || src.starts_with("rtmps://") => notes.push(note(
                NoteKind::NoEquivalent,
                format!("streams.{name}.source"),
                format!(
                    "`{src}`: Caudal has no RTMP/RTMPS pull ingest (only publish-in and push-out via \
                     `[[restream]]`); pull it with `ffmpeg -i {src} -c copy -f flv rtmp://.../live/{name}` \
                     into Caudal instead"
                ),
            )),
            Some(src) if src.starts_with("srt://") => notes.push(note(
                NoteKind::NoEquivalent,
                format!("streams.{name}.source"),
                format!(
                    "`{src}`: Caudal has no SRT pull ingest (only listen-for-push `[srt]` and push-out \
                     `[[srt.push]]`); repoint the encoder to push directly, or relay with ffmpeg/srt-live-transmit"
                ),
            )),
            Some(src) if !src.contains("://") => {
                channels.push((name.clone(), src.to_string()));
                notes.push(note(
                    NoteKind::Approximated,
                    format!("streams.{name}.source"),
                    format!(
                        "file/folder `{src}` -> `[[channel]]` name=\"{name}\" items=[\"{src}\"]; MistServer \
                         serves this on demand as VoD, Caudal's `[[channel]]` plays it as a looping 24/7 \
                         live stream (loop = true) — the closest equivalent, not identical semantics"
                    ),
                ));
            }
            Some(other) => notes.push(note(
                NoteKind::NoEquivalent,
                format!("streams.{name}.source"),
                format!("`{other}`: unrecognized source scheme, not imported"),
            )),
        }

        for k in s.rest.keys() {
            if k == "name" {
                continue;
            }
            notes.push(note(
                NoteKind::NoEquivalent,
                format!("streams.{name}.{k}"),
                "no per-stream Caudal equivalent".to_string(),
            ));
        }
    }

    // --- auto_push / autopushes -> [[restream]] / [record] ---
    let mut restreams: Vec<(String, String)> = Vec::new();
    let mut record_streams: BTreeSet<String> = BTreeSet::new();
    let mut record_dir: Option<String> = None;

    let push_source = if !matches!(file.auto_push, Json::Null) && !push_entries(&file.auto_push).is_empty() {
        Some(("auto_push", &file.auto_push))
    } else if !matches!(file.autopushes, Json::Null) && !push_entries(&file.autopushes).is_empty() {
        notes.push(note(
            NoteKind::Approximated,
            "autopushes".to_string(),
            "legacy positional format: MistServer upgrades this to `auto_push` the first time it \
                 loads the config (controller_storage.cpp); start that MistServer instance once and \
                 re-export, or convert the entries to `auto_push` objects by hand before importing"
                .to_string(),
        ));
        None
    } else {
        None
    };

    if let Some((key, val)) = push_source {
        for entry in push_entries(val) {
            let Some(obj) = entry.as_object() else { continue };
            let stream = obj.get("stream").and_then(Json::as_str);
            let target = obj.get("target").and_then(Json::as_str);
            let (Some(stream), Some(target)) = (stream, target) else {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    key.to_string(),
                    "push entry missing `stream` or `target`, skipped".to_string(),
                ));
                continue;
            };
            let mist_key = format!("{key}[stream={stream}]");

            if target.starts_with("rtmp://") || target.starts_with("rtmps://") {
                restreams.push((stream.to_string(), target.to_string()));
                notes.push(note(
                    NoteKind::Translated,
                    mist_key.clone(),
                    format!("-> `[[restream]]` stream=\"{stream}\" url=\"{target}\""),
                ));
            } else if !target.contains("://") {
                record_streams.insert(stream.to_string());
                if record_dir.is_none()
                    && let Some(parent) = Path::new(target).parent().filter(|p| !p.as_os_str().is_empty())
                {
                    record_dir = Some(parent.display().to_string());
                }
                notes.push(note(
                    NoteKind::Approximated,
                    mist_key.clone(),
                    format!(
                        "file target `{target}` -> `[record]` streams += [\"{stream}\"]; MistServer's \
                         per-push path pattern (e.g. `$Y$m$d`) is not reproduced — Caudal always writes \
                         to `<dir>/<stream>/<timestamp>/`"
                    ),
                ));
            } else {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    mist_key.clone(),
                    format!("target `{target}`: not an rtmp(s):// URL or a local path, no push-out equivalent"),
                ));
            }

            let lost: Vec<&str> = PUSH_SCHEDULING_KEYS.iter().copied().filter(|k| obj.contains_key(*k)).collect();
            if !lost.is_empty() {
                notes.push(note(
                    NoteKind::NoEquivalent,
                    mist_key,
                    format!(
                        "{}: Caudal's `[[restream]]`/`[record]` have no scheduling, variable rules, or \
                         inhibit condition — the push runs immediately and always while the stream is live",
                        lost.join(", ")
                    ),
                ));
            }
        }
    }

    // --- triggers: genuinely different mechanism, never auto-mapped ---
    for (name, defs) in &file.config.triggers {
        let count = match defs {
            Json::Array(a) => a.len(),
            Json::Null => 0,
            _ => 1,
        };
        if count == 0 {
            continue;
        }
        notes.push(note(
            NoteKind::NoEquivalent,
            format!("config.triggers.{name}"),
            format!(
                "{count} handler(s): MistServer triggers are synchronous calls that can rewrite or \
                 cancel the action they fire on (controller_storage.cpp, ~line 930); Caudal's closest \
                 features are `[hooks]` (signed webhooks, fire-and-forget, only on stream.started/\
                 stream.ended) and `[health]` (alerts on stream problems) — neither can rewrite or \
                 block anything. Configure `[hooks]`/`[health]` by hand if you need notifications."
            ),
        ));
    }

    // --- account: never migrated, different hash format ---
    if !file.account.is_empty() {
        let mut names: Vec<&String> = file.account.keys().collect();
        names.sort();
        let names = names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
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

    let toml = build_toml(BuiltConfig {
        http_port,
        rtmp_port,
        srt_port,
        rtsp_port,
        webrtc_port,
        buffer_window_secs: max_dvr_ms.map(|ms| ms.div_ceil(1000).max(1)),
        channels,
        access_rules,
        rtsp_pulls,
        restreams,
        record_streams,
        record_dir,
    });

    Ok(ImportResult { toml, notes })
}

struct BuiltConfig {
    http_port: Option<u16>,
    rtmp_port: Option<u16>,
    srt_port: Option<u16>,
    rtsp_port: Option<u16>,
    webrtc_port: Option<u16>,
    buffer_window_secs: Option<u64>,
    channels: Vec<(String, String)>,
    access_rules: Vec<(String, String)>,
    rtsp_pulls: Vec<(String, String)>,
    restreams: Vec<(String, String)>,
    record_streams: BTreeSet<String>,
    record_dir: Option<String>,
}

fn build_toml(c: BuiltConfig) -> String {
    let mut out = String::new();
    out.push_str(
        "# Generated by `caudal import-mist` from a MistServer config. Validated against\n\
         # Caudal's own rules already (same checks as `caudal check`); read the import\n\
         # report printed alongside this file for what was approximated or dropped.\n\
         # `caudal doctor --config <this file>` checks the setup it describes next.\n",
    );

    if let Some(port) = c.http_port {
        // Loopback, not MistServer's usual 0.0.0.0: without `[admin]`,
        // Caudal refuses to start on a non-loopback address (the commented
        // block below). Set up `[admin]`, then change this to 0.0.0.0 to
        // match MistServer's original reach.
        let _ = write!(out, "\n[server]\nhttp_bind = \"127.0.0.1:{port}\"\n");
    }
    if let Some(port) = c.rtmp_port {
        let _ = write!(out, "\n[rtmp]\nbind = \"0.0.0.0:{port}\"\napp = \"live\"\n");
    }
    if let Some(port) = c.srt_port {
        let _ = write!(out, "\n[srt]\nbind = \"0.0.0.0:{port}\"\n");
    }
    if let Some(secs) = c.buffer_window_secs {
        let _ = write!(out, "\n[buffer]\nwindow_secs = {secs}\n");
    }
    if let Some(port) = c.rtsp_port {
        let _ = write!(out, "\n[rtsp]\nbind = \"0.0.0.0:{port}\"\n");
    }
    for (stream, url) in &c.rtsp_pulls {
        let _ = write!(out, "\n[[rtsp.pull]]\nstream = \"{stream}\"\nurl = \"{url}\"\n");
    }
    if let Some(port) = c.webrtc_port {
        let _ = write!(out, "\n[webrtc]\nudp_bind = \"0.0.0.0:{port}\"\n");
    }
    for (name, item) in &c.channels {
        let _ = write!(out, "\n[[channel]]\nname = \"{name}\"\nitems = [\"{item}\"]\n");
    }
    for (stream, url) in &c.restreams {
        let _ = write!(out, "\n[[restream]]\nstream = \"{stream}\"\nurl = \"{url}\"\n");
    }
    if !c.record_streams.is_empty() {
        out.push_str("\n[record]\nenabled = true\n");
        if let Some(dir) = &c.record_dir {
            let _ = writeln!(out, "dir = \"{dir}\"");
        }
        let streams = c.record_streams.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
        let _ = writeln!(out, "streams = [{streams}]");
    }
    for (stream, host) in &c.access_rules {
        let _ = write!(out, "\n[[access.rules]]\nstreams = [\"{stream}\"]\npublish_allow = [\"{host}\"]\n");
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
fn validate_generated(toml_text: &str) -> Result<(), String> {
    let cfg: config::Config = toml::from_str(toml_text).map_err(|e| e.to_string())?;
    cfg.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_mist_config_imports_to_a_valid_empty_toml() {
        let result = import("{}").unwrap();
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn rtmp_and_http_connectors_translate() {
        let mist = r#"{"config":{"protocols":[
            {"connector":"RTMP","port":1935},
            {"connector":"HTTP","port":8080},
            {"connector":"HLS"}
        ]}}"#;
        let result = import(mist).unwrap();
        assert!(result.toml.contains("[rtmp]\nbind = \"0.0.0.0:1935\""), "{}", result.toml);
        assert!(result.toml.contains("[server]\nhttp_bind = \"127.0.0.1:8080\""), "{}", result.toml);
        assert!(result.notes.iter().any(|n| n.kind == NoteKind::Translated && n.mist.contains("HLS")));
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn push_stream_with_ip_becomes_access_rule() {
        let mist = r#"{"streams":{"cam1":{"source":"push://10.0.0.5","DVR":30000}}}"#;
        let result = import(mist).unwrap();
        assert!(result.toml.contains("[[access.rules]]"), "{}", result.toml);
        assert!(result.toml.contains("publish_allow = [\"10.0.0.5\"]"), "{}", result.toml);
        assert!(result.toml.contains("[buffer]\nwindow_secs = 30"), "{}", result.toml);
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn push_stream_with_hostname_has_no_equivalent() {
        let mist = r#"{"streams":{"cam1":{"source":"push://encoder.example.com"}}}"#;
        let result = import(mist).unwrap();
        assert!(result.notes.iter().any(|n| n.kind == NoteKind::NoEquivalent && n.mist.contains("cam1")));
        assert!(!result.toml.contains("access.rules"));
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn rtsp_pull_source_translates() {
        let mist = r#"{"streams":{"cam1":{"source":"rtsp://user:pass@192.0.2.10/stream1"}}}"#;
        let result = import(mist).unwrap();
        assert!(result.toml.contains("[[rtsp.pull]]"), "{}", result.toml);
        assert!(result.toml.contains("stream = \"cam1\""), "{}", result.toml);
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn rtmp_pull_source_has_no_equivalent() {
        let mist = r#"{"streams":{"cam1":{"source":"rtmp://origin.example.com/live/cam1"}}}"#;
        let result = import(mist).unwrap();
        assert!(result.notes.iter().any(|n| n.kind == NoteKind::NoEquivalent && n.mist.contains("cam1.source")));
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn file_source_becomes_a_channel() {
        let mist = r#"{"streams":{"tv":{"source":"/media/intro.mp4"}}}"#;
        let result = import(mist).unwrap();
        assert!(result.toml.contains("[[channel]]"), "{}", result.toml);
        assert!(result.toml.contains("name = \"tv\""), "{}", result.toml);
        assert!(result.toml.contains("items = [\"/media/intro.mp4\"]"), "{}", result.toml);
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn auto_push_rtmp_target_becomes_restream() {
        let mist = r#"{"auto_push":[{"stream":"main","target":"rtmp://a.rtmp.youtube.com/live2/xxxx"}]}"#;
        let result = import(mist).unwrap();
        assert!(result.toml.contains("[[restream]]"), "{}", result.toml);
        assert!(result.toml.contains("url = \"rtmp://a.rtmp.youtube.com/live2/xxxx\""), "{}", result.toml);
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn auto_push_file_target_becomes_record() {
        let mist = r#"{"auto_push":[{"stream":"main","target":"/records/main.mp4"}]}"#;
        let result = import(mist).unwrap();
        assert!(result.toml.contains("[record]"), "{}", result.toml);
        assert!(result.toml.contains("enabled = true"), "{}", result.toml);
        assert!(result.toml.contains("\"main\""), "{}", result.toml);
        validate_generated(&result.toml).unwrap();
    }

    #[test]
    fn auto_push_with_scheduling_is_flagged_lossy() {
        let mist = r#"{"auto_push":[{"stream":"main","target":"rtmp://x/live","scheduletime":100}]}"#;
        let result = import(mist).unwrap();
        assert!(result.notes.iter().any(|n| n.kind == NoteKind::NoEquivalent && n.detail.contains("scheduletime")));
    }

    #[test]
    fn legacy_autopushes_array_is_not_guessed_at() {
        let mist = r#"{"autopushes":[["main","rtmp://x/live"]]}"#;
        let result = import(mist).unwrap();
        assert!(result.notes.iter().any(|n| n.mist == "autopushes"));
        assert!(!result.toml.contains("[[restream]]"));
    }

    #[test]
    fn triggers_are_never_auto_mapped() {
        let mist = r#"{"config":{"triggers":{"STREAM_BUFFER":[["http://example/hook",false,"",""]]}}}"#;
        let result = import(mist).unwrap();
        assert!(
            result.notes.iter().any(|n| n.kind == NoteKind::NoEquivalent && n.mist == "config.triggers.STREAM_BUFFER")
        );
        assert!(!result.toml.contains("[hooks]"));
    }

    #[test]
    fn accounts_are_never_migrated() {
        let mist = r#"{"account":{"ana":{"password":"5f4dcc3b5aa765d61d8327deb882cf99"}}}"#;
        let result = import(mist).unwrap();
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
    fn invalid_json_is_a_clean_error() {
        let err = import("not json").unwrap_err();
        assert!(err.contains("parsing"), "{err}");
    }
}
