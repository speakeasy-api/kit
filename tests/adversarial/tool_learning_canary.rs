use kit::{
    domain::ids::{ProjectId, RunId},
    telemetry::tool_learning::{
        ErrorClass, ErrorCode, ErrorStage, LearningCommon, LearningOperation, LearningStatus,
        LearningSurface, PointerDomain, ProjectPointerHasher, RetryClass, ToolLearningEvent,
    },
};

#[test]
fn raw_and_encoded_canaries_never_enter_any_learning_event_surface() {
    let project = ProjectId::parse("project_00000000000000000000000001").unwrap();
    let run = RunId::parse("run_00000000000000000000000001").unwrap();
    let hasher = ProjectPointerHasher::new(project, &[13; 32]);
    let raw =
        "LEARNING-CANARY-prompt-query-description-schema-args-output-url-error-reasoning-secret";
    let encoded = "TEVBUk5JTkctQ0FOQVJZLXByb21wdC1xdWVyeS1kZXNjcmlwdGlvbi1zY2hlbWEtYXJncy1vdXRwdXQtdXJsLWVycm9yLXJlYXNvbmluZy1zZWNyZXQ=";
    let pointer = |domain, suffix: &str| {
        hasher.pointer(domain, format!("{raw}:{encoded}:{suffix}").as_bytes())
    };
    let common = |ordinal, operation, key: &str| {
        LearningCommon::new(
            &hasher,
            run,
            ordinal,
            operation,
            LearningSurface::Generic,
            format!("{raw}:{encoded}:{key}").as_bytes(),
            Some(pointer(PointerDomain::Request, key)),
            Some(pointer(PointerDomain::Capability, key)),
            Some(pointer(PointerDomain::Schema, key)),
        )
    };
    let call = pointer(PointerDomain::Call, "call");
    let events = [
        ToolLearningEvent::Opportunity {
            common: common(1, LearningOperation::Projection, "opportunity"),
            offered: 1,
            eager: 1,
            deferred: 0,
            generic_available: true,
            projection: pointer(PointerDomain::Schema, "projection"),
            candidates: Vec::new(),
            detail_artifact: Some(pointer(PointerDomain::Artifact, "candidate-detail")),
        },
        ToolLearningEvent::Search {
            common: common(2, LearningOperation::Search, "search"),
            query: pointer(PointerDomain::Query, "query"),
            status: LearningStatus::Succeeded,
            result_count: 1,
            detail_artifact: Some(pointer(PointerDomain::Artifact, "result-detail")),
        },
        ToolLearningEvent::Inspection {
            common: common(3, LearningOperation::Inspect, "inspect"),
            handle: pointer(PointerDomain::Handle, "handle"),
            status: LearningStatus::Succeeded,
        },
        ToolLearningEvent::Call {
            common: common(4, LearningOperation::Invoke, "call"),
            call: call.clone(),
            binding: Some(pointer(PointerDomain::Binding, "binding")),
            source: Some(pointer(PointerDomain::Source, "source")),
            kind: Some(kit::telemetry::tool_learning::LearningCapabilityKind::Tool),
            sequence: Some(pointer(PointerDomain::Sequence, "sequence")),
            sequence_order: Some(1),
            kernel_intent: Some(pointer(PointerDomain::KernelEvent, "intent")),
        },
        ToolLearningEvent::Error {
            common: common(5, LearningOperation::Invoke, "error"),
            call: call.clone(),
            stage: ErrorStage::ResultValidation,
            class: ErrorClass::Result,
            code: ErrorCode::SensitiveResponse,
            field: Some(pointer(PointerDomain::Field, "field")),
            retry: RetryClass::Never,
            dispatched: true,
            known: true,
        },
        ToolLearningEvent::Outcome {
            common: common(6, LearningOperation::Invoke, "outcome"),
            call,
            status: LearningStatus::Failed,
            dispatched: true,
            known: true,
            cost_microusd: Some(1),
            kernel_outcome: Some(pointer(PointerDomain::KernelEvent, "outcome")),
        },
    ];
    let wire = serde_json::to_string(&events).unwrap();
    assert!(!wire.contains(raw));
    assert!(!wire.contains(encoded));
    assert!(events.iter().all(|event| event.validate().is_ok()));
}
