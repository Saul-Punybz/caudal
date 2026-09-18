test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

e2e:
    CAUDAL_E2E=1 cargo test -p caudal --test e2e -- --test-threads=1

build:
    cargo build --release -p caudal

docker:
    docker build -t caudal .

run:
    cargo run -p caudal -- --config caudal.example.toml

# Build the web UI into crates/caudal-ui/dist (embedded in the binary).
ui:
    cd ui/app && npm ci && npm run build

# Browser playback test (Chromium + WebKit) against the release binary.
browser: build
    cd tests/browser && npm ci && npx playwright install chromium webkit firefox && npm test
