use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{
    ExtensionError, ExtensionIdentity, ExtensionPoint, ExtensionReference, ExtensionRegistry,
    ExtensionVersion,
    registry::{BUILT_IN_COST_TABLE, BUILT_IN_MODEL_ADAPTER, BUILT_IN_PROMPT_MODULES},
};

pub const EXTENSION_CONFIG_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    BuiltIn,
    User,
    Project,
    Run,
    Experiment,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtensionConfigLayer {
    pub schema_version: u16,
    #[serde(default)]
    pub selections: BTreeMap<ExtensionPoint, ExtensionReference>,
    #[serde(default, flatten)]
    additional_fields: BTreeMap<String, Value>,
}

impl ExtensionConfigLayer {
    pub fn empty() -> Self {
        Self {
            schema_version: EXTENSION_CONFIG_SCHEMA_VERSION,
            selections: BTreeMap::new(),
            additional_fields: BTreeMap::new(),
        }
    }

    pub fn select(&mut self, point: ExtensionPoint, extension: ExtensionReference) {
        self.selections.insert(point, extension);
    }

    pub fn additional_fields(&self) -> &BTreeMap<String, Value> {
        &self.additional_fields
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtensionConfigStack {
    pub built_in: ExtensionConfigLayer,
    #[serde(default)]
    pub user: Option<ExtensionConfigLayer>,
    #[serde(default)]
    pub project: Option<ExtensionConfigLayer>,
    #[serde(default)]
    pub run: Option<ExtensionConfigLayer>,
    #[serde(default)]
    pub experiment: Option<ExtensionConfigLayer>,
    #[serde(default, flatten)]
    additional_fields: BTreeMap<String, Value>,
}

impl ExtensionConfigStack {
    pub fn built_ins() -> Self {
        let mut built_in = ExtensionConfigLayer::empty();
        for (point, identity, version) in [
            (
                ExtensionPoint::ModelAdapter,
                BUILT_IN_MODEL_ADAPTER,
                "0.1.0",
            ),
            (
                ExtensionPoint::PromptModule,
                BUILT_IN_PROMPT_MODULES,
                "3.03.2",
            ),
            (ExtensionPoint::CostTable, BUILT_IN_COST_TABLE, "1.0.0"),
        ] {
            built_in.select(
                point,
                ExtensionReference::new(
                    ExtensionIdentity::parse(identity).expect("static built-in identity is valid"),
                    ExtensionVersion::parse(version).expect("static built-in version is valid"),
                ),
            );
        }
        Self {
            built_in,
            user: None,
            project: None,
            run: None,
            experiment: None,
            additional_fields: BTreeMap::new(),
        }
    }

    pub fn additional_fields(&self) -> &BTreeMap<String, Value> {
        &self.additional_fields
    }

    pub fn materialize(
        &self,
        registry: &ExtensionRegistry,
    ) -> Result<EffectiveExtensionConfig, ExtensionError> {
        let layers = [
            (ConfigSource::BuiltIn, Some(&self.built_in)),
            (ConfigSource::User, self.user.as_ref()),
            (ConfigSource::Project, self.project.as_ref()),
            (ConfigSource::Run, self.run.as_ref()),
            (ConfigSource::Experiment, self.experiment.as_ref()),
        ];
        let mut selections = BTreeMap::new();
        let mut provenance = BTreeMap::new();

        for (source, layer) in layers {
            let Some(layer) = layer else { continue };
            if layer.schema_version != EXTENSION_CONFIG_SCHEMA_VERSION {
                return Err(ExtensionError::UnsupportedConfigSchemaVersion {
                    source,
                    found: layer.schema_version,
                });
            }
            for (point, extension) in &layer.selections {
                let descriptor = registry
                    .get(extension)
                    .ok_or_else(|| ExtensionError::UnknownSelection(extension.clone()))?;
                if descriptor.extension_point() != *point {
                    return Err(ExtensionError::ExtensionPointConflict {
                        extension: extension.clone(),
                        expected: descriptor.extension_point(),
                        observed: *point,
                    });
                }
                selections.insert(*point, extension.clone());
                provenance.insert(*point, source);
            }
        }
        for point in ExtensionPoint::ALL {
            if !self.built_in.selections.contains_key(&point) {
                return Err(ExtensionError::MissingBuiltInSelection(point));
            }
        }

        Ok(EffectiveExtensionConfig::new(selections, provenance))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveExtensionConfig {
    selections: BTreeMap<ExtensionPoint, ExtensionReference>,
    provenance: BTreeMap<ExtensionPoint, ConfigSource>,
    digest: [u8; 32],
}

impl EffectiveExtensionConfig {
    fn new(
        selections: BTreeMap<ExtensionPoint, ExtensionReference>,
        provenance: BTreeMap<ExtensionPoint, ConfigSource>,
    ) -> Self {
        let bytes = canonical_bytes(&selections, &provenance);
        Self {
            selections,
            provenance,
            digest: Sha256::digest(bytes).into(),
        }
    }

    pub fn selection(&self, point: ExtensionPoint) -> &ExtensionReference {
        &self.selections[&point]
    }

    pub fn source(&self, point: ExtensionPoint) -> ConfigSource {
        self.provenance[&point]
    }

    pub fn selections(&self) -> &BTreeMap<ExtensionPoint, ExtensionReference> {
        &self.selections
    }

    pub fn provenance(&self) -> &BTreeMap<ExtensionPoint, ConfigSource> {
        &self.provenance
    }

    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn digest_hex(&self) -> String {
        use std::fmt::Write as _;

        let mut value = String::from("sha256:");
        for byte in self.digest {
            write!(value, "{byte:02x}").expect("writing to a string cannot fail");
        }
        value
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_bytes(&self.selections, &self.provenance)
    }
}

#[derive(Serialize)]
struct CanonicalConfig<'a> {
    schema_version: u16,
    selections: &'a BTreeMap<ExtensionPoint, ExtensionReference>,
    provenance: &'a BTreeMap<ExtensionPoint, ConfigSource>,
}

fn canonical_bytes(
    selections: &BTreeMap<ExtensionPoint, ExtensionReference>,
    provenance: &BTreeMap<ExtensionPoint, ConfigSource>,
) -> Vec<u8> {
    serde_json::to_vec(&CanonicalConfig {
        schema_version: EXTENSION_CONFIG_SCHEMA_VERSION,
        selections,
        provenance,
    })
    .expect("extension configuration contains only infallible JSON values")
}
