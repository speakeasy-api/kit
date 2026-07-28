use axum::{
    Router,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};

const INDEX: &str = include_str!("index.html");
const APP_JS: &str = include_str!("app.js");
const APP_CSS: &str = include_str!("app.css");
const CSP: &str = "default-src 'self'; base-uri 'none'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; img-src 'self'; object-src 'none'; script-src 'self'; style-src 'self'";

pub fn routes() -> Router {
    Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/app.js", get(javascript))
        .route("/ui/app.css", get(stylesheet))
}

async fn index() -> Response {
    asset(INDEX, "text/html; charset=utf-8")
}

async fn javascript() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

async fn stylesheet() -> Response {
    asset(APP_CSS, "text/css; charset=utf-8")
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn serves_the_ui_shell_and_assets_without_embedding_credentials() {
        for (path, content_type, marker) in [
            ("/ui", "text/html; charset=utf-8", "Kit Workbench"),
            (
                "/ui/app.js",
                "text/javascript; charset=utf-8",
                "signedHeaders",
            ),
            ("/ui/app.css", "text/css; charset=utf-8", "--accent"),
        ] {
            let response = routes()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
            assert_eq!(response.headers()[header::CONTENT_SECURITY_POLICY], CSP);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(String::from_utf8(body.to_vec()).unwrap().contains(marker));
        }
    }
}
