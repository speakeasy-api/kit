#[test]
fn deferred_registration() {
    deferred_registration_tests::run();
}

#[test]
fn eager_tool_tools() {
    deferred_registration_tests::eager_tools();
}

mod deferred_registration_tests {
    use std::{
        cell::Cell,
        collections::{BTreeMap, BTreeSet},
        rc::Rc,
        sync::Arc,
    };

    use agentkit_tools_core::ToolSpec;
    use kit::{
        api::auth::{
            contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        capabilities::{
            catalog::{
                Availability, CapabilityKind, CatalogAuthority, CatalogEntry, CatalogSchemas, CatalogSearch,
                CatalogSnapshot, CatalogSource, CostStats, LatencyStats, MAX_CATALOG_ENTRIES,
                MAX_CATALOG_PAYLOAD_BYTES, MAX_SUMMARY_BYTES, ReliabilityStats, SideEffects,
                SourceKind, TrustDomain,
            },
            discovery::{BindingId, CapabilityBinding, DiscoverySession},
            kernel::{
                grant::{
                    ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass,
                },
                grant_ext::RequestExtension,
                identity::{
                    CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                    CapabilityVersion, Digest, DigestAlgorithm,
                },
                invoke::{MAX_INVOCATION_ARGUMENT_BYTES, RetrySafety},
            },
            registration::{
                BindingRegistry, DeferredRegistrationDeclaration, DirectInvokeCall,
                InvocationContext, InvocationError, PortableInvokeCall, ProviderCapabilityContract,
                RegistrationCall, RegistrationError, RegistrationMode, RegistrationPlan,
                ValidatedProjectionSupport, MAX_BOUND_INPUT_BYTES, MAX_TOOL_NAME_BYTES,
            },
            schema::{
                JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionError, ProjectionProfile,
                ProjectionTarget, SchemaProjectionSet,
            },
        },
        domain::{
            config::{
                BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
                LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
            },
            ids::{PrincipalId, ProjectId, RunId, WorkspaceId},
        },
    };

    const UID: u32 = 906;

    struct Fixture {
        target: ProjectionTarget,
        profile: ProjectionProfile,
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
        catalog: CatalogSnapshot,
        authenticated: AuthenticatedPrincipal,
        config: RunConfigSnapshot,
        grants: CapabilityGrantSnapshot,
        constraints: ArgumentConstraints,
        bindings: Vec<Arc<CapabilityBinding>>,
    }

    impl Fixture {
        fn new(names: &[&str], projected: bool) -> Self {
            Self::with_documentation(names, projected, 0)
        }

        fn with_documentation(names: &[&str], projected: bool, documentation_bytes: usize) -> Self {
            let entries = names
                .iter()
                .map(|name| (*name, CapabilityKind::Tool))
                .collect::<Vec<_>>();
            Self::with_kinds(&entries, projected, documentation_bytes)
        }

        fn with_kinds(
            entries: &[(&str, CapabilityKind)],
            projected: bool,
            documentation_bytes: usize,
        ) -> Self {
            let target = target("model-a");
            let profile = profile(target.clone(), false);
            let principal_id = PrincipalId::generate().unwrap();
            let project_id = ProjectId::generate().unwrap();
            let workspace_id = WorkspaceId::generate().unwrap();
            let config = config(principal_id, project_id, 100);
            let authenticated = authenticate(principal_id, project_id);
            let catalog = CatalogSnapshot::new(
                entries.iter().map(|(name, kind)| {
                    entry(
                        name,
                        *kind,
                        name,
                        &target,
                        &profile,
                        projected,
                        &format!("summary {name}"),
                        documentation_bytes,
                    )
                }),
                DigestAlgorithm::Sha256,
            )
            .unwrap();
            let constraints = ArgumentConstraints::default();
            let grants = grants_for(
                &catalog,
                &config,
                principal_id,
                project_id,
                workspace_id,
                &constraints,
            );
            let bindings = {
                let session = session(
                    &catalog,
                    &authenticated,
                    &config,
                    &grants,
                    workspace_id,
                    project_id,
                    &constraints,
                );
                entries
                    .iter()
                    .map(|(name, _)| bind_named(&session, name))
                    .collect()
            };
            Self {
                target,
                profile,
                principal_id,
                project_id,
                workspace_id,
                catalog,
                authenticated,
                config,
                grants,
                constraints,
                bindings,
            }
        }

        fn session(&self) -> DiscoverySession<'_> {
            session(
                &self.catalog,
                &self.authenticated,
                &self.config,
                &self.grants,
                self.workspace_id,
                self.project_id,
                &self.constraints,
            )
        }

        fn registry(&self) -> BindingRegistry {
            BindingRegistry::new(self.bindings.iter().cloned()).unwrap()
        }

        fn supported(&self) -> ProviderCapabilityContract {
            supported_contract(&self.profile)
        }

        fn portable(&self) -> ProviderCapabilityContract {
            ProviderCapabilityContract::portable(
                ValidatedProjectionSupport::validate(&self.profile).unwrap(),
            )
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct Observation {
        binding_id: BindingId,
        kind: CapabilityKind,
        capability: CapabilityIdentity,
        schema_digest: Digest,
        authorization_snapshot_digest: Digest,
        input: serde_json::Value,
        input_bytes: Vec<u8>,
    }

    pub fn run() {
        let fixture = Fixture::new(&["route-alpha", "route-beta", "route-gamma"], true);
        let registry = fixture.registry();
        let binding_id = fixture.bindings[0].id();
        assert_eq!(BindingId::parse(&binding_id.to_string()).unwrap(), binding_id);
        for invalid in [
            "binding_v1_00".to_owned(),
            binding_id.to_string().to_uppercase(),
            format!("{}0", binding_id),
        ] {
            assert!(BindingId::parse(&invalid).is_err());
        }

        for _ in 0..100 {
            let plan = registry
                .plan(&fixture.supported(), &fixture.session())
                .unwrap();
            assert_eq!(plan.mode(), RegistrationMode::Deferred);
            assert_eq!(
                operations(&plan),
                ["tools.bind", "tools.inspect", "tools.invoke", "tools.search"]
            );
            let definitions = plan
                .deferred_tools(&registry, &fixture.session())
                .unwrap();
            assert_eq!(definitions.len(), fixture.bindings.len());
            for definition in definitions {
                let binding = fixture
                    .bindings
                    .iter()
                    .find(|binding| binding.id() == definition.binding_id())
                    .unwrap();
                let projection = binding
                    .pinned_entry()
                    .schemas()
                    .input()
                    .projection(&fixture.target)
                    .unwrap();
                assert_eq!(definition.summary(), binding.pinned_entry().search().summary());
                assert_eq!(definition.input_schema(), projection.value());
                assert_eq!(
                    definition.spec().metadata["kit.operation"],
                    format!(
                        "{}.{}",
                        binding.pinned_entry().identity().namespace().as_str(),
                        binding.pinned_entry().identity().name().as_str()
                    )
                );
                assert_eq!(
                    definition.spec().metadata["kit.binding_id"],
                    binding.id().to_string()
                );
                assert_eq!(
                    definition.spec().metadata["kit.schema.digest"],
                    binding.input_schema_digest().to_string()
                );
                assert!(definition.wire_name().len() <= MAX_TOOL_NAME_BYTES);
            }
        }

        let declaration = DeferredRegistrationDeclaration::new(
            fixture.target.clone(),
            fixture.profile.digest(),
        );
        let other_profile = profile(fixture.target.clone(), true);
        let portable_contracts = [
            fixture.portable(),
            ProviderCapabilityContract::new(
                Some(declaration.clone()),
                ValidatedProjectionSupport::validate(&other_profile).unwrap(),
            ),
            ProviderCapabilityContract::new(
                Some(declaration),
                ValidatedProjectionSupport::validate(&profile(target("other-model"), false))
                    .unwrap(),
            ),
        ];
        for contract in &portable_contracts {
            for _ in 0..100 {
                assert_portable(
                    &registry.plan(contract, &fixture.session()).unwrap(),
                    &registry,
                    &fixture.session(),
                );
            }
        }

        let absent = Fixture::new(&["projection-absent"], false);
        for _ in 0..100 {
            let absent_registry = absent.registry();
            assert_portable(
                &absent_registry
                    .plan(&absent.supported(), &absent.session())
                    .unwrap(),
                &absent_registry,
                &absent.session(),
            );
        }

        route_parity(&fixture, &registry);
        rejection_cases(&fixture, &registry);
        expiry_cases(&fixture, &registry);
        registry_generation(&fixture, &registry);
        registry_bounds(&fixture);
        kind_aware_routes();
    }

    fn kind_aware_routes() {
        let fixture = Fixture::with_kinds(
            &[
                ("tool-kind", CapabilityKind::Tool),
                ("resource-kind", CapabilityKind::Resource),
                ("template-kind", CapabilityKind::ResourceTemplate),
                ("prompt-kind", CapabilityKind::Prompt),
            ],
            true,
            0,
        );
        let registry = fixture.registry();
        let plan = registry
            .plan(&fixture.supported(), &fixture.session())
            .unwrap();
        let definitions = plan
            .deferred_tools(&registry, &fixture.session())
            .unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(
            fixture
                .bindings
                .iter()
                .find(|binding| binding.id() == definitions[0].binding_id())
                .unwrap()
                .pinned_entry()
                .kind(),
            CapabilityKind::Tool
        );
        for binding in &fixture.bindings {
            let wrapper = format!(
                r#"{{"binding_id":"{}","input":{{"count":1,"value":"valid"}}}}"#,
                binding.id()
            );
            let observed = plan
                .invoke(
                    &registry,
                    &fixture.session(),
                    PortableInvokeCall::new(wrapper.into_bytes()).into(),
                )
                .map(|bound| observe(&bound.context()))
                .unwrap();
            assert_eq!(observed.kind, binding.pinned_entry().kind());
        }
    }

    pub fn eager_tools() {
        let fixture = Fixture::new(&["eager"], true);
        let registry = fixture.registry();
        let deferred = registry
            .plan(&fixture.supported(), &fixture.session())
            .unwrap();
        let portable = registry
            .plan(&fixture.portable(), &fixture.session())
            .unwrap();
        assert_eq!(
            operations(&deferred),
            ["tools.bind", "tools.inspect", "tools.invoke", "tools.search"]
        );
        assert_eq!(
            operations(&portable),
            ["tools.bind", "tools.inspect", "tools.invoke", "tools.search"]
        );
        assert_eq!(
            names(&deferred),
            ["tools_bind", "tools_inspect", "tools_invoke", "tools_search"]
        );
        assert_eq!(
            names(&portable),
            ["tools_bind", "tools_inspect", "tools_invoke", "tools_search"]
        );
        assert!(
            portable
                .deferred_tools(&registry, &fixture.session())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            deferred
                .deferred_tools(&registry, &fixture.session())
                .unwrap()
                .len(),
            1
        );
        for tool in deferred.eager_tools().iter().chain(portable.eager_tools()) {
            assert_strict_tool(tool);
            assert_eq!(
                tool.metadata["kit.schema.dialect"],
                JSON_SCHEMA_2020_12
            );
        }
        assert_strict_tool(
            deferred
                .deferred_tools(&registry, &fixture.session())
                .unwrap()[0]
                .spec(),
        );
        let search_schema = &deferred
            .eager_tools()
            .iter()
            .find(|tool| operation(tool) == "tools.search")
            .unwrap()
            .input_schema;
        let search_validator = jsonschema::draft202012::options()
            .build(search_schema)
            .unwrap();
        let byte_limit_query = "\u{1f600}".repeat(64);
        assert_eq!(byte_limit_query.len(), 256);
        assert!(search_validator.is_valid(&serde_json::json!({
            "query": byte_limit_query,
            "limit": 1
        })));
        assert!(fixture.session().search(&byte_limit_query, 1).is_ok());
        assert!(!search_validator.is_valid(&serde_json::json!({
            "query": "\u{1f600}".repeat(65),
            "limit": 1
        })));
        assert!(
            deferred
                .eager_tools()
                .windows(2)
                .all(|pair| operation(&pair[0]) < operation(&pair[1]))
        );
        assert!(
            deferred
                .deferred_tools(&registry, &fixture.session())
                .unwrap()
                .windows(2)
                .all(|pair| pair[0].wire_name() < pair[1].wire_name())
        );
        assert_eq!(
            ValidatedProjectionSupport::validate(&restrictive_profile(target("restrictive")))
                .unwrap_err(),
            ProjectionError::UnsupportedConstraint {
                pointer: "/properties/binding_id/pattern".to_owned(),
                keyword: "pattern".to_owned(),
            }
        );
    }

    fn route_parity(fixture: &Fixture, registry: &BindingRegistry) {
        let direct_plan = registry
            .plan(&fixture.supported(), &fixture.session())
            .unwrap();
        let portable_plan = registry
            .plan(&fixture.portable(), &fixture.session())
            .unwrap();
        let mut routed = BTreeSet::new();
        for iteration in 0..120 {
            let binding = &fixture.bindings[iteration % fixture.bindings.len()];
            let value = format!("value-{iteration}");
            let direct_bytes = format!(r#"{{"value":"{value}","count":{iteration}}}"#);
            let canonical = format!(r#"{{"count":{iteration},"value":"value-{iteration}"}}"#)
                .into_bytes();
            let definition = direct_plan
                .deferred_tools(registry, &fixture.session())
                .unwrap()
                .iter()
                .find(|definition| definition.binding_id() == binding.id())
                .unwrap();
            let direct = direct_plan
                .invoke(
                    registry,
                    &fixture.session(),
                    DirectInvokeCall::new(definition.wire_name(), direct_bytes.as_bytes()).into(),
                )
                .map(|bound| observe(&bound.context()))
                .unwrap();
            let wrapper = format!(
                r#"{{"input":{},"binding_id":"{}"}}"#,
                direct_bytes,
                binding.id()
            );
            let portable = portable_plan
                .invoke(
                    registry,
                    &fixture.session(),
                    PortableInvokeCall::new(wrapper.as_bytes()).into(),
                )
                .map(|bound| observe(&bound.context()))
                .unwrap();
            assert_eq!(direct, portable);
            assert_eq!(direct.binding_id, binding.id());
            assert_eq!(direct.capability.namespace().as_str(), "kit.registration");
            assert_eq!(
                direct.capability.name().as_str(),
                ["route-alpha", "route-beta", "route-gamma"]
                    [iteration % fixture.bindings.len()]
            );
            assert_eq!(direct.capability.version().as_str(), "1.0.0");
            assert_eq!(direct.schema_digest, binding.input_schema_digest());
            assert_eq!(
                direct.authorization_snapshot_digest,
                binding.authorization_snapshot_digest()
            );
            assert_eq!(
                direct.input,
                serde_json::json!({"count": iteration, "value": value})
            );
            assert_eq!(direct.input_bytes, canonical);
            routed.insert(direct.binding_id);
        }
        assert_eq!(routed.len(), fixture.bindings.len());
    }

    fn rejection_cases(fixture: &Fixture, registry: &BindingRegistry) {
        let direct = registry
            .plan(&fixture.supported(), &fixture.session())
            .unwrap();
        let portable = registry
            .plan(&fixture.portable(), &fixture.session())
            .unwrap();
        let definition = &direct
            .deferred_tools(registry, &fixture.session())
            .unwrap()[0];
        let wire = definition.wire_name();
        let id = definition.binding_id();

        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, b"{".as_slice()).into(),
            InvocationError::MalformedInput,
        );
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(
                wire,
                br#"{"value":"a","value":"b","count":1}"#.as_slice(),
            )
            .into(),
            InvocationError::MalformedInput,
        );
        for malformed in [
            b"{}".as_slice(),
            br#"{"binding_id":"binding_v1_00","input":{}}"#,
            br#"{"binding_id":1,"input":{}}"#,
            br#"{"binding_id":"binding_v1_0000000000000000000000000000000000000000000000000000000000000000","input":{},"extra":true}"#,
            br#"{"binding_id":"binding_v1_0000000000000000000000000000000000000000000000000000000000000000","binding_id":"binding_v1_0000000000000000000000000000000000000000000000000000000000000000","input":{}}"#,
            br#"{"binding_id":"binding_v1_0000000000000000000000000000000000000000000000000000000000000000","input":{"x":1,"x":2}}"#,
        ] {
            assert_rejected(
                &portable,
                registry,
                &fixture.session(),
                PortableInvokeCall::new(malformed).into(),
                InvocationError::MalformedGenericWrapper,
            );
        }

        let mut unknown = id.to_string().into_bytes();
        let last = unknown.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let unknown = format!(
            r#"{{"binding_id":"{}","input":{{"count":1,"value":"valid"}}}}"#,
            String::from_utf8(unknown).unwrap()
        );
        assert_rejected(
            &portable,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(unknown.as_bytes()).into(),
            InvocationError::UnknownBinding,
        );
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(
                "kit_unknown",
                br#"{"count":1,"value":"valid"}"#.as_slice(),
            )
            .into(),
            InvocationError::UnknownWireName,
        );
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(b"{}".as_slice()).into(),
            InvocationError::MalformedGenericWrapper,
        );
        assert_rejected(
            &portable,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, b"{}".as_slice()).into(),
            InvocationError::WrongMode,
        );
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, br#"{"count":1,"value":""}"#.as_slice()).into(),
            InvocationError::SchemaInvalid,
        );
        let invalid_wrapper = format!(
            r#"{{"binding_id":"{id}","input":{{"count":1,"value":"","extra":true}}}}"#
        );
        assert_rejected(
            &portable,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(invalid_wrapper.as_bytes()).into(),
            InvocationError::SchemaInvalid,
        );

        let oversized = vec![b' '; MAX_INVOCATION_ARGUMENT_BYTES + 1];
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, oversized.clone()).into(),
            InvocationError::InputTooLarge,
        );
        assert_rejected(
            &portable,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(oversized).into(),
            InvocationError::WrapperTooLarge,
        );

        for number in ["18446744073709551616", "0.10000000000000001"] {
            let input = format!(r#"{{"count":{number},"value":"valid"}}"#);
            assert_rejected(
                &direct,
                registry,
                &fixture.session(),
                DirectInvokeCall::new(wire, input.into_bytes()).into(),
                InvocationError::MalformedInput,
            );
            let wrapper = format!(
                r#"{{"binding_id":"{id}","input":{{"count":{number},"value":"valid"}}}}"#
            );
            assert_rejected(
                &portable,
                registry,
                &fixture.session(),
                PortableInvokeCall::new(wrapper.into_bytes()).into(),
                InvocationError::MalformedGenericWrapper,
            );
        }

        let deep = format!("{}0{}", "[".repeat(65), "]".repeat(65));
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, deep.as_bytes()).into(),
            InvocationError::MalformedInput,
        );
        let deep_wrapper = format!(r#"{{"binding_id":"{id}","input":{deep}}}"#);
        assert_rejected(
            &portable,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(deep_wrapper.as_bytes()).into(),
            InvocationError::MalformedGenericWrapper,
        );

        let many_nodes = format!("[{}]", "0,".repeat(100_000).trim_end_matches(','));
        assert_rejected(
            &direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, many_nodes.into_bytes()).into(),
            InvocationError::MalformedInput,
        );

        boundary_calls(fixture, registry, &direct, &portable, wire, id);
    }

    fn boundary_calls(
        fixture: &Fixture,
        registry: &BindingRegistry,
        direct: &RegistrationPlan,
        portable: &RegistrationPlan,
        wire: &str,
        id: BindingId,
    ) {
        let input_overhead = br#"{"count":1,"value":""}"#.len();
        let boundary_input = format!(
            r#"{{"count":1,"value":"{}"}}"#,
            "x".repeat(MAX_BOUND_INPUT_BYTES - input_overhead)
        );
        assert_eq!(boundary_input.len(), MAX_BOUND_INPUT_BYTES);
        direct
            .invoke(
                registry,
                &fixture.session(),
                DirectInvokeCall::new(wire, boundary_input.as_bytes()).into(),
            )
            .unwrap();

        let wrapper = format!(r#"{{"binding_id":"{id}","input":{boundary_input}}}"#);
        assert_eq!(wrapper.len(), MAX_INVOCATION_ARGUMENT_BYTES);
        portable
            .invoke(
                registry,
                &fixture.session(),
                PortableInvokeCall::new(wrapper.as_bytes()).into(),
            )
            .unwrap();

        let padded_input = format!("{boundary_input} ");
        assert_eq!(padded_input.len(), MAX_BOUND_INPUT_BYTES + 1);
        assert_rejected(
            direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, padded_input.as_bytes()).into(),
            InvocationError::InputTooLarge,
        );
        let padded_wrapper = format!(r#"{{"binding_id":"{id}","input":{padded_input}}}"#);
        assert_eq!(padded_wrapper.len(), MAX_INVOCATION_ARGUMENT_BYTES + 1);
        assert_rejected(
            portable,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(padded_wrapper.as_bytes()).into(),
            InvocationError::WrapperTooLarge,
        );

        let mut oversized_input = boundary_input.clone();
        oversized_input.insert(oversized_input.rfind('"').unwrap(), 'x');
        assert_eq!(oversized_input.len(), MAX_BOUND_INPUT_BYTES + 1);
        assert_rejected(
            direct,
            registry,
            &fixture.session(),
            DirectInvokeCall::new(wire, oversized_input.as_bytes()).into(),
            InvocationError::InputTooLarge,
        );
        let oversized_wrapper = format!(
            r#"{{"binding_id":"{id}","input":{}}}"#,
            oversized_input
        );
        assert_rejected(
            portable,
            registry,
            &fixture.session(),
            PortableInvokeCall::new(oversized_wrapper.as_bytes()).into(),
            InvocationError::WrapperTooLarge,
        );
    }

    fn expiry_cases(fixture: &Fixture, registry: &BindingRegistry) {
        let no_grants = CapabilityGrantSnapshot::new(
            &fixture.config,
            Vec::<CapabilityGrant>::new(),
            DigestAlgorithm::Sha256,
        );
        let expired = session(
            &fixture.catalog,
            &fixture.authenticated,
            &fixture.config,
            &no_grants,
            fixture.workspace_id,
            fixture.project_id,
            &fixture.constraints,
        );
        assert_eq!(
            registry.plan(&fixture.supported(), &expired).err().unwrap(),
            RegistrationError::BindingExpired
        );

        let direct = registry
            .plan(&fixture.supported(), &fixture.session())
            .unwrap();
        let portable = registry
            .plan(&fixture.portable(), &fixture.session())
            .unwrap();
        assert_eq!(
            direct
                .deferred_tools(registry, &expired)
                .err()
                .unwrap(),
            RegistrationError::BindingExpired
        );
        let definition = &direct
            .deferred_tools(registry, &fixture.session())
            .unwrap()[0];
        assert_rejected(
            &direct,
            registry,
            &expired,
            DirectInvokeCall::new(
                definition.wire_name(),
                br#"{"count":1,"value":"valid"}"#.as_slice(),
            )
            .into(),
            InvocationError::BindingExpired,
        );
        let wrapper = format!(
            r#"{{"binding_id":"{}","input":{{"count":1,"value":"valid"}}}}"#,
            definition.binding_id()
        );
        assert_rejected(
            &portable,
            registry,
            &expired,
            PortableInvokeCall::new(wrapper.as_bytes()).into(),
            InvocationError::BindingExpired,
        );

        let foreign = Fixture::new(&["foreign"], true);
        assert_eq!(
            direct
                .deferred_tools(registry, &foreign.session())
                .err()
                .unwrap(),
            RegistrationError::BindingExpired
        );
        assert_rejected(
            &direct,
            registry,
            &foreign.session(),
            DirectInvokeCall::new(
                definition.wire_name(),
                br#"{"count":1,"value":"valid"}"#.as_slice(),
            )
            .into(),
            InvocationError::BindingExpired,
        );
        assert_eq!(
            registry
                .plan(&fixture.supported(), &foreign.session())
                .err()
                .unwrap(),
            RegistrationError::BindingExpired
        );
    }

    fn registry_generation(fixture: &Fixture, registry: &BindingRegistry) {
        let plan = registry
            .plan(&fixture.supported(), &fixture.session())
            .unwrap();
        let definition = &plan
            .deferred_tools(registry, &fixture.session())
            .unwrap()[0];
        let next = BindingRegistry::new(
            std::iter::once(Arc::clone(&fixture.bindings[0]))
                .chain(binding_variants(fixture, 1)),
        )
        .unwrap();
        assert_eq!(
            plan.deferred_tools(&next, &fixture.session())
                .err()
                .unwrap(),
            RegistrationError::RegistryMismatch
        );
        assert_rejected(
            &plan,
            &next,
            &fixture.session(),
            DirectInvokeCall::new(
                definition.wire_name(),
                br#"{"count":1,"value":"valid"}"#.as_slice(),
            )
            .into(),
            InvocationError::RegistryMismatch,
        );
    }

    fn registry_bounds(fixture: &Fixture) {
        let consumed = Rc::new(Cell::new(0));
        let counter = Rc::clone(&consumed);
        let duplicate = std::iter::repeat_with(move || {
            counter.set(counter.get() + 1);
            Arc::clone(&fixture.bindings[0])
        });
        assert_eq!(
            BindingRegistry::new(duplicate).unwrap_err(),
            RegistrationError::DuplicateBinding
        );
        assert_eq!(consumed.get(), 2);

        let other = Fixture::new(&["other-catalog"], true);
        assert_eq!(
            BindingRegistry::new([
                Arc::clone(&fixture.bindings[0]),
                Arc::clone(&other.bindings[0]),
            ])
            .unwrap_err(),
            RegistrationError::CatalogMismatch
        );

        let variants = binding_variants(fixture, MAX_CATALOG_ENTRIES);
        let consumed = Rc::new(Cell::new(0));
        let counter = Rc::clone(&consumed);
        let infinite = variants.into_iter().cycle().inspect(move |_| {
            counter.set(counter.get() + 1);
        });
        assert_eq!(
            BindingRegistry::new(infinite).unwrap_err(),
            RegistrationError::RegistrationLimitExceeded
        );
        assert_eq!(consumed.get(), MAX_CATALOG_ENTRIES + 1);

        let large = Fixture::with_documentation(&["large-payload"], true, 64 * 1024);
        let payload = large.bindings[0].pinned_entry().payload_bytes();
        let count = MAX_CATALOG_PAYLOAD_BYTES / payload + 1;
        assert!(count <= MAX_CATALOG_ENTRIES);
        assert_eq!(
            BindingRegistry::new(binding_variants(&large, count)).unwrap_err(),
            RegistrationError::CatalogPayloadExceeded
        );
    }

    fn binding_variants(fixture: &Fixture, count: usize) -> Vec<Arc<CapabilityBinding>> {
        (0..count)
            .map(|index| {
                let config = config(
                    fixture.principal_id,
                    fixture.project_id,
                    u64::try_from(index).unwrap() + 1,
                );
                let grants = grants_for(
                    &fixture.catalog,
                    &config,
                    fixture.principal_id,
                    fixture.project_id,
                    fixture.workspace_id,
                    &fixture.constraints,
                );
                let current = session(
                    &fixture.catalog,
                    &fixture.authenticated,
                    &config,
                    &grants,
                    fixture.workspace_id,
                    fixture.project_id,
                    &fixture.constraints,
                );
                bind_named(
                    &current,
                    fixture.bindings[0]
                        .pinned_entry()
                        .identity()
                        .name()
                        .as_str(),
                )
            })
            .collect()
    }

    fn assert_portable(
        plan: &RegistrationPlan,
        registry: &BindingRegistry,
        current: &DiscoverySession<'_>,
    ) {
        assert_eq!(plan.mode(), RegistrationMode::PortableGeneric);
        assert_eq!(
            operations(plan),
            ["tools.bind", "tools.inspect", "tools.invoke", "tools.search"]
        );
        assert!(plan.deferred_tools(registry, current).unwrap().is_empty());
    }

    fn assert_strict_tool(tool: &ToolSpec) {
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.input_schema["additionalProperties"], false);
        jsonschema::draft202012::options()
            .build(&tool.input_schema)
            .unwrap();
    }

    fn assert_rejected(
        plan: &RegistrationPlan,
        registry: &BindingRegistry,
        current: &DiscoverySession<'_>,
        call: RegistrationCall,
        expected: InvocationError,
    ) {
        let result = plan.invoke(registry, current, call);
        assert_eq!(result.unwrap_err(), expected);
    }

    fn observe(context: &InvocationContext<'_>) -> Observation {
        assert_eq!(
            context.validation_schema().source().normalized_digest(),
            context.schema_digest()
        );
        assert_eq!(context.effect(), EffectClass::WorkspaceRead);
        assert_eq!(context.retry_safety(), RetrySafety::Idempotent);
        Observation {
            binding_id: context.binding_id(),
            kind: context.kind(),
            capability: context.capability().clone(),
            schema_digest: context.schema_digest(),
            authorization_snapshot_digest: context.authorization_snapshot_digest(),
            input: context.input().clone(),
            input_bytes: context.input_bytes().to_vec(),
        }
    }

    fn operations(plan: &RegistrationPlan) -> Vec<&str> {
        plan.eager_tools().iter().map(operation).collect()
    }

    fn names(plan: &RegistrationPlan) -> Vec<&str> {
        plan.eager_tools()
            .iter()
            .map(|tool| tool.name.0.as_str())
            .collect()
    }

    fn operation(tool: &ToolSpec) -> &str {
        tool.metadata["kit.operation"].as_str().unwrap()
    }

    fn supported_contract(profile: &ProjectionProfile) -> ProviderCapabilityContract {
        ProviderCapabilityContract::new(
            Some(DeferredRegistrationDeclaration::new(
                profile.target().clone(),
                profile.digest(),
            )),
            ValidatedProjectionSupport::validate(profile).unwrap(),
        )
    }

    fn target(model: &str) -> ProjectionTarget {
        ProjectionTarget::new("explicit-provider", model, "fixture-adapter@1", 1).unwrap()
    }

    fn profile(target: ProjectionTarget, extra_keyword: bool) -> ProjectionProfile {
        let mut keywords = BTreeSet::from([
            "$schema".to_owned(),
            "additionalProperties".to_owned(),
            "maxLength".to_owned(),
            "maximum".to_owned(),
            "minLength".to_owned(),
            "minimum".to_owned(),
            "pattern".to_owned(),
            "properties".to_owned(),
            "required".to_owned(),
            "title".to_owned(),
            "type".to_owned(),
        ]);
        if extra_keyword {
            keywords.insert("description".to_owned());
        }
        ProjectionProfile::new(
            target,
            JSON_SCHEMA_2020_12,
            keywords,
            serde_json::Value::Bool(true),
            1024 * 1024,
            DigestAlgorithm::Sha256,
        )
        .unwrap()
    }

    fn restrictive_profile(target: ProjectionTarget) -> ProjectionProfile {
        ProjectionProfile::new(
            target,
            JSON_SCHEMA_2020_12,
            BTreeSet::from([
                "$schema".to_owned(),
                "additionalProperties".to_owned(),
                "maxLength".to_owned(),
                "maximum".to_owned(),
                "minLength".to_owned(),
                "minimum".to_owned(),
                "properties".to_owned(),
                "required".to_owned(),
                "title".to_owned(),
                "type".to_owned(),
            ]),
            serde_json::Value::Bool(true),
            1024 * 1024,
            DigestAlgorithm::Sha256,
        )
        .unwrap()
    }

    fn schema(label: &str, documentation_bytes: usize) -> NormalizedSchema {
        let documentation = if documentation_bytes == 0 {
            format!("exact documentation {label}").into_bytes()
        } else {
            vec![b'd'; documentation_bytes]
        };
        NormalizedSchema::ingest(
            serde_json::to_vec(&serde_json::json!({
                "$schema": JSON_SCHEMA_2020_12,
                "title": label,
                "type": "object",
                "properties": {
                    "count": {"type": "integer"},
                    "value": {"type": "string", "minLength": 1}
                },
                "required": ["count", "value"],
                "additionalProperties": false
            }))
            .unwrap(),
            JSON_SCHEMA_2020_12,
            documentation,
            DigestAlgorithm::Sha256,
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        name: &str,
        kind: CapabilityKind,
        schema_label: &str,
        target: &ProjectionTarget,
        profile: &ProjectionProfile,
        projected: bool,
        summary: &str,
        documentation_bytes: usize,
    ) -> CatalogEntry {
        assert!(summary.len() <= MAX_SUMMARY_BYTES);
        let source = CapabilitySource::new("registration-source").unwrap();
        let mut input = SchemaProjectionSet::new(schema(schema_label, documentation_bytes));
        if projected {
            assert_eq!(profile.target(), target);
            input.project(profile).unwrap();
        }
        CatalogEntry::new(
            CapabilityIdentity::new(
                source.clone(),
                CapabilityNamespace::new("kit.registration").unwrap(),
                CapabilityName::new(name).unwrap(),
                CapabilityVersion::new("1.0.0").unwrap(),
                Digest::of(DigestAlgorithm::Sha256, format!("implementation:{name}").as_bytes()),
            ),
            CatalogSource::new(
                SourceKind::Mcp,
                source,
                TrustDomain::new("registration-trust").unwrap(),
            )
            .unwrap(),
            kind,
            CatalogSchemas::new(
                input,
                Some(SchemaProjectionSet::new(schema("output", documentation_bytes))),
            ),
            CatalogSearch::new(summary, ["registration", name]).unwrap(),
            SideEffects::new(EffectClass::WorkspaceRead, RetrySafety::Idempotent),
            CatalogAuthority::new([Grant::WorkspaceRead], Vec::<String>::new()).unwrap(),
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
        )
        .unwrap()
    }

    fn bind_named(session: &DiscoverySession<'_>, name: &str) -> Arc<CapabilityBinding> {
        let result = session.search(name, 1).unwrap().remove(0);
        let inspection = session.inspect(result.handle()).unwrap();
        Arc::new(session.bind(&inspection).unwrap())
    }

    #[allow(clippy::too_many_arguments)]
    fn session<'a>(
        catalog: &'a CatalogSnapshot,
        authenticated: &'a AuthenticatedPrincipal,
        config: &'a RunConfigSnapshot,
        grants: &'a CapabilityGrantSnapshot,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        constraints: &'a ArgumentConstraints,
    ) -> DiscoverySession<'a> {
        DiscoverySession::new(
            catalog,
            authenticated,
            config,
            grants,
            None,
            workspace_id,
            project_id,
            constraints,
            RequestExtension::default(),
        )
    }

    fn config(
        principal_id: PrincipalId,
        project_id: ProjectId,
        max_tokens: u64,
    ) -> RunConfigSnapshot {
        let authority = BTreeSet::from([Grant::WorkspaceRead]);
        LayerStack {
            built_in: ConfigLayer {
                schema_version: CONFIG_SCHEMA_VERSION,
                budgets: BudgetLayer {
                    max_tokens: Some(max_tokens),
                    max_cost_microusd: Some(100),
                    max_turns: Some(10),
                },
                concurrency: ConcurrencyLayer {
                    max_runs: Some(2),
                    max_tools: Some(2),
                },
                retention: RetentionLayer {
                    event_days: Some(7),
                    artifact_days: Some(7),
                },
                provider: Some(Provider::Anthropic),
                executor: Some(Executor::Local),
                grammar_edit: Some(Default::default()),
                grants: Some(authority.clone()),
            },
            user: None,
            project: None,
            run: None,
            experiment: None,
        }
        .materialize(
            RunConfigContext {
                principal_id,
                project_id,
                run_id: RunId::generate().unwrap(),
            },
            &authority,
        )
        .unwrap()
    }

    fn authenticate(
        principal_id: PrincipalId,
        project_id: ProjectId,
    ) -> AuthenticatedPrincipal {
        LocalPeerAuthenticator::new(BTreeMap::from([(
            UID,
            GrantSnapshot::new(
                principal_id,
                project_id,
                BTreeSet::from([Grant::WorkspaceRead]),
            ),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(UID, 1, UID))
        .unwrap()
    }

    fn grants_for(
        catalog: &CatalogSnapshot,
        config: &RunConfigSnapshot,
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
        constraints: &ArgumentConstraints,
    ) -> CapabilityGrantSnapshot {
        CapabilityGrantSnapshot::new(
            config,
            catalog.entries().iter().map(|entry| {
                CapabilityGrant::new(
                    principal_id,
                    project_id,
                    workspace_id,
                    entry.identity().clone(),
                    entry
                        .schemas()
                        .input()
                        .schema()
                        .source()
                        .normalized_digest(),
                    entry.side_effects().effect(),
                    constraints.clone(),
                )
            }),
            DigestAlgorithm::Sha256,
        )
    }
}
