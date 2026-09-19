# Live captions: design note

Batch 10 (PLAN.md). Goal: with `[captions]` enabled for a stream, LL-HLS
viewers get live Spanish or English subtitles (a WebVTT rendition in the
multivariant playlist), produced on the server with no cloud service, a few
seconds behind the audio. ADA Title II deadlines: 26 Apr 2027 / 26 Apr 2028.

Code: `crates/caudal-captions` (engine, audio, chunking, cues),
`crates/caudal-core/src/captions.rs` (the `CaptionSource` seam),
`crates/caudal-hls/src/vtt.rs` + `lib.rs` (the rendition),
`crates/caudal/src/captions.rs` (config, `/metrics`, `fetch-model`).

## Build feature and binary size

Captions are the `captions` cargo feature of the `caudal` binary, on by
default. `cargo build --release -p caudal --no-default-features` leaves
out `caudal-captions` and candle; `[captions]` still parses there, but
setting anything in it fails `caudal check` and startup with "this build
has no captions; rebuild with --features captions" (`caudal doctor` and
`caudal captions fetch-model` say the same). CI's `test-no-captions` job
checks that build's dependency tree, lints it and runs caudal's tests.

Release binary, static musl, stripped, fat LTO (CI `build-static`, 19 Sep
2026, commit 59d58f9):

| target | with captions | without | captions add |
|---|---:|---:|---:|
| x86_64-unknown-linux-musl | 39,085,512 B (37.3 MiB) | 35,996,680 B (34.3 MiB) | 3,088,832 B (2.9 MiB) |
| aarch64-unknown-linux-musl | 32,781,040 B (31.3 MiB) | 30,807,280 B (29.4 MiB) | 1,973,760 B (1.9 MiB) |

The model is not in the binary (`caudal captions fetch-model`, 151-967 MB).

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

## Languages: what is verified and what is not

The models `caudal captions fetch-model` downloads (`tiny`, `base`, `small`)
are Whisper's **multilingual** models (not the English-only `.en` ones),
trained on about 99 languages. `[[captions.stream]] language` accepts
`auto` (detected per chunk) or any 2–3 letter language code the model knows
(`es`, `en`, `pt`, `fr`, `de`, ...).

- **Verified: Spanish and English only.** Measured on an Apple M4 with
  generated speech (table above): 96–99 % of words right with `tiny`,
  language detection between the two correct, viewer labels
  "Español (auto)" / "English (auto)".
- **Not verified: every other language.** Caudal passes the code to the
  model and it will produce captions, but nobody has measured their
  quality. Whisper is known to do well on high-resource languages
  (Portuguese, French, German, Italian) and much worse on low-resource
  ones, especially with `tiny`; for a language other than Spanish or
  English, start with `small` (about 3 cores on CPU) and check the output
  before relying on it.
- The subtitle rendition's `NAME` for other languages is the plain code
  (e.g. `pt (auto)`), not a localized name.

## Pipeline

```
Subscriber (tokio task) --bounded 2048 AUs--> decoder thread per stream
  AAC: rusty_aac (Apache-2.0, patched) \
  Opus: opus-decoder (MIT/Apache)  -> 16 kHz mono -> Chunker -> job queue (4, coalescing)
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
  level and aliasing at 48/44.1/32/22.05 kHz). rusty_aac 0.5.0 ran a
  direct O(N^2) IMDCT: 24.4 ms to decode a 21.3 ms frame on GitHub's
  4-vCPU runner (AMD EPYC 7763), 6.1 ms on an M4, so on that CPU the
  decoder could not keep up with one live stream. `vendor/rusty_aac` adds
  an FFT IMDCT (vendor/README.md); a test holds AAC decoding to at least
  10x faster than real time (5 s decoded in 0.108 s on the same runner).
- **Never blocks ingest.** The stream task `try_send`s access units; when
  the decoder queue is full the frame is dropped and counted
  (`caudal_captions_dropped_audio_seconds_total`). The queue holds 2048
  access units (~44 s of AAC at 48 kHz): at 512 (~11 s) the decoder thread,
  starved of CPU by a busy inference pool, fell behind the 4x-real-time
  pipeline test and lost 6.5 s of audio (below). Chunks never wait for the
  engine either; see "Slow machines" for what happens when it is behind.
  A job that waited more than 10 s is dropped and counted
  (`..._dropped_chunks_total`). Measured in the in-process pipeline test
  and the e2e test on an M4: 0 dropped.
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
- **Slow machines.** Whisper encodes a full 30 s window on every call, so
  a call costs about the same for 0.5 s of audio as for 5 s. On a 4-vCPU
  x86 CI runner (tiny, CPU, direct RTF 0.38) the pipeline test cut ~18 s of
  speech into 13 chunks (4 on an M4), dropped 5, and reported RTF 50.9:
  the decoder thread fell behind, the audio queue dropped frames, every
  hole in the timestamps closed a short chunk, and every scrap paid for a
  full window until the job queue overflowed. Two rules now keep that from
  compounding: (1) while the engine is busy, a stream's new chunks join its
  waiting job (up to 30 s), so a slow engine runs fewer, longer inferences
  instead of dropping; only a chunk that fits nowhere (queue of 4 jobs
  full) is dropped and counted; (2) a chunk shorter than 1 s (a gap closed
  it) waits up to 1 s for the audio after it rather than taking an
  inference alone, except at the end of the stream.
  `crates/caudal-captions/tests/pipeline.rs` reproduces it without a model
  (a stand-in costing 1 s per call, 16 runs of 0.5 s separated by
  timestamp gaps): before, 8 of 15 chunks dropped, every call on a 0.51 s
  scrap; after, 4 calls, 0 dropped, done 2.7 s after the last audio.
  On the 4-vCPU Linux runner it still ran 8 calls of two chunks each and
  took 7.5-10.8 s to clear. Timing probes there showed the engine idle
  every time a chunk arrived: the AAC decoder (above), not the engine, was
  the bottleneck, handing out one 0.5 s chunk every ~0.59 s. With the FFT
  IMDCT the runner matches the M4: 4 calls (1.02, 4.10, 2.05, 0.51 s),
  0 dropped, clear 2.6 s after the last push. The test's bounds come from
  the work: at most one call per second of pushing plus the first and the
  tail, no sub-second call but the tail, clearing within 1 s per call
  plus 1 s, RTF over everything.
  With the real model on the M4's efficiency cores (`taskpolicy -c
  background`, `device = "cpu"`, espeak-ng speech), where tiny runs slower
  than real time: 1 thread, before: 16 chunks, 15 dropped, 1 transcribed;
  4 threads with coalescing but the old 512-AU audio queue: 20 chunks, 4
  inferences, 0 chunks dropped but 6.5 s of audio lost at the decoder
  queue; with the 2048-AU queue: 3 chunks, 0 dropped (RTF 1.9, so it
  still cannot keep up there; it degrades by falling behind, not by
  dropping). `caudal_captions_real_time_factor` is
  inference time / audio time over everything transcribed (it was the last
  chunk's); `..._last_real_time_factor` is the last run's, and
  `..._inferences_total` next to `..._chunks_total` shows the coalescing.
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
- The Linux CI path (espeak-ng voice, CPU, debug build) runs on the Mac
  with `CAUDAL_TTS=espeak CAUDAL_CAPTIONS_DEVICE=cpu` (and
  `CAUDAL_CAPTIONS_THREADS=1` for a slower machine), but not on x86 Linux
  locally; CI is the check there. Its thresholds are looser (WER <= 60%,
  recall >= 50%).
- Static musl builds with candle are built only in CI (`build-static`,
  x86_64 and aarch64); they were not run, only built and measured.
- Speaker changes, music, noisy audio, and languages other than es/en:
  not measured.
