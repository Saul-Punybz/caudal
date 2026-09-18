# Caudal

A media server in Rust: one publisher in, many viewers out, over the protocols
people actually use in 2026. It is a rewrite of
[MistServer](https://github.com/DDVTECH/mistserver) that keeps its breadth and
drops its crash classes.

**Status:** early. The core live buffer is done and tested; no protocol is wired
up yet. See [PLAN.md](PLAN.md).

## Build

```
cargo test --workspace
cargo build --release
```

## License

MIT OR Apache-2.0, at your option.
