//! Stream keys and `ipa` tokens end to end, against the real binary:
//! ffmpeg publishes over RTMP and over SRT with a key in place of the
//! stream name, a wrong key is refused, the keys never show up in the log
//! or the API, and an HLS token bound to an address works only from it.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH, like `e2e.rs`.

// Drives real processes through Unix process groups, like e2e.rs.
#![cfg(unix)]

mod support;

use std::time::Duration;

use jsonwebtoken::{EncodingKey, Header, encode};
use support::{Publisher, Server, have};

const SECRET: &str = "0123456789abcdef0123456789abcdef-e2e";
const RTMP_KEY: &str = "sk_studio_9f8e7d6c5b4a";
const SRT_KEY: &str = "sk_backup_1a2b3c4d5e6f";
const WRONG_KEY: &str = "sk_studio_9f8e7d6c5b4b";

fn enabled() -> bool {
    if std::env::var("CAUDAL_E2E").is_err() {
        eprintln!("SKIP: set CAUDAL_E2E=1 to run end-to-end tests");
        return false;
    }
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not on PATH");
        return false;
    }
    true
}

fn token(claims: serde_json::Value) -> String {
    encode(&Header::default(), &claims, &EncodingKey::from_secret(SECRET.as_bytes()).unwrap()).unwrap()
}

fn exp() -> i64 {
    jsonwebtoken::get_current_timestamp() as i64 + 600
}

#[test]
fn keys_publish_to_their_streams_and_stay_secret() {
    if !enabled() {
        return;
    }
    // Tokens required for both: a key is the only way in without one.
    let s = Server::start_logged(&format!(
        "\n[auth]\nsecret = \"{SECRET}\"\npublish = true\nplay = true\n\
         [[auth.stream_keys]]\nkey = \"{RTMP_KEY}\"\nstream = \"studio\"\n\
         [[auth.stream_keys]]\nkey = \"{SRT_KEY}\"\nstream = \"backup\"\n"
    ));

    // RTMP: the key in OBS's "Stream Key" lands on `studio`.
    let _rtmp = Publisher::rtmp(&s.rtmp_url(RTMP_KEY), 40);
    s.wait_until("/api/v1/streams/studio", Duration::from_secs(20), |b| b.contains("\"h264\""));
    assert_eq!(s.get(&format!("/api/v1/streams/{RTMP_KEY}")).unwrap().0, 404, "nothing under the key");

    // One character off: just a name, and without a token it is refused.
    let _wrong = Publisher::rtmp(&s.rtmp_url(WRONG_KEY), 10);

    // SRT: `streamid=publish/<key>` lands on `backup`.
    if have("srt-live-transmit") {
        let _srt = Publisher::srt(&s.srt_url(SRT_KEY), 30);
        s.wait_until("/api/v1/streams/backup", Duration::from_secs(20), |b| b.contains("\"h264\""));
    } else {
        eprintln!("SKIP (SRT part): srt-live-transmit not on PATH");
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(s.get(&format!("/api/v1/streams/{WRONG_KEY}")).unwrap().0, 404, "wrong key refused");
    let (_, streams) = s.get("/api/v1/streams").unwrap();
    assert!(!streams.contains(WRONG_KEY) && !streams.contains(RTMP_KEY), "{streams}");

    // `ipa`: an HLS token bound to 127.0.0.1 plays from here; one bound to
    // another address does not; an old token without `ipa` still does.
    let here = token(serde_json::json!({"sub": "studio", "act": "play", "exp": exp(), "ipa": "127.0.0.1"}));
    let there = token(serde_json::json!({"sub": "studio", "act": "play", "exp": exp(), "ipa": "10.9.9.9/32"}));
    let plain = token(serde_json::json!({"sub": "studio", "act": "play", "exp": exp()}));
    s.wait_until(&format!("/hls/studio/index.m3u8?token={here}"), Duration::from_secs(20), |b| b.contains("#EXTM3U"));
    assert_eq!(s.get(&format!("/hls/studio/index.m3u8?token={there}")).unwrap().0, 403);
    assert_eq!(s.get(&format!("/hls/studio/index.m3u8?token={plain}")).unwrap().0, 200);
    // The publish-only key is not a play credential.
    assert_eq!(s.get(&format!("/hls/{RTMP_KEY}/index.m3u8")).unwrap().0, 404);

    // The API lists which streams have keys, never the keys.
    let (code, keys) = s.get("/api/v1/stream-keys").unwrap();
    assert_eq!(code, 200);
    assert!(keys.contains("\"studio\"") && keys.contains("\"backup\"") && keys.contains("****"), "{keys}");
    assert!(!keys.contains(RTMP_KEY) && !keys.contains(SRT_KEY), "{keys}");

    let (_, metrics) = s.get("/metrics").unwrap();
    let srt_hits = if have("srt-live-transmit") { 1 } else { 0 };
    assert!(
        metrics.contains(&format!(
            "caudal_stream_key_requests_total{{action=\"publish\",result=\"hit\"}} {}",
            1 + srt_hits
        )),
        "{metrics}"
    );
    assert!(!metrics.contains(RTMP_KEY) && !metrics.contains(SRT_KEY));

    // The log, at debug: the publishes are there under their streams, the
    // keys are not. The wrong key is just a name that was refused.
    let log = s.log();
    assert!(log.contains("studio"), "the log was captured ({} bytes)", log.len());
    for key in [RTMP_KEY, SRT_KEY] {
        assert!(!log.contains(key), "a stream key reached the log:\n{log}");
    }
}
