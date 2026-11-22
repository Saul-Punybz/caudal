# Stream keys and token limits

Two ways to say who may publish or play, on top of `[auth]` tokens:

- **Stream keys.** An encoder publishes with a secret key where the stream
  name goes. Caudal maps the key to the real stream, so the stream name is
  never the secret. This is MistServer's `streamkeys` table.
- **Token limits.** A JWT can be bound to the viewer's IP (the `ipa`
  claim), and each signing key can be limited in what its tokens may grant
  (MistServer's per-key `input` / `output` / `stream` permissions).

Source: `docs/research/MISTSERVER-REVERSE.md` §4.5, §5 (JWT row) and §6.

## Stream keys

```toml
[auth]
# Optional: more keys in their own file, re-read on every reload.
stream_keys_file = "/etc/caudal/stream-keys.toml"
# Optional: a stream that has a publish key takes nothing else.
stream_keys_only = false

[[auth.stream_keys]]
key = "sk_live_main_8f3a9c2e71d4"   # 16 to 128 of A-Z a-z 0-9 - _
stream = "live-main"
publish = true                      # default true
play = false                        # default false
```

The keys file has the same entries under `[[stream_keys]]`:

```toml
[[stream_keys]]
key = "sk_backup_0b6d42f1c9e8a7"
stream = "live-backup"
```

### Publishing with a key

The key goes where the stream name goes, on every protocol:

| Protocol | URL |
|---|---|
| RTMP / RTMPS | `rtmp://host/live/<key>` (OBS: the key goes in "Stream Key") |
| SRT | `srt://host:9000?streamid=publish/<key>` or `#!::r=<key>,m=publish` |
| WHIP | `POST /whip/<key>`, or `POST /whip/<stream>` with `Authorization: Bearer <key>` |
| MoQ | `/moq/publish/<key>` |

A key with `play = true` also plays: `/hls/<key>/index.m3u8`,
`rtsp://host:8554/<key>`, `srt://host:9000?streamid=play/<key>`,
`rtmp://host/live/<key>`, WHEP `POST /whep/<key>`. MoQ play refuses a key
(it cannot serve a renamed stream, same as `[auth.rewrite]`).

In the token slot (`?token=`, `Authorization: Bearer`), a key only counts
for its own stream: `Bearer <key>` on `/whip/live-main` works when the key
is for `live-main`, and is just a bad token anywhere else.

### Order

`Registry::resolve`, which every protocol calls, asks the keys first:

1. **Stream key.** If the name (or the token, for its own stream) is a key
   that allows this action, the request goes to the key's stream.
   `[auth.rewrite]` is skipped, as MistServer skips `PUSH_REWRITE` for a
   key. The key stands in for the `[auth]` token.
2. Otherwise `[auth.rewrite]` routes the name as before. With
   `stream_keys_only = true`, a publish whose final name has a publish key
   is refused here: the key is the only way in.
3. On the final name, either way: kicks, `[[access.rules]]` (CIDR,
   country) and `[auth.hook]`. A keyed request reaches `[auth.hook]` with
   no token, so the key never leaves the server.

One difference from MistServer: there, a key also skips the stream's
`push://` IP whitelist and the `STREAM_PUSH` trigger. In Caudal, network
rules and `[auth.hook]` still apply to a keyed request. A key proves who
the encoder is; it does not override where it is allowed to come from.

Without `stream_keys_only`, a publish under the bare stream name still
works whenever `[auth]` and `[[access.rules]]` allow it (with no `[auth]`
keys, anyone can publish to `live-main` directly). Turn it on, or require
tokens, if the key must be the only way to publish.

### Secrets

- Keys are kept as SHA-256 digests. A lookup hashes what the client sent
  and compares it with every digest in constant time, without an early
  exit.
- Keys are never logged. A refused request that came with a key logs
  `stream=****`. Config errors name the stream, never the key. `Debug`
  output masks them.
- `GET /api/v1/stream-keys` lists which streams have keys, what each key
  allows and where it comes from (`config` or `file`). The key is always
  `"****"`. Keys are added and removed in the config or the keys file.
- `/metrics` has `caudal_stream_key_requests_total{action, result}`:
  `hit` (came with a key), `miss` (the name was not a key), `refused` (the
  stream takes its key only). No key or stream name is ever a label.
- Keep the config and the keys file readable only by Caudal (`chmod
  600`).

### Rotating keys

Edit the config or the keys file and reload (`kill -HUP`, or `POST
/api/v1/config/reload`). A changed keys file counts as a changed `[auth]`
even when the config itself did not change. An invalid file is refused and
the running keys stay. Like every `[auth]` change, a reload governs the
next request only: an encoder already publishing with an old key keeps
publishing until it disconnects (kick it with `DELETE /api/v1/sessions`
for viewers, or restart the encoder).

## Token limits

### `ipa`: bind a token to an address

```json
{ "sub": "live-main", "act": "play", "exp": 1767225600, "ipa": "203.0.113.7" }
```

`ipa` is an address, a CIDR (`"10.0.0.0/8"`), or an array of either. The
request is refused (403) when the client address is outside all of them.
An IPv4-mapped IPv6 address (`::ffff:203.0.113.7`) counts as IPv4. A
malformed entry makes the token invalid. A token without `ipa` works from
any address, as before.

The address is the one every other check uses:

- HLS, WHIP/WHEP, recordings and the other HTTP routes: the TCP peer,
  unless the peer is in `[server] trusted_proxies`; then the right-most
  `X-Forwarded-For` entry that is not itself a trusted proxy. Behind a
  reverse proxy, list it in `trusted_proxies`, or every viewer has the
  proxy's address and only `ipa` tokens for the proxy work.
- RTMP, SRT, RTSP: the transport peer. There is no forwarding header.
- MoQ: the server library does not expose the peer address yet, so a
  token with `ipa` is always refused over MoQ.

When a protocol has no address to give, a token with `ipa` is refused.
It is never waved through.

### Limits per signing key

Each key that signs tokens can be limited in what its tokens may grant,
whatever the token claims. A token must pass both its own `sub` / `act`
and its key's limits.

```toml
[auth]
secret = "at-least-32-characters-of-random-secret"

[auth.secret_permissions]
publish = false            # default true
play = true                # default true
streams = ["live-*"]       # exact names or prefix*; empty = any stream
```

With `jwks_url`, limits go by key id (`kid` in the token header):

```toml
[auth]
jwks_url = "https://auth.example.com/.well-known/jwks.json"

[[auth.key_permissions]]
kid = "encoders-2026"
publish = true
play = false
streams = ["live-*"]
```

A `kid` with no entry is limited only by its tokens' claims, as before.
The limits are checked against the stream actually requested (after any
rewrite), not against the token's `sub` pattern.

MistServer's third permission, `admin`, has no equivalent: Caudal's admin
API uses its own logins and API tokens (`[admin]`).

## Importing from MistServer

`caudal import-mist` turns `streamkeys` into `[[auth.stream_keys]]`
(publish only, as MistServer's keys are). A key shorter than 16
characters, or with characters other than letters, digits, `-` and `_`,
is skipped with a note: give that encoder a new key. MistServer's `jwks`
entries are not imported (Caudal takes one `secret` or one `jwks_url`);
the report says so and points here for the permission settings.

## Not done

- A JWT as the stream name. MistServer also accepts a publish whose stream
  name *is* a JWT with the input permission, and uses its `sub` as the
  stream. In Caudal, send the token as `?token=` or a bearer token instead.
- Keys managed over the API. The listing is read-only; edit the files.
