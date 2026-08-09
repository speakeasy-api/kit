use std::fmt;

use crate::workspace::edit::ir::RootRelativePath;

pub const SYNTAX_EXECUTOR_CONTRACT_VERSION: u16 = 1;
pub const NATIVE_TEXT_VERSION: &str = "kit-native-text-v1";
pub const NATIVE_JSON_VERSION: &str = "kit-native-json-v1";
pub const RUST_GRAMMAR_VERSION: &str = "kit-syn-rust-v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxRequirement {
    path: RootRelativePath,
    language: String,
    version: String,
    required: bool,
}

impl SyntaxRequirement {
    pub fn new(
        path: RootRelativePath,
        language: impl Into<String>,
        version: impl Into<String>,
        required: bool,
    ) -> Result<Self, AdapterContractError> {
        let language = language.into();
        let version = version.into();
        if !valid_identifier(&language) {
            return Err(AdapterContractError::InvalidIdentifier("syntax language"));
        }
        if !valid_identifier(&version) {
            return Err(AdapterContractError::InvalidIdentifier("syntax version"));
        }
        Ok(Self {
            path,
            language,
            version,
            required,
        })
    }

    pub fn path(&self) -> &RootRelativePath {
        &self.path
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub const fn required(&self) -> bool {
        self.required
    }
}

pub struct SyntaxRequest<'a> {
    path: &'a RootRelativePath,
    source: &'a [u8],
}

impl<'a> SyntaxRequest<'a> {
    pub(crate) const fn new(path: &'a RootRelativePath, source: &'a [u8]) -> Self {
        Self { path, source }
    }

    pub const fn path(&self) -> &RootRelativePath {
        self.path
    }

    pub const fn source(&self) -> &[u8] {
        self.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxStatus {
    Pass,
    Fail,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterContractError {
    InvalidIdentifier(&'static str),
    InvalidPath,
}

impl fmt::Display for AdapterContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(field) => write!(formatter, "invalid {field}"),
            Self::InvalidPath => formatter.write_str("invalid adapter path"),
        }
    }
}

impl std::error::Error for AdapterContractError {}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}
