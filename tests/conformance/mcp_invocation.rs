use kit::{
    capabilities::broker::BrokerAuthRequirement, domain::secret::SecretHandle,
    protocols::mcp::transport::McpResultPolicy,
};

#[test]
fn mcp_result_policy_only_controls_bounded_untrusted_presentation() {
    assert!(McpResultPolicy::new(1).is_err());
    assert!(McpResultPolicy::new(1024).is_ok());
    assert!(McpResultPolicy::new(8 * 1024 + 1).is_err());
}

#[test]
fn broker_auth_requirement_preserves_the_exact_scope_set_and_credential_binding() {
    let credential = SecretHandle::parse("test:mcp-auth").unwrap();
    let requirement = BrokerAuthRequirement::from_scopes(["repo:write", "repo:read", "repo:read"])
        .unwrap()
        .with_credential_id(credential.clone());
    assert_eq!(
        requirement
            .scopes()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["repo:read", "repo:write"]
    );
    assert_eq!(requirement.scope(), "repo:read repo:write");
    assert_eq!(requirement.credential_id(), Some(&credential));
}
