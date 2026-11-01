use super::*;
use agentkit_loop::{
    ProviderClassification, ProviderFailure, ProviderFailureReason, ProviderRoute, RetryAccounting,
    RetryProgress,
};

fn scheduled(attempts: u64, millis: u64) -> AgentEvent {
    AgentEvent::ProviderRetry(ProviderRetryEvent::Scheduled(RetryProgress {
        route: ProviderRoute::OpenAiChatGptResponses,
        reason: ProviderFailureReason::Transport,
        upstream: ProviderClassification::default(),
        accounting: accounting(attempts, millis),
        next_delay: Duration::from_millis(25),
    }))
}
fn accounting(attempts: u64, millis: u64) -> RetryAccounting {
    RetryAccounting {
        attempts,
        completed_backoff: Duration::from_millis(millis),
        elapsed: Duration::from_millis(millis),
    }
}
fn progress(update: SessionUpdate) -> ProviderRetryProgress {
    let SessionUpdate::Notice(notice) = update else {
        panic!("expected retry notice")
    };
    serde_json::from_value(notice.meta.unwrap()[META_KEY].clone()).unwrap()
}

#[test]
fn retry_projection_bounds_schedules_and_keeps_exact_terminal_accounting() {
    let projection = Projection::default();
    let first = progress(projection.observe(&scheduled(1, 0)).unwrap());
    assert_eq!(first.attempts, 1);
    assert_eq!(first.next_delay_ms, 25);
    assert_eq!(first.completed_backoff_ms, 0);
    assert!(projection.observe(&scheduled(2, 999)).is_none());
    let next = progress(projection.observe(&scheduled(3, 1000)).unwrap());
    assert_eq!(next.attempts, 3);
    assert_eq!(next.completed_backoff_ms, 1000);
    let further = (4..100)
        .filter_map(|attempt| projection.observe(&scheduled(attempt, attempt * 1000)))
        .count();
    assert_eq!(further + 2, usize::from(MAX_SCHEDULED));
    let terminal = AgentEvent::ProviderRetry(ProviderRetryEvent::Stopped(ProviderFailure {
        last_attempt_reason: None,
        route: ProviderRoute::OpenAiChatGptResponses,
        reason: ProviderFailureReason::RetryExhausted,
        upstream: ProviderClassification::default(),
        accounting: accounting(100, 99999),
    }));
    let last = progress(projection.observe(&terminal).unwrap());
    assert_eq!(last.status, RetryStatus::Stopped);
    assert_eq!(last.attempts, 100);
    assert_eq!(last.completed_backoff_ms, 99999);
    assert_eq!(last.next_delay_ms, 0);
    assert!(projection.observe(&terminal).is_none());
    assert!(projection.observe(&scheduled(1, 0)).is_some());
}

#[test]
fn retry_projection_terminal_without_retry_is_silent_and_success_clears_once() {
    let projection = Projection::default();
    let terminal = AgentEvent::ProviderRetry(ProviderRetryEvent::Succeeded {
        route: ProviderRoute::OpenAiChatGptResponses,
        accounting: accounting(3, 500),
    });
    assert!(projection.observe(&terminal).is_none());
    projection.observe(&scheduled(1, 0)).unwrap();
    assert_eq!(
        progress(projection.observe(&terminal).unwrap()).status,
        RetryStatus::Succeeded
    );
    assert!(projection.observe(&terminal).is_none());
    assert!(projection.observe(&scheduled(1, 0)).is_some());
}

#[test]
fn retry_projection_poison_isolates_only_progress() {
    let projection = Projection::default();
    let shared = projection.clone();
    let _ = std::thread::spawn(move || {
        let _guard = shared.0.lock().unwrap();
        panic!("poison projection");
    })
    .join();
    assert!(projection.observe(&scheduled(1, 0)).is_none());
    assert!(Projection::default().observe(&scheduled(1, 0)).is_some());
}

#[test]
fn retry_projection_notice_wire_is_identical_in_v1_and_v2() {
    let update = Projection::default().observe(&scheduled(1, 0)).unwrap();
    let value = serde_json::to_value(update).unwrap();
    let v2: agent_client_protocol::schema::v2::SessionUpdate =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(v2).unwrap(), value);
    assert_eq!(
        value["_meta"][META_KEY]["route"],
        "open_ai_chat_gpt_responses"
    );
    assert_eq!(value["_meta"][META_KEY]["category"], "transport");
}

#[test]
fn retry_projection_new_turn_discards_abandoned_progress() {
    let projection = Projection::default();
    projection.observe(&scheduled(1, 0)).unwrap();
    assert!(
        projection
            .observe(&AgentEvent::TurnStarted {
                session_id: agentkit_core::SessionId::new("session"),
                turn_id: agentkit_core::TurnId::new("next"),
            })
            .is_none()
    );
    assert!(
        projection
            .observe(&AgentEvent::ProviderRetry(ProviderRetryEvent::Succeeded {
                route: ProviderRoute::OpenAiChatGptResponses,
                accounting: accounting(2, 25),
            }))
            .is_none()
    );
    assert!(projection.observe(&scheduled(1, 0)).is_some());
}
