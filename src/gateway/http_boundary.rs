//! Admission and clean teardown outside the pinned SDK's aborting DELETE.
use super::*;
use axum::{
    body::Body,
    http::{Method, header},
};
use std::sync::Weak;
use tower::ServiceExt as _;

const MAX_BODY: usize = 1024 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const DRAIN_METHOD: &str = "_kit/gateway/drain";

#[derive(Default)]
pub(super) struct Boundary {
    // Writers: admission lookup/prune only. Weak entries retire on completion,
    // cancellation, or unwind; arbitrary connection IDs cannot accumulate.
    admission: Mutex<HashMap<String, Weak<Mutex<bool>>>>,
    // Writers: DELETE publishes, adapter acknowledges/removes, next DELETE
    // prunes expired entries. The drain task owns the only strong sender.
    drains: Mutex<HashMap<String, Weak<watch::Sender<bool>>>>,
}

impl Boundary {
    async fn gate(&self, id: &str) -> Arc<Mutex<bool>> {
        let mut gates = self.admission.lock().await;
        gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = gates.get(id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(false));
        gates.insert(id.to_owned(), Arc::downgrade(&gate));
        gate
    }

    pub(super) async fn acknowledge(&self, token: &str) {
        let sender = self
            .drains
            .lock()
            .await
            .remove(token)
            .and_then(|s| s.upgrade());
        // Wake the DELETE task outside the registry guard. An expired barrier
        // is harmless: its timed-out DELETE did not abort the SDK connection.
        if let Some(sender) = sender {
            sender.send_replace(true);
        }
    }
}

pub(super) async fn handle(
    State((boundary, sdk)): State<(Arc<Boundary>, Router)>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    if request.uri().path() != "/acp/v2" {
        return next.run(request).await;
    }
    if request.method() == Method::POST {
        if request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|length| length > MAX_BODY as u64)
        {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        // DefaultBodyLimit does not constrain the SDK's raw Request handler.
        // Consume with an actual byte bound, including chunked/unknown lengths,
        // before parsing or allowing any frame into the SDK mailbox.
        let (parts, body) = request.into_parts();
        let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        request = axum::extract::Request::from_parts(parts, Body::from(bytes));
    }
    if !matches!(*request.method(), Method::POST | Method::DELETE) {
        return next.run(request).await;
    }
    let Some(id) = request.headers().get("acp-connection-id").cloned() else {
        return next.run(request).await;
    };
    let Ok(id_text) = id.to_str() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let gate = boundary.gate(id_text).await;
    let mut closing = gate.lock().await;
    if *closing {
        return (
            StatusCode::CONFLICT,
            "connection teardown is draining accepted requests",
        )
            .into_response();
    }
    if request.method() == Method::POST {
        // This admission lease deliberately spans only the SDK HTTP handler,
        // which queues the frame before returning 202, not actor execution.
        // Cancellation drops the lease; SDK enqueue itself is synchronous.
        let response = next.run(request).await;
        drop(closing);
        return response;
    }

    // This private acknowledgement capability must not be guessable by an
    // authenticated client that can enqueue arbitrary ACP extension methods.
    let mut nonce = [0_u8; 32];
    if getrandom::fill(&mut nonce).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let token: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let payload = object([
        ("jsonrpc", "2.0".into()),
        ("method", DRAIN_METHOD.into()),
        ("params", object([("token", token.clone().into())])),
    ]);
    let mut barrier = axum::extract::Request::new(Body::from(payload.to_string()));
    *barrier.method_mut() = Method::POST;
    *barrier.uri_mut() = request.uri().clone();
    barrier.headers_mut().insert("acp-connection-id", id);
    barrier.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    let (sender, mut acknowledged) = watch::channel(false);
    let owner = Arc::new(sender);
    {
        let mut drains = boundary.drains.lock().await;
        drains.retain(|_, sender| sender.strong_count() != 0);
        drains.insert(token, Arc::downgrade(&owner));
        // Complete publication and stop admission together, with no await
        // before transferring ownership to the cancellation-independent task.
        *closing = true;
    }
    drop(closing);
    let work = tokio::spawn(async move {
        let _owner = owner;
        // Re-enter the unwrapped SDK router to select POST. Next is already
        // bound to the selected DELETE route and cannot redispatch methods.
        let response = match sdk.oneshot(barrier).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        if response.status() == StatusCode::NOT_FOUND {
            return response;
        }
        let drained = response.status() == StatusCode::ACCEPTED
            && matches!(
                tokio::time::timeout(DRAIN_TIMEOUT, acknowledged.wait_for(|ready| *ready)).await,
                Ok(Ok(_))
            );
        if !drained {
            // Never claim successful teardown or invoke the aborting SDK
            // DELETE when accepted work has not crossed the actor boundary.
            *gate.lock().await = false;
            return (StatusCode::SERVICE_UNAVAILABLE, "accepted request drain was not confirmed; teardown was not performed; submission outcome unknown; do not resubmit work").into_response();
        }
        let response = next.run(request).await;
        drop(gate);
        response
    });
    match work.await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "connection teardown failed; submission outcome unknown",
        )
            .into_response(),
    }
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

    fn request(method: Method) -> axum::extract::Request {
        let mut request = axum::extract::Request::new(Body::from("{}"));
        *request.uri_mut() = "/acp/v2".parse().unwrap();
        *request.method_mut() = method;
        request.headers_mut().insert(
            "acp-connection-id",
            header::HeaderValue::from_static("connection"),
        );
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        request
    }

    fn wrapped(sdk: Router) -> Router {
        sdk.clone().layer(middleware::from_fn_with_state(
            (Arc::new(Boundary::default()), sdk),
            handle,
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_drain_does_not_delete_and_reopens_admission() {
        // Fake the SDK HTTP boundary, not internal drain hooks. A 202 without
        // adapter acknowledgement must never reach the destructive DELETE.
        let sdk = Router::new().route(
            "/acp/v2",
            axum::routing::post(|| async { StatusCode::ACCEPTED })
                .delete(|| async { StatusCode::IM_A_TEAPOT }),
        );
        let app = wrapped(sdk);
        let deletion = tokio::spawn(app.clone().oneshot(request(Method::DELETE)));
        loop {
            let response = app.clone().oneshot(request(Method::POST)).await.unwrap();
            if response.status() == StatusCode::CONFLICT {
                break;
            }
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            tokio::task::yield_now().await;
        }
        tokio::time::advance(DRAIN_TIMEOUT).await;
        assert_eq!(
            deletion.await.unwrap().unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            app.oneshot(request(Method::POST)).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn sdk_unwind_retires_gate_without_claiming_success() {
        async fn sdk_post(Json(message): Json<Value>) -> StatusCode {
            assert_ne!(
                message["method"], DRAIN_METHOD,
                "SDK failure at the actual HTTP boundary"
            );
            StatusCode::ACCEPTED
        }
        let app = wrapped(Router::new().route(
            "/acp/v2",
            axum::routing::post(sdk_post).delete(|| async { StatusCode::IM_A_TEAPOT }),
        ));
        assert_eq!(
            app.clone()
                .oneshot(request(Method::DELETE))
                .await
                .unwrap()
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // The panicking task releases its ownership; weak registrations cannot
        // leave an otherwise usable connection permanently marked closing.
        assert_eq!(
            app.oneshot(request(Method::POST)).await.unwrap().status(),
            StatusCode::ACCEPTED
        );
    }
}
