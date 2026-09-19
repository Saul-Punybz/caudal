//! Fuzzes RTSP server-side request parsing: `rtsp-types`'s wire parser plus
//! `caudal-rtsp`'s own URI/query parsing (`server::parse_uri`) and
//! `Transport` header selection (`server::choose_transport`), the code that
//! runs on every request from any RTSP client before touching a registry or
//! stream, over both plain TCP and RTSPS.
//!
//! Reached through `caudal_rtsp::fuzz::route_request`, a `#[doc(hidden)]`
//! module added only for this: `server` is a private module.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    caudal_rtsp::fuzz::route_request(data);
});
