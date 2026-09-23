# M12 design (Plan agent, 23 Sep 2026) — condensed; nothing built or measured

## Caudal today
- Stream = TrackInfo (codec, timescale, init) + Frame (dts/pts/keyframe/Bytes, video AVCC). caudal-core/src/media.rs:89-150; codecs media.rs:23-39 (Pcm declared, unused). NO raw-video path.
- Ingest: Registry::publish(name, BufferConfig) registry.rs:116 -> Publisher::set_tracks/push (sync). Examples caudal-rtsp/src/pull.rs:143, caudal-srt connection.rs:194, caudal-rtmp session.rs:202.
- Outputs: Registry::subscribe/get_or_demand -> Event {Frame,TracksChanged,Lagged,Cue,End} stream.rs:57-72; subscribe_internal stream.rs:345.
- Transcode: H.264/H.265 only (caudal-transcode lib.rs:199-230), renditions `<name>+<label>`; ffmpeg subprocess (MPEG-TS/MKV stdin, caudal-ts back; ffmpeg.rs:170-190,436) or rusty_h264 in-process thread (rusty.rs:62,198; H.264 decode; no audio encode).
- No pure-Rust AAC/Opus encoder, no AAC decoder. opus-decoder 0.1.1 in caudal-captions.
- rusty_h264 1080p encode ~24 fps/thread extrapolated -> not 1080p60. Decode ~0.27 core @1080p60 (unconfirmed).

## MVP
1. OMT pull ingest: name/URL -> receive -> VMX decode (Rust) -> NV12/I420 + f32 PCM -> ffmpeg (H.264/AAC, MPEG-TS back through caudal-ts) -> publish `<stream>`. Pipe ~187 MB/s at 1080p60; container for raw into ffmpeg: MKV V_UNCOMPRESSED (unconfirmed) or NUT fallback.
2. OMT output of any H.264 stream/rendition: rusty_h264 decode -> I420 -> Sender::send_video (vmx-codec accepts I420, frame.rs:47-48); audio AAC via ffmpeg audio pipe (-f adts -i - -f f32le -), Opus via opus-decoder; others rejected with a log.
3. Tally/quality, API, metrics.
Phase 2: VMX passthrough Codec::Vmx (memory: 600 Mbit/s x 15 s = 1.1 GB > 256 MB max_bytes), OMT relay, VMX MKV record, HEVC/AV1 output, 10-bit/alpha, discovery server mode, UI picker, rusty_h264 encode small sources.

## crates/caudal-omt
- Modules ingest, output, feed (ffmpeg pipe; extract shared helpers from caudal-transcode), time, audio, discovery, lib.
- start_pulls/PullHandle::reload, start_outputs/OutputHandle::reload mirroring caudal-rtsp lib.rs:65-75; wired in crates/caudal/src/subsystems.rs like rtsp_pull (:353).
- Config TOML serde deny_unknown_fields (crates/caudal/src/config.rs:1146-1148, Config :176):
  [omt] ffmpeg; [[omt.pull]] stream, source|url, quality, video_kbps; [[omt.output]] stream, name, quality.
- Threading: std::thread per pull (recv_timeout 100 ms + AtomicBool stop; VMX decode there; tokio mpsc try_send, drop when full — VMX is all-intra). std::thread per output fed by blocking_recv. Drop Receiver/Sender inside spawn_blocking (Drop joins).
- Time: OMT 100 ns; video tb 90000 (ts*9/1000 i128 rounding); audio tb = sample rate, pts = origin + sample count. Shared origin = first ts of either track. Jump >1 s or backwards => discontinuity, re-anchor to last pts + interval. Output: micros*10.
- Tally: program if stream has viewers (StreamStats), preview if internal subscriber; resend on change <=1/s. Output exposes Sender::tally()/video_receivers(); Stream::set_output_viewers("omt", n).
- API: GET /api/v1/omt/sources; omt fields on /api/v1/streams/{name} (api.rs:281-282). Metrics (metrics.rs): caudal_omt_frames_in_total, _frames_dropped_total{reason}, _reconnects_total, _receivers, _tally{state}, _encode_seconds.

## Tests
- Loopback: Sender -> pull (codecs, monotonic pts, A/V <1 frame, reconnect re-anchor); output -> Receiver PSNR vs pattern. mDNS tests #[ignore] on CI.
- Fuzz: omt_frame_map, vmx_decode (mandatory; OMT repo lacks a VMX decoder fuzz target). fuzz-smoke job ci.yml:225-236.
- Interop locally with interop/libomtnet-harness; optional GH workflow with setup-dotnet (unconfirmed).
- bench.yml 1080p60 CPU; soak.yml 60 min OMT churn (<1 MB/h). Never on laptop.

## Deps
- git+rev to OMT during dev; crates.io 0.2.x before tagging. [profile.dev.package.vmx-codec] opt-level=3. MSRV/edition fine. mdns-sd 0.21 new.

## Needs from OMT crates (not in the 4 branches)
1. Send pre-encoded VMX1 frame (PROTOCOL V2/P4). 2. Encoder threads in Sender (sender.rs:236-270). 3. Shared Discovery across senders + interface selection. 4. send_audio -> Result (sender.rs:352-368 panics). 5. Receiver/sender stats (bytes, drops, reconnects, peers). 6. VMX decoder fuzz target. 7. Non-blocking/timed Drop.

## Tasks
T1 ingest/time/audio (owns lib.rs); T2 output/discovery; T3 feed.rs + extract ffmpeg pipe from caudal-transcode {mkv,ffmpeg}.rs; T4 caudal/src/{config,subsystems,api,metrics}.rs + root Cargo.toml; T5 fuzz/, workflows, bench/.

## Risks
Entry condition (real equipment) not met — Saul's go-ahead 23 Sep. v0.1 soak RSS blocker open; OMT adds churn. x264 1080p60 cost + pipe copy unmeasured. ffmpeg MKV raw input unconfirmed. mDNS on runners unconfirmed. AAC->OMT needs ffmpeg (symphonia is MPL-2.0 → license decision). Competing Rust OMT crates decision open.
