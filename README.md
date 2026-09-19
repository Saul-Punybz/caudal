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

## Operate

`caudal doctor` diagnoses a setup before you go live: config, ports, NAT,
TLS, clock, tools, and (with `--url`) a running server's stream codecs.
Each check prints OK/WARN/FAIL with a fix; see `caudal doctor --help`.

## Deploy

A Helm chart is at [deploy/helm/caudal](deploy/helm/caudal) (README there);
a rendered example for `kubectl apply` without Helm is at
[deploy/kubernetes/example.yaml](deploy/kubernetes/example.yaml).

## License

MIT OR Apache-2.0, at your option.
