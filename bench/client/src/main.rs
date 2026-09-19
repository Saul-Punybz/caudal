//! Load and latency client for the Caudal vs MediaMTX benchmark.
//!
//! Load modes (N concurrent viewers, one tokio task each; prints one JSON
//! line with what was received during the measurement window):
//!   hls  <multivariant-or-media-url>   LL-HLS like a low-latency player:
//!        blocking playlist reloads (_HLS_msn/_HLS_part), then every new part,
//!        one HTTP client (connection pool) per viewer, every media playlist
//!        of the first variant followed (video + separate audio renditions).
//!   rtsp <rtsp-url>                    minimal RTSP client, TCP interleaved,
//!        all tracks, RTP payload bytes and sequence gaps counted.
//!   whep <whep-url>                    str0m in RTP mode, recvonly video +
//!        audio, one UDP socket per viewer.
//!
//! Latency modes (one viewer, decodes the burned-in wall-clock stamp):
//!   latency-hls  <url>    follows the video playlist from the live edge and
//!        pipes init + parts into `ffmpeg -f mp4 -i pipe:0`.
//!   latency-rtsp <url>    `ffmpeg -rtsp_transport tcp -i <url>`.
//!   latency-flv  <url>    `ffmpeg -f flv -i <url>`: the no-server baseline
//!        (encoder -> FLV over TCP -> decoder), the floor of the other two.
//! The stamp is a row of 32 blocks (60x40 px) at the top of the frame: bit k
//! (MSB first) of (wall-clock ms mod 2^32) at the moment the frame left the
//! source's filter graph. The client decodes it from ffmpeg's gray output and
//! subtracts it from its own clock (same machine, same clock).

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use url::Url;

/// First distinct error messages, reported in the JSON summary.
static FIRST_ERRORS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Default)]
struct Viewer {
    bytes: AtomicU64,
    /// LL-HLS only: media time fetched, microseconds.
    media_us: AtomicU64,
    requests: AtomicU64,
    errors: AtomicU64,
    timeouts: AtomicU64,
    /// Blocking reloads that returned nothing new.
    empty_reloads: AtomicU64,
    /// Sessions (re)started; >1 means the viewer had to reconnect.
    sessions: AtomicU64,
    /// RTSP: RTP packets received and sequence-number gaps (lost packets).
    packets: AtomicU64,
    lost: AtomicU64,
}

#[derive(Clone, Copy, Default)]
struct Snap {
    bytes: u64,
    media_us: u64,
    requests: u64,
    errors: u64,
    timeouts: u64,
    empty_reloads: u64,
    sessions: u64,
    packets: u64,
    lost: u64,
}

impl Viewer {
    fn snap(&self) -> Snap {
        Snap {
            bytes: self.bytes.load(Relaxed),
            media_us: self.media_us.load(Relaxed),
            requests: self.requests.load(Relaxed),
            errors: self.errors.load(Relaxed),
            timeouts: self.timeouts.load(Relaxed),
            empty_reloads: self.empty_reloads.load(Relaxed),
            sessions: self.sessions.load(Relaxed),
            packets: self.packets.load(Relaxed),
            lost: self.lost.load(Relaxed),
        }
    }
    fn fail(&self, e: &(dyn std::error::Error + 'static)) {
        let timeout = is_timeout(e);
        let mut first = FIRST_ERRORS.lock().unwrap();
        let msg = e.to_string().replace(['"', '\\', '\n'], " ");
        if first.len() < 3 && !first.contains(&msg) {
            first.push(msg);
        }
        if timeout {
            self.timeouts.fetch_add(1, Relaxed);
        } else {
            self.errors.fetch_add(1, Relaxed);
        }
    }
}

fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cur {
        if let Some(r) = c.downcast_ref::<reqwest::Error>()
            && r.is_timeout()
        {
            return true;
        }
        if c.to_string().to_lowercase().contains("timed out") {
            return true;
        }
        cur = c.source();
    }
    false
}

struct Args {
    mode: String,
    url: String,
    viewers: usize,
    warmup: f64,
    duration: f64,
    ramp_ms: u64,
    expect_mbps: f64,
    samples: usize,
}

fn parse_args() -> Args {
    let mut a = std::env::args().skip(1);
    let mode = a.next().unwrap_or_else(|| usage());
    let url = a.next().unwrap_or_else(|| usage());
    let mut args = Args {
        mode,
        url,
        viewers: 1,
        warmup: 10.0,
        duration: 30.0,
        ramp_ms: 5000,
        expect_mbps: 0.0,
        samples: 20,
    };
    while let Some(k) = a.next() {
        let v = a.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--viewers" => args.viewers = v.parse().unwrap(),
            "--warmup" => args.warmup = v.parse().unwrap(),
            "--duration" => args.duration = v.parse().unwrap(),
            "--ramp-ms" => args.ramp_ms = v.parse().unwrap(),
            "--expect-mbps" => args.expect_mbps = v.parse().unwrap(),
            "--samples" => args.samples = v.parse().unwrap(),
            _ => usage(),
        }
    }
    args
}

fn usage() -> ! {
    eprintln!(
        "usage: caudal-bench-client <hls|rtsp|whep|latency-hls|latency-rtsp|latency-flv> <url> \
         [--viewers N] [--warmup S] [--duration S] [--ramp-ms MS] [--expect-mbps M] [--samples N]"
    );
    std::process::exit(2)
}

fn main() {
    let args = parse_args();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    match args.mode.as_str() {
        "hls" | "rtsp" | "whep" => rt.block_on(load(args)),
        "latency-hls" => rt.block_on(latency_hls(args)),
        "latency-rtsp" => latency_rtsp(args),
        "latency-flv" => latency_flv(args),
        _ => usage(),
    }
    // Viewer tasks never end on their own; leave without tearing them down.
    std::process::exit(0);
}

// ---------------------------------------------------------------- load

async fn load(args: Args) {
    let viewers: Vec<Arc<Viewer>> = (0..args.viewers).map(|_| Arc::new(Viewer::default())).collect();
    let started = Instant::now();
    let gap = Duration::from_micros(args.ramp_ms * 1000 / args.viewers.max(1) as u64);
    for v in &viewers {
        let v = v.clone();
        let url = args.url.clone();
        let mode = args.mode.clone();
        tokio::spawn(async move {
            match mode.as_str() {
                "hls" => hls_viewer(url, v).await,
                "rtsp" => rtsp_viewer(url, v).await,
                _ => whep_viewer(url, v).await,
            }
        });
        if !gap.is_zero() {
            tokio::time::sleep(gap).await;
        }
    }
    let warm_end = started + Duration::from_secs_f64(args.warmup);
    tokio::time::sleep_until(warm_end.into()).await;
    let a: Vec<Snap> = viewers.iter().map(|v| v.snap()).collect();
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs_f64(args.duration)).await;
    let b: Vec<Snap> = viewers.iter().map(|v| v.snap()).collect();
    let secs = t0.elapsed().as_secs_f64();

    let mut tot = Snap::default();
    let mut mbps: Vec<f64> = Vec::new();
    let mut kept_up = 0usize;
    let mut reconnected = 0usize;
    let mut media_ratio: Vec<f64> = Vec::new();
    for (x, y) in a.iter().zip(&b) {
        let d = Snap {
            bytes: y.bytes - x.bytes,
            media_us: y.media_us - x.media_us,
            requests: y.requests - x.requests,
            errors: y.errors - x.errors,
            timeouts: y.timeouts - x.timeouts,
            empty_reloads: y.empty_reloads - x.empty_reloads,
            sessions: y.sessions - x.sessions,
            packets: y.packets - x.packets,
            lost: y.lost - x.lost,
        };
        tot.bytes += d.bytes;
        tot.requests += d.requests;
        tot.errors += d.errors;
        tot.timeouts += d.timeouts;
        tot.empty_reloads += d.empty_reloads;
        tot.packets += d.packets;
        tot.lost += d.lost;
        if y.sessions > 1 {
            reconnected += 1;
        }
        let m = d.bytes as f64 * 8.0 / secs / 1e6;
        mbps.push(m);
        if args.mode == "hls" {
            // A player keeps up if it fetched (almost) as much media as wall time.
            let r = d.media_us as f64 / 1e6 / secs;
            media_ratio.push(r);
            if r >= 0.9 {
                kept_up += 1;
            }
        } else if args.expect_mbps > 0.0 && m >= 0.9 * args.expect_mbps {
            kept_up += 1;
        }
    }
    mbps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    media_ratio.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let kept = if args.mode == "hls" || args.expect_mbps > 0.0 { kept_up.to_string() } else { "null".into() };
    println!(
        "{{\"mode\":\"{}\",\"viewers\":{},\"window_s\":{:.2},\"egress_mbps\":{:.2},\"bytes\":{},\
         \"viewer_mbps_min\":{:.3},\"viewer_mbps_p5\":{:.3},\"viewer_mbps_median\":{:.3},\
         \"media_ratio_p5\":{},\"media_ratio_median\":{},\"kept_up\":{},\"reconnected\":{},\
         \"requests\":{},\"errors\":{},\"timeouts\":{},\"empty_reloads\":{},\"rtp_packets\":{},\"rtp_lost\":{},\"first_errors\":[{}]}}",
        args.mode,
        args.viewers,
        secs,
        tot.bytes as f64 * 8.0 / secs / 1e6,
        tot.bytes,
        mbps.first().copied().unwrap_or(0.0),
        pct(&mbps, 5.0),
        pct(&mbps, 50.0),
        opt(pct_opt(&media_ratio, 5.0)),
        opt(pct_opt(&media_ratio, 50.0)),
        kept,
        reconnected,
        tot.requests,
        tot.errors,
        tot.timeouts,
        tot.empty_reloads,
        tot.packets,
        tot.lost,
        FIRST_ERRORS.lock().unwrap().iter().map(|e| format!("\"{e}\"")).collect::<Vec<_>>().join(",")
    );
}

fn pct(v: &[f64], p: f64) -> f64 {
    pct_opt(v, p).unwrap_or(0.0)
}

fn pct_opt(v: &[f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let i = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    Some(v[i.min(v.len() - 1)])
}

fn opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_else(|| "null".into())
}

// ---------------------------------------------------------------- LL-HLS

#[derive(Debug, Clone)]
struct Part {
    msn: u64,
    idx: u32,
    uri: String,
    dur: f64,
    range: Option<(u64, u64)>, // (len, offset)
}

#[derive(Debug, Default)]
struct Playlist {
    map: Option<String>,
    parts: Vec<Part>,
    /// Sequence number of the segment still being written.
    open_msn: u64,
    part_hold_back: Option<f64>,
}

/// Splits an attribute list (`A=1,B="x,y"`) into pairs.
fn attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut key = String::new();
    let mut val = String::new();
    let mut in_key = true;
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            '=' if in_key => in_key = false,
            ',' if !quoted => {
                out.push((key.trim().to_string(), val.clone()));
                key.clear();
                val.clear();
                in_key = true;
            }
            _ if in_key => key.push(c),
            _ => val.push(c),
        }
    }
    if !key.is_empty() {
        out.push((key.trim().to_string(), val));
    }
    out
}

fn attr(s: &str, k: &str) -> Option<String> {
    attrs(s).into_iter().find(|(a, _)| a == k).map(|(_, v)| v)
}

fn parse_media(body: &str) -> Playlist {
    let mut pl = Playlist::default();
    let mut msn = 0u64;
    let mut idx = 0u32;
    let mut after_inf = false;
    for line in body.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            msn = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            pl.map = attr(v, "URI");
        } else if let Some(v) = line.strip_prefix("#EXT-X-SERVER-CONTROL:") {
            pl.part_hold_back = attr(v, "PART-HOLD-BACK").and_then(|x| x.parse().ok());
        } else if let Some(v) = line.strip_prefix("#EXT-X-PART:") {
            let a = attrs(v);
            let get = |k: &str| a.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
            if let Some(uri) = get("URI") {
                let range = get("BYTERANGE").and_then(|r| {
                    let (l, o) = r.split_once('@')?;
                    Some((l.parse().ok()?, o.parse().ok()?))
                });
                pl.parts.push(Part {
                    msn,
                    idx,
                    uri,
                    dur: get("DURATION").and_then(|d| d.parse().ok()).unwrap_or(0.0),
                    range,
                });
                idx += 1;
            }
        } else if line.starts_with("#EXTINF:") {
            after_inf = true;
        } else if !line.is_empty() && !line.starts_with('#') && after_inf {
            after_inf = false;
            msn += 1;
            idx = 0;
        }
    }
    pl.open_msn = msn;
    pl
}

/// First variant's media playlist plus every EXT-X-MEDIA rendition it uses.
fn media_urls(base: &Url, body: &str, video_only: bool) -> Res<Vec<Url>> {
    if !body.contains("#EXT-X-STREAM-INF") {
        return Ok(vec![base.clone()]);
    }
    let mut out = Vec::new();
    let mut next_is_variant = false;
    let mut audio_group: Option<String> = None;
    for line in body.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if out.is_empty() {
                next_is_variant = true;
                audio_group = attr(v, "AUDIO");
            }
        } else if next_is_variant && !line.is_empty() && !line.starts_with('#') {
            out.push(base.join(line)?);
            next_is_variant = false;
        }
    }
    if !video_only {
        for line in body.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("#EXT-X-MEDIA:") {
                let group_ok = audio_group.as_deref().is_none_or(|g| attr(v, "GROUP-ID").as_deref() == Some(g));
                if group_ok && let Some(u) = attr(v, "URI") {
                    let u = base.join(&u)?;
                    if !out.contains(&u) {
                        out.push(u);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Err("multivariant playlist without variants".into());
    }
    Ok(out)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(4)
        .build()
        .unwrap()
}

async fn get(c: &reqwest::Client, v: &Viewer, u: Url, range: Option<(u64, u64)>) -> Res<Vec<u8>> {
    v.requests.fetch_add(1, Relaxed);
    let mut req = c.get(u);
    if let Some((len, off)) = range {
        req = req.header("Range", format!("bytes={}-{}", off, off + len - 1));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }
    let b = resp.bytes().await?.to_vec();
    v.bytes.fetch_add(b.len() as u64, Relaxed);
    Ok(b)
}

async fn hls_viewer(url: String, v: Arc<Viewer>) {
    let c = client();
    loop {
        v.sessions.fetch_add(1, Relaxed);
        let r: Res<()> = async {
            let base = Url::parse(&url)?;
            let body = get(&c, &v, base.clone(), None).await?;
            let urls = media_urls(&base, &String::from_utf8_lossy(&body), false)?;
            // Media time is counted on the first (video) playlist only.
            let followers = urls.into_iter().enumerate().map(|(i, u)| follow(&c, &v, u, None, i == 0));
            futures::future::try_join_all(followers).await?;
            Ok(())
        }
        .await;
        if let Err(e) = r {
            v.fail(e.as_ref());
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Follows one media playlist forever from its live edge. `sink` gets init
/// and every part (latency mode).
async fn follow(
    c: &reqwest::Client,
    v: &Viewer,
    u: Url,
    mut sink: Option<&mut tokio::process::ChildStdin>,
    count_media: bool,
) -> Res<()> {
    use tokio::io::AsyncWriteExt;
    let body = get(c, v, u.clone(), None).await?;
    let mut pl = parse_media(&String::from_utf8_lossy(&body));
    if let Some(m) = &pl.map {
        let init = get(c, v, u.join(m)?, None).await?;
        if let Some(s) = sink.as_deref_mut() {
            s.write_all(&init).await?;
        }
    }
    let Some(last) = pl.parts.last() else {
        return Err("playlist has no EXT-X-PART (low-latency off?)".into());
    };
    let mut next = (last.msn, last.idx);
    loop {
        let new: Vec<Part> = pl.parts.iter().filter(|p| (p.msn, p.idx) >= next).cloned().collect();
        for p in &new {
            let data = get(c, v, u.join(&p.uri)?, p.range).await?;
            if count_media {
                v.media_us.fetch_add((p.dur * 1e6) as u64, Relaxed);
            }
            if let Some(s) = sink.as_deref_mut() {
                s.write_all(&data).await?;
                s.flush().await?;
            }
        }
        if let Some(l) = new.last() {
            next = if l.msn < pl.open_msn { (l.msn + 1, 0) } else { (l.msn, l.idx + 1) };
        } else {
            v.empty_reloads.fetch_add(1, Relaxed);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut q = u.clone();
        q.query_pairs_mut().append_pair("_HLS_msn", &next.0.to_string()).append_pair("_HLS_part", &next.1.to_string());
        let body = get(c, v, q, None).await?;
        pl = parse_media(&String::from_utf8_lossy(&body));
    }
}

// ---------------------------------------------------------------- RTSP

/// A minimal RTSP reader (DESCRIBE, SETUP per track with TCP interleaving,
/// PLAY, then count RTP payload bytes and sequence gaps). Not retina: under
/// overload a server may drop packets for a slow reader, and a load client
/// must count that loss instead of aborting the session on it.
async fn rtsp_viewer(url: String, v: Arc<Viewer>) {
    loop {
        v.sessions.fetch_add(1, Relaxed);
        if let Err(e) = rtsp_session(&url, &v).await {
            v.fail(e.as_ref());
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

type RtspRead = tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>;

struct RtspConn {
    rd: RtspRead,
    wr: tokio::net::tcp::OwnedWriteHalf,
    cseq: u32,
}

impl RtspConn {
    async fn request(&mut self, method: &str, url: &str, headers: &str) -> Res<(Vec<(String, String)>, Vec<u8>)> {
        use tokio::io::AsyncWriteExt;
        self.cseq += 1;
        let req = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: caudal-bench\r\n{headers}\r\n", self.cseq);
        self.wr.write_all(req.as_bytes()).await?;
        loop {
            match read_rtsp_item(&mut self.rd).await? {
                Item::Response(status, h, body) => {
                    if !(200..300).contains(&status) {
                        return Err(format!("RTSP {method} -> {status}").into());
                    }
                    return Ok((h, body));
                }
                Item::Interleaved(..) => {}
            }
        }
    }
}

enum Item {
    Response(u16, Vec<(String, String)>, Vec<u8>),
    Interleaved(u8, Vec<u8>),
}

async fn read_rtsp_item(rd: &mut RtspRead) -> Res<Item> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let first = rd.read_u8().await?;
    if first == b'$' {
        let ch = rd.read_u8().await?;
        let len = rd.read_u16().await? as usize;
        let mut buf = vec![0u8; len];
        rd.read_exact(&mut buf).await?;
        return Ok(Item::Interleaved(ch, buf));
    }
    let mut line = String::new();
    rd.read_line(&mut line).await?;
    let status_line = format!("{}{}", first as char, line.trim_end());
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
    let mut headers = Vec::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await? == 0 {
            return Err("RTSP EOF in headers".into());
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body).await?;
    Ok(Item::Response(status, headers, body))
}

fn header<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
    h.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
}

async fn rtsp_session(url: &str, v: &Viewer) -> Res<()> {
    use tokio::io::AsyncWriteExt;
    let u = Url::parse(url)?;
    let addr = format!("{}:{}", u.host_str().ok_or("no host")?, u.port().unwrap_or(554));
    v.requests.fetch_add(1, Relaxed);
    let tcp = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(&addr))
        .await
        .map_err(|_| "RTSP connect timed out")??;
    tcp.set_nodelay(true)?;
    let (r, w) = tcp.into_split();
    let mut c = RtspConn { rd: tokio::io::BufReader::with_capacity(256 * 1024, r), wr: w, cseq: 0 };
    let (h, sdp) =
        tokio::time::timeout(Duration::from_secs(10), c.request("DESCRIBE", url, "Accept: application/sdp\r\n"))
            .await
            .map_err(|_| "RTSP DESCRIBE timed out")??;
    let base = header(&h, "content-base").unwrap_or(url).to_string();
    let base = if base.ends_with('/') { base } else { format!("{base}/") };
    // One control URL per m= section.
    let mut controls: Vec<Option<String>> = Vec::new();
    for line in String::from_utf8_lossy(&sdp).lines() {
        if line.starts_with("m=") {
            controls.push(None);
        } else if let Some(ctl) = line.strip_prefix("a=control:")
            && let Some(last) = controls.last_mut()
        {
            let ctl = ctl.trim();
            *last = Some(if ctl.starts_with("rtsp://") { ctl.to_string() } else { format!("{base}{ctl}") });
        }
    }
    let mut session = String::new();
    for (i, ctl) in controls.iter().enumerate() {
        let ctl = ctl.as_deref().ok_or("SDP m= section without a=control")?;
        let sess = if session.is_empty() { String::new() } else { format!("Session: {session}\r\n") };
        let t = format!("Transport: RTP/AVP/TCP;unicast;interleaved={}-{}\r\n{sess}", 2 * i, 2 * i + 1);
        let (h, _) = tokio::time::timeout(Duration::from_secs(10), c.request("SETUP", ctl, &t))
            .await
            .map_err(|_| "RTSP SETUP timed out")??;
        if session.is_empty() {
            session = header(&h, "session").ok_or("no Session header")?.split(';').next().unwrap().to_string();
        }
    }
    let play = format!("Session: {session}\r\nRange: npt=0.000-\r\n");
    tokio::time::timeout(Duration::from_secs(10), c.request("PLAY", url, &play))
        .await
        .map_err(|_| "RTSP PLAY timed out")??;

    let mut last_seq: [Option<u16>; 8] = [None; 8];
    let mut keepalive = Instant::now();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(10), read_rtsp_item(&mut c.rd))
            .await
            .map_err(|_| "RTSP read timed out")??;
        if let Item::Interleaved(ch, pkt) = item
            && ch % 2 == 0
            && pkt.len() >= 12
        {
            let cc = (pkt[0] & 0x0f) as usize;
            v.bytes.fetch_add(pkt.len().saturating_sub(12 + 4 * cc) as u64, Relaxed);
            v.packets.fetch_add(1, Relaxed);
            let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
            let slot = &mut last_seq[(ch as usize / 2).min(7)];
            if let Some(prev) = *slot {
                let gap = seq.wrapping_sub(prev);
                if gap > 1 && gap < 0x8000 {
                    v.lost.fetch_add(u64::from(gap - 1), Relaxed);
                }
            }
            *slot = Some(seq);
        }
        if keepalive.elapsed() > Duration::from_secs(15) {
            keepalive = Instant::now();
            c.cseq += 1;
            let req = format!("GET_PARAMETER {url} RTSP/1.0\r\nCSeq: {}\r\nSession: {session}\r\n\r\n", c.cseq);
            c.wr.write_all(req.as_bytes()).await?;
        }
    }
}

// ---------------------------------------------------------------- WHEP

async fn whep_viewer(url: String, v: Arc<Viewer>) {
    loop {
        v.sessions.fetch_add(1, Relaxed);
        if let Err(e) = whep_session(&url, &v).await {
            v.fail(e.as_ref());
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn whep_session(url: &str, v: &Viewer) -> Res<()> {
    use str0m::change::SdpAnswer;
    use str0m::media::{Direction, MediaKind};
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event, Input, Output, Rtc};

    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let local = sock.local_addr()?;
    let mut rtc =
        Rtc::builder().clear_codecs().enable_h264(true).enable_opus(true).enable_pcmu(true).enable_pcma(true).set_rtp_mode(true).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let mut api = rtc.sdp_api();
    api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    let (offer, pending) = api.apply().ok_or("no offer")?;

    v.requests.fetch_add(1, Relaxed);
    if std::env::var_os("BENCH_DUMP_SDP").is_some() {
        eprintln!("{}", offer.to_sdp_string());
    }
    let c = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let resp = c.post(url).header("Content-Type", "application/sdp").body(offer.to_sdp_string()).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("WHEP HTTP {status}: {}", body.chars().take(160).collect::<String>()).into());
    }
    let answer = SdpAnswer::from_sdp_string(&resp.text().await?)?;
    rtc.sdp_api().accept_answer(pending, answer)?;

    let mut buf = vec![0u8; 2048];
    let mut last_rx = Instant::now();
    loop {
        let deadline = loop {
            match rtc.poll_output()? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    let _ = sock.send_to(&t.contents, t.destination).await;
                }
                Output::Event(Event::RtpPacket(p)) => {
                    v.bytes.fetch_add(p.payload.len() as u64, Relaxed);
                    last_rx = Instant::now();
                }
                Output::Event(Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected)) => {
                    return Err("ICE disconnected".into());
                }
                Output::Event(_) => {}
            }
        };
        if last_rx.elapsed() > Duration::from_secs(10) {
            return Err("WHEP: no media timed out".into());
        }
        let wait = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match tokio::time::timeout(wait, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, source))) => {
                if let Ok(r) = Receive::new(Protocol::Udp, source, local, &buf[..n]) {
                    rtc.handle_input(Input::Receive(Instant::now(), r))?;
                }
            }
            _ => rtc.handle_input(Input::Timeout(Instant::now()))?,
        }
    }
}

// ---------------------------------------------------------------- latency

const W: usize = 1920;
const H: usize = 40;

fn now_ms32() -> u32 {
    (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() % (1u128 << 32)) as u32
}

fn decode_stamp(strip: &[u8]) -> u32 {
    let y = H / 2;
    let mut v = 0u32;
    for k in 0..32 {
        let x = k * 60 + 30;
        v = (v << 1) | u32::from(strip[y * W + x] > 128);
    }
    v
}

fn ffmpeg_decoder(input: &[&str]) -> std::process::Child {
    let mut args: Vec<&str> = vec!["-hide_banner", "-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay"];
    args.extend_from_slice(input);
    args.extend_from_slice(&[
        "-an",
        "-vf",
        "crop=1920:40:0:0",
        "-pix_fmt",
        "gray",
        "-f",
        "rawvideo",
        "-flush_packets",
        "1",
        "pipe:1",
    ]);
    Command::new("ffmpeg")
        .args(&args)
        .stdin(if input.contains(&"pipe:0") { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("ffmpeg")
}

/// Reads decoded strips, takes one sample per second after a 3 s settle
/// (the first frames of a session arrive in a burst and are stale).
fn collect_samples(mut out: impl Read, samples: usize, extra: String) {
    let mut strip = vec![0u8; W * H];
    let start = Instant::now();
    let mut last_sample: Option<Instant> = None;
    let mut got: Vec<f64> = Vec::new();
    let mut invalid = 0u64;
    let mut frames = 0u64;
    while got.len() < samples {
        if out.read_exact(&mut strip).is_err() {
            break;
        }
        let now = now_ms32();
        frames += 1;
        let stamp = decode_stamp(&strip);
        let lat = now.wrapping_sub(stamp) as i32;
        if !(0..60_000).contains(&lat) {
            invalid += 1;
            continue;
        }
        if start.elapsed() < Duration::from_secs(3) {
            continue;
        }
        if last_sample.is_none_or(|t| t.elapsed() >= Duration::from_millis(1000)) {
            last_sample = Some(Instant::now());
            got.push(lat as f64);
        }
    }
    let list = got.iter().map(|x| format!("{x:.0}")).collect::<Vec<_>>().join(",");
    let mut sorted = got.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{{\"samples_ms\":[{}],\"n\":{},\"median_ms\":{},\"p95_ms\":{},\"frames\":{},\"invalid_stamps\":{}{}}}",
        list,
        got.len(),
        opt(pct_opt(&sorted, 50.0)),
        opt(pct_opt(&sorted, 95.0)),
        frames,
        invalid,
        extra
    );
}

fn latency_rtsp(args: Args) {
    let mut child = ffmpeg_decoder(&["-rtsp_transport", "tcp", "-i", &args.url]);
    let out = child.stdout.take().unwrap();
    collect_samples(out, args.samples, String::new());
    let _ = child.kill();
    let _ = child.wait();
}

/// Baseline without a server: the publisher's FLV straight over TCP.
fn latency_flv(args: Args) {
    let mut child = ffmpeg_decoder(&["-f", "flv", "-i", &args.url]);
    let out = child.stdout.take().unwrap();
    collect_samples(out, args.samples, String::new());
    let _ = child.kill();
    let _ = child.wait();
}

async fn latency_hls(args: Args) {
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-probesize",
            "32768",
            "-analyzeduration",
            "0",
            "-f",
            "mp4",
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            "crop=1920:40:0:0",
            "-pix_fmt",
            "gray",
            "-f",
            "rawvideo",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("ffmpeg");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let v = Arc::new(Viewer::default());
    let c = client();
    let base = Url::parse(&args.url).unwrap();
    let body = get(&c, &v, base.clone(), None).await.unwrap();
    let urls = media_urls(&base, &String::from_utf8_lossy(&body), true).unwrap();
    let media = urls[0].clone();
    let hold = {
        let b = get(&c, &v, media.clone(), None).await.unwrap();
        parse_media(&String::from_utf8_lossy(&b)).part_hold_back
    };
    let feeder = {
        let v = v.clone();
        tokio::spawn(async move {
            if let Err(e) = follow(&c, &v, media, Some(&mut stdin), true).await {
                eprintln!("latency-hls: follow ended: {e}");
            }
        })
    };
    let samples = args.samples;
    let extra = format!(",\"part_hold_back_s\":{}", opt(hold));
    let reader = tokio::task::spawn_blocking(move || {
        let out = stdout.into_owned_fd().expect("fd");
        collect_samples(std::fs::File::from(out), samples, extra);
    });
    let _ = reader.await;
    feeder.abort();
    let _ = child.kill().await;
}
