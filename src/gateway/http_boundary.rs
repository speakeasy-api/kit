//! Bounded HTTP admission outside the SDK's native graceful teardown.
use super::*;
use axum::http::{Method, header};
use tokio::sync::Semaphore;

const MAX_BODY: usize = 1024 * 1024;
const MAX_REQUESTS: usize = 32;

pub(super) struct Boundary {
    // Each outer POST/DELETE owns one permit until its HTTP future finishes
    // or is dropped. The SDK independently owns connection draining.
    requests: Arc<Semaphore>,
}

impl Default for Boundary {
    fn default() -> Self {
        Self {
            requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
        }
    }
}

pub(super) async fn handle(
    State(boundary): State<Arc<Boundary>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if request.uri().path() != "/acp/v2" {
        return next.run(request).await;
    }
    if !matches!(*request.method(), Method::POST | Method::DELETE) {
        return next.run(request).await;
    }
    let Ok(permit) = boundary.requests.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway request admission is full; request was not admitted; prior submission outcome may be unknown; do not resubmit accepted work",
        )
            .into_response();
    };
    if request.method() == Method::POST
        && request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|length| length > MAX_BODY as u64)
    {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    // The SDK bounds actual bytes (including unknown lengths), atomically
    // seals ingress on DELETE, and owns draining independently of this waiter.
    let response = next.run(request).await;
    drop(permit);
    response
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::disallowed_methods,
        clippy::disallowed_macros
    )]
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt as _;

    fn request(method: Method, body: Body) -> axum::extract::Request {
        let mut request = axum::extract::Request::new(body);
        *request.uri_mut() = "/acp/v2".parse().unwrap();
        *request.method_mut() = method;
        request
    }

    fn wrapped(service: Router) -> Router {
        service.layer(middleware::from_fn_with_state(
            Arc::new(Boundary::default()),
            handle,
        ))
    }

    fn pending_body() -> Body {
        Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >())
    }

    fn unpollable_body() -> Body {
        Body::from_stream(futures_util::stream::poll_fn(
            |_| -> std::task::Poll<Option<Result<axum::body::Bytes, std::io::Error>>> {
                panic!("rejected request body must not be polled")
            },
        ))
    }

    async fn read_body(request: axum::extract::Request) -> StatusCode {
        axum::body::to_bytes(request.into_body(), MAX_BODY)
            .await
            .unwrap();
        StatusCode::ACCEPTED
    }

    #[tokio::test]
    async fn saturation_rejects_before_body_poll_and_cancellation_reuses_permit() {
        // This service tests outer HTTP admission, not SDK drain semantics.
        for admitted_method in [Method::POST, Method::DELETE] {
            let app = wrapped(
                Router::new().route("/acp/v2", axum::routing::post(read_body).delete(read_body)),
            );
            let mut waiting = Vec::new();
            for _ in 0..MAX_REQUESTS {
                let mut pending = Box::pin(
                    app.clone()
                        .oneshot(request(admitted_method.clone(), pending_body())),
                );
                assert!(futures_util::poll!(&mut pending).is_pending());
                waiting.push(pending);
            }
            for method in [Method::POST, Method::DELETE] {
                assert_eq!(
                    app.clone()
                        .oneshot(request(method, unpollable_body()))
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::SERVICE_UNAVAILABLE
                );
            }
            drop(waiting.pop());
            // Each completion must release the same freed slot for reuse.
            for method in [Method::POST, Method::DELETE] {
                assert_eq!(
                    app.clone()
                        .oneshot(request(method, Body::empty()))
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::ACCEPTED
                );
            }
        }
    }

    #[tokio::test]
    async fn declared_oversize_rejects_before_body_poll_and_releases_permit() {
        let app = wrapped(Router::new().route("/acp/v2", axum::routing::post(read_body)));
        // More rejections than capacity detects a permit leaked on preflight.
        for _ in 0..=MAX_REQUESTS {
            let mut oversized = request(Method::POST, unpollable_body());
            oversized.headers_mut().insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_str(&(MAX_BODY + 1).to_string()).unwrap(),
            );
            assert_eq!(
                app.clone().oneshot(oversized).await.unwrap().status(),
                StatusCode::PAYLOAD_TOO_LARGE
            );
        }
        let mut within_limit = request(Method::POST, Body::empty());
        within_limit.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_str(&MAX_BODY.to_string()).unwrap(),
        );
        assert_eq!(
            app.oneshot(within_limit).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
    }
}
