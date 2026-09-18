# caudal-auth notes

## Design choices
- `jsonwebtoken` built with `default-features = false, features = ["rust_crypto", "use_pem"]`.
  Plain `jsonwebtoken = "11"` compiles but panics at runtime (`CryptoProvider::get_default`)
  because neither `rust_crypto` nor `aws_lc_rs` is enabled by default.
- HS256: `Validation::algorithms` is pinned to `[HS256]` and the header's `alg` is checked
  before even building the key, so `alg: none` and cross-algorithm tokens are rejected by
  construction, not by a denylist.
- JWKS: allowed algorithms are pinned to `{RS256, ES256, EdDSA}` — an HS256 token can never
  be accepted through a JWKS-backed `Authorizer`, which is the algorithm-confusion case that
  matters most (a symmetric secret forged from a public key).
- JWKS cache (`src/jwks.rs`) is a small hand-rolled cache as REUSE.md notes nothing fresher
  exists: keyed by `kid`, background task fetches once at spawn and then every `refresh`;
  an unknown `kid` triggers at most one extra fetch per 30s (`std::sync::Mutex<Option<Instant>>`
  gate). On fetch failure the last good key set is kept and a warning is logged; never panics
  on a malformed JWKS document.
- `Authorizer::new`/`JwksCache::spawn` guard `tokio::runtime::Handle::try_current()`: outside
  a runtime the cache just starts empty (logged) instead of panicking on `tokio::spawn`.
- `validation.validate_aud = false` — we never issue or expect `aud`; jsonwebtoken's default
  behavior rejects a token that carries an unexpected `aud` claim, which would be a surprising
  failure mode for a claim this crate never asked for.
- Webhooks: 1 initial attempt + up to 3 retries at 1s/5s/25s (4 total attempts max), 5s
  per-request timeout, 5xx/network-error retries, 4xx stops immediately. Signed with
  `standardwebhooks::Webhook::sign` (Standard Webhooks v1).

## Not supported / out of scope
- P384/RS384/RS512/PS*/ES384 are not accepted even though `jsonwebtoken` supports them —
  only the three algorithms named in the batch brief (RS256, ES256, EdDSA) are allowed for
  JWKS, and only HS256 for a shared secret.
- No `aud`/`iss` claim support (not part of the fixed claim shape: `sub`, `act`, `exp`, `nbf`).
- The unknown-`kid` rate limiter uses `std::time::Instant`, not the tokio clock, so it cannot
  be fast-forwarded with `tokio::time::pause()` in tests; the integration test instead proves
  "exactly one refetch" by making two checks back-to-back within the same real-time window.
- Webhook retry/backoff tests run on real time (not `tokio::time::pause`): pausing the clock
  on the same `current_thread` runtime that also hosts the loopback axum receiver caused the
  request's own timeout to race the paused clock and fail spuriously. The 5xx-retry test costs
  ~6s wall clock as a result.
