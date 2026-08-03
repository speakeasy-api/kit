use kit::{capabilities::kernel::invoke::CanonicalOutput, domain::events::ArtifactRef};

#[test]
fn opaque_artifact_references_cannot_enter_kernel_event_reachability() {
    assert!(ArtifactRef::parse(&format!("artifact-ref:{}", "ab".repeat(32))).is_err());
    assert!(
        serde_json::from_value::<CanonicalOutput>(serde_json::json!({
            "media_type": "application/vnd.kit.canonical-result+json",
            "body": [],
            "artifact_digests": [format!("artifact-ref:{}", "ab".repeat(32))]
        }))
        .is_err()
    );
}
