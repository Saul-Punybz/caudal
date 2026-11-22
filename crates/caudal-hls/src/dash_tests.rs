//! DASH manifest tests: the MPD rendered from a real packager fed the
//! H.264 + AAC fixture, parsed as XML, its timeline checked against the
//! packager's own segments, and every URL it names fetched through the
//! router.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use caudal_core::{BufferConfig, Publisher, Registry};
use mp4_atom::{Any, Decode, Moof};
use tower::ServiceExt;

use super::tests::mp4demux::{Demuxed, demux};
use super::{HlsConfig, dash, packager::Packager, router};

const CFG: HlsConfig = HlsConfig {
    part_ms: 200,
    segment_ms: 2000,
    cue_tags: true,
    cue_out_tags: false,
    reconnect_grace: Duration::ZERO,
    ts: false,
};

fn fixture() -> Demuxed {
    demux(include_bytes!("../tests/fixtures/av.mp4"))
}

fn feed(pkg: &mut Packager, fx: &Demuxed, loops: std::ops::Range<i64>, t0: SystemTime) {
    for n in loops {
        for f in fx.looped(n) {
            pkg.push(&f, t0 + Duration::from_micros(fx.micros(&f) as u64));
        }
    }
}

fn mpd(pkg: &Packager, token: Option<&str>) -> String {
    let snap = dash::Snap::of(pkg).expect("complete segments");
    let ctx = dash::Ctx {
        token,
        time_url: Some("http://example.test/dash/time".into()),
        now: SystemTime::UNIX_EPOCH + Duration::from_secs(2_000),
    };
    dash::render(("cam", &snap), &[], &ctx)
}

fn children<'a, 'i>(n: roxmltree::Node<'a, 'i>, tag: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    n.children().filter(move |c| c.has_tag_name(tag))
}

fn num<T: std::str::FromStr>(n: roxmltree::Node<'_, '_>, attr: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    n.attribute(attr).unwrap_or_else(|| panic!("{attr} missing")).parse().unwrap()
}

/// `(t, d)` of every `S` of one `SegmentTemplate`, and its startNumber.
fn timeline(tpl: roxmltree::Node<'_, '_>) -> (u64, Vec<(i64, u64)>) {
    let tl = children(tpl, "SegmentTimeline").next().unwrap();
    (num(tpl, "startNumber"), children(tl, "S").map(|s| (num(s, "t"), num(s, "d"))).collect())
}

#[test]
fn names() {
    use dash::File;
    assert_eq!(dash::parse_name("v12.m4s"), Some(File::Media { video: true, msn: 12 }));
    assert_eq!(dash::parse_name("a0.m4s"), Some(File::Media { video: false, msn: 0 }));
    assert_eq!(dash::parse_name("v-init0.mp4"), Some(File::Init { video: true, generation: 0 }));
    assert_eq!(dash::parse_name("a-init3.mp4"), Some(File::Init { video: false, generation: 3 }));
    for bad in ["v.m4s", "v01.m4s", "x1.m4s", "v1.mp4", "v-init.mp4", "a-1.m4s", "v1.p0.m4s", "init.mp4", "index.m3u8"]
    {
        assert_eq!(dash::parse_name(bad), None, "{bad}");
    }
    assert_eq!(dash::time_url(Some("h:8080"), None).as_deref(), Some("http://h:8080/dash/time"));
    assert_eq!(dash::time_url(Some("h"), Some("https, http")).as_deref(), Some("https://h/dash/time"));
    assert_eq!(dash::time_url(Some("h\"><x"), None), None);
    assert_eq!(dash::time_url(None, None), None);
}

#[test]
fn mpd_is_well_formed_and_its_timeline_is_the_packagers() {
    let fx = fixture();
    let mut pkg = Packager::new(CFG);
    pkg.set_tracks(&fx.tracks);
    feed(&mut pkg, &fx, 0..4, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    let text = mpd(&pkg, Some("a.b.c"));
    let doc = roxmltree::Document::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let root = doc.root_element();
    assert!(root.has_tag_name(("urn:mpeg:dash:schema:mpd:2011", "MPD")), "{text}");
    assert_eq!(root.attribute("type"), Some("dynamic"));
    assert_eq!(root.attribute("availabilityStartTime"), Some("1970-01-01T00:16:40.000Z"));
    assert_eq!(root.attribute("publishTime"), Some("1970-01-01T00:33:20.000Z"));
    for a in ["minimumUpdatePeriod", "timeShiftBufferDepth", "suggestedPresentationDelay", "minBufferTime"] {
        assert!(root.attribute(a).is_some_and(|v| v.starts_with("PT") && v.ends_with('S')), "{a}: {text}");
    }
    let utc: Vec<_> = children(root, "UTCTiming").collect();
    assert_eq!(utc[0].attribute("schemeIdUri"), Some("urn:mpeg:dash:utc:http-iso:2014"));
    assert_eq!(utc[0].attribute("value"), Some("http://example.test/dash/time"));

    let periods: Vec<_> = children(root, "Period").collect();
    assert_eq!(periods.len(), 1, "{text}");
    assert_eq!(periods[0].attribute("start"), Some("PT0.000S"));
    let sets: Vec<_> = children(periods[0], "AdaptationSet").collect();
    assert_eq!(sets.len(), 2, "video and audio apart: {text}");
    let complete: Vec<_> = pkg.segments.iter().filter(|s| s.full.is_some()).collect();
    assert!(complete.len() >= 3, "{}", complete.len());

    for (lane, set) in sets.iter().enumerate() {
        let video = lane == 0;
        assert_eq!(set.attribute("contentType"), Some(if video { "video" } else { "audio" }));
        let rep = children(*set, "Representation").next().unwrap();
        assert!(num::<u64>(rep, "bandwidth") > 1000);
        if video {
            assert!(rep.attribute("codecs").unwrap().starts_with("avc1."), "{text}");
            assert_eq!((num::<u32>(rep, "width"), num::<u32>(rep, "height")), (256, 144));
        } else {
            assert_eq!(rep.attribute("codecs"), Some("mp4a.40.2"));
            assert_eq!(num::<u32>(rep, "audioSamplingRate"), 48_000);
            assert!(children(rep, "AudioChannelConfiguration").next().is_some());
        }
        let tpl = children(rep, "SegmentTemplate").next().unwrap();
        let l = if video { 'v' } else { 'a' };
        assert_eq!(tpl.attribute("media"), Some(format!("../cam/{l}$Number$.m4s?token=a.b.c").as_str()));
        assert_eq!(tpl.attribute("initialization"), Some(format!("../cam/{l}-init0.mp4?token=a.b.c").as_str()));
        let ts: u32 = num(tpl, "timescale");
        let pto: i64 = num(tpl, "presentationTimeOffset");
        let (start, entries) = timeline(tpl);
        assert_eq!(start, complete[0].msn);
        assert_eq!(entries.len(), complete.len());
        for (s, seg) in entries.iter().zip(&complete) {
            let run = seg.run(lane).unwrap();
            assert_eq!(*s, (run.start, run.dur), "the S of s{} is its tfdt and exact length", seg.msn);
            // ...and that is what the fragments themselves say.
            let tfdt = Moof::decode(&mut &seg.full.as_ref().unwrap()[..]).unwrap().traf[lane]
                .tfdt
                .as_ref()
                .unwrap()
                .base_media_decode_time;
            assert_eq!(tfdt as i64, run.start);
        }
        // The timeline adds up to the segments' own durations, and its
        // offset is the period's start (the stream's first keyframe, which
        // may already have left the window), on this track's clock.
        let total: u64 = entries.iter().map(|e| e.1).sum();
        let want: f64 = complete.iter().map(|s| s.duration()).sum();
        let tol = if video { 1e-6 } else { 0.03 }; // audio frames do not land on video cuts
        assert!((total as f64 / f64::from(ts) - want).abs() < tol, "{total}/{ts} vs {want}");
        let mark = complete[0].period;
        assert_eq!(mark.msn, 0);
        let anchor = mark.anchor as i128 * i128::from(ts) / i128::from(mark.timescale);
        assert!((anchor - i128::from(pto)).abs() <= 1, "{anchor} vs {pto}");
        assert!(entries[0].0 >= pto - i64::from(ts) / 20, "nothing listed before the period starts");
        // Presentation time of the first listed S equals its media time
        // since the first keyframe, i.e. its start on the wall-clock timeline.
        let pres_us = (entries[0].0 - pto) as f64 / f64::from(ts) * 1e6;
        assert!((pres_us - complete[0].start_us as f64).abs() < 30_000.0, "{pres_us} vs {}", complete[0].start_us);
    }
}

#[test]
fn a_republish_opens_a_new_period_after_the_old_one() {
    const GRACE: HlsConfig = HlsConfig { reconnect_grace: Duration::from_secs(10), ..CFG };
    let fx = fixture();
    let mut pkg = Packager::new(GRACE);
    pkg.set_tracks(&fx.tracks);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    feed(&mut pkg, &fx, 0..2, t0);
    pkg.suspend();
    pkg.resume();
    pkg.set_tracks(&fx.tracks);
    // The new encoder starts its clock at zero again, 30 s later.
    feed(&mut pkg, &fx, 0..1, t0 + Duration::from_secs(30));
    feed(&mut pkg, &fx, 1..2, t0 + Duration::from_secs(30));
    let text = mpd(&pkg, None);
    let doc = roxmltree::Document::parse(&text).unwrap();
    let periods: Vec<_> = children(doc.root_element(), "Period").collect();
    assert_eq!(periods.len(), 2, "{text}");
    let start = |p: roxmltree::Node<'_, '_>| -> f64 {
        p.attribute("start").unwrap().trim_start_matches("PT").trim_end_matches('S').parse().unwrap()
    };
    assert_eq!(start(periods[0]), 0.0);
    assert!(start(periods[1]) >= 30.0, "starts at its wall clock: {text}");
    assert_ne!(periods[0].attribute("id"), periods[1].attribute("id"));

    // Each period's numbers continue the msn sequence, and the second
    // period's timeline sits at its own offset (the new clock).
    fn tpl<'a, 'i>(p: roxmltree::Node<'a, 'i>) -> roxmltree::Node<'a, 'i> {
        let set = children(p, "AdaptationSet").next().unwrap();
        let rep = children(set, "Representation").next().unwrap();
        children(rep, "SegmentTemplate").next().unwrap()
    }
    let (n0, e0) = timeline(tpl(periods[0]));
    let (n1, e1) = timeline(tpl(periods[1]));
    assert_eq!(n1, n0 + e0.len() as u64, "{text}");
    assert_eq!(e1[0].0, num::<i64>(tpl(periods[1]), "presentationTimeOffset"));
    assert!(e1[0].0 < e0.last().unwrap().0, "the new publisher's clock restarted");

    // The period's start never moves on later updates, even once its
    // first segment leaves the window.
    let id = periods[1].attribute("id").unwrap().to_owned();
    let before = start(periods[1]);
    feed(&mut pkg, &fx, 2..8, t0 + Duration::from_secs(30));
    let text = mpd(&pkg, None);
    let doc = roxmltree::Document::parse(&text).unwrap();
    let periods: Vec<_> = children(doc.root_element(), "Period").collect();
    assert_eq!(periods.len(), 1, "the old period left the window: {text}");
    assert_eq!(periods[0].attribute("id"), Some(id.as_str()));
    assert_eq!(start(periods[0]), before);
}

#[test]
fn an_ended_stream_stops_asking_for_updates() {
    let fx = fixture();
    let mut pkg = Packager::new(CFG);
    pkg.set_tracks(&fx.tracks);
    feed(&mut pkg, &fx, 0..2, SystemTime::UNIX_EPOCH);
    assert!(mpd(&pkg, None).contains("minimumUpdatePeriod="));
    pkg.end();
    assert!(!mpd(&pkg, None).contains("minimumUpdatePeriod="));
}

struct Reply {
    status: StatusCode,
    content_type: String,
    body: Bytes,
}

async fn get(app: &Router, path: &str) -> Reply {
    let req = Request::get(path).header("host", "caudal.test:8080").body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let content_type = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Reply { status, content_type, body }
}

async fn publish(reg: &Arc<Registry>, name: &str, fx: &Demuxed) -> Publisher {
    let p = reg.publish(name, BufferConfig::default()).unwrap();
    p.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    p
}

async fn wait_mpd(app: &Router, path: &str, reps: usize) -> String {
    let t0 = Instant::now();
    loop {
        let r = get(app, path).await;
        let text = String::from_utf8_lossy(&r.body).into_owned();
        if r.status == StatusCode::OK && text.matches("<Representation ").count() >= reps && text.contains("<S ") {
            assert_eq!(r.content_type, "application/dash+xml");
            return text;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "no MPD; last {}:\n{text}", r.status);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Resolves a SegmentTemplate URL (relative to `/hls/{name}/manifest.mpd`)
/// to a path on the router.
fn resolve(uri: &str) -> String {
    let rel = uri.strip_prefix("../").expect("relative to the manifest");
    format!("/hls/{rel}")
}

#[tokio::test]
async fn every_url_the_mpd_names_is_served_and_decodes() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG, Vec::new());
    let p = publish(&reg, "cam", &fx).await;
    for n in 0..4 {
        for f in fx.looped(n) {
            p.push(f).unwrap();
        }
    }
    let text = wait_mpd(&app, "/hls/cam/manifest.mpd?token=t.k.n", 2).await;
    let doc = roxmltree::Document::parse(&text).unwrap();
    let utc = children(doc.root_element(), "UTCTiming").next().unwrap();
    assert_eq!(utc.attribute("value"), Some("http://caudal.test:8080/dash/time"));
    let time = get(&app, "/dash/time").await;
    assert_eq!(time.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&time.body).ends_with('Z'));

    let period = children(doc.root_element(), "Period").next().unwrap();
    for set in children(period, "AdaptationSet") {
        let video = set.attribute("contentType") == Some("video");
        let rep = children(set, "Representation").next().unwrap();
        let tpl = children(rep, "SegmentTemplate").next().unwrap();
        let init = get(&app, &resolve(tpl.attribute("initialization").unwrap())).await;
        assert_eq!(init.status, StatusCode::OK);
        assert_eq!(init.content_type, if video { "video/mp4" } else { "audio/mp4" });
        let mut buf = &init.body[..];
        let mut moov = None;
        while !buf.is_empty() {
            if let Any::Moov(m) = Any::decode(&mut buf).unwrap() {
                moov = Some(m);
            }
        }
        let moov = moov.expect("moov");
        assert_eq!(moov.trak.len(), 1, "one track per init segment");
        let track_id = moov.trak[0].tkhd.track_id;
        assert_eq!(moov.trak[0].mdia.hdlr.handler, mp4_atom::FourCC::new(if video { b"vide" } else { b"soun" }));

        let (start, entries) = timeline(tpl);
        for (k, (t, d)) in entries.iter().enumerate() {
            let uri = tpl.attribute("media").unwrap().replace("$Number$", &(start + k as u64).to_string());
            assert!(uri.ends_with("?token=t.k.n"), "{uri}");
            let seg = get(&app, &resolve(&uri)).await;
            assert_eq!(seg.status, StatusCode::OK, "{uri}");
            // One track, its tfdt the S@t, its sample durations the S@d.
            let mut buf = &seg.body[..];
            let mut first = None;
            let mut sum = 0u64;
            while !buf.is_empty() {
                let moof = Moof::decode(&mut buf).unwrap();
                let _mdat = mp4_atom::Mdat::decode(&mut buf).unwrap();
                assert_eq!(moof.traf.len(), 1);
                assert_eq!(moof.traf[0].tfhd.track_id, track_id);
                first.get_or_insert(moof.traf[0].tfdt.as_ref().unwrap().base_media_decode_time as i64);
                sum += moof.traf[0].trun[0].entries.iter().map(|e| u64::from(e.duration.unwrap())).sum::<u64>();
            }
            assert_eq!(first, Some(*t), "{uri}");
            assert_eq!(sum, *d, "{uri}");
        }
        // Past the window: gone.
        let stale = get(&app, &format!("/hls/cam/{}0.m4s", if video { 'v' } else { 'a' })).await;
        assert!(start == 0 || stale.status == StatusCode::NOT_FOUND);
    }
    let missing = get(&app, "/hls/cam/v-init7.mp4").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    // MistServer's URL serves the same manifest, and its relative URLs
    // resolve under /dash/ too; nothing but DASH files is served there.
    let alias = get(&app, "/dash/cam/index.mpd").await;
    assert_eq!(alias.status, StatusCode::OK);
    assert_eq!(alias.content_type, "application/dash+xml");
    let tpl_start = String::from_utf8_lossy(&alias.body)
        .split("startNumber=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(get(&app, &format!("/dash/cam/v{tpl_start}.m4s")).await.status, StatusCode::OK);
    assert_eq!(get(&app, "/dash/cam/v-init0.mp4").await.status, StatusCode::OK);
    assert_eq!(get(&app, "/dash/cam/index.m3u8").await.status, StatusCode::NOT_FOUND);
    drop(p);
}

#[tokio::test]
async fn a_family_lists_every_rendition_as_a_representation() {
    let fx = fixture();
    let reg = Registry::new();
    let app = router(reg.clone(), CFG, Vec::new());
    let root = publish(&reg, "abr", &fx).await;
    let low = publish(&reg, "abr+low", &fx).await;
    for n in 0..3 {
        for f in fx.looped(n) {
            root.push(f.clone()).unwrap();
            low.push(f).unwrap();
        }
    }
    let text = wait_mpd(&app, "/hls/abr/manifest.mpd", 3).await;
    let doc = roxmltree::Document::parse(&text).unwrap();
    let period = children(doc.root_element(), "Period").next().unwrap();
    let sets: Vec<_> = children(period, "AdaptationSet").collect();
    let video: Vec<_> = children(sets[0], "Representation").collect();
    let ids: Vec<_> = video.iter().map(|r| r.attribute("id").unwrap()).collect();
    assert!(ids.contains(&"abr") && ids.contains(&"abr+low"), "{text}");
    for r in &video {
        let tpl = children(*r, "SegmentTemplate").next().unwrap();
        let id = r.attribute("id").unwrap();
        assert_eq!(tpl.attribute("media"), Some(format!("../{id}/v$Number$.m4s").as_str()));
        let (start, _) = timeline(tpl);
        let seg = get(&app, &format!("/hls/{id}/v{start}.m4s")).await;
        assert_eq!(seg.status, StatusCode::OK, "{id}");
    }
    assert_eq!(children(sets[1], "Representation").count(), 1, "audio from the lead rendition");

    // A rendition's own manifest is just that rendition.
    let single = wait_mpd(&app, "/hls/abr+low/manifest.mpd", 2).await;
    assert_eq!(single.matches("<Representation ").count(), 2, "{single}");
    assert!(!single.contains("../abr/"), "{single}");
    drop((root, low));
}
