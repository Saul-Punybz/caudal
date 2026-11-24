//! LL-DASH over a real socket: H.264 + AAC published in real time, the
//! low-latency manifest (`ll.mpd`) read, and the segment it announces as
//! open requested right away. The response must start with its first
//! chunk long before the segment is done, keep streaming until it closes,
//! and add up, byte for byte, to the same segment fetched once complete;
//! several viewers at once get the same bytes, and a request for the
//! segment after it waits for it to open and streams it too. No external
//! tools; players read `ll.mpd` in `tests/dash.rs`.

#[path = "../src/mp4demux.rs"]
mod mp4demux;

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudal_core::{BufferConfig, Registry};
use caudal_hls::{HlsConfig, router};

/// One response read as it arrives: its `Content-Length` (if any), when
/// the first body bytes came and when the body ended, from `t0`.
struct Timed {
    length: Option<String>,
    first: Duration,
    end: Duration,
    body: Vec<u8>,
}

fn timed_get(url: &str, t0: Instant) -> Timed {
    let mut r = ureq::get(url).call().unwrap_or_else(|e| panic!("GET {url}: {e}"));
    let length = r.headers().get("content-length").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut reader = r.body_mut().with_config().limit(64 << 20).reader();
    let (mut body, mut buf, mut first) = (Vec::new(), [0u8; 4096], None);
    loop {
        let n = reader.read(&mut buf).unwrap_or_else(|e| panic!("read {url}: {e}"));
        if n == 0 {
            break;
        }
        first.get_or_insert_with(|| t0.elapsed());
        body.extend_from_slice(&buf[..n]);
    }
    Timed { length, first: first.expect("an empty body"), end: t0.elapsed(), body }
}

fn get(url: &str) -> Vec<u8> {
    timed_get(url, Instant::now()).body
}

fn attr<'a>(tag: &'a str, name: &str) -> &'a str {
    let key = format!(" {name}=\"");
    let at = tag.find(&key).unwrap_or_else(|| panic!("no {name} in {tag}")) + key.len();
    &tag[at..at + tag[at..].find('"').unwrap()]
}

/// The number of the last segment the video `SegmentTemplate` lists (the
/// open one in `ll.mpd`) and how many complete ones precede it.
fn last_number(mpd: &str) -> Option<u64> {
    let tpl = &mpd[mpd.find("<SegmentTemplate")?..];
    let start: u64 = attr(tpl, "startNumber").parse().ok()?;
    let tl = &tpl[..tpl.find("</SegmentTimeline>")?];
    let n = tl.matches("<S ").count() as u64;
    (n >= 3).then(|| start + n - 1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_open_segment_streams_its_chunks_as_they_are_made() {
    let fx = Arc::new(mp4demux::demux(include_bytes!("fixtures/av.mp4")));
    let reg = Registry::new();
    let app = router(reg.clone(), HlsConfig { ts: false, ..HlsConfig::default() }, Vec::new());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let publisher = reg.publish("llcam", BufferConfig::default()).unwrap();
    publisher.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let feeder = {
        let fx = fx.clone();
        tokio::spawn(async move {
            let t0 = tokio::time::Instant::now();
            for n in 0..i64::MAX {
                for f in fx.looped(n) {
                    tokio::time::sleep_until(t0 + Duration::from_micros(fx.micros(&f).max(0) as u64)).await;
                    if publisher.push(f).is_err() {
                        return;
                    }
                }
            }
        })
    };

    let base = format!("http://127.0.0.1:{port}/hls/llcam");
    let result = tokio::task::spawn_blocking(move || {
        // Wait for a manifest with two complete segments and an open one,
        // then for the next segment to open: request it at once.
        let t0 = Instant::now();
        let mut seen = None;
        let (mpd, open) = loop {
            assert!(t0.elapsed() < Duration::from_secs(30), "ll.mpd never announced an open segment");
            let mpd = String::from_utf8(get(&format!("{base}/ll.mpd"))).unwrap();
            if let Some(n) = last_number(&mpd) {
                match seen {
                    Some(prev) if n > prev => break (mpd, n),
                    _ => seen = Some(n),
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(mpd.contains("availabilityTimeComplete=\"false\""), "{mpd}");
        let t_req = Instant::now();
        let url = |l: char, n: u64| format!("{base}/{l}{n}.m4s");
        // Several viewers of the open segment, its audio, and one asking
        // for the segment after it, all at once.
        let viewers: Vec<_> = (0..6)
            .map(|_| {
                let u = url('v', open);
                std::thread::spawn(move || timed_get(&u, t_req))
            })
            .collect();
        let audio = {
            let u = url('a', open);
            std::thread::spawn(move || timed_get(&u, t_req))
        };
        let early = {
            let u = url('v', open + 1);
            std::thread::spawn(move || timed_get(&u, t_req))
        };
        let viewers: Vec<Timed> = viewers.into_iter().map(|h| h.join().unwrap()).collect();
        let audio = audio.join().unwrap();
        let early = early.join().unwrap();
        // Complete now: served whole, as before.
        let whole_v = timed_get(&url('v', open), Instant::now());
        let whole_a = get(&url('a', open));
        let whole_next = get(&url('v', open + 1));
        (viewers, audio, early, whole_v, whole_a, whole_next)
    })
    .await
    .unwrap();
    feeder.abort();
    let (viewers, audio, early, whole_v, whole_a, whole_next) = result;

    assert!(whole_v.length.is_some(), "a complete segment has a Content-Length");
    for (i, v) in viewers.iter().enumerate() {
        eprintln!("viewer {i}: first chunk after {:?}, last after {:?}, {} bytes", v.first, v.end, v.body.len());
        assert!(v.length.is_none(), "the open segment is chunked, not sized");
        // 2 s segments of 200 ms parts, asked for within ~a part of opening.
        assert!(v.first < Duration::from_millis(700), "first chunk only after {:?}", v.first);
        assert!(v.end > v.first + Duration::from_millis(800), "it streamed: {:?} .. {:?}", v.first, v.end);
        assert!(v.body == whole_v.body, "viewer {i}: the chunks add up to the complete segment, byte for byte");
    }
    assert!(audio.length.is_none() && audio.body == whole_a, "audio streams the same way");
    assert!(audio.first < Duration::from_millis(700), "audio first chunk after {:?}", audio.first);
    assert!(early.first > viewers[0].end - Duration::from_millis(300), "the next one waited for its segment to open");
    assert!(early.body == whole_next, "the next segment, asked for early, adds up too");
    // Each chunk is one moof + mdat pair: the streamed body is several.
    let moofs = whole_v.body.windows(4).filter(|w| w == b"moof").count();
    assert!(moofs >= 5, "{moofs} chunks in a 2 s segment of 200 ms parts");
}
