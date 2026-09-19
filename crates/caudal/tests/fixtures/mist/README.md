# MistServer config fixtures

These three `.json` files are hand-written to match MistServer's own
`config.json` shape, not captured from a real MistServer install. The shape
was read from MistServer's source (github.com/DDVTECH/mistserver,
Unlicense) — see the doc comment at the top of
`crates/caudal/src/import_mist.rs` for exact file/line references
(`controller_connectors.cpp`, `controller_streams.cpp`,
`controller_push.cpp`, `controller_storage.cpp`, `controller.cpp`).

- `basic_rtmp_hls.json` — RTMP + HTTP(+HLS) connectors, one `push://`
  stream restricted to an IP, a DVR window.
- `push_and_record.json` — an RTMP connector, `auto_push` with an RTMP
  restream target and a file (record) target, one scheduled push, two
  triggers, and a local user account (password not migrated).
- `rtsp_webrtc_files.json` — RTSP + TSSRT + WebRTC connectors, a connector
  Caudal has no output for (`HTTPTS`), an RTSP pull source, a single-file
  and a folder source (both become channels), and the legacy `autopushes`
  array form (not auto-upgraded by this importer — see the note it
  produces).
