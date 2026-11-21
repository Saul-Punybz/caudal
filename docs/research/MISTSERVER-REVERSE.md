# MistServer, reverse-engineered from source

Reference for building Caudal. Read `PLAN.md` and `STATUS.md` first; this file does
not repeat the issue list there.

| | |
|---|---|
| Upstream | https://github.com/DDVTECH/mistserver (Unlicense) |
| Pinned commit | `ddd9bec7e143f41c39c61b1d3473cbcf1a42f9b6` |
| Commit date | 2026-11-02T17:04:46+01:00 ("Fixed IPv4 subnet matching") |
| Read on | 2026-11-19, shallow clone, nothing built or run |
| Size | 192,518 lines of `.cpp`/`.h` (`wc -l`) |

Citation rules:
- A bare path such as `lib/dtsc.cpp:3079` is a MistServer path at the commit above.
- `C:crates/...` is a Caudal path in this repo, read on the same day.
- Line numbers drift. Caudal's `crates/caudal/src/import_mist.rs:16-18` already cites
  `output_hls.cpp:193` and `output_tssrt.cpp:410`. At this commit those names are set at
  `src/output/output_hls.cpp:247` and `src/output/output_tssrt.cpp:417`.
- Two read-only research agents (Sonnet) built the connector catalog (section 3) and the
  trigger/API/stats tables (section 4). I spot-checked their citations against the source:
  `output_hls.cpp:247`, `output_webrtc.cpp:547`, `output_cmaf.cpp:128`,
  `input_buffer.cpp:49-65`, `output_hls.cpp:105-173`, `session.cpp:290-300`,
  `controller_push.cpp:600-608`, `triggers.cpp:130-155, 286-297` and `jwt.cpp:184-214`.
  All matched. Rows I did not spot-check are still from their source reading, not from
  docs.

---

## 0. The findings that matter most for Caudal

| # | Finding | Evidence | What it means for Caudal |
|---|---|---|---|
| 1 | The controller is no longer thread-per-connection. HEAD runs one `Event::Loop` (`Controller::E`) plus one log-reader thread. The 6,857-thread deadlock in #119 came from the old design. `configMutex` is still a global `std::mutex` held for every API request. | `src/controller/controller.cpp:451, 547-553, 802-806`; `src/controller/controller_api.cpp:137-162, 805` | PLAN's "#119" row is still a fair motive, but say "fixed upstream by moving to an event loop; a global config lock remains". Don't claim MistServer still spawns a thread per API call. |
| 2 | Config persistence is lazy and not atomic. Changes are written to disk only after 60 s with no further change, using `std::ofstream` straight over the target file. There is no temp file and no rename. A crash inside that window loses the change. A crash mid-write truncates the file. | `src/controller/controller.cpp:166-172`; `src/controller/controller_storage.cpp:270-280, 447-508` | This is the code path behind the "config not persisting" issues. Caudal's write-temp-then-rename approach is the right fix. `import-mist` should expect a truncated or empty `config.json`. |
| 3 | Viewer JWTs are not enforced on their own. A valid token only *skips* the `USER_NEW` trigger. With no `USER_NEW` handler configured, a missing or bad token still plays. `exp` is optional, and `iss`/`aud` are never checked. | `src/session.cpp:290-300`; `lib/jwt.cpp:184-214` | Caudal's `[auth] play = true` (deny without a valid token, `exp` required: `C:crates/caudal-auth/src/claims.rs:14`) is strictly stronger. Say so in docs. |
| 4 | Admin login uses a challenge that is not random: `md5(D-M-YYYY + client IP)` (zero-based month), so it repeats for a whole day per IP. Stored passwords are unsalted `md5(password)`. Local connections without `X-Forwarded-For` skip auth. The Prometheus path sits *before* the auth gate, so it is protected only by being secret. | `src/controller/controller_api.cpp:174-181, 281-309, 725-730, 767-780`; `src/controller/controller.cpp:121` | Never port this handshake. `import-mist` already refuses to migrate the MD5 hashes (`C:crates/caudal/src/import_mist.rs:35-37`). Keep `/metrics` behind config. |
| 5 | Every timestamp is integer milliseconds. RTP time is divided by `rate/1000` with integer math. Out-of-order packets (`t < lastms`) are dropped. The part duration field is 16-bit ms. | `lib/rtp.cpp:993, 1040`; `lib/dtsc.cpp:3103-3117, 2355-2357` | This confirms PLAN's "ms rounding" row with exact lines. Caudal's native-timescale model (`C:crates/caudal-core/src/media.rs:3, 93`) is the fix. Don't copy the drop-if-older rule for B-frame streams; it is DTS-only. |
| 6 | One OS process per viewer connection. A listening `MistOutX` forks and re-execs itself with `--connection_handler <ip>`, with the socket as stdin/stdout. Inputs are their own processes too, under a per-stream "angel" process that restarts them. | `src/output/mist_out.cpp:52-89`; `src/input/input.cpp:560-660` | This is the real cost behind #280 (TS-HLS CPU). Caudal's single async process is the main architectural win. Benchmark claims should compare against this model. |
| 7 | Triggers are the whole integration surface: 33 named triggers, blocking or not per handler, over HTTP POST or as a local executable (15 s hard kill), with `text/plain` newline-separated payloads. Caudal has 2 non-blocking webhook events. | `src/controller/controller_capabilities.cpp:250-505`; `lib/triggers.cpp:54-159, 286-297`; `C:crates/caudal-auth/src/lib.rs:152-155` | This is the biggest parity gap for people migrating (section 6). |

---

## 1. Architecture

### 1.1 Process model

| Process | Role | Spawned by | Supervision | Evidence |
|---|---|---|---|---|
| `MistController` (angel) | Parent that re-spawns the real controller. Exit 0 means stop. Exit 42 or USR1 re-execs the binary (rolling update). Otherwise it restarts with +1 s backoff each time. | init / systemd (`mistserver.service:12` runs `MistController -c /etc/mistserver.conf`) | `waitpid` loop | `src/controller/controller.cpp:874-987` |
| `MistController` (child, `CTRL_ATHEIST=1`) | Config, API (TCP 4242 plus UDP API), stats, push scheduler, connector supervisor | the angel | event loop timers: stats 1 s, load 1 s, push check 1 s, variables 750 ms, `statusMonitor` 3 s | `src/controller/controller.cpp:648-672` |
| `MistOut<Name>` listener | One per enabled protocol in `config.protocols` that owns a port (RTMP, RTSP, HTTP, TSSRT, DTSC...) | `CheckProtocols` | Restart-loop guard: no retry within `recentAttempts × 1 s`, then the error "Restart looping" | `src/controller/controller_connectors.cpp:236-275, 278-285` |
| `MistOut<Name> --connection_handler <ip>` | One per accepted connection (viewer or pusher) | the listener: fork plus `execvp(argv[0])`, socket as stdin/stdout | none (dies with the connection) | `src/output/mist_out.cpp:52-89` |
| HTTP-family outputs (HLS, CMAF, MP4, WebRTC signaling...) | No own port. `MistOutHTTP` routes by `url_match`/`url_prefix` and hands off to the right binary. | `MistOutHTTP` | none | `src/output/output_http.cpp:58, 152-181`; `src/controller/controller_connectors.cpp:176-181` |
| `MistIn<Name>` angel plus child | One per active stream. The angel holds `SEM_INPUT`, re-spawns the child after an abnormal exit, and fires `INPUT_ABORT`. | `Util::startInput`, called by any output that wants the stream, or by the controller for `always_on` | the angel's waitpid loop. Exit 0/1 is clean, anything else restarts. | `lib/stream.cpp:611-763`; `src/input/input.cpp:464-553, 560-660`; `src/controller/controller_streams.cpp:170-178` |
| `MistInBuffer` | The live buffer for `push://` streams; the pushing output writes into it | `startInput` with `sourceOverride` | as above | `src/input/input_buffer.cpp:46, 142-147` |
| `MistSession` | One per session id: accounting plus `USER_NEW`/`USER_END` | `Comms::Sessions` when `/MstSession<id>` does not exist | exits after 15 s inactive | `lib/comms.cpp:386-440`; `src/session.cpp:454-455` |
| `MistUtilCache`, `MistUtilLoad`, `MistProc*` | cache, load balancer, transcoders | controller (cache) / operator / stream `processes` | — | `src/controller/controller.cpp:664-710`; section 4.6 |

Rolling restart: before re-exec, the controller writes `cmd`/`pid` for each running
connector to SHM `/MstCnns`. The new controller reads it back and sends `SIGUSR1` so the
listeners re-exec as well (`src/controller/controller_connectors.cpp:55-93`).

Hard limits found in code:
- API sockets with fd ≥ 1024 are rejected, because the loop uses `select()`
  (`src/controller/controller_api.cpp:139-143`).
- Stream names are limited to 100 characters (`lib/stream.cpp:613-617`).
- `startInput` waits up to 240 × 250 ms = 60 s for a stream state and again for the input
  to come up (`lib/stream.cpp:624-632, 750-758`).

### 1.2 Shared memory (all IPC is SHM pages, semaphores and a UDP API)

| Name pattern | Content | Size | Evidence |
|---|---|---|---|
| `/MstMeta<stream>` | Stream metadata (RelAccX; DTSC meta) | 8 MiB, 2 KiB per item | `lib/defines.h:150-152` |
| `/MstTrak<stream>@<pid>-<n>` | Per-track index: parts, keys, fragments, pages | 16 MiB item, 64 MiB len | `lib/defines.h:154-156` |
| `/MstData<stream>@<track>_<page>` | Media data pages | flips at 8 MiB or 60 s; VoD min 20 s | `lib/defines.h:138-144, 217` |
| `/MstSTATE<stream>` | 1 byte of stream state (below) | — | `lib/defines.h:225, 231-237` |
| `/MstSemInpt<stream>` | "Input alive" semaphore: `streamAlive()` does `tryWait` on it | — | `lib/defines.h:250`; `lib/stream.cpp:493-506` |
| `/MstStat`, `/MstUser<stream>`, `/MstSession<id>` | Connection stats records (layout in 4.4) | 16 MiB / 512 KiB / 1 MiB | `lib/defines.h:199-206` |
| `/MstSess` | Session cache: 165 B items, about 32K sessions | 5 MiB | `lib/defines.h:263-265` |
| `/MstTRGR<NAME>` | Trigger handlers per trigger type | 32 KiB | `lib/defines.h:248` |
| `/MstJWK`, `/MstStrmKeys`, `/MstGlobalConfig`, `/MstSCnf<stream>`, `/MstCapa`, `/MstProt`, `/MstVars` | Config fan-out to child processes | — | `lib/defines.h:226-261` |
| `/MstPush` | Active pushes, so they survive a controller restart | 8 MiB | agent read, `src/controller/controller_push.cpp:156-189` |

Stream states: `OFF 0, INIT 1, BOOT 2, WAIT 3, READY 4, SHUTDOWN 5, INVALID 255`
(`lib/defines.h:231-237`). The controller maps these to the UI's `online` 0/1/2 plus an
`error` string such as "Waiting for data..." (`src/controller/controller_streams.cpp:105-136`).

`RelAccX` is the record format for every page: typed fields (`RAX_UINT`, `RAX_STRING`,
`RAX_RAW`, `RAX_DTSC`, nested), a ring with `deleted`/`endPos`, and a ready flag
(`lib/util.h:107-179`). Caudal has no reason to copy it. It exists only because the
processes are separate.

### 1.3 DTSC (MistServer's internal media format)

| Item | Detail | Evidence |
|---|---|---|
| Packet kinds | `DTSC` header, `DTPD` (v1), `DTP2` (v2: 4 B len, 4 B track id, **8 B time in ms**, packed body), `DTCM` command | `lib/dtsc.h:58-63, 105-110` |
| Stream fields | vod, live, tracks, source, uuid, maxkeepaway, bufferwindow, bootmsoffset, utcoffset, utcsource, minfragduration | `lib/dtsc.cpp:1281-1291` |
| Track fields | id, type, codec, firstms/lastms/nowms, bps, maxbps, lang, init (up to 1 MiB), rate, size, channels, width, height, fpks, efpks, missedFrags | `lib/dtsc.cpp:2285-2306` |
| Parts (one per frame) | size u32, **duration u16 (ms)**, offset i16 (ms, the CTS offset) | `lib/dtsc.cpp:2355-2357` |
| Keys | firstpart, bpos, duration, number, parts, time, size | `lib/dtsc.cpp:2365-2371` |
| Fragments | duration, keys, firstkey, size. A new fragment starts at the first key that is ≥ `minfragduration` (default 1900 ms) after the previous fragment's first key. | `lib/dtsc.cpp:2383-2386, 3172-3203`; `lib/defines.h:169` |
| Audio "keys" | A non-video track gets a synthetic key every `AUDIO_KEY_INTERVAL` = 2047 ms | `lib/dtsc.cpp:3135-3138`; `lib/defines.h:282` |
| Out-of-order input | `packTime < lastms` is logged once and dropped | `lib/dtsc.cpp:3103-3117` |
| Jitter | Per-track `jitterTimer` with 8 peak windows. It starts at 120 ms and feeds `minKeepAway` (how far viewers stay behind live). | `lib/dtsc.h:347-357`; `lib/dtsc.cpp:3004-3051, 3082-3084` |
| Defaults | 4 tracks, 60 fragments, 60 keys, 1800 parts, 10 pages to start; resized ×2 on demand | `lib/defines.h:159-167`; `lib/dtsc.cpp:3121-3124` |

Caudal equivalent: `caudal-core`'s ring with native timescales and `Event::Lagged`
(`C:crates/caudal-core/src/stream.rs:63, 403`). There is nothing to port. Two ideas are
worth taking: the measured-jitter keep-away (viewers sit `jitter` ms behind live) and the
fragment rule of "first keyframe after N ms".

### 1.4 Stream lifecycle

1. **Lookup.** An output gets a stream name. If the name is a JWS it is replaced by its
   `sub` (`src/output/output.cpp:364-369`). `PLAY_REWRITE` may rename it
   (`output.cpp:240-243`).
2. **Start the input.** `Output::connect` calls `Util::startInput(name, override, true,
   isPushing())` (`src/output/output.cpp:382`). If that fails, `DEFAULT_STREAM` and the
   `defaultStream` setting pick a fallback (`output.cpp:401-423`).
3. **`startInput`** sanitizes the name and waits out BOOT/INIT. If the input is already
   alive it returns. Otherwise it runs `STREAM_LOAD` (false = deny) and `STREAM_SOURCE`
   (may override the source), resolves `source` → input by `source_match`, builds argv from
   the stream config through `optionsToArguments`, spawns `MistIn<Name> -s <stream> <src>`
   and polls `streamAlive` for up to 60 s (`lib/stream.cpp:611-763, 858-896`).
4. **Input ready.** It fires `STREAM_READY` (false = the input exits) and starts the event
   loop (`src/input/input.cpp:912-971`).
5. **Viewers attach.** They map the meta/track pages and load data pages by key
   (`src/output/output.cpp:754-822`). `CONN_OPEN` (`output.cpp:1875`), `CONN_PLAY`
   (`:264`) and the session's `USER_NEW` gate them.
6. **Pushing.** The output sets `pushing`, waits for an old buffer to finish shutting down
   (`waitForStreamPushReady`, `output.cpp:3029-3070`), then checks the source
   `push://[ip][@password]` (password match skips the IP check; otherwise the IP must match)
   and `STREAM_PUSH` (`output.cpp:2971-3027`). Stream keys or an INPUT-permission JWT can
   stand in for the configured name (`output.cpp:2926-2965`).
7. **Timeout.** The input stays up while `now - activityCounter < inputtimeout`. The
   default is `INPUT_TIMEOUT` = 2 × `STATS_DELAY` = 30 s. Activity means at least one
   connected user, or `always_on`. Then `STREAM_UNLOAD` may veto the shutdown
   (`src/input/input.cpp:111, 146-155, 1009-1043`; `lib/defines.h:121-128`).
   **Comment vs code:** the doc comment at `input.cpp:1005-1007` says live streams wait
   "twice the biggest fragment duration". The code only uses `inputTimeout`.
8. **Viewer idle.** An output gives up after "Connection idle for 5 minutes"
   (`output.cpp:1576`). Pages that nobody reads are dropped after `pagetimeout`, default
   15 s (`input.cpp:244-253`; `lib/defines.h:172`).
9. **End.** `STREAM_END` fires from the controller's stats once the stream goes inactive
   (`src/controller/controller_statistics.cpp:751-760`).

---

## 2. Config and controller API

### 2.1 Files and load/save

| Item | Detail | Evidence |
|---|---|---|
| Default path | `config.json` in the working directory (`-c/--config`). The packages use `/etc/mistserver.conf`, which is JSON too. | `src/controller/controller.cpp:357-362`; `mistserver.service:12`; `mistserver.init:13` |
| `-C/--configrw` | `r` reloads when the file mtime changes; `w` writes after 60 s quiet; `rw` (default) does both | `src/controller/controller.cpp:364-370, 495-505, 135-172` |
| `config_split` | `{section: "/path/file.json"}`: a top-level section can live in its own file, read and written separately | `src/controller/controller_storage.cpp:455-481, 513-524`; `controller.cpp:138-145` |
| Write | `std::ofstream` truncate + write, no fsync, no rename | `src/controller/controller_storage.cpp:270-280` |
| Not written | `log`, `online`, `error` | `src/controller/controller_storage.cpp:437-443` |
| On exit | `writeConfigToDisk(true)` | `src/controller/controller.cpp:843` |
| Fan-out | `writeConfig()` writes protocols, streams, global config, triggers and JWKs to SHM pages for the children | `src/controller/controller_storage.cpp:838-920, 957-1050` |
| Migrations on load | `autopushes` array → `auto_push` object; `variables` array → object; `controller.prometheus` → `config.prometheus`; defaults for `push_settings`, session modes, `bandwidth.exceptions` (RFC1918 + loopback) | `src/controller/controller_storage.cpp:526-690` |

### 2.2 JSON schema (top-level keys found in code)

| Key | Shape | Evidence |
|---|---|---|
| `account` | `{user: {password: md5hex}}` | `controller.cpp:121`; `controller_api.cpp:301-302` |
| `config.protocols` | `[{connector: "RTMP", port, interface, ...connector capa options}]` | `controller_connectors.cpp:37-44, 257` |
| `config.controller` | `{port, interface, username}` (`prometheus` here is legacy) | `controller_storage.cpp:540-556` |
| `config.prometheus` | the path segment that serves metrics | `controller_storage.cpp:557-565` |
| `config.accesslog`, `config.debug`, `config.trustedproxy`, `config.serverid`, `config.location`, `config.defaultStream`, `config.cache.path` | scalars and arrays | `controller_storage.cpp:531-616`; `controller.cpp:664-680` |
| `config.session{Viewer,Input,Output,Unspecified,StreamInfo}Mode`, `config.tknMode` | bitmasks (4.4) | `controller_storage.cpp:567-584` |
| `config.weights` | load weights | `controller_storage.cpp:599-605` |
| `config.triggers` | `{NAME: [[handler, sync, [streams]] or {handler, sync, streams, params, default}]}` | `controller_storage.cpp:979-1031` |
| `streams` | `{name: {source, always_on, tags[], processes[], DVR, ...any option of the matching input's capa}}`. Keys map 1:1 to `MistIn*` CLI flags. | `lib/stream.cpp:858-896`; `controller_streams.cpp:170-178, 260-302` |
| `auto_push` | `{id: {stream, target, scheduletime, completetime, start_rule, end_rule, inhibit}}` | `controller_push.cpp:50-97, 530-570` |
| `push_settings` | `{wait: 3, maxspeed: 0}` | `controller_storage.cpp:527-530` |
| `streamkeys` | `{key: stream}` | `controller_api.cpp:1597-1640` |
| `jwks` | `[jwk or [jwk, perms] or "https://.../jwks"]` | `controller_storage.cpp:166, 200-240` |
| `variables`, `extwriters`, `tags`, `bandwidth{limit,exceptions}`, `admin_http`, `ui_settings`, `log` | misc | `controller_storage.cpp:607-660`; `controller_api.cpp:1306, 1729` |

There is no schema file. The stream object is open-ended because each input's `capa`
defines its keys. `import-mist` should treat unknown stream keys as "no Caudal
equivalent", not as errors.

### 2.3 HTTP / WebSocket / UDP API

| Item | Detail | Evidence |
|---|---|---|
| Port | TCP 4242 (`config.controller.port`, `--port`). Optional TLS via mbedtls when `--certificate`/`--key` are set, with SNI. | `controller.cpp:293-296, 527-645` |
| `/api` | JSON reply. Body is `application/json`, or a `command=<json>` query/form var. JSONP with `callback=`/`jsonp=`. | `controller_api.cpp:782-832` |
| `/api2` | same, with `minimal: true` (skips the full config/streams/log) | `controller_api.cpp:803` |
| any other path | serves the embedded UI (`server.html.h`) | `controller_api.cpp:789-801` |
| `/ws`, `/ws/stream/<name>` | push channel: `["log"..]`, `["access"..]`, `["stream"..]`, `["streammeta"..]`. Auth is `["auth",{username,password}]`, with a 10 s limit. | `controller_api.cpp:393-447, 758-765` |
| `/<prometheus>`, `/<prometheus>.json` | metrics, **before** the auth check | `controller_api.cpp:767-780` |
| `/http/<token>/...` | time-limited admin proxy into a spawned `MistOutHTTP` | `controller_api.cpp:617-722` |
| UDP `localhost:4242` | internal commands from children, run as `local: true` under `configMutex` | `controller.cpp:728-788` |
| `Authorization: json {...}` | header form of `authorize` | `controller_api.cpp:735-754` |

**Login handshake** (`controller_api.cpp:174-309`):
1. The client sends `{authorize:{username, password:""}}` and gets
   `{authorize:{status:"CHALL", challenge}}`.
2. The client sends `password = md5(md5(plain) + challenge)`. The server compares it with
   `md5(stored + challenge)` and answers `OK`.
3. With no accounts at all, the server answers `NOACC`. In that state `new_username`/
   `new_password` creates one (`ACC_MADE`), unauthenticated.

The challenge is `md5("<mday>-<mon0>-<year>" + host)` (`:174-181`). Local, non-proxied
connections are authorized automatically (`:725-730`). The UI (`lsp/mist.js`) stores
`MD5(password)` and does the same handshake. It POSTs JSON to `/api`, never `/api2`
(agent read: `lsp/mist.js:4410, 17559-17567, 17787-17815`).

**Commands** (top-level keys of `handleAPICommands`, `controller_api.cpp:861-2187`; agent
inventory, may be incomplete):

| Group | Keys |
|---|---|
| Config | `config`, `config_backup`, `config_reload`, `config_restore`, `save`, `ui_settings`, `capabilities`, `version`, `api_endpoint`, `shutdown` (local only) |
| Streams | `streams`, `addstream`, `deletestream`, `deletestreamsource`, `active_streams`, `stats_streams`, `nuke_stream`, `no_unconfigured_streams`, `deletecache`, `playlist`, `browse`, `enumerate_sources` |
| Protocols | `addprotocol`, `deleteprotocol`, `updateprotocol` |
| Auth | `jwks`, `addjwks`, `deletejwks`, `streamkeys`, `streamkey_add`, `streamkey_del`, `admin_http_add/_extend/_delete/_list` |
| Sessions | `clients`, `totals`, `invalidate_sessions`, `stop_sessions`, `stop_sessid`, `invalidate_sessid`, `stop_tag`, `tag_sessid`, `untag_sessid` |
| Tags | `tags`, `add_tag`, `remove_tag`, `tag_stream`, `untag_stream`, `stream_tags` |
| Push | `push_start`, `push_list`, `push_stop`, `push_stop_graceful`, `push_auto_add`, `push_auto_remove`, `push_auto_list`, `push_settings`, `proc_list` |
| Other | `bandwidth`, `load`, `log`, `clearstatlogs`, `variable_list/_add/_remove`, `external_writer_list/_add/_remove`, `autoupdate`/`update`/`checkupdate`/`abortupdate` (UPDATER builds) |
| Internal (UDP) | `trigger_stat`, `trigger_fail`, `push_status_update`, `proc_status_update`, `cache_udp_socket` |

### 2.4 `configMutex`

| Observation | Evidence |
|---|---|
| A global `std::mutex`, held for the entire processing of every HTTP, WebSocket and UDP API request | `controller_api.cpp:438, 805`; `controller.cpp:780` |
| Also taken by the 3 s status monitor for disk read/write, by push checks and by stats | `controller.cpp:161, 169`; `controller_push.cpp:417`; `controller_statistics.cpp:498` |
| Manually `unlock()`ed and re-`lock()`ed *inside* a `lock_guard` scope around blocking `getOutputOf(MistIn* --getcapa/--enumerate)` calls. An exception in between would double-unlock, and the event loop is blocked for the duration of the subprocess anyway. | `controller_api.cpp:1553-1555, 2171-2173` |
| The signal handler calls `mutabort->try_lock()`, which is not async-signal-safe | `lib/config.cpp:644-671` |
| Nested with `statsMutex` (recursive) and `actPushMut` (recursive) in fixed orders. A deadlock would need someone to take them in the opposite order; I found no such site, but did not prove it. | `controller_statistics.cpp:498-499`; `controller_push.cpp:22, 417` |

Copy: nothing. Caudal's immutable snapshot plus atomic swap (`C:crates/caudal/src/reload.rs`)
already avoids all of it.

---

## 3. Inputs and outputs

Built always unless noted. Gates: SRT needs libsrt (`meson.build:193-210`); RIST needs
librist (`:212-216`); WebRTC/HTTPS need mbedtls + libsrtp2 (`:140-181`); data channels need
usrsctp (`:183-190`); `NOLLHLS` removes LL-HLS (`:106`); AV needs `WITH_AV` ("not
distributable"); V4L2 needs `linux/videodev2.h` (`src/input/meson.build:25-37`).

### 3.1 Inputs (`src/input/`)

| Name | Source match | Codecs | Notes | Evidence |
|---|---|---|---|---|
| Buffer | `push://[host][@password]` | any | Live DVR buffer. `--buffer` 50000 ms, `--cut`, `--resume`, `maxkeepaway` 45000, `segmentsize` 1900, `fallback_stream` | `input_buffer.cpp:46-147` |
| TSSRT | `srt://` (libsrt) | H264, HEVC, MPEG2, AAC, MP3, AC3, MP2, opus, rawts | Listener or caller. `acceptable`: 0 streamid = `stream+streamid`, 1 ignore, 2 must match | `input_tssrt.cpp:50-159, 203-236` |
| TSRIST | `rist://` (librist) | as TSSRT | optional | `input_tsrist.cpp:85-102` |
| TS | file `.ts/.m2ts`, `tsudp://`, `ts-exec:`, http(s) `.ts`, `http-ts://`, s3 | H264, HEVC, MPEG2, AAC, AC3, MP2, opus, rawts (no MP3 listed) | UDP push `udp://$host:$port` | `input_ts.cpp:177-252` |
| RTSP | `rtsp://` pull, UDP or TCP | H264, HEVC, MPEG2, VP8, VP9, AAC, MP3, AC3, ALAW, ULAW, PCM, opus, MP2 | | `input_rtsp.cpp:42-64` |
| SDP | `*.sdp` (raw RTP described by SDP) | as RTSP | | `input_sdp.cpp:39-63` |
| HLS | `.m3u8` file/http/https/s3, `http-hls://` | H264, AAC, AC3, MP3 | live and VoD pull | `input_hls.cpp:551-597` |
| MP4 | `.mp4/.mov` file/http/s3 | HEVC, H264, H263, VP6, VP8, VP9, AV1, AAC, AC3, MP3, opus | | `input_mp4.cpp:25-64` |
| EBML | `.mkv/.mka/.webm` file/http/s3, `mkv-exec:` | widest set (AV1, VP9, FLAC, DTS, subtitles...) | | `input_ebml.cpp:11-82` |
| FLV | `*.flv` | H264, H263, VP6, AAC, MP3 | | `input_flv.cpp:19-35` |
| DTSC | `*.dtsc`, `dtsc://`, `dtscs://` | H264, H263, VP6, theora, AAC, MP3, vorbis | Mist-to-Mist pull (origin/edge) | `input_dtsc.cpp:19-49` |
| ISMV | `*.ismv` | H264, AAC | Smooth file | `input_ismv.cpp:15-27` |
| OGG, MP3, AAC, FLAC | by extension | theora/vorbis/opus; MP3; AAC; FLAC | | `input_ogg.cpp:37-50`, `input_mp3.cpp:16-28`, `input_aac.cpp:63-75`, `input_flac.cpp:20-32` |
| H264 | `h264-exec:` (Annex B on stdin) | H264 | | `input_h264.cpp:7-18` |
| SubRip | `*.srt`, `*.vtt` | subtitle | | `input_subrip.cpp:7-19` |
| Folder | `/dir/` | — | serves files in a folder as streams | `input_folder.cpp:11-18` |
| Playlist | `*.pls` | — | 24/7 playlist | `input_playlist.cpp:12-15` |
| Balancer | `balance:http://LB[?fallback=URL]` | — | asks MistUtilLoad `?source=` and re-execs as that input | `input_balancer.cpp:9-155` |
| V4L2 | `v4l2:` | — | Linux capture | `input_v4l2.cpp:16-23` |
| AV | `/*` via libav | dynamic | `WITH_AV` only | `input_av.cpp:16-39` |

### 3.2 Outputs (`src/output/`)

Port "HTTP" means the output is served through the HTTP connector (default 8080,
`output_http_internal.cpp:208`).

| Name | Protocol / URL | Codecs | Port | Notes | 2026 | Evidence |
|---|---|---|---|---|---|---|
| RTMP | RTMP(S) in and out | classic FLV set + Enhanced RTMP: AV1, HEVC, VP8, VP9, opus, AC3, EAC3, FLAC | 1935 | push URL `rtmp://$host:$port/$password/$stream`; `acceptable`, `maxkbps` | live | `output_rtmp.cpp:503-602, 1567-1570` |
| RTSP | RTSP server, in and out | H264, HEVC, MPEG2, VP8, VP9, AV1, AAC, MP3, AC3, ALAW, ULAW, PCM, opus, MP2 | 5554 | `rtsp://...?pass=` | live | `output_rtsp.cpp:63-112` |
| TSSRT | SRT listener/caller/rendezvous | HEVC, H264, MPEG2, AV1, AAC, MP3, AC3, MP2, opus, PCM, JSON, SCTE35, rawts | 8889/UDP | `latency` 120, `passphrase` 10-79 chars, `conntimeo` 3000, `enforcedencryption`, `sockopts` | live | `output_tssrt.cpp:417-675` |
| TSRIST | RIST push only | as TS | — | `profile` default 1 | live | `output_tsrist.cpp:225-319` |
| TS | raw TS over TCP, or push `tsudp://`, `tsrtp://`, `tstcp://` (incl. multicast) | HEVC, H264, MPEG2, AV1, AAC, MP3, AC3, MP2, opus, JSON | 8888 | | live | `output_ts.cpp:143-197` |
| HTTPTS | `/$.ts` | + SCTE35 | HTTP | | live | `output_httpts.cpp:60-103` |
| HLS | `/hls/$/index.m3u8` (TS segments) | HEVC, H264, MPEG2, AAC, MP3, AC3, MP2, subtitle | HTTP | v3 playlist, `SAMPLE-AES` if encrypted, `listlimit` | live | `output_hls.cpp:95-190, 247-315` |
| CMAF | `/cmaf/$/index.m3u8` (HLS v7 + LL-HLS), `/cmaf/$/index.mpd` (DASH, LL chunked), `/cmaf/$/Manifest` (Smooth) | H264, HEVC, AV1, VP8, VP9, AAC, AC3, MP3, opus, subtitle | HTTP | `partduration` 500 ms, `packaging` separate or muxed, CTR128 encryption, push `cmaf://` | HLS/DASH live; **Smooth dead** | `output_cmaf.cpp:128-320, 562-588, 965-987, 1324-1391`; `lib/hls_support.cpp:422-566` |
| WebRTC | `/webrtc/$` (WebSocket signalling), `/whip/$`, `/whep/$`; push out `whip://` | H264, VP8, VP9, AV1, opus, ALAW, ULAW, JSON/subtitle over data channel | HTTP + UDP 18203 | ICE-lite, host candidates only, NACK, ulpfec, goog-remb | live | `output_webrtc.cpp:15, 547-743`; `lib/sdp_media.cpp:893, 994-1021` |
| MP4 | `/$.mp4`, `/$.fmp4`, `/$.3gp` | H264, HEVC, AV1, VP8, VP9, AAC, MP3, AC3, opus | HTTP | progressive and fMP4, push to file | live | `output_mp4.cpp:122-174` |
| EBML | `/$.mkv`, `/$.webm` | widest set | HTTP | push to file, `mkv-exec:` | live | `output_ebml.cpp:51-84` |
| DTSC | Mist internal protocol | any | 4200 | origin to edge | live (Mist-only) | `output_dtsc.cpp:265-340` |
| JPG | `/$.jpg`, `.mjpg` | JPEG | HTTP | thumbnails (needs a JPEG track) | live | `output_jpg.cpp:56-79` |
| JSON, JSONLine | `/$.json` (metadata, subtitles), TCP 3456 | JSON | HTTP / 3456 | | live | `output_json.cpp:20-25`; `output_jsonline.cpp:60-86` |
| SDP | `/$.sdp` | RTP set | HTTP | | niche | `output_sdp.cpp:86-121` |
| SubRip | `/$.srt`, `/$.vtt` | subtitle | HTTP | | live | `output_srt.cpp:15-21` |
| AAC, MP3, FLAC, WAV, OGG | `/$.<ext>` | single codec | HTTP | audio-only progressive | live | `output_{aac,mp3,flac,wav,ogg}.cpp` |
| H264, WSRaw | `/$.h264`, `/$.raw` (also over WS) | H264/HEVC; any | HTTP | raw elementary streams, used by the MSE/WebCodecs players | niche | `output_h264.cpp:328-334`; `output_wsraw.cpp:72-79` |
| HTTP | player and embed: `/$.html`, `/embed_$.js`, `/json_$.js`, `/player.js`, `/hlsjs.js`... | — | 8080 | the main HTTP router | live | `output_http_internal.cpp:128-208` |
| HTTPS | TLS wrapper around HTTP | — | 4433 | mbedtls | live | `output_https.cpp:60-120` |
| HTTPMinimalServer | `/static/` | — | HTTP | static files | live | `output_http_minimalserver.cpp:19-30` |
| RAW | TCP 666 | any | 666 | | niche | `output_raw.cpp:30-59` |
| Forward | `rawhls://` push only | rawhls | — | | niche | `output_forward.cpp:15-38` |
| **HDS** | `/dynamic/$/manifest.f4m` | FLV set | HTTP | method `flash/11` | **dead** | `output_hds.cpp:155-180` |
| **FLV (HTTP)** | `/$.flv` | FLV set | HTTP | `player_url` `/oldflashplayer.swf` | **dead as a player target** (still used by flv.js) | `output_flv.cpp:9-46` |
| **Flash players** | `/flashplayer.swf`, `/oldflashplayer.swf` (embedded SWF byte arrays) | — | HTTP | | **dead** | `flashPlayer.h:1`; `output_http_internal.cpp:3, 153-154, 850-858` |
| **Smooth** | `/cmaf/$/Manifest`, ISMV input | H264, AAC | HTTP | part of CMAF, not its own output | **dead** | `output_cmaf.cpp:180-191, 1324-1391` |

Processes (`src/process/`): FFMPEG (external binary; H264/VP9/AAC/MP3/opus targets,
`process_ffmpeg.cpp:281-588`), AV (libav, `WITH_AV`), Livepeer (remote transcoding),
MKVExec (pipe through any exe), TSDemux, Composer (raw video mosaic).
(Agent read: `process_*.cpp` capa blocks.)

---

## 4. Triggers, push, load balancer, access, stats

### 4.1 Trigger execution (`lib/triggers.cpp`)

| Aspect | Behaviour | Evidence |
|---|---|---|
| Handler kinds | `http://` or `https://` → POST. Anything else is an executable path. | `triggers.cpp:59, 86` |
| HTTP request | body = payload, `Content-Type: text/plain`; headers `X-Trigger`, `X-Instance`, `X-Name`, `X-PID`, `X-Trigger-UUID`, `X-Trigger-UnixMillis`, `X-UUID` | `triggers.cpp:60-77` |
| Executable | `argv = [exe, TRIGGER]`, payload on stdin, env `MIST_TRIGGER`, `MIST_TRIG_DEF`, `MIST_PID`... | `triggers.cpp:90-122` |
| Blocking (`sync`) | response body (HTTP 2xx) or stdout to EOF. Exe: SIGTERM at 10 s, SIGKILL at 14 s, gives up at 15 s and uses the default. | `triggers.cpp:123-156` |
| Non-blocking | the default response is used immediately | `triggers.cpp:157-159` |
| Default | per-handler `default`, else `"true"` | `triggers.cpp:289-290` |
| Several handlers | `retVal &= stringToBool(response)`: any false denies. Non-blocking handlers contribute their *default*, so an async handler with `default:"false"` denies too. | `triggers.cpp:291-297` |
| HTTP timeout | none set explicitly (Downloader default) | not verified |
| Stream filter | exact name, base before `+`/space, `#tag`, `!` negation, empty = all | `triggers.cpp:227-276` |
| Stats | `mist_trigger_count/time/fails{trigger}` | `controller_statistics.cpp:2287-2289` |
| Stale doc | the header says every trigger is async and ignored; the code supports sync | `triggers.cpp:15-16` |

Payloads are newline-separated lines. There is no JSON envelope, except where a single
line is itself JSON.

### 4.2 Trigger catalog

Blocking means the response is used. "bool" means false denies. "override" means a
non-empty response replaces a value. All rows were checked against
`controller_capabilities.cpp:250-505`; call sites are listed.

| Trigger | Fires | Stream | Payload lines | Response | Call site |
|---|---|---|---|---|---|
| SYSTEM_START | config stable at boot | no | version | bool (false = shut down) | `controller_storage.cpp:1055` |
| SYSTEM_STOP | before shutdown | no | reason | bool (false = cancel shutdown) | `controller.cpp:821-825` |
| OUTPUT_START / OUTPUT_STOP | connector start/stop | no | connector cmd | ignored | `controller_connectors.cpp:265, 285` |
| STREAM_ADD / STREAM_CONFIG / STREAM_REMOVE | stream config edits | yes | name, config JSON | bool | `controller_streams.cpp:217, 241, 398` |
| STREAM_SOURCE | before input start | yes | name | override source | `lib/stream.cpp:666` |
| STREAM_LOAD | before input start | yes | name | bool | `lib/stream.cpp:663` |
| STREAM_READY | input loaded | yes | name, input type | bool (false = input exits) | `input.cpp:932` |
| STREAM_UNLOAD | before input unload | yes | name, input type | bool (false = stay up) | `input.cpp:1041` |
| STREAM_PUSH | incoming push | yes | name, ip, connector, url | bool | `output.cpp:3012` |
| RTMP_PUSH_REWRITE | RTMP publish | no | url, host | override URL; empty denies | `output_rtmp.cpp:1802` |
| PUSH_REWRITE | any incoming push | no | url, host, stream | override stream; empty denies | 13 outputs, e.g. `output_rtmp.cpp:1837`, `output_tssrt.cpp:307`, `output_webrtc.cpp:1375` |
| SRT_ACCEPT | SRT handshake | no | local, remote, streamid | empty rejects; else SRT socket options | `lib/socket_srt.cpp:173-181` |
| LIVE_TRACK_LIST | track list changed | yes | name, tracks JSON | ignored | `input_buffer.cpp:624` |
| STREAM_BUFFER | EMPTY/FULL/DRY/RECOVER | yes | name, state, health JSON | ignored | `input_buffer.cpp:302-410` |
| LIVE_BANDWIDTH | every keyframe over the threshold | yes | name, bytes/s | bool (false = kill buffer) | `input_buffer.cpp:284` |
| STREAM_END | stream inactive | yes | name, bytes down/up, viewers, inputs, outputs, viewer-seconds | ignored | `controller_statistics.cpp:751` |
| INPUT_ABORT | input crashed | yes | name, source, exe, pid, reason codes | ignored | `input.cpp:850` |
| PUSH_INPUT_CLOSE | push input closed | yes | name, host, exe, pid, reasons, tracks | ignored | `output.cpp:2185` |
| PUSH_OUT_START | before an outgoing push | yes | name, target | override target; empty aborts | `lib/stream.cpp:915` |
| PUSH_END | outgoing push stopped | yes | id, name, target before/after vars, last 10 logs, status | ignored | `controller_push.cpp:145` |
| RECORDING_END / OUTPUT_END | file push or output finished | yes | name, target, connector, bytes, secs, times, reason | ignored | `output.cpp:2915, 2921` |
| CONN_OPEN / CONN_CLOSE | connection accepted/closed | yes | name, ip, connector, url | bool / ignored | `output.cpp:1875, 2202` |
| CONN_PLAY | after connecting to the stream | yes | name, ip, connector, url | bool | `output.cpp:264` |
| PLAY_REWRITE | before connecting | yes | name, ip, connector, url | override stream | `output.cpp:240` |
| USER_NEW | new session (viewers only), **only if no valid JWT** | yes | name, ip, token, connector, url, sessid (+ a 7th `true/false` line at one call site) | bool; the denial is cached with the session | `session.cpp:290-300, 421-426` |
| USER_END | session end | yes | sessid, name, connector, ip, duration, up, down, tags | ignored | `session.cpp:459-496` |
| DEFAULT_STREAM | stream unavailable | yes | default, requested, ip, output, url | override fallback | `output.cpp:401`; `output_http_internal.cpp:344` |
| LIVEPEER_SEGMENT_REJECTED | Livepeer 422 | yes | options, base64 segment... | ignored | `process_livepeer.cpp:446` |

`SYSTEM_CONFIG` is documented in a comment (`controller_storage.cpp:838-839`) but has no
call site. It is never fired.

### 4.3 Push rules and recording (`controller_push.cpp`, `lib/stream.cpp`)

| Item | Detail | Evidence |
|---|---|---|
| Manual | `push_start {stream, target}`; `#tag` or `name+` match many streams | `controller_api.cpp:2022` |
| Target picking | `PUSH_OUT_START` first, then variables, then the `MistOut*` whose `push_urls` matches (`rtmp://`, `srt://`, `whip://`, `/path.mkv`...) | `lib/stream.cpp:914-990` |
| Variables | `$stream`, `$basename`, `$wildcard`, `$pluswildcard`, `$datetime`, `$year`...`$second`, `$wday`, `$yday`, `$week`, `$uuid`, `$source`, custom variables; `${x}` form; 9 levels max | `lib/stream.cpp:164-222` |
| Recording | a push to a file path, e.g. `/rec/$stream_$datetime.mkv`; `RECORDING_END` at the end | `output.cpp:1805-1871, 2915` |
| Auto-push | `auto_push` rules start when the stream becomes active (`doAutoPush` from stats). Fields: `scheduletime`/`completetime` (unix), `start_rule`/`end_rule` `[var, op, value]` (ops 0-1 const, 2/3 ==/!=, 10-13 numeric, 20-23 lexical), `inhibit`. | `controller_push.cpp:244-300, 381-398, 602-633`; `controller_statistics.cpp:248` |
| Retry | `runPushCheck` every 1 s restarts a dead push only if `push_settings.wait` > 0 or the rule has a schedule; `maxspeed` caps starts per cycle | `controller_push.cpp:410-505` |
| Survives restart | active pushes go to SHM `/MstPush` | `controller_push.cpp:156-189, 360-373` |
| Suspected bug | `doAutoPush` skips a rule when `scheduletime < now`, but the comment says "skip if scheduled in the future". The condition looks inverted. | `controller_push.cpp:603-604` (read by me; behaviour not run) |

### 4.4 Sessions and stats

| Item | Detail | Evidence |
|---|---|---|
| Session id | `sha256` over a concatenation chosen by a bitmask: 8 stream, 4 IP, 2 token, 1 protocol | `lib/comms.cpp:680-707` |
| Defaults | viewer 14 (stream+IP+token), input/output 15, unspecified 0, `tknMode` 15 | `lib/defines.h:303-310` |
| Types | id prefix `I` input, `O` output, `U` unspecified, none = viewer | `lib/comms.cpp:359-385`; `controller_statistics.cpp:2113-2119` |
| Per-connection record | now, time, lastsecond, down, up, host (16 B), stream (100), connector (20), pktcount, pktloss, pktretrans, pktrecover, rtt, latency | `lib/comms.cpp:470-485` |
| Liveness | viewer/output counted if seen within 15 s (`STATS_DELAY`), input within 2 s; history kept 600 s (`STAT_CUTOFF`) | `lib/defines.h:121-124`; `controller_statistics.cpp:564-603` |
| `clients` API | `{streams, protocols, fields, time}` → `{time, fields, data[][]}`. Fields: host, stream, protocol, conntime, position, down, up, downbps, upbps, sessid, pkt*, rtt, latency, tags | `controller_statistics.cpp:1363-1503` |
| `totals` API | time series of clients, bps, inputs, outputs, loss % | `controller_statistics.cpp:1922` (response format not verified) |
| Bandwidth | `bandwidth.exceptions` (RFC1918 by default) are excluded from counts; `limit` is reported, not enforced | `controller_storage.cpp:607-613`; `controller_statistics.cpp:211-221` |
| Access log | `--accesslog` (default `LOG`) | `controller.cpp:350-356` |

Prometheus names (`controller_statistics.cpp:2089-2360`): `mist_logs`, `mist_cpu`,
`mist_mem_total`, `mist_mem_used`, `mist_shm_total`, `mist_shm_used`,
`mist_viewseconds_total`, `mist_sessions_count{sessType}`, `mist_bw_total{direction}`,
`mist_bw_other`, `mist_bw_limit`, `mist_packets_total{pkttype}`, `mist_outputs{output}`,
`mist_sessions_total{sessType}`, `mist_sessions{stream,sessType}`, `mist_viewcount{stream}`,
`mist_viewseconds{stream}`, `mist_bw{stream,direction}`, `mist_bw_dir{...}`,
`mist_packets{stream,pkttype}`, `mist_packets_dir{...}`, `mist_rtt{stream,direction}`,
`mist_latency{stream,direction}`, `mist_trigger_count/time/fails{trigger}`. The `.json`
variant is what MistUtilLoad polls.

### 4.5 Access control

| Mechanism | Where | Evidence |
|---|---|---|
| Push IP whitelist + password | stream `source` = `push://1.2.3.0/24@secret` | `output.cpp:2990-3026` |
| Stream keys | `streamkeys {key: stream}`, publish to the key | `lib/stream.cpp:584-609`; `output.cpp:2926-2965` |
| JWT | see 5.6 | `lib/jwt.cpp` |
| Trigger gates | `CONN_OPEN`, `CONN_PLAY`, `USER_NEW`, `STREAM_PUSH`, `PUSH_REWRITE`, `SRT_ACCEPT` | 4.2 |
| Per-stream play IP lists | none found in stream config; only via triggers or the JWT `ipa` claim | agent grep + `jwt.cpp:204` |
| Geo | `\todo` in the JWT code | `lib/jwt.cpp:211` |
| Trusted proxies | `config.trustedproxy` | `controller_storage.cpp:566, 715-724` |

### 4.6 Load balancer (`src/utils/util_load.cpp`, separate binary `MistUtilLoad`)

| Item | Detail | Evidence |
|---|---|---|
| Model | Polls each server's `http://host:4242/<passphrase>.json` | `util_load.cpp:808-830` |
| Score | `cpu + ram + bw + geo + bonus(if the stream is already there)`. Defaults: cpu 500, ram 500, bw 1000, geo 1000, bonus 50. A server over its bandwidth scores 0. | `util_load.cpp:20-24, 187-189, 247-272` |
| Endpoints | `/<stream>` → best host (plain text, or 307 with `?proto=`); `?source=`, `?ingest=`, `?weights=`, `?addserver=`, `?delserver=`, `?lstserver`, `?viewers`, `?streamstats`, `?host=` | `util_load.cpp:458-800` |
| Client geo | `X-Latitude`/`X-Longitude` headers or `lat`/`lon` vars | `util_load.cpp:603-655, 729-800` |
| Use from Mist | `balance:` input (edge asks for an origin, then pulls DTSC) | `input_balancer.cpp:94-155` |

---

## 5. Mechanisms: copy or avoid

| Mechanism | MistServer does | Verdict for Caudal | Evidence |
|---|---|---|---|
| Timestamps | ms integers everywhere; RTP `/ (rate/1000)`; drop if `t < lastms` | **Avoid.** Keep native timescales (done). | `lib/rtp.cpp:993`; `lib/dtsc.cpp:3103` |
| RTMP rollover | compensates 24-bit and 32-bit timestamp wraps within ±0xfff | **Copy** the rule into `caudal-rtmp` tests (compare with the Type-3 delta fix in STATUS) | `output_rtmp.cpp:2482-2507`; `lib/rtmpchunks.cpp:506-535` |
| TS PTS rollover | per-track rollover counter | **Copy** (check `caudal-ts` has it) | `lib/ts_stream.cpp:563-567` |
| Keyframe / fragment | fragment = first key ≥ 1900 ms after the last; audio-only gets a synthetic key every 2047 ms | **Copy** the audio-only key rule if `caudal-core` lacks one (audio-only streams need seek points) | `lib/dtsc.cpp:3135-3203` |
| Jitter keep-away | the viewer start point is `lastms - (jitter + lookahead)`; speed control via `realTime`/`maxSkipAhead` 7500 ms | **Consider** the measured-jitter idea for the LL-HLS live edge | `output.cpp:122-167, 1488-1560`; `lib/dtsc.cpp:3016-3051` |
| TS-HLS | segment = DTSC fragment; target duration = biggest fragment/1000 + 1; window ≥ 4× target | Done: `/hls/{name}/ts.m3u8` (Mist's `index.m3u8` path is LL-HLS in Caudal); keyframe-cut segments near `[hls] segment_ms`, window 6, target = longest segment rounded up. | `output_hls.cpp:105-190` |
| LL-HLS | in CMAF: `partduration` 500 ms default, blocking reload, preload hints, `llhls=0` opt-out, `NOLLHLS` build flag | Caudal is already ahead (200 ms parts, Apple-validated). Nothing to copy. | `output_cmaf.cpp:210-218, 269-320`; `lib/hls_support.cpp:422-566` |
| DASH | LL-DASH, chunked, `availabilityTimeOffset`, `UTCTiming` `time.txt` | Reference if DASH is ever built (backlog) | `output_cmaf.cpp:562-588, 965-987` |
| WebRTC | own stack on mbedtls DTLS + libsrtp2; ICE-lite, host candidates only (no TURN/STUN gathering), NACK buffer, ulpfec, goog-remb, data channels via usrsctp; WS signalling + WHIP/WHEP + WHIP push out | Caudal uses str0m. **Copy two features:** WHIP *push out* (`whip://`) and a JSON/metadata data channel. **Avoid** the hand-written signalling. | `output_webrtc.cpp:15, 180-187, 470-476, 649-706, 742-743, 1880-1936`; `lib/sdp_media.cpp:893, 994-1021`; `output_webrtc_srtp.cpp:57-101` |
| SRT | libsrt; listener/caller/rendezvous; streamid → `stream+id` or must-match; `SRT_ACCEPT` returns socket options | Caudal (rsrt) lacks rendezvous and a per-connection accept hook. Streamid routing exists (`C:crates/caudal-srt/src/connection.rs:102-108`). | `output_tssrt.cpp:417-675`; `input_tssrt.cpp:94-159`; `lib/socket_srt.cpp:173-181` |
| JWT | own implementation (HS/RS/PS/ES); keys from config or JWKS URL with per-key permissions `{input, output, admin, stream}`; claims `nbf`, `exp`, `sub` (wildcards), `ipa`. Not checked: `iss`, `aud`, `iat`. HMAC compared with `!=` (not constant-time). | **Copy** the per-key permission scoping and the `ipa` claim. **Avoid** the optional `exp` and the non-constant-time compare. | `lib/jwt.cpp:41-53, 76-139, 184-214, 263-326`; `lib/defines.h:240-246` |
| Config writes | 60 s lazy, non-atomic | **Avoid** (Caudal is atomic) | 2.1 |
| configMutex | global lock around every API call | **Avoid** (done) | 2.4 |
| Process per viewer | fork + exec per connection | **Avoid** (done) | 1.1 |
| Restart-loop guard | no restart within `attempts × 1 s` of the last start | **Copy** for subsystems that restart (restream, pulls) | `controller_connectors.cpp:239-252` |
| Push restore after restart | SHM `/MstPush` | Caudal restarts are whole-process. Recover from config instead. | `controller_push.cpp:156-189` |

---

## 6. Parity: MistServer → Caudal

States: **has** = I found the code. **partial** = code exists but is narrower (the gap is
named). **missing** = grep found nothing. **won't** = deliberately out of scope (PLAN).
Caudal greps were run over `crates/**/*.rs` on 2026-11-19.

### 6.1 Ingest

| MistServer | Caudal | Evidence (Caudal) |
|---|---|---|
| RTMP publish (incl. Enhanced RTMP HEVC/AV1) | has | `C:crates/caudal-rtmp/src/lib.rs:57`, `demux.rs` (av1) |
| RTMPS publish | missing (rtmps exists only for restream *out*) | `C:crates/caudal-restream/src/url.rs` |
| SRT listener | has | `C:crates/caudal-srt/src/lib.rs:102` |
| SRT caller pull / rendezvous | missing (caller is used for push out only) | `C:crates/caudal-srt/src/lib.rs:10-13` |
| SRT streamid routing | has | `C:crates/caudal-srt/src/connection.rs:102-108` |
| WHIP | has | `C:crates/caudal-webrtc/src/ingest.rs` |
| RTSP pull | has | `C:crates/caudal-rtsp/src/pull.rs` |
| RTSP publish (ANNOUNCE/RECORD into the server) | not verified (server exists; I did not confirm RECORD) | `C:crates/caudal-rtsp/src/server.rs` |
| RIST | missing | grep "rist" hits only `caudal-captions` (an unrelated word match) |
| TS over UDP/multicast in | missing | `udp://` hits only `caudal-multicast` (output) |
| HLS pull input | missing | no m3u8 reader outside the hls/record crates |
| DTSC origin/edge pull | different approach: MoQ edge pull | `C:crates/caudal-cluster/src/pull.rs` |
| File inputs MP4/TS (VoD play, channels) | partial: MP4/MOV + TS via channels; MKV mentioned only in transcode | `C:crates/caudal-channel/src/source.rs:1-10` |
| MKV/FLV/OGG/MP3/AAC/FLAC/WAV files | missing | — |
| Folder / `.pls` playlist | partial: `[[channel]]` items list | `C:crates/caudal/src/config.rs:399-404` |
| `always_on` pull | partial: pulls are configured and always on; on-demand only for cluster edges | `C:crates/caudal-cluster/src/lib.rs` |
| Stream wildcards `name+suffix` | not verified (no `+` handling found by grep) | — |
| Stream keys (key → stream) | missing as a table; JWT tokens cover publish | `C:crates/caudal-auth/src/lib.rs` |
| Push IP whitelist + password | has (CIDR + country, per stream glob) | `C:crates/caudal/src/config.rs:1143-1167`; `C:crates/caudal-access/src/lib.rs:137` |
| V4L2, SDP, H264-exec, MKV-exec | missing | — |

### 6.2 Output

| MistServer | Caudal | Evidence |
|---|---|---|
| LL-HLS (CMAF) | has (ahead: Apple-validated, rendition reports) | `C:crates/caudal-hls/src/packager.rs` |
| TS-HLS | has: `/hls/{name}/ts.m3u8`, v3, TS segments incl. MPEG-2 + MP2, on demand (`docs/HLS.md`) | `C:crates/caudal-hls/src/tsseg.rs` |
| DASH / LL-DASH | missing (backlog) | no `.mpd` |
| WHEP | has | `C:crates/caudal-webrtc/src/egress.rs` |
| WebRTC WebSocket signalling | won't (WHIP/WHEP only) | — |
| WebRTC data channel (metadata/subtitles) | missing | no sctp/datachannel |
| WHIP push out (`whip://`) | missing | — |
| RTMP(S) push out | has | `C:crates/caudal-restream/src/lib.rs` |
| RTMP play (viewers pull RTMP) | missing (not verified otherwise) | `C:crates/caudal-rtmp/src/lib.rs:26` is ingest-only |
| SRT play / SRT push out | has | `C:crates/caudal-srt/src/play.rs`, `push.rs` |
| RTSP server | has (TCP, UDP unicast, RTSPS, multicast) | `C:crates/caudal-rtsp/src/server.rs`, `udp.rs` |
| TS UDP/RTP multicast out | has | `C:crates/caudal-multicast/src/lib.rs` |
| HTTP-TS `/x.ts`, raw TS TCP | missing | — |
| Progressive MP4/fMP4/MKV/WebM over HTTP | missing (clips export MP4 files only) | `C:crates/caudal/src/api.rs` `/api/v1/clips` |
| JPG thumbnails | missing (backlog) | no jpeg |
| JSON metadata / WebVTT / SRT subtitles | partial: WebVTT rendition in LL-HLS | `C:crates/caudal-hls/src/lib.rs:92` |
| MoQ | Caudal only | `C:crates/caudal-moq` |
| OMT | Caudal only | `C:crates/caudal-omt` |
| Embed player / `json_$.js` stream info | partial: `/play/{name}`; no stream-info JSON endpoint for players | routes list |
| Audio-only outputs (MP3/AAC/WAV/OGG/FLAC) | missing | — |
| HDS / Flash / Smooth | won't (dead) | `PLAN.md` |

### 6.3 Platform

| MistServer | Caudal | Evidence |
|---|---|---|
| JSON config + API editing | different: TOML, file is the source of truth; edits via file + reload | `C:crates/caudal/src/config.rs:176-211`, `reload.rs:1` |
| Add/remove stream or protocol via API | missing (read-only `/api/v1/streams`) | `C:crates/caudal/src/api.rs:434-436` |
| Atomic config write | has (per PLAN; I did not read the write path) | not verified in code |
| Hot reload | has (SIGHUP + POST) | `C:crates/caudal/src/reload.rs:1-44` |
| Admin auth | has, stronger (argon2id, OIDC, API tokens) | `C:crates/caudal-admin` |
| `/ws` live log/stream feed | not verified (UI says WebSocket/SSE in PLAN; not grepped) | — |
| Triggers (33, blocking) | **partial, small**: 2 non-blocking events, Standard Webhooks signing | `C:crates/caudal-auth/src/lib.rs:152-167`; `hooks_delivery.rs` |
| Blocking play/publish auth hook (`USER_NEW`/`STREAM_PUSH`) | missing (the only `/hook` route is a test receiver) | `C:crates/caudal-auth/tests/hooks.rs:37` |
| Rewrite hooks (`PUSH_REWRITE`, `PLAY_REWRITE`, `STREAM_SOURCE`) | missing | — |
| Health alerts (`STREAM_BUFFER`-like) | has | `C:crates/caudal-health` |
| JWT with JWKS | has (HS256 secret; RS256/ES256/EdDSA via JWKS; `exp` required) | `C:crates/caudal-auth/src/lib.rs:64, 111-114`; `claims.rs:14` |
| JWT per-key permissions, `ipa` claim | missing (not found) | — |
| Auto-push rules with schedules and conditions | partial: `[[restream]]` (always), `[[record.schedule]]` cron | `C:crates/caudal/src/config.rs:294, 895-905` |
| Recording via push + target variables | partial: `[record]` dir/segments/retention/upload; no `$datetime`-style templates | `C:crates/caudal/src/config.rs:911-923` |
| Load balancer (MistUtilLoad) | partial: origin-edge cluster with `/api/v1/cluster/locate/{name}`; no weighted CPU/RAM/BW/geo scoring | `C:crates/caudal-cluster` |
| Sessions (`clients`, per-viewer bytes, kick by session/tag) | missing: only viewer counts | metric `caudal_viewers` |
| Prometheus | partial: streams, viewers, bytes/frames in, plus subsystem metrics; no bytes-out, sessions, RTT, packet loss, CPU/mem | metrics name list from grep |
| Stream tags | missing | — |
| Custom variables, external writers | missing | — |
| Transcoding processes | has (ffmpeg ladder, rusty_h264) | `C:crates/caudal-transcode` |
| Livepeer, Composer | missing | — |
| Capabilities endpoint | missing | — |
| DVR window | has (`[buffer] window_secs`) | `C:crates/caudal/src/config.rs:155-159` |
| MistServer config import | has | `C:crates/caudal/src/import_mist.rs` |

### 6.4 Biggest gaps, ranked for migrating MistServer users

1. **Blocking triggers.** `USER_NEW`, `STREAM_PUSH`, `PUSH_REWRITE`, `PLAY_REWRITE` and
   `STREAM_SOURCE` are how MistServer deployments do auth and routing. Caudal has 2 async
   events.
2. **Session accounting.** Per-viewer `clients`, `stop_sessions`, `USER_END` billing data,
   bytes-out metrics.
3. **API-driven stream CRUD.** Panels built on MistServer call `addstream`/`deletestream`/
   `push_start` at runtime.
4. **Pull inputs beyond RTSP:** SRT caller, HLS, TS/UDP, RTMP pull.
5. **TS-HLS and DASH** for legacy players; progressive MP4; thumbnails.
6. **Auto-push with schedules and conditions**, and push target templates.

---

## 7. Not verified

| Item | Why | How to check (runtime, not on the owner's laptop) |
|---|---|---|
| Whether `configMutex` can still deadlock at HEAD | lock order read, not proven | Run MistServer under `valgrind --tool=helgrind MistController -c test.json` in a Linux VM while scripting `addstream`, `push_start` and `clients` in a loop |
| The `doAutoPush` `scheduletime` inversion is a real bug | read only | `curl -s localhost:4242/api -d 'command={"push_auto_add":{"stream":"x","target":"/tmp/x.mkv","scheduletime":<now+60>}}'`, publish `x`, check `push_list` before and after the time |
| HTTP trigger timeout | Downloader default not read | point a trigger at `nc -l 9000` (never answers) and time the publish |
| HEVC over WebRTC | in the preference string, not in capa codecs (`output_webrtc.cpp:559-567, 606`) | WHEP with an HEVC source in Safari |
| `totals` response format; Prometheus JSON `loc`/`streams` | not read in full | `curl localhost:4242/api -d 'command={"totals":{}}'`, `curl localhost:4242/<prom>.json` |
| Whether the API `attempts` counter limits brute force | nothing acting on it was found | 20 bad logins, then a good one |
| `LOAD_BALANCE` meson option effect | not traced | `meson configure` in a build dir |
| Complete API command list | agent inventory by grep | read `controller_api.cpp:861-2187` end to end |
| Caudal: RTSP RECORD ingest, `/ws`/SSE feed, stream `+` wildcards, atomic config write | not read in this pass | `grep -n "RECORD\|ANNOUNCE" crates/caudal-rtsp/src/server.rs`; read the config write path |
| Caudal: RTMP rollover and TS PTS rollover handling | not read | `grep -rn "rollover\|wrap" crates/caudal-rtmp crates/caudal-ts` |
| Upstream README claims | the README was deliberately not used as a source | — |

## 8. Resolved on 19 Nov 2026 (source at `ddd9bec` + upstream docs/issues)

| Item | Answer | Evidence |
|---|---|---|
| Blocking HTTP trigger timeout | **5 s of silence per attempt, up to 5 attempts** (`dataTimeout = 5`, `retryCount = 5`), so a dead handler can hold a blocking trigger ~25 s or more before `defaultResponse` is used. The docs only say blocking triggers "wait for a response" and give no number. | `lib/downloader.cpp:17-18, 183-252`; `lib/triggers.cpp:59-80`; [docs](https://docs.mistserver.org/mistserver/integration/triggers/) |
| `scheduletime` in `doAutoPush` | **Real inversion.** The comment says "Skip if scheduled in the future", the code skips when `scheduletime < now`: a push scheduled in the past is skipped, one in the future starts at once. Not reported upstream as far as searched. Runtime check still pending. | `src/controller/controller_push.cpp:603-604` |
| API `attempts` counter vs brute force | **Never read.** Incremented on every unauthorized request and declared once; nothing limits failed logins. | `controller_api.cpp:819`, `controller_api.h:18` (only two hits in the controller) |
| HEVC over WebRTC | **Docs and code disagree.** The WebRTC howto lists H.265; the output's `capa` codecs list H264, VP8, VP9, AV1 only, while the preference default includes HEVC. Track selection works from `capa`, so HEVC is most likely never offered. Runtime check (Safari WHEP) still pending. | `src/output/output_webrtc.cpp:559-567, 606`; [docs](https://docs.mistserver.org/howto/playback/webrtc) |
| `totals` response | `{start, end, fields, interval: [[count, secs], …], data: [[…], …]}`; request takes `streams`, `protocols`, `fields`, `start`, `end` (negative = seconds ago); an array of requests gives an array of answers. | [docs](https://docs.mistserver.org/mistserver/integration/api/calls/totals) |
| `configMutex` deadlock | **Undecided.** #119 (6,857 threads on `configMutex`) was closed 9 Jan 2023 with no comment; #94 "Deadlock on boot" (2022) is still open with no comment. No newer report found. | `gh issue view 119/94 -R DDVTECH/mistserver` |
| Caudal: RTSP RECORD ingest | Not present (no `RECORD`/`ANNOUNCE` in `caudal-rtsp`). | grep |
| Caudal: live `/ws` or SSE feed | Not present in the server; only `caudal-moq` speaks WebSocket. | grep |
| Caudal: `name+` stream wildcards | Not present. | grep |
| Caudal: atomic config write | **Caudal never writes its config** (only `import_mist` writes a new file). PLAN's "atomic write" row describes a path that does not exist yet; it applies when the API/UI starts editing the TOML. | grep `fs::write` |
| Caudal: timestamp rollover | Handled: RTMP 32-bit ms extended per stream (`crates/caudal-rtmp/src/demux.rs:53-63`), TS 33-bit via `TsClock` (`crates/caudal-ts/src/demux.rs:5-36`). | source |
