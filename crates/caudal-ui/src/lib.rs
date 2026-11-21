//! Serves the web UI built from `ui/app` (copied into `dist/` by
//! `just ui`). Unknown paths fall back to `index.html` so client-side
//! routes like `/streams/main` load the app.

use axum::Router;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "dist/"]
struct Dist;

fn file(path: &str) -> Option<Response> {
    let f = Dist::get(path)?;
    let mime = f.metadata.mimetype().to_owned();
    // Vite fingerprints everything under assets/; index.html must revalidate.
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    Some(([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache.to_owned())], f.data).into_response())
}

async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(r) = file(if path.is_empty() { "index.html" } else { path }) {
        return r;
    }
    // Paths with an extension are real files that do not exist.
    if path.rsplit('/').next().is_some_and(|last| last.contains('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    file("index.html").unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// Merge LAST: it is the fallback for every path no other router claims.
pub fn router() -> Router {
    Router::new().fallback(serve)
}

#[cfg(test)]
mod tests {
    use super::Dist;

    /// The committed build is complete: index.html is embedded and every
    /// `/assets/...` file it references is embedded too, so a rebuild that
    /// left stale or missing files in `dist/` fails here, not in a browser.
    #[test]
    fn index_html_references_only_embedded_assets() {
        let index = Dist::get("index.html").expect("dist/index.html is embedded");
        let html = std::str::from_utf8(&index.data).expect("utf-8");
        let refs: Vec<&str> =
            html.split('"').filter_map(|s| s.strip_prefix('/')).filter(|s| s.starts_with("assets/")).collect();
        assert!(!refs.is_empty(), "index.html references no assets");
        for path in refs {
            assert!(Dist::get(path).is_some(), "{path} is referenced by index.html but not embedded");
        }
    }
}
