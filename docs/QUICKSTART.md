# Quickstart

From download to a live stream in about ten minutes: install Caudal, write
a minimal config, start it, confirm the setup with `caudal doctor`. For
"OBS is live and a browser plays it," continue with [OBS.md](OBS.md).

## 1. Get the binary

### Release tarball (Linux)

Each `v0.1.0` release ships static, dependency-free binaries for two
targets:

- `caudal-0.1.0-x86_64-unknown-linux-musl.tar.gz`
- `caudal-0.1.0-aarch64-unknown-linux-musl.tar.gz`

Download the one matching your machine from the release page, verify it
against the release's `SHA256SUMS`, and unpack it:

```
tar xzf caudal-0.1.0-x86_64-unknown-linux-musl.tar.gz
./caudal --version
```

Each tarball also contains `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`,
and `caudal.example.toml` to start from.

### Docker

```
docker pull ghcr.io/saul-punybz/caudal:0.1.0
```

The image is `FROM scratch` (no shell) and, like the binary, refuses to
bind to anything but loopback until you configure `[admin]` — see step 2.
Run it with your config mounted in:

```
docker run --rm -p 1935:1935 -p 8080:8080 \
  -v "$PWD/caudal.toml:/caudal.toml" \
  ghcr.io/saul-punybz/caudal:0.1.0 --config /caudal.toml
```

### macOS: build from source

There is no macOS release binary yet (CI only produces the two Linux musl
targets above). Build with Cargo:

```
git clone https://github.com/Saul-Punybz/caudal.git
cd caudal
cargo build --release -p caudal
./target/release/caudal --version
```

Requires a recent stable Rust toolchain (`rustup` is the easiest way to
get one). `cargo test --workspace` runs the test suite if you want to
verify the build first.

## 2. Write a minimal `caudal.toml`

Binding only to `127.0.0.1` needs no `[admin]` section. To reach the
server from another machine (or from inside a container, as above), add
one admin user first:

```
printf '%s\n' 'a passphrase at least 8 characters long' | ./caudal hash-password
```

This reads the password from stdin (one line, minimum 8 characters) and
prints an argon2id PHC hash on stdout. Put it in the config:

```toml
[server]
http_bind = "0.0.0.0:8080"

[admin]
[[admin.users]]
name = "ana"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$..."   # from hash-password above

[rtmp]
bind = "0.0.0.0:1935"
app = "live"

[hls]
part_ms = 200
segment_ms = 2000
```

`[hls] segment_ms = 2000` is the default and matches the keyframe interval
OBS.md asks you to set. `caudal.example.toml` in the repo (or in the
tarball) documents every other section — TLS/ACME, JWT auth, recording,
WebRTC, MoQ, clustering, and more — each one commented out with what it
does.

Validate the file before starting the server:

```
./caudal check caudal.toml
```

## 3. Run it

```
./caudal --config caudal.toml
```

or, with the example config as-is (loopback only, no admin needed):

```
./caudal --config caudal.example.toml
```

On success you'll see RTMP listening on `:1935` and HTTP on `:8080` in the
logs.

## 4. Confirm the setup with `caudal doctor`

```
./caudal doctor --config caudal.toml
```

`doctor` checks the config, whether its TCP/UDP ports are free, WebRTC NAT
reachability, TLS certificates and ACME domains (if configured), the
system clock (with `--online`), `ffmpeg` availability, and the open-file
limit — printing one `OK`/`WARN`/`FAIL` line per check with a fix for
anything that isn't `OK`. Exit code is 0 if everything is `OK`, 2 if only
`WARN`s, 1 if anything `FAIL`s. Add `--url http://host:8080` once the
server is running and a stream is live to also flag codecs a browser
can't play over the outputs you configured.

Once `doctor` is clean, go to [OBS.md](OBS.md) to publish from OBS and
watch it in a browser.
