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
pub struct FormatterDescriptor {
    id: String,
    version: String,
    files: Vec<RootRelativePath>,
    command: Option<FormatterCommandDescriptor>,
}

impl FormatterDescriptor {
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        mut files: Vec<RootRelativePath>,
    ) -> Result<Self, AdapterContractError> {
        let id = id.into();
        let version = version.into();
        if !valid_identifier(&id) {
            return Err(AdapterContractError::InvalidIdentifier("formatter id"));
        }
        if !valid_identifier(&version) {
            return Err(AdapterContractError::InvalidIdentifier("formatter version"));
        }
        files.sort();
        if files.is_empty() || files.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AdapterContractError::InvalidFileSet);
        }
        Ok(Self {
            id,
            version,
            files,
            command: None,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn files(&self) -> &[RootRelativePath] {
        &self.files
    }

    pub fn with_command(mut self, command: FormatterCommandDescriptor) -> Self {
        self.command = Some(command);
        self
    }

    pub fn command(&self) -> Option<&FormatterCommandDescriptor> {
        self.command.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatterCommandDescriptor {
    image: String,
    program: String,
    arguments: Vec<String>,
    requested_binary_digest: String,
    requested_config_digest: String,
}

impl FormatterCommandDescriptor {
    pub fn new(
        image: impl Into<String>,
        program: impl Into<String>,
        arguments: Vec<String>,
        requested_binary_digest: impl Into<String>,
        requested_config_digest: impl Into<String>,
    ) -> Result<Self, AdapterContractError> {
        let value = Self {
            image: image.into(),
            program: program.into(),
            arguments,
            requested_binary_digest: requested_binary_digest.into(),
            requested_config_digest: requested_config_digest.into(),
        };
        if !valid_sha256_reference(&value.image)
            || !valid_virtual_program(&value.program)
            || value.arguments.len() > 256
            || value
                .arguments
                .iter()
                .any(|argument| argument.len() > 4096 || argument.contains(['\0', '\n', '\r']))
            || !valid_digest(&value.requested_binary_digest)
            || !valid_digest(&value.requested_config_digest)
        {
            return Err(AdapterContractError::InvalidCommand);
        }
        Ok(value)
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn requested_binary_digest(&self) -> &str {
        &self.requested_binary_digest
    }

    pub fn requested_config_digest(&self) -> &str {
        &self.requested_config_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterContractError {
    InvalidIdentifier(&'static str),
    InvalidFileSet,
    InvalidPath,
    InvalidCommand,
}

impl fmt::Display for AdapterContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(field) => write!(formatter, "invalid {field}"),
            Self::InvalidFileSet => formatter.write_str("invalid formatter file set"),
            Self::InvalidPath => formatter.write_str("invalid adapter path"),
            Self::InvalidCommand => formatter.write_str("invalid formatter command descriptor"),
        }
    }
}

impl std::error::Error for AdapterContractError {}

pub(crate) fn safe_text(source: &[u8]) -> bool {
    std::str::from_utf8(source).is_ok() && !source.contains(&0) && coherent_newlines(source)
}

fn coherent_newlines(bytes: &[u8]) -> bool {
    let mut lf = false;
    let mut crlf = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf = true;
                index += 2;
            }
            b'\r' => return false,
            b'\n' => {
                lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    !(lf && crlf)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

fn valid_digest(value: &str) -> bool {
    value.split_once(':').is_some_and(|(algorithm, hex)| {
        matches!(algorithm, "blake3" | "sha256")
            && hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_sha256_reference(value: &str) -> bool {
    value.rsplit_once("@sha256:").is_some_and(|(name, hex)| {
        !name.is_empty()
            && hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_virtual_program(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 4096
        && !value.contains(['\0', '\n', '\r', '\\'])
        && value
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && !matches!(part, "." | ".."))
}
