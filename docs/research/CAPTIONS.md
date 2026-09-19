# Live captions: design note

Batch 10 (PLAN.md). Goal: with `[captions]` enabled for a stream, LL-HLS
viewers get live Spanish or English subtitles (a WebVTT rendition in the
multivariant playlist), produced on the server with no cloud service, a few
seconds behind the audio. ADA Title II deadlines: 26 Apr 2027 / 26 Apr 2028.

Code: `crates/caudal-captions` (engine, audio, chunking, cues),
`crates/caudal-core/src/captions.rs` (the `CaptionSource` seam),
`crates/caudal-hls/src/vtt.rs` + `lib.rs` (the rendition),
`crates/caudal/src/captions.rs` (config, `/metrics`, `fetch-model`).

## Engine: candle, not whisper.cpp

Whisper (OpenAI) run by `candle-transformers` 0.9.2 (pure Rust; Metal on
macOS, CPU elsewhere). `whisper-rs` (whisper.cpp bindings, C++) was the
fallback if candle could not keep up with `small`/`base` on an M4. It keeps
up with all three sizes, so no C++ dependency.

candle is pinned to **0.9.2**: 0.10 and 0.11 make `candle-core` depend on
`tokenizers` with the `onig` feature, a C regex library (Oniguruma) built by
`cc`. We also do not use the `tokenizers` crate at all: decoding Whisper's
byte-level BPE needs only the vocabulary (`src/tokenizer.rs`), and the
log-mel front end is ours too (`src/mel.rs`, librosa's Slaney filters, a
`rustfft` STFT), single-threaded so the thread cap below really caps.

Weights are read into memory with `VarBuilder::from_buffered_safetensors`,
not mmapped: mmap is `unsafe` in candle, and the repo has no `unsafe`.

### Measured (19 Sep 2026, Apple M4, release build, `threads = 4`)

Speech: original text (not copyrighted material) spoken by macOS `say -v
Paulina` (es, 28.3 s) and `say -v Samantha` (en, 23.5 s), 16 kHz. The audio
goes through the same chunker as live audio (cuts at pauses, 1.5-5 s
chunks). RTF = inference time / audio time; below 1 keeps up. Word accuracy
= 1 - WER against the script. Reproduce with
`cargo run --release -p caudal-captions --example transcribe -- <model_dir>/whisper-<m> speech.wav --script speech.txt --language es --device metal|cpu`.

| model | size | RTF Metal | RTF CPU (4 thr) | worst chunk (CPU) | accuracy es | accuracy en |
|---|---|---|---|---|---|---|
| tiny | 151 MB | 0.040 | 0.113-0.120 | 0.19 | 95.9% | 98.6% |
| base | 290 MB | 0.080 | 0.24 | 0.53 | 87.8% | 100% |
| small | 967 MB | 0.22 | 0.77-0.84 | 1.36 | 98.6% | 100% |

- One sample per language, synthetic voices: accuracy numbers are
  indicative, not a benchmark. base's Spanish score is lower than tiny's on
  this sample because one chunk boundary fell inside "se acerca" and base
  "heard" a phrase there; the rest is word-for-word.
- `small` on CPU is close to 1 (worst chunk 1.36): on a machine without
  Metal use `base` or `tiny`, or more threads.
- Encoder input: the full 30 s window (the shape Whisper was trained on)
  even for a 2 s chunk. Feeding only the frames the chunk needs is 2-3x
  faster but tiny and base then produce garbage (WER > 90% in English):
  measured with `--short-window`, kept only as an experiment flag.
- Debug test builds are usable because the model crates are built with
  `opt-level = 3` in the dev profile (workspace `Cargo.toml`); the engine
  test measured RTF 0.05 for tiny on Metal in a debug build.

## Pipeline

```
Subscriber (tokio task) --bounded 512 AUs--> decoder thread per stream
  AAC: rusty_aac (Apache-2.0)      \
  Opus: opus-decoder (MIT/Apache)  -> 16 kHz mono -> Chunker -> job queue (4)
                                                                  |
                              one engine thread + rayon pool (`threads`)
                                                                  |
                                   Whisper greedy decode -> cues -> CueStore
```

- **Audio** is decoded in process, in pure Rust. The existing decoder path
  (`caudal-transcode`) is an ffmpeg child process; captions must not need
  ffmpeg. `symphonia` (MPL-2.0) is excluded by `deny.toml`, so AAC-LC uses
  `rusty_aac` (same Remade-With-Rust family as `rusty_h264`, already used)
  and Opus uses `opus-decoder`, which decodes straight to 16 kHz mono. AAC
  goes through our windowed-sinc resampler (`audio.rs`, tested for passband
  level and aliasing at 48/44.1/32/22.05 kHz).
- **Never blocks ingest.** The stream task `try_send`s access units; when
  the decoder queue is full the frame is dropped and counted
  (`caudal_captions_dropped_audio_seconds_total`). Chunks go to the engine
  with `try_send` too; a chunk that does not fit, or that waited more than
  10 s, is dropped and counted (`..._dropped_chunks_total`). Measured in the
  in-process pipeline test and the e2e test: 0 dropped.
- **Chunking** (`chunk.rs`): a chunk ends at a pause of 300 ms once it has
  1.5 s, or at the quietest 100 ms of its last second when it reaches 5 s.
  Chunks that never rise above the silence threshold are never sent to the
  model (saves CPU and avoids Whisper "hearing" words in silence). Whisper's
  own no-speech rule (`no_speech_prob > 0.6` and `avg_logprob < -1`) drops
  what gets through; a repetition guard stops decoding loops.
- **Resource guard.** Inference runs only inside one rayon pool of exactly
  `threads` threads (1-64), one job at a time, so captioning never uses
  more than `threads` cores for all streams together; `max_streams` caps
  how many streams share them. Per stream it also costs one light decoder
  thread. Size it with the table above: a stream needs about RTF x
  `threads` cores' worth of time; the queue drops (and counts) the rest.
- **Crash policy.** Inference is wrapped in `catch_unwind`; after a panic
  the worker restores its model from a pristine clone and keeps going
  (`caudal_captions_engine_panics_total`), and the release profile keeps
  `panic = "unwind"`. A missing or bad model is logged with the fetch
  command and the server runs on without captions. NOT covered: a crash
  below Rust (a Metal driver abort) would take the process down, which is
  why `device = "cpu"` exists; running inference in a child process like
  ffmpeg is possible later if that ever shows up.

## Timing: cues start at the live edge

A chunk's text exists only after the chunk ends and is transcribed. By then
players already hold the media where the words were spoken, so a cue placed
back at the speech would never be seen. Each chunk's cues start at the
stream's live edge when the text is ready (`Stream::newest_micros`), or
after the previous chunk's reading time if that is later, and share the
chunk's speech duration by length (42-character lines, two per cue, at
least 1 s each). Viewers read the words a few seconds after they hear them,
like a live stenographer's captions. Measured in the e2e test: first cue at
5.3 s media time for speech that starts at 0 s (the first chunk is ~4.6 s
long), text placed 0.3-0.4 s after its chunk ended (`caudal_captions_latency_seconds`).

The last cue of a chunk stays up at least one segment plus a second
(`[hls] segment_ms + 1000`): subtitle segments are published when the media
segment completes, and a player at the live edge may fetch one a little
after the cue's start time; hls.js then still shows it for the rest of its
span.

## LL-HLS rendition

- Multivariant: `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="cc",NAME="Español
  (auto)",LANGUAGE="es",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,URI="../<root>/subs.m3u8"`
  and `SUBTITLES="cc"` on every variant. Captions belong to the family root
  (`main`); renditions (`main+480p`) never match a `[[captions.stream]]`
  glob and share the root's track. `language = "auto"` omits `LANGUAGE`.
  No `wvtt` in `CODECS`: the segments are plain WebVTT files, not fMP4; the
  validator reports the codec as `Other: wvtt` and asks for nothing more.
- Subtitle playlist: the same segments as `index.m3u8` (msn, `EXTINF`,
  discontinuities, `PROGRAM-DATE-TIME`, `EXT-X-SERVER-CONTROL`,
  `EXT-X-PART-INF`), each `c{msn}.vtt`, rendered once when the media
  segment (or part) closes, under the packager lock, so a listed file never
  changes and never is missing. Blocking reload works as for the main
  playlist.
- **Parts: only for Apple's media stack.** Apple's validator requires every
  rendition of an LL-HLS stream to be low-latency: without parts it reports
  MUST -50094 (no `EXT-X-PART-INF`), -50095 (no `EXT-X-SERVER-CONTROL`),
  and, with both but no `EXT-X-PART`, -50096 (`HOLD-BACK`, still reported
  with `HOLD-BACK` present; tried 6.000, 6.001, 10.000). With parts
  (`c{msn}.p{i}.vtt`, `INDEPENDENT=YES`, preload hint) it reports nothing
  new. But hls.js 1.7.3 stalls on subtitle parts: it loads the first parts
  of the live segment and then never finishes the fragment (browser check
  below, twice, also through a proxy that listed only complete segments'
  parts). So `subs.m3u8` lists parts when the request's user agent is
  `AppleCoreMedia` (AVFoundation: Safari's native player, iOS/tvOS apps,
  and Apple's validator) and complete segments only for everyone else. Cue
  text and times are identical either way. This is user-agent selection;
  if hls.js learns subtitle parts, drop it.
- `X-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000` in every file: local
  WebVTT time is media time, and fMP4 time runs 10 s ahead of it
  (`SHIFT_SECS`). The same map in every file matters: with a per-segment
  `LOCAL`, hls.js's float arithmetic put the copies of a cue that spans two
  segments 1-10 ms apart and showed both. NOT verified: streams running
  longer than 2^33 ticks (26.5 h) of fMP4 time, where MPEG-TS-style
  rollover handling in players applies.
- Cue text is escaped (`&`, `<`, `>`) and cannot end a cue early.

## Verification

- Unit: mel filters and spectrogram, tokenizer, chunker, resampler, cue
  layout and store, WebVTT segmenting and timing, `X-TIMESTAMP-MAP` math,
  subtitle playlist tags (with and without parts, discontinuities,
  `ENDLIST`), multivariant tags, config validation.
- Engine (real model, gated on `CAUDAL_WHISPER_MODELS`): Spanish and
  English speech, WER, RTF, language detection, silence.
- In-process pipeline: speech encoded to AAC 48 kHz, published at 4x real
  time, cues on the stream's clock, recall 98%.
- e2e (`crates/caudal/tests/e2e.rs`, `live_captions_reach_ll_hls_as_webvtt`):
  ffmpeg publishes the speech over RTMP; master lists the rendition; the
  WebVTT segments hold 98% of the script's words (tiny); timing checks;
  Apple `mediastreamvalidator` 1.26: only the two MUSTs already known for
  every LL-HLS stream here (-50120 HTTP/2, -50125 rendition reports).
- Browser (manual, Playwright Chromium + hls.js 1.7.3 against a live
  server, model `base`): the video element has a text track "Español
  (auto)", `es`; with it showing, cues arrive continuously and `activeCues`
  follows the speech; screenshot shows the caption over the video.
- CI: `espeak-ng` makes the speech on Linux (macOS uses `say`), and
  `caudal captions fetch-model tiny` fetches the model into a cached
  directory, so the tests run rather than SKIP (CI fails on SKIP).

## Not done / not verified

- CEA-608 captions in MPEG-TS (PLAN.md lists it next to WebVTT): not
  built; Caudal has no TS output that carries them yet.
- Safari native playback of the rendition (with parts) was not tried; only
  the validator exercised the parts path.
- The Linux CI path (espeak-ng voice, CPU, debug build) was not run
  locally: espeak-ng is not installed on the dev machine. Its thresholds
  are looser (WER <= 60%, recall >= 50%).
- Static musl builds with candle were not built locally (no zig).
- Speaker changes, music, noisy audio, and languages other than es/en:
  not measured.
