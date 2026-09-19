//! Fuzzes WHIP/WHEP SDP offer handling: `str0m`'s `SdpOffer::from_sdp_string`
//! and `Rtc::sdp_api().accept_offer` as driven by `caudal-webrtc::negotiate`,
//! plus the answer-side `answer_codecs` line scan. This is the first thing
//! an unauthenticated `POST /whip/{name}` or `/whep/{name}` body goes
//! through (`Access` is checked before `negotiate` runs, but the SDP body
//! itself is attacker-controlled either way since a valid token is cheap to
//! obtain when publishing is open).
//!
//! Reached through `caudal_webrtc::fuzz`, a `#[doc(hidden)]` module added
//! only for this. Never touches `engine.rs` (owned by other work): no UDP
//! socket, no peer loop, just offer parsing and answer generation.
//!
//! Input layout: `[op: u8][rest]`. Even `op`: `rest` is a full SDP offer
//! body fed to `negotiate_offer` (a real WHIP/WHEP offer captured from
//! ffmpeg or a browser is a good seed). Odd `op`: `rest` as UTF-8 text fed
//! straight to `answer_codecs`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&op, rest)) = data.split_first() else { return };
    if op % 2 == 0 {
        caudal_webrtc::fuzz::negotiate_offer(rest);
    } else if let Ok(sdp) = std::str::from_utf8(rest) {
        caudal_webrtc::fuzz::answer_codecs(sdp);
    }
});
