use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use kit::api::http::health::{self, HealthState, LIVENESS_PATH, READINESS_PATH};
use kit::{
    api::auth::contract::{
        Authenticator, Authorizer, GrantSnapshot, PrincipalGrant, ResourceScope, ScopedAuthorizer,
    },
    api::auth::local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
    },
};

type Transition = fn(&HealthState);

const REQUIRED_TRANSITIONS: [Transition; 5] = [
    |state| state.set_auth_ready(true),
    |state| state.set_store_ready(true),
    |state| state.set_lease_ready(true),
    |state| state.set_startup_reconciliation_ready(true),
    |state| state.set_admission_ready(true),
];

fn permutations(values: &mut [Transition], start: usize, check: &mut impl FnMut(&[Transition])) {
    if start == values.len() {
        check(values);
        return;
    }
    for index in start..values.len() {
        values.swap(start, index);
        permutations(values, start + 1, check);
        values.swap(start, index);
    }
}

fn ready_state() -> HealthState {
    let state = HealthState::new();
    for transition in REQUIRED_TRANSITIONS {
        transition(&state);
    }
    state
}

async fn response(app: Router, path: &str) -> (StatusCode, Value, usize) {
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 256).await.unwrap();
    let length = bytes.len();
    (status, serde_json::from_slice(&bytes).unwrap(), length)
}

#[test]
fn every_boot_order_stays_unready_until_every_required_gate_is_ready() {
    let mut transitions = REQUIRED_TRANSITIONS;
    let mut checked = 0;
    permutations(&mut transitions, 0, &mut |order| {
        let state = HealthState::new();
        assert!(state.is_live());
        assert!(!state.is_ready());
        for (index, transition) in order.iter().enumerate() {
            transition(&state);
            assert_eq!(state.is_ready(), index == order.len() - 1);
        }
        checked += 1;
    });
    assert_eq!(checked, 120);
}

#[test]
fn readiness_regresses_with_each_required_gate_but_liveness_does_not() {
    let regressions: [Transition; 5] = [
        |state| state.set_auth_ready(false),
        |state| state.set_store_ready(false),
        |state| state.set_lease_ready(false),
        |state| state.set_startup_reconciliation_ready(false),
        |state| state.set_admission_ready(false),
    ];
    for regress in regressions {
        let state = ready_state();
        regress(&state);
        assert!(!state.is_ready());
        assert!(state.is_live());
    }

    let state = ready_state();
    state.set_process_loop_healthy(false);
    assert!(!state.is_live());
    assert!(!state.is_ready());
}

#[test]
fn shutdown_is_terminal_and_thread_safe() {
    let state = ready_state();
    assert!(state.is_ready());
    state.begin_shutdown();
    assert!(state.is_shutting_down());
    assert!(!state.is_ready());

    let threads: Vec<_> = REQUIRED_TRANSITIONS
        .into_iter()
        .map(|transition| {
            let state = state.clone();
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    transition(&state);
                    state.begin_shutdown();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(state.is_shutting_down());
    assert!(!state.is_ready());
    assert!(state.is_live());
}

async fn require_auth(request: Request<Body>, next: Next) -> Response {
    if request.headers().contains_key(header::AUTHORIZATION) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

#[tokio::test]
async fn health_routes_are_public_without_unprotecting_other_routes() {
    let state = ready_state();
    let protected = Router::new()
        .route("/private", get(|| async { StatusCode::NO_CONTENT }))
        .layer(middleware::from_fn(require_auth));
    let app = protected.merge(health::routes(state));

    assert_eq!(response(app.clone(), LIVENESS_PATH).await.0, StatusCode::OK);
    assert_eq!(
        response(app.clone(), READINESS_PATH).await.0,
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(Request::get("/private").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.oneshot(
            Request::get("/private")
                .header(header::AUTHORIZATION, "Bearer fixture")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn responses_have_bounded_health_schemas() {
    let state = HealthState::new();
    let app = health::routes(state.clone());

    let (status, body, length) = response(app.clone(), LIVENESS_PATH).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"live": true, "version": env!("CARGO_PKG_VERSION")})
    );
    assert_eq!(
        body.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["live", "version"])
    );
    assert!(length <= 128);

    let (status, body, length) = response(app.clone(), READINESS_PATH).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({
            "ready": false,
            "version": env!("CARGO_PKG_VERSION"),
            "backup": null,
            "telemetry": null,
        })
    );
    assert_eq!(
        body.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["backup", "ready", "telemetry", "version"])
    );
    assert!(length <= 128);

    for transition in REQUIRED_TRANSITIONS {
        transition(&state);
    }
    assert_eq!(
        response(app.clone(), READINESS_PATH).await.0,
        StatusCode::OK
    );
    state.set_process_loop_healthy(false);
    assert_eq!(
        response(app.clone(), LIVENESS_PATH).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        response(app, READINESS_PATH).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn project_creation_is_principal_scoped_without_widening_project_access() {
    let principal = PrincipalId::generate().unwrap();
    let exact_project = ProjectId::generate().unwrap();
    let other_project = ProjectId::generate().unwrap();
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(principal, exact_project, [Grant::WorkspaceWrite])
            .with_principal_grant(PrincipalGrant::CreateProject),
    )]));
    let authenticated = authenticator
        .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
        .unwrap();

    assert!(
        ScopedAuthorizer
            .authorize(
                &authenticated,
                ResourceScope::project_creation(principal, other_project),
                Grant::WorkspaceWrite,
            )
            .is_ok()
    );
    assert!(
        ScopedAuthorizer
            .authorize(
                &authenticated,
                ResourceScope::new(principal, exact_project),
                Grant::WorkspaceWrite,
            )
            .is_ok()
    );
    assert!(
        ScopedAuthorizer
            .authorize(
                &authenticated,
                ResourceScope::new(principal, other_project),
                Grant::WorkspaceWrite,
            )
            .is_err()
    );
    assert!(
        ScopedAuthorizer
            .authorize(
                &authenticated,
                ResourceScope::project_creation(
                    PrincipalId::generate().unwrap(),
                    ProjectId::generate().unwrap(),
                ),
                Grant::WorkspaceWrite,
            )
            .is_err()
    );
}
