# ESTADO — Caudal

**Última actualización:** 17 sept 2026

## Qué es
Reescritura open source de MistServer en Rust. Plan completo y evidencia en `PLAN.md`.

## Dónde vamos
- **M0 hecho:** `crates/caudal-core`, con el modelo de medios (tracks, frames en su reloj nativo) y el buffer en vivo (un publicador, muchos espectadores; el espectador lento salta al keyframe, memoria acotada por tiempo y bytes). 13 tests pasan, clippy limpio.
- `crates/caudal`: binario vacío todavía.

## Hallazgo 17 sept (noche)
SRT y RIST **sí existen en Rust puro**: `rsrt` (cesbo, verificado contra libsrt 1.5.6: 668 tests + interop) y `rist-core` (wavey-ai, Simple+Main, interop contra librist). Detalle en `REUSE.md`. Ya no hay que portar gosrt ni libRIST.

## Qué sigue
**M1: el servidor.** Config TOML, API HTTP (axum) con la lista de streams y sus stats, `/metrics` Prometheus y apagado limpio. Después, **M2+M3:** RTMP entra y LL-HLS sale, el primer camino que se ve en un navegador.

## Decisiones tomadas
- Nombre **Caudal** (libre en crates.io al 17 sept 2026).
- Licencia MIT OR Apache-2.0. MistServer es Unlicense (dominio público), así que no hay restricción.
- Un solo proceso async (tokio), no un proceso por conexión como MistServer.
- HDS, Flash y Smooth Streaming no se portan.

## Cómo correr
```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Fuente de referencia
Clon de MistServer: `git clone --depth 1 https://github.com/DDVTECH/mistserver`.
