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
