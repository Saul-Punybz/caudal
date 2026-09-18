//! WHIP and WHEP against a real `Registry` and the router on free ports.
//!
//! Machine rule: run with `--test-threads=1`; the ffmpeg child is killed in
//! `Drop`, never left to exit on its own.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{
    Access, AudioParams, BufferConfig, Codec, Denied, Event, Frame, Gate, GateFuture, Registry, StartAt, TrackId,
    TrackInfo, VideoParams,
};
use caudal_webrtc::{WebRtcConfig, router};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Input, Output, Rtc};

// ---------- harness ----------

struct Server {
    http: String,
}

async fn serve(registry: Arc<Registry>) -> Server {
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let app = router(registry, WebRtcConfig { udp_bind: udp, public_ips: vec![], buffer: BufferConfig::default() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { http }
}

struct Resp {
    status: u16,
    location: Option<String>,
    www_auth: Option<String>,
    body: String,
}

/// Blocking HTTP on a helper thread (ureq is sync).
async fn http(method: &'static str, url: String, body: Option<String>, bearer: Option<&'static str>) -> Resp {
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let res = match (method, body) {
            ("POST", Some(b)) => {
                let mut r = agent.post(&url).header("Content-Type", "application/sdp");
                if let Some(t) = bearer {
                    r = r.header("Authorization", &format!("Bearer {t}"));
                }
                r.send(b)
            }
            ("DELETE", _) => agent.delete(&url).call(),
            _ => unreachable!(),
        };
        let mut res = res.expect("http request");
        let h = |n: &str| res.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let (location, www_auth) = (h("location"), h("www-authenticate"));
        Resp {
            status: res.status().as_u16(),
            location,
            www_auth,
            body: res.body_mut().read_to_string().unwrap_or_default(),
        }
    })
    .await
    .unwrap()
}

fn ffmpeg_has_whip() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-muxers"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("whip"))
        .unwrap_or(false)
}

/// ffmpeg publishing over WHIP; killed on drop.
struct Ffmpeg(Child);

impl Ffmpeg {
    /// Same command as `Publisher::whip` in the server's e2e support.
    fn whip(url: &str, secs: u32) -> Self {
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", &secs.to_string()])
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
                "-bf",
                "0",
                "-g",
                "60",
                "-b:v",
                "2M",
            ])
            .args(["-c:a", "libopus", "-b:a", "96k", "-ar", "48000", "-ac", "2"])
            .args(["-f", "whip", url])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ffmpeg");
        Self(child)
    }

    fn exited(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if let Some(v) = f() {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

fn avcc_nal_types(data: &[u8]) -> Option<Vec<u8>> {
    let mut types = Vec::new();
    let mut d = data;
    while !d.is_empty() {
        if d.len() < 4 {
            return None;
        }
        let len = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
        if len == 0 || d.len() < 4 + len {
            return None;
        }
        types.push(d[4] & 0x1f);
        d = &d[4 + len..];
    }
    Some(types)
}

// ---------- (a) + (b): ffmpeg WHIP ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ffmpeg_whip_publishes_h264_and_opus() {
    if !ffmpeg_has_whip() {
        eprintln!("SKIP: this ffmpeg has no WHIP muxer");
        return;
    }
    let registry = Registry::new();
    let srv = serve(registry.clone()).await;
    let mut ff = Ffmpeg::whip(&format!("{}/whip/cam", srv.http), 8);

    let stream = wait_for(Duration::from_secs(15), || registry.get("cam").filter(|s| s.tracks().len() == 2))
        .await
        .expect("stream with two tracks");
    let tracks = stream.tracks();
    assert_eq!(tracks[0].id, TrackId(0));
    assert_eq!(tracks[0].codec, Codec::H264);
    assert_eq!(tracks[0].timescale, 90_000);
    let v = tracks[0].video.expect("video params");
    assert_eq!((v.width, v.height), (1280, 720));
    assert_eq!(tracks[0].init[0], 1, "avcC version");
    assert_eq!(tracks[1].id, TrackId(1));
    assert_eq!(tracks[1].codec, Codec::Opus);
    assert_eq!(tracks[1].timescale, 48_000);
    assert_eq!(&tracks[1].init[..8], b"OpusHead");
    assert_eq!(tracks[1].audio.map(|a| a.sample_rate), Some(48_000));

    // Read from the first keyframe for ~3 s of media.
    let mut sub = stream.subscribe_internal(StartAt::Oldest);
    let mut last: [Option<i64>; 2] = [None, None];
    let mut first: [Option<Arc<Frame>>; 2] = [None, None];
    let mut counts = [0usize; 2];
    let t0 = Instant::now();
    while counts[0] < 90 && t0.elapsed() < Duration::from_secs(10) {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.expect("frames keep coming");
        let Event::Frame(f) = ev else { continue };
        let i = f.track.0 as usize;
        if let Some(prev) = last[i] {
            assert!(f.dts > prev, "track {i}: dts {} after {prev}", f.dts);
        }
        assert_eq!(f.pts, f.dts);
        last[i] = Some(f.dts);
        counts[i] += 1;
        if i == 0 {
            let types = avcc_nal_types(&f.data).expect("valid AVCC");
            assert_eq!(f.keyframe, types.contains(&5), "keyframe flag matches IDR: {types:?}");
        } else {
            assert!(f.keyframe);
        }
        first[i].get_or_insert(f);
    }
    let fv = first[0].clone().expect("video frames");
    let fa = first[1].clone().expect("audio frames");
    assert!(fv.keyframe, "first video frame is a keyframe");
    assert!(fv.dts < 90_000, "video starts near 0: {}", fv.dts);
    assert!(fa.dts < 48_000, "audio starts near 0: {}", fa.dts);
    assert!(counts[0] >= 90 && counts[1] >= 50, "counts {counts:?}");
    // Audio and video stay on one timeline (within 500 ms).
    let (vs, as_) = (last[0].unwrap() as f64 / 90_000.0, last[1].unwrap() as f64 / 48_000.0);
    assert!((vs - as_).abs() < 0.5, "video at {vs:.3}s, audio at {as_:.3}s");

    // ffmpeg ends after 8 s and DELETEs its session: the stream goes away.
    let gone = wait_for(Duration::from_secs(15), || registry.get("cam").is_none().then_some(())).await;
    assert!(ff.exited() || gone.is_some());
    assert!(gone.is_some(), "stream ended after ffmpeg's DELETE");
}

// ---------- str0m as the remote peer ----------

struct Client {
    rtc: Rtc,
    sock: tokio::net::UdpSocket,
    local: SocketAddr,
}

impl Client {
    async fn new() -> Self {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        let mut rtc = Rtc::builder().clear_codecs().enable_h264(true).enable_opus(true).build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        Self { rtc, sock, local }
    }

    /// An SDP offer with the given m-lines.
    fn offer(&mut self, media: &[(MediaKind, Direction)]) -> (String, str0m::change::SdpPendingOffer) {
        let mut api = self.rtc.sdp_api();
        for (k, d) in media {
            api.add_media(*k, *d, None, None, None);
        }
        let (offer, pending) = api.apply().unwrap();
        (offer.to_sdp_string(), pending)
    }

    fn accept(&mut self, pending: str0m::change::SdpPendingOffer, answer: &str) {
        let answer = SdpAnswer::from_sdp_string(answer).expect("answer parses");
        self.rtc.sdp_api().accept_answer(pending, answer).expect("answer accepted");
    }

    /// Drives the client until `f` returns true for an event or time is up.
    async fn run_until(&mut self, timeout: Duration, mut f: impl FnMut(&str0m::Event) -> bool) -> bool {
        let end = Instant::now() + timeout;
        let mut buf = vec![0u8; 2048];
        loop {
            let deadline = loop {
                match self.rtc.poll_output().unwrap() {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        let _ = self.sock.send_to(&t.contents, t.destination).await;
                    }
                    Output::Event(e) => {
                        if f(&e) {
                            return true;
                        }
                    }
                }
            };
            let now = Instant::now();
            if now >= end {
                return false;
            }
            let wait = deadline.min(end).saturating_duration_since(now).max(Duration::from_millis(1));
            match tokio::time::timeout(wait, self.sock.recv_from(&mut buf)).await {
                Ok(Ok((n, source))) => {
                    if let Ok(r) = Receive::new(Protocol::Udp, source, self.local, &buf[..n]) {
                        let _ = self.rtc.handle_input(Input::Receive(Instant::now(), r));
                    }
                }
                _ => {
                    let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
                }
            }
        }
    }
}

// ---------- (b) DELETE ends a WHIP stream ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whip_delete_ends_the_stream() {
    let registry = Registry::new();
    let srv = serve(registry.clone()).await;
    let mut c = Client::new().await;
    let (offer, _pending) =
        c.offer(&[(MediaKind::Video, Direction::SendOnly), (MediaKind::Audio, Direction::SendOnly)]);

    let r = http("POST", format!("{}/whip/del", srv.http), Some(offer.clone()), None).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body.contains("a=ice-lite"), "{}", r.body);
    assert!(r.body.contains("H264/90000") && r.body.contains("opus/48000"), "{}", r.body);
    let loc = r.location.expect("Location");
    assert!(loc.starts_with("/whip/del/"), "{loc}");
    assert!(registry.get("del").is_some());

    // A second publisher on the same name is refused.
    let busy = http("POST", format!("{}/whip/del", srv.http), Some(offer), None).await;
    assert_eq!(busy.status, 409);

    assert_eq!(http("DELETE", format!("{}{loc}", srv.http), None, None).await.status, 200);
    let gone = wait_for(Duration::from_secs(3), || registry.get("del").is_none().then_some(())).await;
    assert!(gone.is_some(), "stream still registered after DELETE");
    assert_eq!(http("DELETE", format!("{}{loc}", srv.http), None, None).await.status, 404);

    // Garbage in: 400, and the server keeps working.
    let bad = http("POST", format!("{}/whip/x", srv.http), Some("v=0\r\nnonsense".into()), None).await;
    assert_eq!(bad.status, 400);
    let (vp8_only, _) = {
        let mut c =
            Client { rtc: Rtc::builder().clear_codecs().enable_vp8(true).build(Instant::now()), ..Client::new().await };
        c.offer(&[(MediaKind::Video, Direction::SendOnly)])
    };
    let r = http("POST", format!("{}/whip/x", srv.http), Some(vp8_only), None).await;
    assert_eq!(r.status, 406, "{}", r.body);
    assert!(registry.get("x").is_none());
}

// ---------- (c) the gate ----------

struct OnlyGood;

impl Gate for OnlyGood {
    fn check<'a>(&'a self, _: Access, _: &'a str, token: Option<&'a str>) -> GateFuture<'a> {
        Box::pin(async move {
            match token {
                None => Err(Denied::Missing),
                Some("good") => Ok(()),
                Some(_) => Err(Denied::Refused("bad token".into())),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whip_gate_refuses() {
    let registry = Registry::new();
    registry.set_gate(Arc::new(OnlyGood));
    let srv = serve(registry.clone()).await;
    let mut c = Client::new().await;
    let (offer, _p) = c.offer(&[(MediaKind::Video, Direction::SendOnly)]);

    let r = http("POST", format!("{}/whip/g", srv.http), Some(offer.clone()), None).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.www_auth.as_deref(), Some("Bearer"));
    let r = http("POST", format!("{}/whip/g", srv.http), Some(offer.clone()), Some("bad")).await;
    assert_eq!(r.status, 403);
    let r = http("POST", format!("{}/whip/g?token=bad", srv.http), Some(offer.clone()), None).await;
    assert_eq!(r.status, 403);
    assert!(registry.get("g").is_none(), "nothing published");

    let r = http("POST", format!("{}/whip/g?token=good", srv.http), Some(offer), None).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(registry.get("g").is_some());
}

// ---------- (d) WHEP ----------

/// The fixture as AVCC access units (split on AUD), plus its SPS and PPS.
fn fixture() -> (Vec<(bool, Bytes)>, Vec<u8>, Vec<u8>) {
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clip.h264")).unwrap();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < raw.len() {
        if raw[i] == 0 && raw[i + 1] == 0 && raw[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(raw.len(), |&n| n - 3);
        while e > s && raw[e - 1] == 0 {
            e -= 1;
        }
        nals.push(raw[s..e].to_vec());
    }
    let (mut sps, mut pps) = (Vec::new(), Vec::new());
    let mut aus: Vec<(bool, Bytes)> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut key = false;
    for nal in nals {
        match nal[0] & 0x1f {
            9 => {
                if !cur.is_empty() {
                    aus.push((key, Bytes::from(std::mem::take(&mut cur))));
                }
                key = false;
                continue;
            }
            7 => sps = nal.clone(),
            8 => pps = nal.clone(),
            5 => key = true,
            _ => {}
        }
        cur.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        cur.extend_from_slice(&nal);
    }
    if !cur.is_empty() {
        aus.push((key, Bytes::from(cur)));
    }
    (aus, sps, pps)
}

fn avcc_record(sps: &[u8], pps: &[u8]) -> Bytes {
    let mut r = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    r.extend_from_slice(sps);
    r.push(1);
    r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    r.extend_from_slice(pps);
    Bytes::from(r)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whep_sends_h264_that_reassembles_into_a_keyframe() {
    let registry = Registry::new();
    let srv = serve(registry.clone()).await;
    let (aus, sps, pps) = fixture();
    assert!(aus.len() >= 20 && aus[0].0, "fixture starts on a keyframe");

    // A synthetic live stream: the clip in a loop at 30 fps.
    let publisher = registry.publish("live", BufferConfig::default()).unwrap();
    publisher
        .set_tracks(vec![
            TrackInfo {
                id: TrackId(0),
                codec: Codec::H264,
                timescale: 90_000,
                init: avcc_record(&sps, &pps),
                lang: None,
                video: Some(VideoParams { width: 320, height: 240, fps: Some(30.0) }),
                audio: None,
            },
            TrackInfo {
                id: TrackId(1),
                codec: Codec::Opus,
                timescale: 48_000,
                init: Bytes::from_static(b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00"),
                lang: None,
                video: None,
                audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
            },
        ])
        .unwrap();
    // The fixture carries SPS/PPS in-band on its first IDR only; strip them
    // so the test proves the server re-sends them from the avcC.
    let strip = |d: &Bytes| -> Bytes {
        let mut out = Vec::new();
        let mut s = &d[..];
        while s.len() >= 4 {
            let n = u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize;
            if !matches!(s[4] & 0x1f, 7 | 8) {
                out.extend_from_slice(&s[..4 + n]);
            }
            s = &s[4 + n..];
        }
        Bytes::from(out)
    };
    let feeder = tokio::spawn(async move {
        let mut n: i64 = 0;
        loop {
            for (key, data) in &aus {
                let f = Frame { track: TrackId(0), dts: n * 3000, pts: n * 3000, keyframe: *key, data: strip(data) };
                // 33 ms of audio as ~1.65 20 ms Opus packets: send one per video frame
                // with the matching 48 kHz timestamp (a silent Opus TOC frame).
                let a = Frame {
                    track: TrackId(1),
                    dts: n * 1600,
                    pts: n * 1600,
                    keyframe: true,
                    data: Bytes::from_static(&[0xf8, 0xff, 0xfe]),
                };
                if publisher.push(f).is_err() || publisher.push(a).is_err() {
                    return;
                }
                n += 1;
                tokio::time::sleep(Duration::from_millis(33)).await;
            }
        }
    });

    let mut c = Client::new().await;
    let (offer, pending) = c.offer(&[(MediaKind::Video, Direction::RecvOnly), (MediaKind::Audio, Direction::RecvOnly)]);
    let r = http("POST", format!("{}/whep/live", srv.http), Some(offer), None).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let loc = r.location.expect("Location");
    assert!(loc.starts_with("/whep/live/"), "{loc}");
    c.accept(pending, &r.body);

    let mut connected = false;
    let mut got: Option<Vec<u8>> = None;
    let mut audio = 0;
    let ok = c
        .run_until(Duration::from_secs(10), |e| match e {
            str0m::Event::Connected => {
                connected = true;
                false
            }
            str0m::Event::MediaData(d) if d.params.spec().codec == str0m::format::Codec::Opus => {
                audio += 1;
                got.is_some() && audio >= 5
            }
            str0m::Event::MediaData(d) => {
                let types: Vec<u8> = split_types(&d.data);
                if types.contains(&5) && got.is_none() {
                    got = Some(types);
                }
                got.is_some() && audio >= 5
            }
            _ => false,
        })
        .await;
    assert!(connected, "ICE + DTLS came up");
    assert!(ok, "received no keyframe or no audio over WHEP (audio packets: {audio}, video: {got:?})");
    let types = got.unwrap();
    assert!(types.contains(&7) && types.contains(&8), "SPS/PPS sent before the IDR: {types:?}");

    // Unknown stream: 404. DELETE ends the session.
    let mut c2 = Client::new().await;
    let (offer2, _) = c2.offer(&[(MediaKind::Video, Direction::RecvOnly)]);
    assert_eq!(http("POST", format!("{}/whep/nope", srv.http), Some(offer2), None).await.status, 404);
    assert_eq!(http("DELETE", format!("{}{loc}", srv.http), None, None).await.status, 200);
    assert_eq!(http("DELETE", format!("{}{loc}", srv.http), None, None).await.status, 404);
    feeder.abort();
}

/// NAL types in an Annex B buffer.
fn split_types(d: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    let mut i = 0;
    while i + 3 < d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            t.push(d[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    t
}
