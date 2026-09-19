# caudal (Helm chart)

Deploys the [Caudal](../../../README.md) media server on Kubernetes: a
Deployment running the `FROM scratch` image built by the repo's
`Dockerfile`, a ConfigMap holding `caudal.toml`, a Service for the TCP
protocols (HTTP/API/UI, RTMP), and a second Service (or `hostNetwork`) for
the UDP ones (WebRTC, SRT, MoQ).

## Quick start

Caudal refuses to listen on a non-loopback address without an admin login
(`crates/caudal-admin`'s `check_exposure`), and this chart always binds
`0.0.0.0` inside the pod — so it needs a password hash before it will
render at all:

```sh
printf '%s\n' 'your password' | docker run --rm -i ghcr.io/saul-punybz/caudal:0.1.0 hash-password
# or, from a checkout: printf '%s\n' 'your password' | cargo run -p caudal -- hash-password

helm install caudal deploy/helm/caudal \
  --set admin.passwordHash='$argon2id$v=19$...' \
  --set webrtc.publicIps='{203.0.113.9}'
```

Without `admin.passwordHash` or `admin.existingSecret` set, `helm
lint`/`helm template`/`helm install` all fail immediately with a message
explaining what to set — this is intentional (see "Admin login" below),
not a bug.

## Networking

Caudal speaks two shapes of protocol, and this chart exposes them
differently:

- **TCP** (HTTP/HTTPS API+UI+LL-HLS+WHIP/WHEP signaling, RTMP) — a normal
  `Service` (`ClusterIP` by default; put an Ingress or a LoadBalancer in
  front like any other TCP server).
- **UDP media that carries its own address inside the protocol**: WebRTC
  (ICE), SRT, MoQ/QUIC. For WebRTC specifically, the address Caudal
  *advertises* to a browser (`[webrtc] public_ips`) has to be an address
  that browser can actually route a UDP packet to. A ClusterIP is never
  that address; a cloud LoadBalancer's VIP usually isn't either once
  SNAT/NAT is involved, unless you tell Caudal to advertise it.

Two ways to run the UDP side (see `values.yaml`'s `udp.*`):

1. `udp.service.enabled: true` (default): a `LoadBalancer` Service fronts
   the UDP ports. Get its external IP after it's assigned and feed it
   back in:
   ```sh
   kubectl get svc caudal-udp -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
   helm upgrade caudal deploy/helm/caudal --reuse-values --set webrtc.publicIps='{<that ip>}'
   ```
   Only correct with `replicaCount: 1` — a Service load-balances per
   5-tuple, not per packet, and only one pod can be "the pod behind that
   public IP" at a time.
2. `udp.hostNetwork: true`: the pod uses the node's network namespace
   directly — no Service, no SNAT, no chicken-and-egg IP lookup. Set
   `webrtc.publicIps` to that node's real public IP. This is the same
   approach self-hosted WebRTC/SRT/TURN servers commonly use in
   Kubernetes (e.g. coturn, ingress-nginx's optional host networking).
   Also effectively one caudal per node.

Either way, **`replicaCount` above 1 does not give you a cluster**: each
replica is an independent server with its own in-memory ring buffer and
(if `[record]` is on) its own recordings directory. Origin/edge clustering
is on Caudal's roadmap (see the repo's `PLAN.md`) but not implemented.

## Admin login (`[admin]`)

`admin.passwordHash` is the output of `caudal hash-password` (an
argon2id hash, not the raw password). The chart puts it in a Secret it
creates (`<release>-admin`), never in the ConfigMap. If you manage
secrets yourself (sealed-secrets, external-secrets, vault-agent, ...), set
`admin.existingSecret` (+ `admin.existingSecretKey`, default
`password-hash`) instead and skip `admin.passwordHash` entirely.

Since the image is `FROM scratch` (no shell to run a merge script in), a
small `busybox` `initContainer` concatenates the ConfigMap's `caudal.toml`
with a `[[admin.users]]` block built from the Secret into a shared
`emptyDir`, and the main container reads that merged file
(`--config /work/caudal.toml`) — `caudal` itself only ever takes one
config file.

To run genuinely open (no login at all — only ever appropriate behind an
already-authenticated network boundary), set `admin.enabled: false` *and*
`admin.allowUnauthenticated: true`; the second flag has to be explicit or
the render still fails, same as `caudal-admin`'s own refusal.

## Recording (`[record]`)

`persistence.enabled: true` creates a PVC mounted at
`persistence.mountPath` (default `/data/recordings`) and sets `[record]
dir` to match; `record.enabled: true` turns recording on in the config.
Without persistence, recordings live on the pod's ephemeral filesystem and
are lost on every restart.

## Everything else in `caudal.toml`

Ports, HLS timing, the buffer window, WebRTC/SRT/MoQ toggles, and
recording are values (see `values.yaml`). Sections this chart doesn't
model yet — `[tls]`, `[auth]`, `[hooks]`, `[health]`, `[transcode]`,
`[[channel]]`, `[[restream]]`, `[rtsp]` — can be added verbatim via
`extraConfig` (see `caudal.example.toml` at the repo root for their
syntax); it's appended to the generated file as-is.

## Verifying this chart

```sh
helm lint deploy/helm/caudal -f my-values.yaml
helm template caudal deploy/helm/caudal -f my-values.yaml | kubectl apply --dry-run=client -f -
```

`deploy/kubernetes/example.yaml` is a checked-in `helm template` snapshot
for reading without installing Helm; install/upgrade from the chart
itself, not by hand-editing that file.

### What was and wasn't verified when this chart was written

- `helm lint` and `helm template` (including the `admin.passwordHash` /
  `admin.existingSecret` / `admin.enabled=false` branches, `record` +
  `persistence`, and `udp.hostNetwork`) — clean.
- The rendered `caudal.toml`, merged the way the `merge-config`
  initContainer does, passes `caudal check` and a real `caudal` process
  started from it answers `/healthz` and `/readyz`.
- **Not verified**: an actual scheduled Pod on a real (or `kind`) cluster —
  this machine's Docker daemon was unreachable (`docker version` failing
  with a 500) when this chart was built, so the initContainer merge step,
  the securityContext (non-root, read-only root filesystem), and the
  probes have not been exercised inside an actual container runtime. Run
  the two commands above plus a real `helm install` on a working cluster
  before depending on this in production.
