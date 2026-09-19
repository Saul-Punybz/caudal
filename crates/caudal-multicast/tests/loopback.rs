//! Real multicast over the host's own stack: a synthetic stream goes out
//! to a 239.255.0.0/16 group with `IP_MULTICAST_LOOP` on, and a plain
//! socket that joined the group checks what arrives: datagram sizes, TS
//! sync, PAT/PMT repetition, PCR, continuity counters, RTP headers, and
//! that a backlog delivered all at once still leaves paced to its own
//! timestamps.
//!
//! Uses the OS's default multicast interface (the default route), which
//! GitHub's Linux runners and a typical Mac both have.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{AudioParams, BufferConfig, Codec, Frame, Registry, TrackId, TrackInfo, VideoParams};
use caudal_multicast::{Format, Interface, MulticastTarget};
use mp4_atom::Atom;

const FPS: i64 = 25;

fn h264_init() -> Bytes {
    let sps: &[u8] = &[0x67, 0x42, 0x00, 0x0A, 0x8C, 0x8D, 0x40, 0x50, 0x1E, 0xD0, 0x0F, 0x08, 0x84, 0x6A];
    let pps: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
    let avcc = mp4_atom::Avcc::new(sps, pps).unwrap();
    let mut out = Vec::new();
    avcc.encode_body(&mut out).unwrap();
    Bytes::from(out)
}

fn tracks() -> Vec<TrackInfo> {
    let asc: u16 = (2u16 << 11) | (3u16 << 7) | (2u16 << 3);
    vec![
        TrackInfo {
            id: TrackId(0),
            codec: Codec::H264,
            timescale: 90_000,
            init: h264_init(),
            lang: None,
            video: Some(VideoParams { width: 16, height: 16, fps: None }),
            audio: None,
        },
        TrackInfo {
            id: TrackId(1),
            codec: Codec::Aac,
            timescale: 48_000,
            init: Bytes::copy_from_slice(&asc.to_be_bytes()),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        },
    ]
}

/// One AVCC-framed NAL of `len` bytes (IDR or non-IDR slice).
fn video_frame(i: i64) -> Frame {
    let key = i % FPS == 0;
    let len = if key { 20_000 } else { 1_500 };
    let mut data = Vec::with_capacity(len + 4);
    data.extend_from_slice(&(len as u32).to_be_bytes());
    data.push(if key { 0x65 } else { 0x41 });
    data.resize(len + 4, 0xAB);
    let dts = i * 90_000 / FPS;
    Frame { track: TrackId(0), dts, pts: dts, keyframe: key, data: Bytes::from(data) }
}

fn audio_frame(i: i64) -> Frame {
    let dts = i * 1024;
    Frame { track: TrackId(1), dts, pts: dts, keyframe: true, data: Bytes::from(vec![0x21; 300]) }
}

/// Pushes `secs` of media (video + audio interleaved by DTS), either all at
/// once or in real time.
async fn publish(publisher: &caudal_core::Publisher, from_frame: i64, secs: i64, real_time: bool) {
    let mut audio_i = from_frame * 48_000 / FPS / 1024;
    for i in from_frame..from_frame + secs * FPS {
        let video_us = i * 1_000_000 / FPS;
        while audio_i * 1024 * 1_000_000 / 48_000 <= video_us {
            publisher.push(audio_frame(audio_i)).unwrap();
            audio_i += 1;
        }
        publisher.push(video_frame(i)).unwrap();
        if real_time {
            tokio::time::sleep(Duration::from_millis(1000 / FPS as u64)).await;
        }
    }
}

fn group() -> SocketAddr {
    let port = UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
    SocketAddr::from((Ipv4Addr::new(239, 255, 77, (port % 250) as u8 + 1), port))
}

/// A socket that joined `group` on the default interface.
fn receiver(group: SocketAddr) -> UdpSocket {
    let SocketAddr::V4(g) = group else { unreachable!() };
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
    s.set_reuse_address(true).unwrap();
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, g.port())).into()).unwrap();
    s.join_multicast_v4(g.ip(), &Ipv4Addr::UNSPECIFIED).unwrap();
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    s
}

/// Receives until `quiet` passes without a datagram (after at least one
/// arrived) or `limit` elapses. Returns (arrival time, bytes).
fn collect(sock: &UdpSocket, quiet: Duration, limit: Duration) -> Vec<(Instant, Vec<u8>)> {
    let mut out = Vec::new();
    let t0 = Instant::now();
    let mut last = Instant::now();
    let mut buf = [0u8; 2048];
    while t0.elapsed() < limit {
        match sock.recv(&mut buf) {
            Ok(n) => {
                last = Instant::now();
                out.push((last, buf[..n].to_vec()));
            }
            Err(_) if !out.is_empty() && last.elapsed() > quiet => break,
            Err(_) => {}
        }
    }
    out
}

fn target(stream: &str, group: SocketAddr, format: Format) -> MulticastTarget {
    MulticastTarget {
        stream: stream.into(),
        group,
        format,
        ttl: 1,
        interface: Interface::Default,
        loopback: true,
        pacing: true,
    }
}

struct TsCheck {
    pat: usize,
    pmt: usize,
    pcrs: Vec<u64>,
    cc_errors: usize,
}

/// Walks every TS packet: counts PAT/PMT, collects PCRs on the video PID
/// and checks each PID's continuity counter.
fn check_ts(payloads: &[&[u8]]) -> TsCheck {
    let mut cc: HashMap<u16, u8> = HashMap::new();
    let mut c = TsCheck { pat: 0, pmt: 0, pcrs: Vec::new(), cc_errors: 0 };
    for p in payloads.iter().flat_map(|d| d.chunks(188)) {
        assert_eq!(p.len(), 188);
        assert_eq!(p[0], 0x47, "sync byte");
        let pid = u16::from_be_bytes([p[1], p[2]]) & 0x1FFF;
        let has_payload = p[3] & 0x10 != 0;
        let counter = p[3] & 0x0F;
        if has_payload
            && let Some(prev) = cc.insert(pid, counter)
            && counter != (prev + 1) & 0x0F
        {
            c.cc_errors += 1;
        }
        match pid {
            0 => c.pat += 1,
            0x1000 => c.pmt += 1,
            _ => {}
        }
        // Adaptation field with the PCR flag.
        if p[3] & 0x20 != 0 && p[4] >= 7 && p[5] & 0x10 != 0 && pid == 0x101 {
            let b = &p[6..12];
            let base = (u64::from(b[0]) << 25)
                | (u64::from(b[1]) << 17)
                | (u64::from(b[2]) << 9)
                | (u64::from(b[3]) << 1)
                | (u64::from(b[4]) >> 7);
            c.pcrs.push(base);
        }
    }
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_over_udp_multicast() {
    let registry = Registry::new();
    let group = group();
    let rx = receiver(group);
    let handle = caudal_multicast::start(registry.clone(), vec![target("tv1", group, Format::Ts)]);

    let publisher = registry.publish("tv1", BufferConfig::default()).unwrap();
    publisher.set_tracks(tracks()).unwrap();
    publish(&publisher, 0, 2, true).await;
    drop(publisher); // the stream ends: the output flushes and stops.

    let got = tokio::task::spawn_blocking(move || collect(&rx, Duration::from_millis(600), Duration::from_secs(10)))
        .await
        .unwrap();
    assert!(got.len() > 50, "received {} datagrams", got.len());
    let (last, full) = got.split_last().unwrap();
    assert!(full.iter().all(|(_, d)| d.len() == 1316), "every datagram but the last is 7 x 188 bytes");
    assert!(last.1.len() <= 1316 && last.1.len() % 188 == 0);

    let payloads: Vec<&[u8]> = got.iter().map(|(_, d)| d.as_slice()).collect();
    let ts = check_ts(&payloads);
    assert_eq!(ts.cc_errors, 0, "continuity counters are continuous");
    // ~2 s of media: PSI at least every 100 ms, so ~20 at a minimum.
    assert!(ts.pat >= 15 && ts.pmt >= 15, "PAT {} / PMT {}", ts.pat, ts.pmt);
    assert!(ts.pcrs.len() >= 20, "PCR at least every 40 ms of wall time: got {}", ts.pcrs.len());
    assert!(
        ts.pcrs.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] <= 3600),
        "PCR advances, at most 40 ms apart (one per 25 fps frame)"
    );

    let mut body = String::new();
    handle.render_metrics(&mut body);
    assert!(body.contains("caudal_multicast_packets_total{stream=\"tv1\""), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backlog_is_paced_to_its_own_timestamps() {
    let registry = Registry::new();
    let group = group();
    let rx = receiver(group);
    let _handle = caudal_multicast::start(registry.clone(), vec![target("tv2", group, Format::Ts)]);

    let publisher = registry.publish("tv2", BufferConfig::default()).unwrap();
    publisher.set_tracks(tracks()).unwrap();
    // Give the output a moment to subscribe, then hand it 2 s of media in
    // one go: unpaced, it would all leave within milliseconds.
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish(&publisher, 0, 2, false).await;

    let got = tokio::task::spawn_blocking(move || collect(&rx, Duration::from_millis(800), Duration::from_secs(10)))
        .await
        .unwrap();
    drop(publisher);
    assert!(got.len() > 50, "received {} datagrams", got.len());
    let spread = got.last().unwrap().0 - got.first().unwrap().0;
    assert!(spread >= Duration::from_millis(1500), "2 s of media must take about 2 s to leave, took {spread:?}");
    // No burst: never more than a keyframe's worth at the smoothing rate
    // in any 10 ms slice. A 20 kB keyframe is 16 datagrams; unpaced, the
    // whole 2 s (~70 datagrams) lands in one slice.
    let mut max_in_10ms = 0;
    for (i, (t, _)) in got.iter().enumerate() {
        let n = got[i..].iter().take_while(|(u, _)| *u - *t < Duration::from_millis(10)).count();
        max_in_10ms = max_in_10ms.max(n);
    }
    assert!(max_in_10ms <= 20, "at most {max_in_10ms} datagrams within 10 ms");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtp_mp2t_headers() {
    let registry = Registry::new();
    let group = group();
    let rx = receiver(group);
    let _handle = caudal_multicast::start(registry.clone(), vec![target("tv3", group, Format::Rtp)]);

    let publisher = registry.publish("tv3", BufferConfig::default()).unwrap();
    publisher.set_tracks(tracks()).unwrap();
    publish(&publisher, 0, 1, true).await;
    drop(publisher);

    let got = tokio::task::spawn_blocking(move || collect(&rx, Duration::from_millis(600), Duration::from_secs(10)))
        .await
        .unwrap();
    assert!(got.len() > 20, "received {} datagrams", got.len());
    let ssrc = &got[0].1[8..12];
    let mut prev: Option<(u16, u32)> = None;
    for (_, d) in &got[..got.len() - 1] {
        assert_eq!(d.len(), 12 + 1316);
        assert_eq!(d[0], 0x80, "RTP v2, no padding/extension/CSRC");
        assert_eq!(d[1] & 0x7F, 33, "payload type 33 (MP2T)");
        assert_eq!(&d[8..12], ssrc, "one SSRC per session");
        assert_eq!(d[12], 0x47, "payload starts on a TS packet");
        let seq = u16::from_be_bytes([d[2], d[3]]);
        let ts = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
        if let Some((pseq, pts)) = prev {
            assert_eq!(seq, pseq.wrapping_add(1), "sequence numbers are consecutive");
            assert!(ts.wrapping_sub(pts) < 90_000, "90 kHz timestamps advance monotonically");
        }
        prev = Some((seq, ts));
    }
    let payloads: Vec<&[u8]> = got.iter().map(|(_, d)| &d[12..]).collect();
    assert_eq!(check_ts(&payloads).cc_errors, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejoins_on_republish() {
    let registry = Registry::new();
    let group = group();
    let rx = receiver(group);
    let handle = caudal_multicast::start(registry.clone(), vec![target("tv4", group, Format::Ts)]);

    for round in 0..2 {
        let publisher = registry.publish("tv4", BufferConfig::default()).unwrap();
        publisher.set_tracks(tracks()).unwrap();
        publish(&publisher, 0, 1, true).await;
        drop(publisher);
        let got = tokio::task::spawn_blocking({
            let rx = rx.try_clone().unwrap();
            move || collect(&rx, Duration::from_millis(600), Duration::from_secs(10))
        })
        .await
        .unwrap();
        assert!(got.len() > 10, "round {round}: received {} datagrams", got.len());
    }
    let mut body = String::new();
    handle.render_metrics(&mut body);
    assert!(body.contains("caudal_multicast_send_errors_total{stream=\"tv4\",group=\""), "{body}");
}
