# caudal-tls notes

## Design

- **Crypto provider: `ring`**, not `aws-lc-rs`. Keeps the static musl build
  simple (no cc/cmake toolchain needed to cross-compile a second provider);
  Caudal doesn't need FIPS or aws-lc-rs's perf edge here. `rustls`,
  `tokio-rustls` and `rustls-acme` are all built with `default-features =
  false` plus an explicit `ring` feature so nothing pulls in aws-lc-rs by
  accident. `rustls::crypto::ring::default_provider().install_default()` is
  called once at the top of `serve()`.
- **hyper-util over axum-server.** `hyper_util::server::conn::auto::Builder`
  gives ALPN-driven h1/h2 dispatch, `hyper_util::server::graceful::GracefulShutdown`
  gives per-connection graceful drain, and `hyper_util::service::TowerToHyperService`
  lets `axum::Router` (which already implements `tower::Service`) serve
  directly — no need for `axum-server`'s own abstractions, and this keeps
  full control over ALPN order and where `ConnectInfo` gets inserted.
- **`ConnectInfo` is inserted per connection**, not per request: each
  accepted connection gets `app.clone().layer(Extension(ConnectInfo(addr)))`
  before being handed to hyper, mirroring what
  `axum::serve(..).into_make_service_with_connect_info` already does for the
  plain-HTTP listener in `crates/caudal/src/main.rs`.
- **Files reload is poll-based** (mtime, every 5s), per the batch brief.
  `FileCertResolver` (src/resolver.rs) swaps an `Arc<CertifiedKey>` behind
  `arc_swap::ArcSwap`; a bad reload logs at error and keeps serving the
  previous cert. To avoid hammering a persistently-bad file, the poller
  advances its "last seen mtime" on both success and failure, so a failed
  reload only retries once the file changes again.
- **ACME (`CertSource::Acme`)** wraps `rustls_acme::AcmeState`. We don't use
  its `axum`/`tower` integration or `TokioIncoming` convenience wrapper — we
  build the accept loop ourselves (same one `Files` uses, via the
  `accept::Handshake` trait) so both cert sources share graceful shutdown,
  the handshake timeout, and per-connection logging. Per connection: peek
  the ClientHello with `tokio_rustls::LazyConfigAcceptor`; if it's a
  TLS-ALPN-01 probe (`rustls_acme::is_tls_alpn_challenge`), finish the
  handshake with `state.challenge_rustls_config()` and close — nothing to
  serve over HTTP on that connection. Otherwise, finish with a config built
  from `state.default_rustls_config()` but with `alpn_protocols` overridden
  to `[h2, http/1.1]` (the crate's own default has no ALPN set).

## Untested

- **`CertSource::Acme` has no integration test and has never been run
  against a real ACME CA** (not even Let's Encrypt's staging environment) —
  doing so needs a publicly resolvable domain pointed at this machine on
  port 443, which isn't available here. The code follows `rustls-acme`
  0.15.4's own `examples/low_level_tokio.rs` pattern closely (same
  `LazyConfigAcceptor` / `is_tls_alpn_challenge` / `challenge_rustls_config`
  / `default_rustls_config` calls), and `cargo build`/`clippy` pass, but
  nobody has watched it complete an actual order. Whoever wires this up
  against a real domain should watch the first run's logs (`acme event` /
  `acme error`) closely and expect to debug the happy path.
- Files-path reload is tested with a `tokio::time::sleep` before rewriting
  the cert to dodge a same-second mtime collision on a coarse filesystem
  clock; that's a real (if unlikely) flakiness source on filesystems with
  1-second mtime resolution under heavy load.

## Test run

`cargo test -p caudal-tls -- --test-threads=1`: 3/3 integration tests green
(`serves_h2_over_alpn_with_h1_fallback`, `hot_reloads_certificate_without_restart`,
`graceful_shutdown_returns_within_cap`), ~5s total. `curl -sk --http2 -o
/dev/null -w '%{http_version}' https://127.0.0.1:<port>/ping` (run inside the
first test via `spawn_blocking`, skipped if curl isn't on PATH) reports `2`.
`cargo clippy -p caudal-tls --all-targets -- -D warnings`: clean.
