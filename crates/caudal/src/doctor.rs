//! `caudal doctor`: reads the config (and, with `--url`, queries a running
//! server) and prints exactly what is wrong with the setup — one OK / WARN
//! / FAIL line per check, with a one-line fix for anything not OK.
//!
//! Offline by default: the only I/O is reading local files, binding local
//! sockets (and immediately releasing them) and local DNS resolution for
//! `[tls] acme_domains`. The `--online` flag additionally makes one HTTPS
//! request, to compare the system clock against a reference server.
//!
//! Exit code: 0 all OK, 1 any FAIL, 2 only WARNs (see [`Report::exit_code`]).

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn new(status: Status, name: impl Into<String>, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self { name: name.into(), status, detail: detail.into(), fix }
    }

    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Status::Ok, name, detail, None)
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(Status::Warn, name, detail, Some(fix.into()))
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(Status::Fail, name, detail, Some(fix.into()))
    }
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:<4}] {}: {}", self.status.label(), self.name, self.detail)?;
        if let Some(fix) = &self.fix {
            write!(f, "\n         fix: {fix}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub online: bool,
    pub url: Option<String>,
}

pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn exit_code(&self) -> ExitCode {
        if self.checks.iter().any(|c| c.status == Status::Fail) {
            ExitCode::FAILURE
        } else if self.checks.iter().any(|c| c.status == Status::Warn) {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        }
    }

    pub fn print(&self) {
        for c in &self.checks {
            println!("{c}");
        }
    }
}

/// Runs every check that applies to `config_path` (same resolution as
/// starting the server: an explicit path, else `caudal.toml` if present,
/// else built-in defaults) and `opts`.
pub fn run(config_path: Option<&Path>, opts: &Options) -> Report {
    let mut checks = Vec::new();

    let Some(cfg) = load_config(config_path.map(Path::to_path_buf), &mut checks) else {
        // An invalid config makes every other check meaningless: the
        // addresses, certs and tools we'd check come from it.
        return Report { checks };
    };

    check_binds(&cfg, &mut checks);
    check_webrtc_nat(&cfg, &mut checks);
    check_tls(&cfg, &mut checks);
    check_acme(&cfg, &mut checks);
    check_clock(opts.online, &mut checks);
    check_ffmpeg(&cfg, &mut checks);
    check_ulimit(&cfg, &mut checks);
    check_captions(&mut checks);
    if let Some(url) = &opts.url {
        check_running_server(url, &mut checks);
    }

    Report { checks }
}

fn load_config(explicit: Option<PathBuf>, checks: &mut Vec<Check>) -> Option<Config> {
    match crate::resolve_config(explicit) {
        Ok((cfg, Some(path))) => {
            checks.push(Check::ok("config", format!("{} parses", path.display())));
            Some(cfg)
        }
        Ok((cfg, None)) => {
            checks.push(Check::ok("config", "no caudal.toml found; using built-in defaults"));
            Some(cfg)
        }
        Err(e) => {
            checks.push(Check::fail("config", e, "fix the reported key, then rerun `caudal check <path>`"));
            None
        }
    }
}

// ---------------------------------------------------------------- binds ---

/// Probes whether `http_bind` is a caudal server answering `/healthz`,
/// with a short timeout: used to tell "this port is held by an already
/// running caudal" apart from "something else has it".
fn probe_healthz(http_bind: SocketAddr) -> bool {
    let addr = if http_bind.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), http_bind.port())
    } else {
        http_bind
    };
    let url = format!("http://{addr}/healthz");
    let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_millis(500))).build();
    let agent = ureq::Agent::new_with_config(config);
    matches!(agent.get(&url).call(), Ok(r) if r.status() == 200)
}

fn tcp_bind_check(name: &str, addr: SocketAddr, http_bind: SocketAddr, checks: &mut Vec<Check>) {
    match TcpListener::bind(addr) {
        Ok(_) => checks.push(Check::ok(name, format!("{addr} is free"))),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if probe_healthz(http_bind) {
                checks.push(Check::new(
                    Status::Warn,
                    name,
                    format!("{addr} is in use — looks like a caudal server is already running with this config (its /healthz answered)"),
                    None,
                ));
            } else {
                checks.push(Check::fail(
                    name,
                    format!("{addr} is already in use"),
                    format!("stop whatever is bound to {addr}, or change this address in the config"),
                ));
            }
        }
        Err(e) => checks.push(Check::fail(
            name,
            format!("cannot bind {addr}: {e}"),
            "check the address is valid and this process can bind it (ports below 1024 need root or CAP_NET_BIND_SERVICE)",
        )),
    }
}

fn udp_bind_check(name: &str, addr: SocketAddr, http_bind: SocketAddr, checks: &mut Vec<Check>) {
    match UdpSocket::bind(addr) {
        Ok(_) => checks.push(Check::ok(name, format!("{addr} is free"))),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if probe_healthz(http_bind) {
                checks.push(Check::new(
                    Status::Warn,
                    name,
                    format!("{addr} is in use — looks like a caudal server is already running with this config (its /healthz answered)"),
                    None,
                ));
            } else {
                checks.push(Check::fail(
                    name,
                    format!("{addr} is already in use"),
                    format!("stop whatever is bound to {addr}, or change this address in the config"),
                ));
            }
        }
        Err(e) => checks.push(Check::fail(name, format!("cannot bind {addr}: {e}"), "check the address is valid")),
    }
}

fn check_binds(cfg: &Config, checks: &mut Vec<Check>) {
    let http_bind = cfg.server.http_bind;
    tcp_bind_check("bind http", http_bind, http_bind, checks);
    if let Some(tls_bind) = cfg.tls.bind {
        tcp_bind_check("bind https", tls_bind, http_bind, checks);
    }
    tcp_bind_check("bind rtmp", cfg.rtmp.bind, http_bind, checks);
    if let Some(bind) = cfg.rtsp.bind {
        tcp_bind_check("bind rtsp", bind, http_bind, checks);
    }
    if let Some(bind) = cfg.rtsp.tls_bind {
        tcp_bind_check("bind rtsps", bind, http_bind, checks);
    }

    // UDP: WebRTC, SRT, MoQ, as named in the plan.
    udp_bind_check("bind webrtc (udp)", cfg.webrtc.udp_bind, http_bind, checks);
    udp_bind_check("bind srt (udp)", cfg.srt.bind, http_bind, checks);
    if cfg.moq.enabled {
        udp_bind_check("bind moq (udp)", cfg.moq.bind, http_bind, checks);
    }
}

// -------------------------------------------------------------- webrtc ---

fn is_private_or_unroutable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
        }
    }
}

fn check_webrtc_nat(cfg: &Config, checks: &mut Vec<Check>) {
    let ip = cfg.webrtc.udp_bind.ip();
    if cfg.webrtc.public_ips.is_empty() && is_private_or_unroutable(ip) {
        checks.push(Check::warn(
            "webrtc candidates",
            format!(
                "[webrtc] udp_bind is {ip} (private/unspecified) and no public_ips are set: ICE candidates will only be reachable from inside this network"
            ),
            "set [webrtc] public_ips = [\"<this server's public IP>\"], or put it behind a relay/reverse proxy that has one",
        ));
    } else {
        checks.push(Check::ok(
            "webrtc candidates",
            "public_ips is set, or the bind address is already routable: NAT is unlikely to block ICE",
        ));
    }
    checks.push(Check::ok(
        "webrtc reachability",
        "not testable from this host: this only checks the advertised address is plausible, not that a client across the internet can actually reach it — test with a real WHEP player from outside this network",
    ));
}

// ------------------------------------------------------------------ tls ---

/// Loads a PEM cert chain + key pair and checks: both files readable, the
/// key matches the leaf certificate (`CertifiedKey::from_der` rejects a
/// mismatched pair), and the leaf's expiry.
fn check_cert_pair(label: &str, cert_path: &Path, key_path: &Path, checks: &mut Vec<Check>) {
    let cert_bytes = match std::fs::read(cert_path) {
        Ok(b) => b,
        Err(e) => {
            checks.push(Check::fail(
                label,
                format!("cannot read {}: {e}", cert_path.display()),
                "check the path and file permissions",
            ));
            return;
        }
    };
    let key_bytes = match std::fs::read(key_path) {
        Ok(b) => b,
        Err(e) => {
            checks.push(Check::fail(
                label,
                format!("cannot read {}: {e}", key_path.display()),
                "check the path and file permissions",
            ));
            return;
        }
    };

    let certs: Result<Vec<CertificateDer<'static>>, _> = CertificateDer::pem_slice_iter(&cert_bytes).collect();
    let certs = match certs {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            checks.push(Check::fail(
                label,
                format!("{} contains no certificates", cert_path.display()),
                "check the file is a PEM certificate chain",
            ));
            return;
        }
        Err(e) => {
            checks.push(Check::fail(
                label,
                format!("cannot parse {}: {e}", cert_path.display()),
                "check the file is a PEM certificate chain",
            ));
            return;
        }
    };

    let key = match PrivateKeyDer::from_pem_slice(&key_bytes) {
        Ok(k) => k,
        Err(e) => {
            checks.push(Check::fail(
                label,
                format!("cannot parse {}: {e}", key_path.display()),
                "check the file is a PEM private key",
            ));
            return;
        }
    };

    let provider = rustls::crypto::ring::default_provider();
    match CertifiedKey::from_der(certs.clone(), key, &provider) {
        Ok(_) => checks.push(Check::ok(label, "cert and key are readable and match each other")),
        Err(e) => {
            checks.push(Check::fail(
                label,
                format!("cert/key do not match or are unusable: {e}"),
                "regenerate or re-pair the certificate and key files",
            ));
            return;
        }
    }

    check_cert_expiry_and_names(label, &certs[0], checks);
}

fn check_cert_expiry_and_names(label: &str, leaf: &CertificateDer<'_>, checks: &mut Vec<Check>) {
    let (_, cert) = match x509_parser::parse_x509_certificate(leaf.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            checks.push(Check::warn(
                format!("{label} expiry"),
                format!("could not parse the certificate to check its expiry: {e}"),
                "inspect it manually, e.g. `openssl x509 -in <cert> -noout -dates`",
            ));
            return;
        }
    };
    let now = now_unix();
    let not_after = cert.validity().not_after.timestamp();
    let days_left = (not_after - now) as f64 / 86_400.0;
    if now > not_after {
        checks.push(Check::fail(
            format!("{label} expiry"),
            format!("expired {:.1} days ago", -days_left),
            "renew the certificate",
        ));
    } else if days_left < 14.0 {
        checks.push(Check::warn(
            format!("{label} expiry"),
            format!("expires in {days_left:.1} days"),
            "renew the certificate soon",
        ));
    } else {
        checks.push(Check::ok(format!("{label} expiry"), format!("valid for {days_left:.0} more days")));
    }

    let names = subject_alt_names(&cert);
    if names.is_empty() {
        checks.push(Check::warn(
            format!("{label} names"),
            "no subjectAltName DNS/IP entries found on this certificate",
            "add the hostnames clients will connect with as SANs — most clients ignore the legacy CN field",
        ));
    } else {
        checks.push(Check::ok(format!("{label} names"), format!("covers: {}", names.join(", "))));
    }
}

fn subject_alt_names(cert: &x509_parser::certificate::X509Certificate<'_>) -> Vec<String> {
    let Ok(Some(ext)) = cert.subject_alternative_name() else { return Vec::new() };
    ext.value
        .general_names
        .iter()
        .filter_map(|gn| match gn {
            x509_parser::extensions::GeneralName::DNSName(s) => Some((*s).to_string()),
            x509_parser::extensions::GeneralName::IPAddress(ip) => Some(format_ip_bytes(ip)),
            _ => None,
        })
        .collect()
}

fn format_ip_bytes(ip: &[u8]) -> String {
    match ip.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(ip).unwrap()).to_string(),
        16 => IpAddr::from(<[u8; 16]>::try_from(ip).unwrap()).to_string(),
        _ => "?".to_string(),
    }
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn check_tls(cfg: &Config, checks: &mut Vec<Check>) {
    if let (Some(cert), Some(key)) = (&cfg.tls.cert, &cfg.tls.key) {
        check_cert_pair("tls", cert, key, checks);
    }
    if let (Some(cert), Some(key)) = (&cfg.rtsp.tls_cert, &cfg.rtsp.tls_key) {
        check_cert_pair("rtsp tls", cert, key, checks);
    }
}

/// Resolves `domain` the same way a browser or ACME CA would: whatever the
/// system resolver returns for port 443.
fn resolve_domain(domain: &str) -> io::Result<Vec<IpAddr>> {
    Ok((domain, 443).to_socket_addrs()?.map(|a| a.ip()).collect())
}

fn check_acme(cfg: &Config, checks: &mut Vec<Check>) {
    if cfg.tls.acme_domains.is_empty() {
        return;
    }
    check_acme_domains(resolve_domain, &cfg.tls.acme_domains, &cfg.webrtc.public_ips, checks);
}

/// Split out from [`check_acme`] so tests can inject a fake resolver
/// instead of making real DNS lookups.
fn check_acme_domains(
    resolve: impl Fn(&str) -> io::Result<Vec<IpAddr>>,
    domains: &[String],
    known_public_ips: &[IpAddr],
    checks: &mut Vec<Check>,
) {
    for domain in domains {
        let name = format!("acme dns: {domain}");
        match resolve(domain) {
            Ok(ips) if ips.is_empty() => {
                checks.push(Check::fail(
                    name,
                    "resolved to no addresses",
                    "point this domain's DNS at this server before requesting a certificate",
                ));
            }
            Ok(ips) => {
                let ip_list = ips.iter().map(IpAddr::to_string).collect::<Vec<_>>().join(", ");
                if known_public_ips.is_empty() {
                    checks.push(Check::ok(name, format!("resolves to {ip_list} (this host's public IP is not configured in [webrtc] public_ips, so it cannot be cross-checked)")));
                } else if ips.iter().any(|ip| known_public_ips.contains(ip)) {
                    checks
                        .push(Check::ok(name, format!("resolves to {ip_list}, matching this host's known public IP")));
                } else {
                    checks.push(Check::warn(
                        name,
                        format!("resolves to {ip_list}, none of which match this host's known public IP(s) ({}): the ACME HTTP-01 challenge will fail", known_public_ips.iter().map(IpAddr::to_string).collect::<Vec<_>>().join(", ")),
                        "point this domain's DNS at this server, or update [webrtc] public_ips if it changed",
                    ));
                }
            }
            Err(e) => {
                checks.push(Check::fail(
                    name,
                    format!("could not resolve: {e}"),
                    "check the domain is spelled correctly and its DNS is set up",
                ));
            }
        }
    }
}

// ---------------------------------------------------------------- clock ---

fn check_clock(online: bool, checks: &mut Vec<Check>) {
    if !online {
        checks.push(Check::ok(
            "clock",
            "not checked — pass --online to compare it against a reference HTTPS server's Date header",
        ));
        return;
    }
    // A host chosen only for a stable, always-on HTTPS endpoint with a
    // `Date` header; no payload is read.
    const REFERENCE: &str = "https://cloudflare.com";
    match ureq::head(REFERENCE).call() {
        Ok(resp) => match resp.headers().get("date").and_then(|v| v.to_str().ok()) {
            Some(date_header) => match httpdate_to_unix(date_header) {
                Some(remote) => {
                    let drift = now_unix() - remote;
                    if drift.abs() > 60 {
                        checks.push(Check::fail(
                            "clock",
                            format!("system clock is off by {drift}s from {REFERENCE}"),
                            "sync the clock (e.g. `sudo sntp -sS time.apple.com` or enable NTP) — TLS certificate validation fails outside a small drift",
                        ));
                    } else if drift.abs() > 5 {
                        checks.push(Check::warn(
                            "clock",
                            format!("system clock is off by {drift}s from {REFERENCE}"),
                            "sync the clock with NTP",
                        ));
                    } else {
                        checks.push(Check::ok("clock", format!("within {drift}s of {REFERENCE}")));
                    }
                }
                None => checks.push(Check::warn(
                    "clock",
                    format!("could not parse the Date header from {REFERENCE}"),
                    "check the clock manually",
                )),
            },
            None => checks.push(Check::warn(
                "clock",
                format!("{REFERENCE} sent no Date header"),
                "check the clock manually",
            )),
        },
        Err(e) => checks.push(Check::warn(
            "clock",
            format!("could not reach {REFERENCE}: {e}"),
            "check network access, or check the clock manually",
        )),
    }
}

/// Parses an RFC 7231 `Date` header (`Sun, 06 Nov 1994 08:49:37 GMT`) into
/// a Unix timestamp, without pulling in a date-parsing crate for one field.
fn httpdate_to_unix(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let mut time = parts[4].split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let min: i64 = time.next()?.parse().ok()?;
    let sec: i64 = time.next()?.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Howard Hinnant's `days_from_civil`, proleptic Gregorian calendar, days
/// since the Unix epoch.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ------------------------------------------------------------------ ffmpeg ---

fn check_ffmpeg(cfg: &Config, checks: &mut Vec<Check>) {
    if cfg.transcode.ladder.is_empty() {
        return;
    }
    if cfg.transcode.engine != "ffmpeg" {
        checks.push(Check::ok(
            "ffmpeg",
            format!("[transcode] engine is \"{}\", ffmpeg is not needed", cfg.transcode.engine),
        ));
        return;
    }
    match std::process::Command::new(&cfg.transcode.ffmpeg).arg("-version").output() {
        Ok(out) if out.status.success() => {
            let first_line = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string();
            checks.push(Check::ok("ffmpeg", first_line));
        }
        Ok(out) => checks.push(Check::fail(
            "ffmpeg",
            format!("{} -version exited with {}", cfg.transcode.ffmpeg.display(), out.status),
            "check the [transcode] ffmpeg path",
        )),
        Err(e) => checks.push(Check::fail(
            "ffmpeg",
            format!("{} not runnable: {e}", cfg.transcode.ffmpeg.display()),
            "install ffmpeg and make sure it is on PATH, or set [transcode] engine = \"rusty_h264\"",
        )),
    }
}

// ------------------------------------------------------------------ ulimit ---

/// A conservative floor: one fd per HTTP/RTMP/SRT connection plus files
/// for HLS segments and recordings. A busy server needs far more; this
/// only catches the default (usually 256 or 1024) that guarantees trouble.
const RECOMMENDED_NOFILE: u64 = 4096;

#[cfg(unix)]
fn check_ulimit(_cfg: &Config, checks: &mut Vec<Check>) {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let soft = limit.current.unwrap_or(u64::MAX);
    if soft < RECOMMENDED_NOFILE {
        checks.push(Check::warn(
            "ulimit -n",
            format!(
                "open-file limit is {soft}, below the recommended {RECOMMENDED_NOFILE} for a media server under load"
            ),
            format!(
                "raise it: `ulimit -n {RECOMMENDED_NOFILE}` for this shell, or `LimitNOFILE={RECOMMENDED_NOFILE}` in the systemd unit"
            ),
        ));
    } else {
        checks.push(Check::ok("ulimit -n", format!("open-file limit is {soft}")));
    }
}

#[cfg(not(unix))]
fn check_ulimit(_cfg: &Config, checks: &mut Vec<Check>) {
    checks.push(Check::ok("ulimit -n", "not checked on this platform"));
}

// ---------------------------------------------------------------- captions ---

/// Whether this build has live captions. A config that sets `[captions]`
/// in a build without them already failed the config check.
fn check_captions(checks: &mut Vec<Check>) {
    if cfg!(feature = "captions") {
        checks.push(Check::ok("captions", "built in (feature `captions`)"));
    } else {
        checks.push(Check::ok("captions", crate::captions::NOT_BUILT));
    }
}

// ------------------------------------------------------------ running server ---

#[derive(Debug, serde::Deserialize)]
struct DoctorTrack {
    kind: String,
    codec: String,
}

#[derive(Debug, serde::Deserialize)]
struct DoctorStream {
    name: String,
    tracks: Vec<DoctorTrack>,
}

fn check_running_server(url: &str, checks: &mut Vec<Check>) {
    let base = url.trim_end_matches('/');
    let endpoint = format!("{base}/api/v1/streams");
    let body = match ureq::get(&endpoint).call() {
        Ok(mut r) if r.status() == 200 => r.body_mut().read_to_string().unwrap_or_default(),
        Ok(r) => {
            checks.push(Check::fail(
                "running server",
                format!("{endpoint} returned {}", r.status()),
                "check --url points at a caudal server",
            ));
            return;
        }
        Err(e) => {
            checks.push(Check::fail(
                "running server",
                format!("could not reach {endpoint}: {e}"),
                "check --url and that the server is running",
            ));
            return;
        }
    };

    let streams: Vec<DoctorStream> = match serde_json::from_str(&body) {
        Ok(s) => s,
        Err(e) => {
            checks.push(Check::fail(
                "running server",
                format!("could not parse {endpoint}: {e}"),
                "check --url points at a caudal server, not a proxy or a different API",
            ));
            return;
        }
    };

    if streams.is_empty() {
        checks.push(Check::ok("running server", format!("{endpoint} is reachable; no live streams to check")));
        return;
    }

    let mut any_issue = false;
    for s in &streams {
        for t in &s.tracks {
            match (t.kind.as_str(), t.codec.as_str()) {
                ("video", "h265") => {
                    any_issue = true;
                    checks.push(Check::warn(
                        format!("codec: {} (video)", s.name),
                        "HEVC has no consistent browser support over WHEP/WebRTC (it plays fine over LL-HLS and MoQ)",
                        "transcode to H.264 or AV1 for WebRTC/WHEP output, or serve this stream over LL-HLS/MoQ only",
                    ));
                }
                ("audio", "aac") => {
                    any_issue = true;
                    checks.push(Check::warn(
                        format!("codec: {} (audio)", s.name),
                        "AAC has no WebRTC/WHEP browser support (WHEP needs Opus; AAC plays fine over LL-HLS)",
                        "transcode audio to Opus for WebRTC/WHEP output",
                    ));
                }
                _ => {}
            }
        }
    }
    if !any_issue {
        checks.push(Check::ok(
            "running server",
            format!("{endpoint}: {} stream(s), no codec incompatibilities found", streams.len()),
        ));
    }
    checks.push(Check::ok(
        "running server: b-frames",
        "not checked — /api/v1/streams does not expose whether a track uses B-frames (WebRTC/WHEP needs a B-frame-free encode); inspect with `ffprobe -show_frames` if WHEP playback stutters",
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no check named {name} in {checks:?}"))
    }

    impl std::fmt::Debug for Check {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}({:?})", self.name, self.status)
        }
    }

    // ---- config ----

    #[test]
    fn config_fail_short_circuits_the_rest_of_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("caudal.toml");
        std::fs::write(&bad, "http_bnid = 1\n").unwrap();
        let report = run(Some(&bad), &Options::default());
        assert_eq!(report.checks.len(), 1, "{:?}", report.checks);
        assert_eq!(report.checks[0].status, Status::Fail);
        assert_eq!(report.exit_code(), ExitCode::FAILURE);
    }

    #[test]
    fn captions_in_a_build_without_them_is_a_config_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caudal.toml");
        std::fs::write(&path, "[[captions.stream]]\nstreams = [\"a\"]\nlanguage = \"es\"\n").unwrap();
        let report = run(Some(&path), &Options::default());
        let config = find(&report.checks, "config");
        if cfg!(feature = "captions") {
            assert_eq!(config.status, Status::Ok, "{}", config.detail);
            assert!(find(&report.checks, "captions").detail.starts_with("built in"));
        } else {
            assert_eq!(config.status, Status::Fail);
            assert!(config.detail.contains(crate::captions::NOT_BUILT), "{}", config.detail);
        }
    }

    #[test]
    fn missing_config_file_falls_back_to_defaults_and_keeps_checking() {
        let report = run(Some(Path::new("/nonexistent/caudal.toml")), &Options::default());
        assert_eq!(find(&report.checks, "config").status, Status::Fail);
    }

    #[test]
    fn no_config_path_uses_defaults() {
        // `cargo test`'s cwd is this crate's root, which has no
        // `caudal.toml`: same "fall back to defaults" path `run(None, ..)`
        // takes when a real user has none either. Not mutating cwd here
        // keeps this test safe to run alongside every other test in the
        // binary.
        let report = run(None, &Options::default());
        assert_eq!(find(&report.checks, "config").status, Status::Ok);
    }

    // ---- binds ----

    #[test]
    fn free_port_is_ok() {
        let mut checks = Vec::new();
        // Bind once to claim a specific ephemeral port, then release it:
        // the address is very likely still free for `tcp_bind_check` to
        // bind a moment later.
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        tcp_bind_check("bind http", addr, addr, &mut checks);
        assert_eq!(checks[0].status, Status::Ok, "{:?}", checks);
    }

    #[test]
    fn busy_port_without_a_running_caudal_fails() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let unrelated_http = "127.0.0.1:0".parse().unwrap();
        let mut checks = Vec::new();
        tcp_bind_check("bind rtmp", addr, unrelated_http, &mut checks);
        assert_eq!(checks[0].status, Status::Fail, "{:?}", checks);
        drop(listener);
    }

    #[test]
    fn udp_free_port_is_ok() {
        let mut checks = Vec::new();
        let addr = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let http = "127.0.0.1:0".parse().unwrap();
        udp_bind_check("bind webrtc (udp)", addr, http, &mut checks);
        assert_eq!(checks[0].status, Status::Ok);
    }

    // ---- webrtc NAT ----

    #[test]
    fn private_udp_bind_without_public_ips_warns() {
        let cfg: Config = toml::from_str("[webrtc]\nudp_bind = \"192.168.1.5:8189\"\n").unwrap();
        let mut checks = Vec::new();
        check_webrtc_nat(&cfg, &mut checks);
        assert_eq!(find(&checks, "webrtc candidates").status, Status::Warn);
    }

    #[test]
    fn public_ips_set_silences_the_nat_warning() {
        let cfg: Config =
            toml::from_str("[webrtc]\nudp_bind = \"192.168.1.5:8189\"\npublic_ips = [\"203.0.113.9\"]\n").unwrap();
        let mut checks = Vec::new();
        check_webrtc_nat(&cfg, &mut checks);
        assert_eq!(find(&checks, "webrtc candidates").status, Status::Ok);
    }

    #[test]
    fn unspecified_bind_without_public_ips_still_warns() {
        let cfg: Config = toml::from_str("[webrtc]\nudp_bind = \"0.0.0.0:8189\"\n").unwrap();
        let mut checks = Vec::new();
        check_webrtc_nat(&cfg, &mut checks);
        assert_eq!(find(&checks, "webrtc candidates").status, Status::Warn);
    }

    // ---- TLS cert/key ----

    /// Writes a fresh self-signed cert/key pair to `<dir>/<san>.{cert,key}.pem`
    /// (the SAN is unique per call in every test here, so this also keeps
    /// each pair's files from colliding with another pair in the same dir).
    fn write_cert_pair(dir: &Path, san: &str) -> (PathBuf, PathBuf) {
        let mut params = rcgen::CertificateParams::new(vec![san.to_string()]).unwrap();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(30);
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_path = dir.join(format!("{san}.cert.pem"));
        let key_path = dir.join(format!("{san}.key.pem"));
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn matching_cert_and_key_are_ok_with_names_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_cert_pair(dir.path(), "stream.example.com");
        let mut checks = Vec::new();
        check_cert_pair("tls", &cert, &key, &mut checks);
        assert_eq!(find(&checks, "tls").status, Status::Ok, "{:?}", checks);
        assert_eq!(find(&checks, "tls expiry").status, Status::Ok, "{:?}", checks);
        let names = find(&checks, "tls names");
        assert_eq!(names.status, Status::Ok);
        assert!(names.detail.contains("stream.example.com"), "{}", names.detail);
    }

    #[test]
    fn mismatched_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _key) = write_cert_pair(dir.path(), "a.example.com");
        let (_cert2, other_key) = write_cert_pair(dir.path(), "b.example.com");
        let mut checks = Vec::new();
        check_cert_pair("tls", &cert, &other_key, &mut checks);
        assert_eq!(find(&checks, "tls").status, Status::Fail, "{:?}", checks);
    }

    #[test]
    fn missing_cert_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut checks = Vec::new();
        check_cert_pair("tls", &dir.path().join("nope.pem"), &dir.path().join("nope-key.pem"), &mut checks);
        assert_eq!(find(&checks, "tls").status, Status::Fail);
    }

    #[test]
    fn expired_cert_fails_expiry_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["expired.example.com".to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(30);
        params.not_after = now - time::Duration::days(1);
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

        let mut checks = Vec::new();
        check_cert_pair("tls", &cert_path, &key_path, &mut checks);
        assert_eq!(find(&checks, "tls expiry").status, Status::Fail, "{:?}", checks);
    }

    // ---- ACME DNS ----

    #[test]
    fn acme_domain_resolving_to_known_public_ip_is_ok() {
        let mut checks = Vec::new();
        let resolve = |_: &str| Ok(vec!["203.0.113.9".parse().unwrap()]);
        check_acme_domains(resolve, &["stream.example.com".into()], &["203.0.113.9".parse().unwrap()], &mut checks);
        assert_eq!(checks[0].status, Status::Ok, "{:?}", checks);
    }

    #[test]
    fn acme_domain_resolving_elsewhere_warns() {
        let mut checks = Vec::new();
        let resolve = |_: &str| Ok(vec!["198.51.100.1".parse().unwrap()]);
        check_acme_domains(resolve, &["stream.example.com".into()], &["203.0.113.9".parse().unwrap()], &mut checks);
        assert_eq!(checks[0].status, Status::Warn, "{:?}", checks);
    }

    #[test]
    fn acme_domain_with_unknown_public_ip_is_ok_but_says_so() {
        let mut checks = Vec::new();
        let resolve = |_: &str| Ok(vec!["198.51.100.1".parse().unwrap()]);
        check_acme_domains(resolve, &["stream.example.com".into()], &[], &mut checks);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].detail.contains("not configured"));
    }

    #[test]
    fn acme_domain_that_does_not_resolve_fails() {
        let mut checks = Vec::new();
        let resolve = |_: &str| Err(io::Error::new(io::ErrorKind::NotFound, "nxdomain"));
        check_acme_domains(resolve, &["nope.example.com".into()], &[], &mut checks);
        assert_eq!(checks[0].status, Status::Fail);
    }

    // ---- clock ----

    #[test]
    fn offline_clock_check_is_ok_and_makes_no_claim() {
        let mut checks = Vec::new();
        check_clock(false, &mut checks);
        assert_eq!(find(&checks, "clock").status, Status::Ok);
    }

    #[test]
    fn httpdate_parses_a_known_instant() {
        // 2024-01-01T00:00:00Z
        assert_eq!(httpdate_to_unix("Mon, 01 Jan 2024 00:00:00 GMT"), Some(1_704_067_200));
        assert_eq!(httpdate_to_unix("garbage"), None);
    }

    // ---- ffmpeg ----

    #[test]
    fn ffmpeg_check_skipped_without_ladders() {
        let cfg = Config::default();
        let mut checks = Vec::new();
        check_ffmpeg(&cfg, &mut checks);
        assert!(checks.is_empty());
    }

    #[test]
    fn ffmpeg_check_ok_for_non_ffmpeg_engine() {
        let cfg: Config = toml::from_str(
            "[transcode]\nengine = \"rusty_h264\"\n[[transcode.ladder]]\nstreams = [\"a\"]\n[[transcode.ladder.rendition]]\nlabel = \"lo\"\nheight = 360\nvideo_kbps = 500\n",
        )
        .unwrap();
        let mut checks = Vec::new();
        check_ffmpeg(&cfg, &mut checks);
        assert_eq!(find(&checks, "ffmpeg").status, Status::Ok);
    }

    #[test]
    fn ffmpeg_check_fails_when_binary_missing() {
        let cfg: Config = toml::from_str(
            "[transcode]\nffmpeg = \"/nonexistent/ffmpeg-binary\"\n[[transcode.ladder]]\nstreams = [\"a\"]\n[[transcode.ladder.rendition]]\nlabel = \"lo\"\nheight = 360\nvideo_kbps = 500\n",
        )
        .unwrap();
        let mut checks = Vec::new();
        check_ffmpeg(&cfg, &mut checks);
        assert_eq!(find(&checks, "ffmpeg").status, Status::Fail);
    }

    // ---- ulimit ----

    #[test]
    fn ulimit_check_produces_one_result() {
        let cfg = Config::default();
        let mut checks = Vec::new();
        check_ulimit(&cfg, &mut checks);
        assert_eq!(checks.len(), 1);
    }

    // ---- running server ----

    /// Answers exactly one HTTP request with `body` as a JSON response,
    /// on a background thread, over a plain `TcpListener` — no HTTP
    /// server crate needed for a fixture this small.
    fn serve_once_json(body: &'static str) -> (SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (addr, handle)
    }

    #[test]
    fn running_server_flags_hevc_and_aac_for_webrtc() {
        let body =
            r#"[{"name":"cam1","tracks":[{"kind":"video","codec":"h265"},{"kind":"audio","codec":"aac"}],"stats":{}}]"#;
        let (addr, handle) = serve_once_json(body);
        let mut checks = Vec::new();
        check_running_server(&format!("http://{addr}"), &mut checks);
        handle.join().unwrap();
        assert_eq!(find(&checks, "codec: cam1 (video)").status, Status::Warn, "{:?}", checks);
        assert_eq!(find(&checks, "codec: cam1 (audio)").status, Status::Warn, "{:?}", checks);
    }

    #[test]
    fn running_server_with_no_streams_is_ok() {
        let (addr, handle) = serve_once_json("[]");
        let mut checks = Vec::new();
        check_running_server(&format!("http://{addr}"), &mut checks);
        handle.join().unwrap();
        assert_eq!(find(&checks, "running server").status, Status::Ok, "{:?}", checks);
    }

    #[test]
    fn running_server_unreachable_fails() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap(); // nothing listening after drop
        let mut checks = Vec::new();
        check_running_server(&format!("http://{addr}"), &mut checks);
        assert_eq!(find(&checks, "running server").status, Status::Fail);
    }

    // ---- report exit codes ----

    #[test]
    fn exit_code_reflects_worst_status() {
        let ok = Report { checks: vec![Check::ok("a", "fine")] };
        assert_eq!(ok.exit_code(), ExitCode::SUCCESS);
        let warn = Report { checks: vec![Check::ok("a", "fine"), Check::warn("b", "meh", "fix it")] };
        assert_eq!(format!("{:?}", warn.exit_code()), format!("{:?}", ExitCode::from(2)));
        let fail = Report { checks: vec![Check::warn("a", "meh", "fix"), Check::fail("b", "broken", "fix it")] };
        assert_eq!(fail.exit_code(), ExitCode::FAILURE);
    }
}
