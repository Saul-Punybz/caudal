//! Origin-edge clustering: several Caudal servers act as one.
//!
//! A publisher pushes to any origin; viewers connect to any edge; an edge
//! pulls a stream from its origins over MoQ when its first viewer asks for
//! it, and republishes it into its own registry so every output protocol
//! serves it unchanged. Design notes: `docs/research/CLUSTER.md`.
//!
//! - [`origin_router`]: `GET /api/v1/cluster/locate/{name}` on origins,
//!   authenticated with the shared secret. The MoQ side is `caudal-moq`'s
//!   output as is; the caller's access gate accepts cluster tokens there
//!   ([`Secret::verify`]).
//! - [`Edge::start`]: installs the pull-on-demand source on an edge's
//!   registry.
//!
//! The moq-dev crates are pinned to the same exact versions as
//! `caudal-moq`, and none of their types appear in this crate's API.

mod convert;
mod pull;
mod timing;
mod token;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use caudal_core::{BufferConfig, Demand, DemandFuture, Registry};

pub use pull::PullMetric;
pub use token::{AUDIENCE, MIN_SECRET_LEN, Secret, SecretError};

/// What an edge needs to pull from its origins.
#[derive(Debug, Clone)]
pub struct EdgeConfig {
    /// Shows up as the token's `iss` and in the origins' logs.
    pub node_id: String,
    pub secret: Secret,
    /// HTTP base URLs of the origins, tried in this order.
    pub origins: Vec<url::Url>,
    /// Stop pulling this long after the last viewer left (also the grace
    /// period for the first viewer to show up in the counts).
    pub idle_timeout: Duration,
    /// End the local stream when no origin has had it for this long.
    pub source_timeout: Duration,
    /// Ring buffer of the republished stream.
    pub buffer: BufferConfig,
}

/// A running edge. Dropping it does not stop pulls already running; the
/// registry keeps the demand source installed.
#[derive(Clone)]
pub struct Edge {
    inner: Arc<pull::Inner>,
}

struct EdgeDemand(Arc<pull::Inner>);

impl Demand for EdgeDemand {
    fn demand<'a>(&'a self, name: &'a str) -> DemandFuture<'a> {
        self.0.demand(name)
    }
}

impl Edge {
    /// Installs the pull-on-demand source on `registry`. Must be called
    /// inside a tokio runtime (pulls are spawned on it).
    pub fn start(registry: &Arc<Registry>, cfg: EdgeConfig) -> Result<Self, String> {
        if cfg.origins.is_empty() {
            return Err("[cluster] role = \"edge\" needs at least one entry in `origins`".into());
        }
        let inner = pull::Inner::new(
            registry,
            pull::Settings {
                node_id: cfg.node_id,
                secret: cfg.secret,
                origins: cfg.origins,
                idle_timeout: cfg.idle_timeout,
                source_timeout: cfg.source_timeout,
                buffer: cfg.buffer,
            },
        );
        registry.set_demand(Arc::new(EdgeDemand(inner.clone())));
        Ok(Self { inner })
    }

    /// Active pulls, sorted by stream name.
    pub fn pulls(&self) -> Vec<PullMetric> {
        self.inner.metrics()
    }

    /// Appends the edge's Prometheus metrics to `out`.
    pub fn render_metrics(&self, out: &mut String) {
        let pulls = self.pulls();
        let _ = writeln!(out, "# HELP caudal_cluster_pulls Streams this edge is pulling from an origin.");
        let _ = writeln!(out, "# TYPE caudal_cluster_pulls gauge");
        for p in &pulls {
            let _ =
                writeln!(out, "caudal_cluster_pulls{{stream=\"{}\",origin=\"{}\"}} 1", esc(&p.stream), esc(&p.origin));
        }
        let _ = writeln!(
            out,
            "# HELP caudal_cluster_pull_setup_seconds Viewer demand (or origin loss) to first frame on this edge."
        );
        let _ = writeln!(out, "# TYPE caudal_cluster_pull_setup_seconds gauge");
        for p in &pulls {
            if let Some(d) = p.setup {
                let _ = writeln!(
                    out,
                    "caudal_cluster_pull_setup_seconds{{stream=\"{}\",origin=\"{}\"}} {:.3}",
                    esc(&p.stream),
                    esc(&p.origin),
                    d.as_secs_f64()
                );
            }
        }
        let _ = writeln!(out, "# HELP caudal_cluster_failovers_total Times a pull lost its origin and moved on.");
        let _ = writeln!(out, "# TYPE caudal_cluster_failovers_total counter");
        for p in &pulls {
            let _ = writeln!(out, "caudal_cluster_failovers_total{{stream=\"{}\"}} {}", esc(&p.stream), p.failovers);
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[derive(Clone)]
struct OriginState {
    registry: Arc<Registry>,
    secret: Secret,
    node_id: String,
}

/// `GET /api/v1/cluster/locate/{name}` with `Authorization: Bearer <cluster
/// token>`: `200 {"stream", "node_id", "tracks"}` when `name` is live here,
/// `404` when not, `401`/`403` without a valid token. Merge into an
/// origin's HTTP app. The route stays outside the admin login (the cluster
/// token is its credential).
pub fn origin_router(registry: Arc<Registry>, secret: Secret, node_id: String) -> axum::Router {
    axum::Router::new().route("/api/v1/cluster/locate/{name}", get(locate)).with_state(OriginState {
        registry,
        secret,
        node_id,
    })
}

async fn locate(State(st): State<OriginState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "cluster token required")
            .into_response();
    };
    let Some(edge) = st.secret.verify(token) else {
        tracing::info!(stream = %name, "cluster: locate refused (bad token)");
        return (StatusCode::FORBIDDEN, "bad cluster token").into_response();
    };
    match st.registry.get(&name) {
        Some(s) if !s.is_ended() => {
            tracing::debug!(stream = %name, %edge, "cluster: located");
            let body = serde_json::json!({ "stream": name, "node_id": st.node_id, "tracks": s.tracks().len() });
            ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "not live here").into_response(),
    }
}
