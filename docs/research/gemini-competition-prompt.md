# Prompt for Google Gemini (Deep Research)

Paste everything below the line.

---

You are a market and technology analyst for live video infrastructure. Research thoroughly and cite a source (URL + date seen) for every number, price and non-obvious claim. If you cannot verify something, write "unverified". Never invent prices, star counts or market sizes. Today is September 2026; prefer sources from 2025–2026.

## The product being analysed: Caudal

Caudal is an open-source (MIT OR Apache-2.0) live media server written in Rust by Puny.bz (a small software company in Puerto Rico). It is a from-scratch rewrite of MistServer (C++). Current state:

- **Footprint:** one static binary, about 3 MB with the web UI included; about 14 MB of RAM with one live 720p stream. Runs on Linux x86_64 and arm64 (Raspberry Pi class), macOS.
- **Ingest:** RTMP and Enhanced RTMP (H.264, HEVC, AV1), SRT (with AES), WebRTC WHIP (from browsers and ffmpeg 8+), RTSP camera pull (in progress).
- **Output:** LL-HLS / CMAF (passes Apple's mediastreamvalidator; measured about 1.1–1.7 s behind live in Chrome and Firefox; Safari's native player reaches about 0.4–0.85 s over HTTP/2 when it joins in low-latency mode), WebRTC WHEP (sub-second), Media over QUIC over WebTransport (Chrome and Firefox), SRT out (pull and push), RTSP server (in progress).
- **Operations:** recording to disk and S3-compatible storage, VOD playback, MP4 clips by time range; JWT/JWKS tokens for publish and play; signed webhooks; HTTPS with HTTP/2 and automatic Let's Encrypt; Prometheus metrics; embedded Material Design 3 web UI; adaptive-bitrate transcoding via external ffmpeg or a pure-Rust H.264 encoder (in progress).
- **Planned:** clustering (origin-edge), a pure-Rust implementation of Open Media Transport (OMT, an open alternative to NDI), WASM plugins, per-viewer quality-of-experience analytics, backup-source failover, SCTE-35 and captions, multi-tenancy.
- **Positioning so far:** memory-safe (MistServer and other C/C++ servers have had crash and CVE-class bugs), small and self-hostable, every modern protocol in one binary.

## What to research

1. **Competitive landscape.** Compare Caudal against: MediaMTX, SRS, OvenMediaEngine, MistServer, Ant Media Server (Community and Enterprise), Red5 / Red5 Pro, Wowza Streaming Engine, Flussonic, Nimble Streamer, nginx-rtmp, LiveKit, mediasoup/Janus (only for broadcast use), Dolby OptiView (Millicast), Mux, Cloudflare Stream/Calls, AWS IVS and Elemental, Livepeer. For each: license or pricing (published prices with source), protocols, latency claims, strengths, weaknesses and common complaints (GitHub issues, Reddit r/VideoEngineering, reviews).
2. **Differentiation.** Where is Caudal genuinely different or better, with evidence? Where is it weaker today?
3. **Gaps we have not considered.** Features customers ask for or pay for that are NOT in the lists above. Rank them by evidence of demand (issue reactions, forum threads, vendor headline features). Examples to check, not to assume: multistreaming to YouTube/Twitch/Facebook, stream health alerts, captions and translation with AI, watermarking/forensic marking, geo-blocking, analytics dashboards, scheduling and playlists, mobile publishing SDKs, OBS integration, Kubernetes operators, Raspberry Pi appliances, compliance features (accessibility laws, public-meeting records).
4. **Markets and segments.** Size and growth of live-streaming infrastructure (cite reports). Which segments self-host and why: houses of worship, education, municipal and government meetings, local TV and radio, high school and college sports, security cameras, events, auctions and betting, telemedicine, IRL streamers, broadcast contribution. Include a Latin America, Caribbean and Puerto Rico / Spanish-language section: local providers, price sensitivity, needs.
5. **Memory-safety angle.** Government guidance favoring memory-safe languages (CISA, NSA, ONCD, EU) with dates and links, and whether it affects procurement of media infrastructure.
6. **Business models.** How open-source media servers make money (open core, support, hosted cloud, licensing, marketplace appliances), with real examples and, if public, revenue or funding.

## Output format

1. Executive summary (10 bullets max).
2. Competitor comparison table (one row per product).
3. "Where Caudal wins" and "Where Caudal is behind", each with evidence.
4. Top 10 improvements Caudal has not considered, ranked, each with the evidence of demand and a one-line suggestion for how to build it.
5. Top 5 target segments with a positioning statement each (3 lines max) and what they would pay for.
6. Risks (technical, market, legal/licensing).
7. Sources list.
