use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fmt,
    sync::Arc,
};

use agentkit_core::MetadataMap;
use agentkit_tools_core::{ToolName, ToolSpec};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value, json};

use crate::capabilities::{
    catalog::{CapabilityKind, MAX_CATALOG_ENTRIES, MAX_CATALOG_PAYLOAD_BYTES},
    discovery::{BindingId, CapabilityBinding, DiscoverySession},
    kernel::{
        grant::EffectClass,
        identity::{CapabilityIdentity, Digest, DigestAlgorithm, put_bytes, put_digest},
        invoke::{MAX_INVOCATION_ARGUMENT_BYTES, RetrySafety},
    },
    schema::{
        JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionError, ProjectionProfile,
        ProjectionTarget, SchemaProjectionSet, SchemaValidation, number_is_lossless,
    },
};

pub const MAX_TOOL_NAME_BYTES: usize = 64;
pub const MAX_BOUND_INPUT_BYTES: usize =
    MAX_INVOCATION_ARGUMENT_BYTES - GENERIC_WRAPPER_OVERHEAD_BYTES;

const REGISTRATION_FORMAT_VERSION: u16 = 1;
const MAX_INPUT_DEPTH: usize = 64;
const MAX_INPUT_NODES: usize = 100_000;
const GENERIC_WRAPPER_DEPTH_OVERHEAD: usize = 1;
const GENERIC_WRAPPER_NODE_OVERHEAD: usize = 2;
const GENERIC_WRAPPER_OVERHEAD_BYTES: usize = br#"{"binding_id":"","input":}"#.len() + 75;
const SEARCH_SCHEMA: &[u8] = br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","additionalProperties":false,"properties":{"limit":{"maximum":100,"minimum":1,"type":"integer"},"query":{"maxLength":64,"minLength":1,"type":"string"}},"required":["query","limit"],"type":"object"}"#;
const INSPECT_SCHEMA: &[u8] = br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","additionalProperties":false,"properties":{"handle":{"maxLength":128,"minLength":1,"type":"string"}},"required":["handle"],"type":"object"}"#;
const BIND_SCHEMA: &[u8] = INSPECT_SCHEMA;
const INVOKE_SCHEMA: &[u8] = br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","additionalProperties":false,"properties":{"binding_id":{"maxLength":75,"minLength":75,"pattern":"^binding_v1_[0-9a-f]{64}$","type":"string"},"input":true},"required":["binding_id","input"],"type":"object"}"#;

const CORE_EAGER_TOOLS: [(&str, &str, &str, &[u8]); 3] = [
    (
        "tools.search",
        "tools_search",
        "Search available capabilities.",
        SEARCH_SCHEMA,
    ),
    (
        "tools.inspect",
        "tools_inspect",
        "Inspect one capability definition.",
        INSPECT_SCHEMA,
    ),
    (
        "tools.bind",
        "tools_bind",
        "Bind an inspected capability.",
        BIND_SCHEMA,
    ),
];

const HOST_TOOLS: [(&str, &str, &str, &[u8]); 4] = [
    CORE_EAGER_TOOLS[0],
    CORE_EAGER_TOOLS[1],
    CORE_EAGER_TOOLS[2],
    (
        "tools.invoke",
        "tools_invoke",
        "Invoke a bound capability.",
        INVOKE_SCHEMA,
    ),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredRegistrationDeclaration {
    target: ProjectionTarget,
    profile_digest: Digest,
}

impl DeferredRegistrationDeclaration {
    pub const fn new(target: ProjectionTarget, profile_digest: Digest) -> Self {
        Self {
            target,
            profile_digest,
        }
    }

    pub const fn target(&self) -> &ProjectionTarget {
        &self.target
    }

    pub const fn profile_digest(&self) -> Digest {
        self.profile_digest
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProjectionSupport {
    target: ProjectionTarget,
    profile_digest: Digest,
    tools: Arc<[ToolSpec]>,
}

impl ValidatedProjectionSupport {
    pub fn validate(profile: &ProjectionProfile) -> Result<Self, ProjectionError> {
        let mut tools = Vec::with_capacity(HOST_TOOLS.len());
        for (operation, wire_name, description, schema) in HOST_TOOLS {
            let normalized = NormalizedSchema::ingest(
                schema,
                JSON_SCHEMA_2020_12,
                operation,
                DigestAlgorithm::Sha256,
            )?;
            let mut projections = SchemaProjectionSet::new(normalized);
            let projection = projections.project(profile)?;
            tools.push(host_tool(
                operation,
                wire_name,
                description,
                projection.value().clone(),
            ));
        }
        tools.sort_by(|left, right| operation(left).cmp(operation(right)));
        Ok(Self {
            target: profile.target().clone(),
            profile_digest: profile.digest(),
            tools: tools.into(),
        })
    }

    pub const fn target(&self) -> &ProjectionTarget {
        &self.target
    }

    pub const fn profile_digest(&self) -> Digest {
        self.profile_digest
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderCapabilityContract {
    declaration: Option<DeferredRegistrationDeclaration>,
    support: ValidatedProjectionSupport,
}

impl ProviderCapabilityContract {
    pub const fn new(
        declaration: Option<DeferredRegistrationDeclaration>,
        support: ValidatedProjectionSupport,
    ) -> Self {
        Self {
            declaration,
            support,
        }
    }

    pub const fn portable(support: ValidatedProjectionSupport) -> Self {
        Self::new(None, support)
    }

    pub const fn declaration(&self) -> Option<&DeferredRegistrationDeclaration> {
        self.declaration.as_ref()
    }

    pub const fn validated_support(&self) -> &ValidatedProjectionSupport {
        &self.support
    }

    fn deferred_evidence(
        &self,
    ) -> Option<(
        &DeferredRegistrationDeclaration,
        &ValidatedProjectionSupport,
    )> {
        let declaration = self.declaration.as_ref()?;
        let support = &self.support;
        (declaration.target == support.target
            && declaration.profile_digest == support.profile_digest)
            .then_some((declaration, support))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationMode {
    Deferred,
    PortableGeneric,
}

pub struct DeferredToolDefinition {
    binding_id: BindingId,
    spec: ToolSpec,
}

impl DeferredToolDefinition {
    pub const fn binding_id(&self) -> BindingId {
        self.binding_id
    }

    pub const fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub fn wire_name(&self) -> &str {
        &self.spec.name.0
    }

    pub fn summary(&self) -> &str {
        &self.spec.description
    }

    pub const fn input_schema(&self) -> &Value {
        &self.spec.input_schema
    }
}

pub struct RegistrationPlan {
    mode: RegistrationMode,
    registry_digest: Digest,
    eager: Arc<[ToolSpec]>,
    deferred: Arc<[DeferredToolDefinition]>,
}

impl RegistrationPlan {
    pub const fn mode(&self) -> RegistrationMode {
        self.mode
    }

    pub fn eager_tools(&self) -> &[ToolSpec] {
        &self.eager
    }

    pub fn deferred_tools<'a>(
        &'a self,
        registry: &BindingRegistry,
        current: &DiscoverySession<'_>,
    ) -> Result<&'a [DeferredToolDefinition], RegistrationError> {
        if self.registry_digest != registry.digest {
            return Err(RegistrationError::RegistryMismatch);
        }
        for binding in registry.bindings.values() {
            binding
                .validate(current)
                .map_err(|_| RegistrationError::BindingExpired)?;
        }
        Ok(&self.deferred)
    }

    pub(crate) fn deferred_tools_authorized<'a>(
        &'a self,
        registry: &BindingRegistry,
        mut validate: impl FnMut(&CapabilityBinding) -> bool,
    ) -> Result<&'a [DeferredToolDefinition], RegistrationError> {
        if self.registry_digest != registry.digest {
            return Err(RegistrationError::RegistryMismatch);
        }
        if registry.bindings.values().any(|binding| !validate(binding)) {
            return Err(RegistrationError::BindingExpired);
        }
        Ok(&self.deferred)
    }

    pub fn invoke(
        &self,
        registry: &BindingRegistry,
        current: &DiscoverySession<'_>,
        call: RegistrationCall,
    ) -> Result<BoundRegistrationCall, InvocationError> {
        if self.registry_digest != registry.digest {
            return Err(InvocationError::RegistryMismatch);
        }
        let normalized = match (self.mode, call) {
            (RegistrationMode::Deferred, RegistrationCall::Direct(call)) => {
                let binding_id = self
                    .deferred
                    .binary_search_by(|definition| definition.wire_name().cmp(&call.wire_name))
                    .ok()
                    .map(|index| self.deferred[index].binding_id())
                    .ok_or(InvocationError::UnknownWireName)?;
                NormalizedCall::direct(binding_id, &call.bytes)?
            }
            (_, RegistrationCall::Portable(call)) => NormalizedCall::portable(&call.bytes)?,
            _ => return Err(InvocationError::WrongMode),
        };
        let binding = registry
            .bindings
            .get(&normalized.binding_id)
            .ok_or(InvocationError::UnknownBinding)?;
        binding
            .validate(current)
            .map_err(|_| InvocationError::BindingExpired)?;
        match binding
            .pinned_entry()
            .schemas()
            .input()
            .schema()
            .validate(&normalized.input)
        {
            SchemaValidation::Valid => {}
            SchemaValidation::Invalid(path) => return Err(InvocationError::SchemaInvalid(path)),
            SchemaValidation::Unsupported => return Err(InvocationError::SchemaUnsupported),
        }
        Ok(BoundRegistrationCall {
            binding: Arc::clone(binding),
            input: normalized.input,
            input_bytes: normalized.bytes,
        })
    }

    pub(crate) fn invoke_authorized(
        &self,
        registry: &BindingRegistry,
        call: RegistrationCall,
        mut validate: impl FnMut(&CapabilityBinding) -> bool,
    ) -> Result<BoundRegistrationCall, InvocationError> {
        if self.registry_digest != registry.digest {
            return Err(InvocationError::RegistryMismatch);
        }
        let normalized = match (self.mode, call) {
            (RegistrationMode::Deferred, RegistrationCall::Direct(call)) => {
                let binding_id = self
                    .deferred
                    .binary_search_by(|definition| definition.wire_name().cmp(&call.wire_name))
                    .ok()
                    .map(|index| self.deferred[index].binding_id())
                    .ok_or(InvocationError::UnknownWireName)?;
                NormalizedCall::direct(binding_id, &call.bytes)?
            }
            (_, RegistrationCall::Portable(call)) => NormalizedCall::portable(&call.bytes)?,
            _ => return Err(InvocationError::WrongMode),
        };
        let binding = registry
            .bindings
            .get(&normalized.binding_id)
            .ok_or(InvocationError::UnknownBinding)?;
        if !validate(binding) {
            return Err(InvocationError::BindingExpired);
        }
        match binding
            .pinned_entry()
            .schemas()
            .input()
            .schema()
            .validate(&normalized.input)
        {
            SchemaValidation::Valid => {}
            SchemaValidation::Invalid(path) => return Err(InvocationError::SchemaInvalid(path)),
            SchemaValidation::Unsupported => return Err(InvocationError::SchemaUnsupported),
        }
        Ok(BoundRegistrationCall {
            binding: Arc::clone(binding),
            input: normalized.input,
            input_bytes: normalized.bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BindingRegistry {
    bindings: Arc<BTreeMap<BindingId, Arc<CapabilityBinding>>>,
    digest: Digest,
}

impl BindingRegistry {
    pub fn new<I>(bindings: I) -> Result<Self, RegistrationError>
    where
        I: IntoIterator<Item = Arc<CapabilityBinding>>,
    {
        let mut by_id = BTreeMap::new();
        let mut count = 0_usize;
        let mut payload_bytes = 0_usize;
        let mut catalog_digest = None;
        for binding in bindings {
            count = count
                .checked_add(1)
                .ok_or(RegistrationError::RegistrationLimitExceeded)?;
            if count > MAX_CATALOG_ENTRIES {
                return Err(RegistrationError::RegistrationLimitExceeded);
            }
            let id = binding.id();
            let entry = match by_id.entry(id) {
                Entry::Vacant(entry) => entry,
                Entry::Occupied(_) => return Err(RegistrationError::DuplicateBinding),
            };
            match catalog_digest {
                Some(digest) if digest != binding.catalog_digest() => {
                    return Err(RegistrationError::CatalogMismatch);
                }
                None => catalog_digest = Some(binding.catalog_digest()),
                Some(_) => {}
            }
            payload_bytes = payload_bytes
                .checked_add(binding.pinned_entry().payload_bytes())
                .ok_or(RegistrationError::CatalogPayloadExceeded)?;
            if payload_bytes > MAX_CATALOG_PAYLOAD_BYTES {
                return Err(RegistrationError::CatalogPayloadExceeded);
            }
            entry.insert(binding);
        }
        let algorithm = catalog_digest
            .map(Digest::algorithm)
            .unwrap_or(DigestAlgorithm::Sha256);
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"KIT-BINDING-REGISTRY\0");
        canonical.extend_from_slice(&REGISTRATION_FORMAT_VERSION.to_be_bytes());
        if let Some(catalog_digest) = catalog_digest {
            canonical.push(1);
            put_digest(&mut canonical, catalog_digest);
        } else {
            canonical.push(0);
        }
        canonical.extend_from_slice(&(by_id.len() as u64).to_be_bytes());
        for binding_id in by_id.keys() {
            put_bytes(&mut canonical, binding_id.to_string().as_bytes());
        }
        Ok(Self {
            bindings: Arc::new(by_id),
            digest: Digest::of(algorithm, &canonical),
        })
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn plan(
        &self,
        provider: &ProviderCapabilityContract,
        current: &DiscoverySession<'_>,
    ) -> Result<RegistrationPlan, RegistrationError> {
        self.plan_authorized(provider, |binding| binding.validate(current).is_ok())
    }

    pub(crate) fn plan_authorized(
        &self,
        provider: &ProviderCapabilityContract,
        mut validate: impl FnMut(&CapabilityBinding) -> bool,
    ) -> Result<RegistrationPlan, RegistrationError> {
        if self.bindings.values().any(|binding| !validate(binding)) {
            return Err(RegistrationError::BindingExpired);
        }

        let declaration = provider
            .deferred_evidence()
            .map(|(declaration, _)| declaration);
        let deferred = declaration.is_some_and(|declaration| {
            self.bindings
                .values()
                .filter(|binding| binding.pinned_entry().kind() == CapabilityKind::Tool)
                .all(|binding| {
                    binding
                        .pinned_entry()
                        .schemas()
                        .input()
                        .projection(declaration.target())
                        .is_some_and(|projection| {
                            projection.profile_digest() == declaration.profile_digest()
                                && projection.digest() == binding.input_schema_digest()
                        })
                })
        });
        let mode = if deferred {
            RegistrationMode::Deferred
        } else {
            RegistrationMode::PortableGeneric
        };
        let eager = provider
            .validated_support()
            .tools
            .iter()
            .cloned()
            .collect::<Vec<_>>();

        let mut definitions = Vec::new();
        if let Some(declaration) = declaration.filter(|_| deferred) {
            let mut wire_names = eager
                .iter()
                .map(|tool| tool.name.0.clone())
                .collect::<BTreeSet<_>>();
            definitions.reserve(self.bindings.len());
            for binding in self.bindings.values() {
                if binding.pinned_entry().kind() != CapabilityKind::Tool {
                    continue;
                }
                let projection = binding
                    .pinned_entry()
                    .schemas()
                    .input()
                    .projection(declaration.target())
                    .expect("deferred eligibility checked every projection");
                let wire_name = direct_wire_name(binding.id());
                if wire_name.len() > MAX_TOOL_NAME_BYTES || !wire_names.insert(wire_name.clone()) {
                    return Err(RegistrationError::WireNameCollision);
                }
                let identity = binding.pinned_entry().identity();
                let mut metadata = MetadataMap::new();
                metadata.insert(
                    "kit.operation".to_owned(),
                    json!(format!(
                        "{}.{}",
                        identity.namespace().as_str(),
                        identity.name().as_str()
                    )),
                );
                metadata.insert("kit.binding_id".to_owned(), json!(binding.id().to_string()));
                metadata.insert(
                    "kit.schema.digest".to_owned(),
                    json!(binding.input_schema_digest().to_string()),
                );
                metadata.insert(
                    "kit.schema.profile_digest".to_owned(),
                    json!(projection.profile_digest().to_string()),
                );
                definitions.push(DeferredToolDefinition {
                    binding_id: binding.id(),
                    spec: ToolSpec::new(
                        ToolName::new(wire_name),
                        binding.pinned_entry().search().summary(),
                        projection.value().clone(),
                    )
                    .with_metadata(metadata),
                });
            }
            definitions.sort_by(|left, right| left.wire_name().cmp(right.wire_name()));
        }
        Ok(RegistrationPlan {
            mode,
            registry_digest: self.digest,
            eager: eager.into(),
            deferred: definitions.into(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct DirectInvokeCall {
    wire_name: String,
    bytes: Arc<[u8]>,
}

impl DirectInvokeCall {
    pub fn new(wire_name: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            wire_name: wire_name.into(),
            bytes: bytes.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PortableInvokeCall {
    bytes: Arc<[u8]>,
}

impl PortableInvokeCall {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum RegistrationCall {
    Direct(DirectInvokeCall),
    Portable(PortableInvokeCall),
}

#[derive(Clone, Debug)]
pub struct BoundRegistrationCall {
    binding: Arc<CapabilityBinding>,
    input: Value,
    input_bytes: Vec<u8>,
}

impl BoundRegistrationCall {
    pub(crate) fn direct(
        binding: Arc<CapabilityBinding>,
        bytes: &[u8],
    ) -> Result<Self, InvocationError> {
        let normalized = NormalizedCall::direct(binding.id(), bytes)?;
        match binding
            .pinned_entry()
            .schemas()
            .input()
            .schema()
            .validate(&normalized.input)
        {
            SchemaValidation::Valid => Ok(Self {
                binding,
                input: normalized.input,
                input_bytes: normalized.bytes,
            }),
            SchemaValidation::Invalid(path) => Err(InvocationError::SchemaInvalid(path)),
            SchemaValidation::Unsupported => Err(InvocationError::SchemaUnsupported),
        }
    }

    pub fn context(&self) -> InvocationContext<'_> {
        InvocationContext {
            binding: &self.binding,
            input: &self.input,
            input_bytes: &self.input_bytes,
        }
    }

    pub fn binding(&self) -> &CapabilityBinding {
        &self.binding
    }

    pub fn input_bytes(&self) -> &[u8] {
        &self.input_bytes
    }
}

impl From<DirectInvokeCall> for RegistrationCall {
    fn from(value: DirectInvokeCall) -> Self {
        Self::Direct(value)
    }
}

impl From<PortableInvokeCall> for RegistrationCall {
    fn from(value: PortableInvokeCall) -> Self {
        Self::Portable(value)
    }
}

#[derive(Debug)]
pub struct InvocationContext<'a> {
    binding: &'a CapabilityBinding,
    input: &'a Value,
    input_bytes: &'a [u8],
}

impl<'a> InvocationContext<'a> {
    pub const fn binding_id(&self) -> BindingId {
        self.binding.id()
    }

    pub fn capability(&self) -> &CapabilityIdentity {
        self.binding.pinned_entry().identity()
    }

    pub const fn binding(&self) -> &'a CapabilityBinding {
        self.binding
    }

    pub fn kind(&self) -> CapabilityKind {
        self.binding.pinned_entry().kind()
    }

    pub const fn schema_digest(&self) -> Digest {
        self.binding.input_schema_digest()
    }

    pub const fn authorization_snapshot_digest(&self) -> Digest {
        self.binding.authorization_snapshot_digest()
    }

    pub fn validation_schema(&self) -> &NormalizedSchema {
        self.binding.pinned_entry().schemas().input().schema()
    }

    pub fn output_schema(&self) -> Option<&NormalizedSchema> {
        self.binding
            .pinned_entry()
            .schemas()
            .output()
            .map(SchemaProjectionSet::schema)
    }

    pub fn effect(&self) -> EffectClass {
        self.binding.pinned_entry().side_effects().effect()
    }

    pub fn retry_safety(&self) -> RetrySafety {
        self.binding.pinned_entry().side_effects().retry_safety()
    }

    pub const fn input(&self) -> &'a Value {
        self.input
    }

    pub const fn input_bytes(&self) -> &'a [u8] {
        self.input_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    RegistrationLimitExceeded,
    CatalogPayloadExceeded,
    CatalogMismatch,
    DuplicateBinding,
    RegistryMismatch,
    BindingExpired,
    WireNameCollision,
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RegistrationLimitExceeded => "capability registration limit exceeded",
            Self::CatalogPayloadExceeded => "capability registration payload limit exceeded",
            Self::CatalogMismatch => "capability bindings come from different catalogs",
            Self::DuplicateBinding => "capability binding is registered more than once",
            Self::RegistryMismatch => "registration plan does not match the binding registry",
            Self::BindingExpired => "capability binding expired",
            Self::WireNameCollision => "provider tool wire name collides",
        })
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvocationError {
    MalformedInput,
    MalformedGenericWrapper,
    WrapperTooLarge,
    InputTooLarge,
    WrongMode,
    RegistryMismatch,
    UnknownWireName,
    UnknownBinding,
    BindingExpired,
    SchemaInvalid(String),
    SchemaUnsupported,
}

impl fmt::Display for InvocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MalformedInput => "capability invocation input is malformed",
            Self::MalformedGenericWrapper => "generic capability invocation wrapper is malformed",
            Self::WrapperTooLarge => "generic capability invocation wrapper exceeds its byte limit",
            Self::InputTooLarge => "capability invocation input exceeds its byte limit",
            Self::WrongMode => "capability invocation route does not match its registration plan",
            Self::RegistryMismatch => "registration plan does not match the binding registry",
            Self::UnknownWireName => "deferred capability wire name is unknown",
            Self::UnknownBinding => "capability binding is unknown",
            Self::BindingExpired => "capability binding expired",
            Self::SchemaInvalid(_) => "capability invocation input does not match its schema",
            Self::SchemaUnsupported => "capability invocation schema cannot be validated",
        })
    }
}

impl std::error::Error for InvocationError {}

struct NormalizedCall {
    binding_id: BindingId,
    input: Value,
    bytes: Vec<u8>,
}

impl NormalizedCall {
    fn direct(binding_id: BindingId, bytes: &[u8]) -> Result<Self, InvocationError> {
        if bytes.len() > MAX_BOUND_INPUT_BYTES {
            return Err(InvocationError::InputTooLarge);
        }
        preflight_input(bytes, MAX_INPUT_DEPTH, MAX_INPUT_NODES)
            .map_err(|_| InvocationError::MalformedInput)?;
        let input = parse_input(bytes).map_err(|_| InvocationError::MalformedInput)?;
        Self::finish(binding_id, input)
    }

    fn portable(bytes: &[u8]) -> Result<Self, InvocationError> {
        if bytes.len() > MAX_INVOCATION_ARGUMENT_BYTES {
            return Err(InvocationError::WrapperTooLarge);
        }
        preflight_input(
            bytes,
            MAX_INPUT_DEPTH + GENERIC_WRAPPER_DEPTH_OVERHEAD,
            MAX_INPUT_NODES + GENERIC_WRAPPER_NODE_OVERHEAD,
        )
        .map_err(|_| InvocationError::MalformedGenericWrapper)?;
        let wrapper = serde_json::from_slice::<GenericWrapper>(bytes)
            .map_err(|_| InvocationError::MalformedGenericWrapper)?;
        let binding_id = BindingId::parse(&wrapper.binding_id)
            .map_err(|_| InvocationError::MalformedGenericWrapper)?;
        validate_input_shape(&wrapper.input.0)
            .map_err(|_| InvocationError::MalformedGenericWrapper)?;
        Self::finish(binding_id, wrapper.input.0)
    }

    fn finish(binding_id: BindingId, input: Value) -> Result<Self, InvocationError> {
        let bytes = serde_json::to_vec(&input).map_err(|_| InvocationError::MalformedInput)?;
        if bytes.len() > MAX_BOUND_INPUT_BYTES {
            return Err(InvocationError::InputTooLarge);
        }
        Ok(Self {
            binding_id,
            input,
            bytes,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericWrapper {
    binding_id: String,
    input: UniqueValue,
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut input: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some((key, value)) = input.next_entry::<String, UniqueValue>()? {
            if values.insert(key, value.0).is_some() {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
        }
        Ok(UniqueValue(Value::Object(
            values.into_iter().collect::<Map<_, _>>(),
        )))
    }
}

fn parse_input(bytes: &[u8]) -> Result<Value, ()> {
    let value = serde_json::from_slice::<UniqueValue>(bytes)
        .map_err(|_| ())?
        .0;
    validate_input_shape(&value)?;
    Ok(value)
}

fn validate_input_shape(value: &Value) -> Result<(), ()> {
    fn visit(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), ()> {
        *nodes = nodes.checked_add(1).ok_or(())?;
        if depth > MAX_INPUT_DEPTH || *nodes > MAX_INPUT_NODES {
            return Err(());
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, depth + 1, nodes)?;
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, depth + 1, nodes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    visit(value, 0, &mut 0)
}

fn preflight_input(bytes: &[u8], max_depth: usize, max_nodes: usize) -> Result<(), ()> {
    fn add_node(nodes: &mut usize, max_nodes: usize) -> Result<(), ()> {
        *nodes = nodes.checked_add(1).ok_or(())?;
        (*nodes <= max_nodes).then_some(()).ok_or(())
    }

    let mut stack = [0_u8; MAX_INPUT_DEPTH + GENERIC_WRAPPER_DEPTH_OVERHEAD];
    let mut depth = 0_usize;
    let mut nodes = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                add_node(&mut nodes, max_nodes)?;
                if depth == max_depth {
                    return Err(());
                }
                stack[depth] = bytes[index];
                depth += 1;
            }
            b'}' | b']' => {
                let expected = if bytes[index] == b'}' { b'{' } else { b'[' };
                if depth == 0 || stack[depth - 1] != expected {
                    return Err(());
                }
                depth -= 1;
            }
            b'"' => {
                index += 1;
                loop {
                    let byte = *bytes.get(index).ok_or(())?;
                    match byte {
                        b'"' => break,
                        b'\\' => {
                            index += 1;
                            match *bytes.get(index).ok_or(())? {
                                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                                b'u' => {
                                    for _ in 0..4 {
                                        index += 1;
                                        if !bytes.get(index).is_some_and(u8::is_ascii_hexdigit) {
                                            return Err(());
                                        }
                                    }
                                }
                                _ => return Err(()),
                            }
                        }
                        0..=0x1f => return Err(()),
                        _ => {}
                    }
                    index += 1;
                }
                let mut next = index + 1;
                while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                    next += 1;
                }
                if bytes.get(next) != Some(&b':') {
                    add_node(&mut nodes, max_nodes)?;
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && !matches!(
                        bytes[index],
                        b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}'
                    )
                {
                    index += 1;
                }
                if !number_is_lossless(&bytes[start..index]) {
                    return Err(());
                }
                add_node(&mut nodes, max_nodes)?;
                continue;
            }
            b't' | b'f' | b'n' => {
                add_node(&mut nodes, max_nodes)?;
                while bytes.get(index + 1).is_some_and(u8::is_ascii_alphabetic) {
                    index += 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    (depth == 0).then_some(()).ok_or(())
}

fn host_tool(operation_name: &str, wire_name: &str, description: &str, schema: Value) -> ToolSpec {
    let mut metadata = MetadataMap::new();
    metadata.insert("kit.operation".to_owned(), json!(operation_name));
    metadata.insert("kit.schema.dialect".to_owned(), json!(JSON_SCHEMA_2020_12));
    ToolSpec::new(ToolName::new(wire_name), description, schema).with_metadata(metadata)
}

fn operation(tool: &ToolSpec) -> &str {
    tool.metadata["kit.operation"]
        .as_str()
        .expect("host tool operation metadata is a string")
}

pub(crate) fn direct_wire_name(binding_id: BindingId) -> String {
    let digest = binding_id.to_string();
    format!("kit_{}", &digest[digest.len() - 60..])
}
