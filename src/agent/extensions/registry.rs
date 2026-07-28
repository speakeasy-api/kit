use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    CompatibilityRange, ContentDigest, ContractVersion, ExtensionDescriptor, ExtensionError,
    ExtensionIdentity, ExtensionPoint, ExtensionReference, ExtensionVersion,
};

pub const BUILT_IN_MODEL_ADAPTER: &str = "kit.durable-model-adapter";
pub const BUILT_IN_PROMPT_MODULES: &str = "kit.prompt-modules";
pub const BUILT_IN_COST_TABLE: &str = "kit.cost-table";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutOfProcessProtocol {
    Mcp,
    Acp,
    A2a,
    KitPlugin,
}

pub const OUT_OF_PROCESS_PROTOCOLS: [OutOfProcessProtocol; 4] = [
    OutOfProcessProtocol::Mcp,
    OutOfProcessProtocol::Acp,
    OutOfProcessProtocol::A2a,
    OutOfProcessProtocol::KitPlugin,
];

/// Possession means the caller is already trusted to execute inside the daemon.
pub(crate) struct TrustedExtensionToken(());

impl TrustedExtensionToken {
    pub(crate) const fn daemon_bootstrap() -> Self {
        Self(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExtensionRegistry {
    entries: BTreeMap<ExtensionReference, ExtensionDescriptor>,
}

impl ExtensionRegistry {
    pub fn from_descriptors(
        descriptors: impl IntoIterator<Item = ExtensionDescriptor>,
    ) -> Result<Self, ExtensionError> {
        let mut registry = Self::default();
        for descriptor in descriptors {
            insert_descriptor(&mut registry.entries, descriptor)?;
        }
        Ok(registry)
    }

    pub fn get(&self, reference: &ExtensionReference) -> Option<&ExtensionDescriptor> {
        self.entries.get(reference)
    }

    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = &ExtensionDescriptor> {
        self.entries.values()
    }

    pub fn descriptor_for(&self, point: ExtensionPoint) -> Option<&ExtensionDescriptor> {
        self.entries
            .values()
            .find(|descriptor| descriptor.extension_point() == point)
    }

    pub fn assert_schema(
        &self,
        extension: &ExtensionReference,
        observed: &ContentDigest,
    ) -> Result<(), ExtensionError> {
        let descriptor = self
            .entries
            .get(extension)
            .ok_or_else(|| ExtensionError::UnknownSelection(extension.clone()))?;
        if descriptor.schema_digest() != observed {
            return Err(ExtensionError::SchemaDrift {
                extension: extension.clone(),
                expected: descriptor.schema_digest().clone(),
                observed: observed.clone(),
            });
        }
        Ok(())
    }

    pub fn assert_implementation(
        &self,
        extension: &ExtensionReference,
        observed: &ContentDigest,
    ) -> Result<(), ExtensionError> {
        let descriptor = self
            .entries
            .get(extension)
            .ok_or_else(|| ExtensionError::UnknownSelection(extension.clone()))?;
        if descriptor.implementation_digest() != observed {
            return Err(ExtensionError::ImplementationDrift {
                extension: extension.clone(),
                expected: descriptor.implementation_digest().clone(),
                observed: observed.clone(),
            });
        }
        Ok(())
    }

    pub fn reject_untrusted_in_process(
        &self,
        descriptor: &ExtensionDescriptor,
    ) -> Result<(), ExtensionError> {
        descriptor.validate()?;
        Err(ExtensionError::OutOfProcessRequired {
            extension: descriptor.reference(),
            protocols: OUT_OF_PROCESS_PROTOCOLS.to_vec(),
        })
    }

    pub(crate) fn register_in_process(
        &mut self,
        _trusted: &TrustedExtensionToken,
        descriptor: ExtensionDescriptor,
    ) -> Result<(), ExtensionError> {
        insert_descriptor(&mut self.entries, descriptor)
    }
}

pub fn validate_contracts(
    descriptors: impl IntoIterator<Item = ExtensionDescriptor>,
) -> Result<(), ExtensionError> {
    let mut entries = BTreeMap::new();
    for descriptor in descriptors {
        insert_descriptor(&mut entries, descriptor)?;
    }
    Ok(())
}

fn insert_descriptor(
    entries: &mut BTreeMap<ExtensionReference, ExtensionDescriptor>,
    descriptor: ExtensionDescriptor,
) -> Result<(), ExtensionError> {
    descriptor.validate()?;
    let reference = descriptor.reference();
    if let Some(existing) = entries.get(&reference) {
        if existing.schema_digest() != descriptor.schema_digest() {
            return Err(ExtensionError::SchemaDrift {
                extension: reference,
                expected: existing.schema_digest().clone(),
                observed: descriptor.schema_digest().clone(),
            });
        }
        if existing.extension_point() != descriptor.extension_point() {
            return Err(ExtensionError::ExtensionPointConflict {
                extension: reference,
                expected: existing.extension_point(),
                observed: descriptor.extension_point(),
            });
        }
        return Err(ExtensionError::DuplicateIdentityVersion(reference));
    }
    entries.insert(reference, descriptor);
    Ok(())
}

pub fn built_in_descriptors() -> [ExtensionDescriptor; 3] {
    [
        built_in(
            ExtensionPoint::ModelAdapter,
            BUILT_IN_MODEL_ADAPTER,
            "0.1.0",
            b"kit.extension.model-adapter.schema.v1",
            include_bytes!("../adapters/model.rs"),
        ),
        built_in(
            ExtensionPoint::PromptModule,
            BUILT_IN_PROMPT_MODULES,
            "3.03.2",
            b"kit.extension.prompt-module.schema.v1",
            include_bytes!("../prompt/mod.rs"),
        ),
        built_in(
            ExtensionPoint::CostTable,
            BUILT_IN_COST_TABLE,
            "1.0.0",
            b"kit.extension.cost-table.schema.v1",
            include_bytes!("../accounting/cost.rs"),
        ),
    ]
}

fn built_in(
    point: ExtensionPoint,
    identity: &str,
    version: &str,
    schema: &[u8],
    implementation: &[u8],
) -> ExtensionDescriptor {
    ExtensionDescriptor::new(
        point,
        ExtensionIdentity::parse(identity).expect("static built-in identity is valid"),
        ExtensionVersion::parse(version).expect("static built-in version is valid"),
        CompatibilityRange::new(ContractVersion::new(1, 0, 0), ContractVersion::new(2, 0, 0)),
        ContentDigest::sha256(schema),
        ContentDigest::sha256(implementation),
    )
    .expect("static built-in descriptor is compatible")
}
