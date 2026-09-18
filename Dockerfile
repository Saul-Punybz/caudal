# Builder stage
FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p caudal --target x86_64-unknown-linux-musl && \
    cp /build/target/x86_64-unknown-linux-musl/release/caudal /caudal

# Final stage
FROM scratch

COPY --from=builder /caudal /caudal

EXPOSE 1935 8080

ENTRYPOINT ["/caudal"]
