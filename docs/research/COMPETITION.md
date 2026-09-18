# Caudal: competition, differentiation, gaps

Compiled 18 Sep 2026 from five Haiku research agents (open-source servers,
commercial/cloud, SFUs + MoQ, user pain points, markets) and checked by hand.
Claims marked **verified** were checked against the source; everything else
is as reported and **unverified**. A Gemini Deep Research run
(`gemini-competition-prompt.md`) was received the same day; see the cross-check at the end.

## Corrections to the agents (verified)

| Agent claim | Reality |
|---|---|
| "LiveKit was acquired by AWS in 2024" | No evidence; LiveKit is independent and sells on AWS Marketplace. |
| "CISA mandates eliminating memory-safety risks by 2026" | Non-binding guidance: software for critical infrastructure should publish a memory-safety roadmap by 1 Jan 2026 (CISA/FBI *Product Security Bad Practices*, 2024). No enforcement. |
| "ADA Title II deadline April 2026" | DOJ extended it in April 2026: **26 Apr 2027** (entities ≥ 50K people), **26 Apr 2028** (smaller). WCAG 2.1 AA; video needs synchronized captions. |
| "Media over QUIC is unique to Caudal" | False: MediaMTX supports MoQ publish/read (mediamtx.org/docs/read/moq). |
| "MediaMTX CPU scaling is broken" | Overstated: the cited issues (#4963, #4678) are closed. |
| "Caudal lacks DVR / WHEP" | False: 50 s DVR buffer + recording/VOD (batch 6); WHEP since M5. |
| Safari 26.4 supports WebTransport | **True** (macOS + iOS, Mar 2026, webkit.org). MoQ may work in Safari; to test. |

## Landscape (open source)

| Project | Lang | License | Stars (18 Sep 2026) | Notes |
|---|---|---|---|---|
| SRS | C++ | MIT | 29.3K | broad protocols incl. GB28181; clustering |
| MediaMTX | Go | MIT | 20.2K | the reference "one binary" server; has MoQ; 54 MB binary / 38 MB idle RSS (measured by us, 17 Sep) |
| nginx-rtmp | C | BSD-2 | 14.0K | old, still popular for RTMP→HLS |
| Owncast | Go | MIT | 11.5K | self-hosted Twitch-like, RTMP in only |
| Ant Media CE | Java | (unclear) | 4.7K | community edition limited; money in Enterprise |
| Red5 | Java | Apache-2.0 | 3.4K | WebRTC in paid Pro |
| OvenMediaEngine | C++ | AGPL-3.0 | 3.3K | sub-second WebRTC + LL-HLS, embedded ABR; commercial license |
| xiu | Rust | MIT | 2.3K | Rust peer; stale since Mar 2026 |
| MistServer | C++ | Unlicense | ~0.5K (repo) | the codebase we rewrote |

Commercial (prices as reported by agents, **unverified**): Wowza Streaming Engine ~$195/mo per instance, Flussonic ~$169/mo, Nimble ~$50/mo per server, Ant Media Enterprise ~$109/mo, Red5 Pro $30–$300/mo, AWS IVS ~$0.07–0.14 per viewer-hour, Mux and Cloudflare usage-based, Dolby OptiView custom.

## Where Caudal is genuinely different (verified by our own tests)

1. **One small, memory-safe binary with every modern protocol**: RTMP/E-RTMP, SRT, WHIP, RTSP (in progress) in; LL-HLS, WHEP, MoQ, SRT, RTSP (in progress) out; recording/VOD/clips; tokens; webhooks; HTTPS/HTTP2/ACME; UI. ~3 MB vs MediaMTX's 54 MB.
2. **Apple-validated LL-HLS** with rendition reports coming (batch 7) and measured latency: Chrome 1.66 s, Firefox 1.10 s steady; Safari 0.4–0.85 s over HTTP/2 when it joins in low-latency mode.
3. **Permissive license** (MIT OR Apache-2.0) where the closest feature-peer, OvenMediaEngine, is AGPL.
4. **MoQ + WHEP + LL-HLS side by side** in the same player UI (MoQ itself is also in MediaMTX).
5. **Planned: the first pure-Rust Open Media Transport** (open NDI alternative), MIT/Apache.

## Where Caudal is behind

Clustering / origin-edge (SRS, MistServer, Ant Enterprise, Red5 Pro have it); DASH output; multi-DRM (Widevine/FairPlay via partners); maturity and community (stars, production users); ABR still in progress; RTSP in progress; no Kubernetes story; the admin API and UI have no login yet.

## Improvements not considered before (ranked)

| # | Improvement | Why (evidence) |
|---|---|---|
| 1 | **Automatic live captions** (local Whisper-class speech-to-text → WebVTT in LL-HLS, CEA-608 in TS; Spanish + English) | ADA Title II requires synchronized captions for state/local government video by Apr 2027/2028 (verified). Municipal meetings are a natural segment, incl. Puerto Rico. No open-source server offers it built in (to verify). |
| 2 | **Multistreaming to YouTube / Twitch / Facebook** (RTMP push per stream, with per-destination status) | The most common creator request (commercial Restream/StreamYard exist); we have SRT push, not RTMP push. |
| 3 | **Admin login for the UI and API** (OIDC/SSO, LDAP optional) | The admin surface is open today; MediaMTX has an open LDAP request (#5403). Needed before any public deployment. |
| 4 | **Stream health alerts** (no keyframes, bitrate drop, publisher gone) to webhooks, email, Slack | Asked in HN/MistServer threads; we already emit start/end webhooks. |
| 5 | **Recording schedules** ("record Mon–Fri 9–5") | MediaMTX #4599 (open). Clips via API are already done. |
| 6 | **MoQ on Safari 26.4+** | WebTransport reached Safari 26.4 (verified); test WebCodecs + our player and lift the UI's Safari gate. |
| 7 | **Geo-blocking / IP allow-lists** per stream | Territorial rights for sports and events; tokens exist, GeoIP does not. |
| 8 | **Helm chart + Kubernetes operator** | Enterprise ask; planned Helm chart only. |
| 9 | **Raspberry Pi appliance image** (flash-and-go for churches, schools, cameras) | arm64 static builds exist; packaging is the gap. |
| 10 | **DASH output** | Competitors ship it; low priority (hls.js/Safari cover most), `dash-mpd` crate exists. |

## Segments to target first

1. **Municipal and government meetings**: captions (ADA Title II), recording as public record, tokens; memory-safety guidance as a procurement argument (non-binding).
2. **Houses of worship and local radio/TV in Puerto Rico / LatAm**: price-sensitive; one small binary, Spanish UI and captions. Market sizes reported by agents are unverified.
3. **Surveillance / RTSP cameras**: pull cameras, record, re-serve over WebRTC/HLS (RTSP lands in batch 7).
4. **Low-latency commerce**: auctions, betting, live shopping over WHEP/MoQ.
5. **Broadcast contribution**: SRT in/out now, OMT later.

## Business models seen (as reported)

Open core + cloud (Grafana: >$400M ARR reported Sep 2025), sponsorship + consulting (OvenMediaEngine), Enterprise editions (Ant Media, Red5 Pro), hosted cloud on top of open source (LiveKit Cloud). For Puny.bz: support and managed hosting for the segments above; captions and multistreaming as paid managed add-ons are the most natural first offers.

## Gemini Deep Research cross-check (received 18 Sep 2026)

**Agreement with the Haiku study (strong signal, two independent sources):**
multistreaming to YouTube/Twitch/Facebook is the #1 gap; stream health alerts;
AI captions; clustering; geo-blocking; Helm/Kubernetes; Raspberry Pi appliance.

**Gemini claims corrected (verified):**

| Gemini | Reality |
|---|---|
| "Strict mandates" by CISA/ONCD; position as "CISA-compliant" | Non-binding guidance. Do not say "CISA-compliant". Say "memory-safe, aligned with CISA Secure by Design guidance". |
| MistServer is GPL | Unlicense (public domain), per the repo's `UNLICENSE`. |
| Caudal needs external ffmpeg to restream | Caudal pushes over SRT natively; RTMP push to platforms is the actual gap. |
| An OBS plugin is needed for WHIP | OBS 30+ ships a native WHIP output; a guide / "copy WHIP URL + token" in the UI is enough. |
| Market $21.1B (2025) → $24.4B (2026) → $70.7B (2033) | Contradicts the Haiku figure attributed to the same publisher ($43.3B → $91.7B). Neither is used until read from the original report. |

**New from Gemini, adopted into the backlog:** SCTE-35 passthrough moves up
(FAST channels); forensic watermarking; web/mobile player SDKs; legal caution:
describe OMT on its own merits, don't market it with the NDI trademark.

**New from Gemini, NOT adopted:** 24/7 linear channels from VOD playlists:
that is ANTENA787's job (Saul's playout project); Caudal can ingest its output.

**Known licensing trade-off (decision already made by Saul: MIT OR Apache-2.0):**
a permissive license allows clouds to host Caudal commercially without
contributing back; AGPL + commercial licensing (OvenMediaEngine's model)
would prevent that at the cost of adoption. Recorded as an accepted risk.
