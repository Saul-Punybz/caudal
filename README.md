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

Live captions (local Whisper speech-to-text, `[captions]`) are the
`captions` cargo feature, on by default. `cargo build --release -p caudal
--no-default-features` builds without them: 2.9 MiB smaller on
x86_64-linux-musl (37.3 to 34.3 MiB) and 1.9 MiB on aarch64 (31.3 to
29.4 MiB), and a config that sets `[captions]` is rejected. See
[docs/research/CAPTIONS.md](docs/research/CAPTIONS.md).

## Operate

`caudal doctor` diagnoses a setup before you go live: config, ports, NAT,
TLS, clock, tools, and (with `--url`) a running server's stream codecs.
Each check prints OK/WARN/FAIL with a fix; see `caudal doctor --help`.

Migrating from MistServer: `caudal import-mist <mistserver.conf|config.json>`
reads its config and writes a `caudal.toml` plus a report of every setting
translated, approximated, or with no Caudal equivalent (user accounts, for
one — hash new passwords with `caudal hash-password`). It fails without
writing anything if the generated file doesn't pass `caudal check`.

## Deploy

A Helm chart is at [deploy/helm/caudal](deploy/helm/caudal) (README there);
a rendered example for `kubectl apply` without Helm is at
[deploy/kubernetes/example.yaml](deploy/kubernetes/example.yaml).

## License

MIT OR Apache-2.0, at your option.
