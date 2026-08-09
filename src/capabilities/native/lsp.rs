//! Optional staged-view LSP diagnostics for kit_edit (RFC 18.3).
//!
//! When `.kit/native.json` declares an `lsp` object, the edit pipeline runs one
//! bounded shadow LSP session against the staged view after syntax passes and
//! before materialization. Diagnostics never block the edit: the result either
//! carries a bounded `diagnostics` array for the changed files or a
//! `diagnostics_unavailable` reason (server missing, crash, timeout, fallback).

use std::{path::PathBuf, time::Duration};

use serde_json::{Value, json};
use url::Url;

use crate::{
    domain::{
        events::ContentDigest,
        ids::{PrincipalId, ProjectId, WorkspaceId},
    },
    executor::profile::{
        Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
    },
    verify::lsp::{
        facts::{FactLimits, LiveDiagnostic},
        launcher::{NativeLspServerConfig, StdioLspLauncher},
        session::{
            ExecutionProfileIdentity, PositionEncoding, ServerIdentity, SessionLimits,
        },
        shadow::{
            ShadowAdapterCapabilities, ShadowAdapterRegistry, ShadowAdapterRequest,
            ShadowDiagnosticScope, ShadowLimits, ShadowLspRunner, ShadowOutcome,
        },
    },
    workspace::edit::{ir::EditLimits, stage::StagedEdit},
};

/// Highest LSP severity forwarded into the edit result: 1 = error, 2 = warning.
const MAX_FORWARDED_SEVERITY: u8 = 2;
const NATIVE_LSP_SERVER_VERSION: &str = "kit-native-lsp-config-v1";
/// Byte cap for one diagnostic message inside the kit_edit result.
const MAX_RESULT_MESSAGE_BYTES: usize = 2_048;
/// Total serialized budget for the `diagnostics` array: the kit_edit output is
/// hard-capped at 64 KiB and a committed edit must never fail on result size.
const MAX_RESULT_DIAGNOSTIC_BYTES: usize = 16 * 1024;

/// Outcome of the optional staged-diagnostics pass. Never an error: LSP
/// unavailability must not fail the edit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeEditLspOutcome {
    /// Feature off or no changed file matches the configured languages.
    Skipped,
    /// Bounded error/warning diagnostics for the changed files.
    Diagnostics(Vec<Value>),
    /// The configured server could not produce staged diagnostics.
    Unavailable(String),
}

/// Per-dispatcher context for running the staged shadow diagnostics pass.
pub(crate) struct NativeEditLspGate {
    config: NativeLspServerConfig,
    root: PathBuf,
    principal_id: PrincipalId,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
}

impl NativeEditLspGate {
    /// `root` must be the dispatcher's canonicalized workspace root: the shadow
    /// runner revalidates it and the server child runs with it as its working
    /// directory. Staged buffers are delivered over `didOpen`, never on disk.
    pub(crate) fn new(
        config: NativeLspServerConfig,
        root: PathBuf,
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
    ) -> Self {
        Self {
            config,
            root,
            principal_id,
            project_id,
            workspace_id,
        }
    }

    pub(crate) fn run(&self, staged: &StagedEdit<'_>) -> NativeEditLspOutcome {
        let matched = staged.changes().iter().any(|change| {
            let path = change.path().as_str();
            self.config.matches_path(path, syntax_language_key(path))
        });
        if !matched {
            return NativeEditLspOutcome::Skipped;
        }
        match self.run_shadow(staged) {
            Ok(diagnostics) => NativeEditLspOutcome::Diagnostics(diagnostics),
            Err(reason) => NativeEditLspOutcome::Unavailable(reason),
        }
    }

    fn run_shadow(&self, staged: &StagedEdit<'_>) -> Result<Vec<Value>, String> {
        let root_url = Url::from_directory_path(&self.root)
            .map_err(|()| "invalid_workspace_root".to_owned())?;
        let session_limits = SessionLimits::default();
        let launcher = StdioLspLauncher::new(&self.config, self.root.clone(), session_limits.codec);
        let mut runner = ShadowLspRunner::new(
            launcher,
            session_limits,
            EditLimits::default(),
            FactLimits::default(),
            ShadowLimits::default(),
        )
        .map_err(|error| format!("shadow_runner_invalid:{error:?}"))?;
        let deadline = runner
            .deadline_after(self.config.wall_time())
            .map_err(|error| format!("shadow_deadline:{error:?}"))?;
        let request = self.adapter_request()?;
        let registry = ShadowAdapterRegistry::from_trusted_request(&request)
            .map_err(|error| format!("shadow_registry:{error:?}"))?;
        let decision = registry.resolve(request);
        match runner.run_staged(
            staged,
            &self.root,
            &root_url,
            ShadowDiagnosticScope::Document,
            decision,
            deadline,
        ) {
            Ok(ShadowOutcome::Completed(report)) => Ok(serialize_diagnostics(
                report.diagnostics(),
                self.config.max_diagnostics(),
            )),
            Ok(ShadowOutcome::Fallback(record)) => {
                Err(format!("shadow_fallback:{:?}", record.reason()))
            }
            Err(error) => Err(format!("shadow_error:{error:?}")),
        }
    }

    fn adapter_request(&self) -> Result<ShadowAdapterRequest, String> {
        let server = ServerIdentity {
            server_artifact: labeled_digest(
                "kit-native-lsp-command-v1",
                &[self.config.command().as_bytes()],
            ),
            configuration: labeled_digest(
                "kit-native-lsp-arguments-v1",
                &self
                    .config
                    .arguments()
                    .iter()
                    .map(String::as_bytes)
                    .collect::<Vec<_>>(),
            ),
        };
        let isolation_identity = labeled_digest(
            "kit-native-lsp-root-v1",
            &[self.root.as_os_str().as_encoded_bytes()],
        );
        let profile = execution_profile(self.config.wall_time())
            .ok_or_else(|| "shadow_profile_invalid".to_owned())?;
        ShadowAdapterRequest::new(
            self.principal_id,
            self.project_id,
            self.workspace_id,
            server,
            NATIVE_LSP_SERVER_VERSION,
            PositionEncoding::Utf16,
            ShadowAdapterCapabilities::new(true, true, true),
            isolation_identity,
            profile,
        )
        .map_err(|error| format!("shadow_request_invalid:{error:?}"))
    }
}

fn serialize_diagnostics(diagnostics: &[LiveDiagnostic], max_diagnostics: u64) -> Vec<Value> {
    let bound = usize::try_from(max_diagnostics).unwrap_or(usize::MAX);
    let mut serialized = Vec::new();
    let mut retained_bytes = 0_usize;
    for diagnostic in diagnostics {
        if serialized.len() >= bound {
            break;
        }
        if diagnostic
            .severity()
            .is_some_and(|severity| severity > MAX_FORWARDED_SEVERITY)
        {
            continue;
        }
        let value = json!({
            "path": diagnostic.path().as_path().as_str(),
            "range": {
                "start": diagnostic.range().start(),
                "end": diagnostic.range().end(),
            },
            "severity": diagnostic.severity(),
            "code": diagnostic.code().map(|code| match code {
                crate::verify::lsp::facts::DiagnosticCode::Integer(value) => json!(value),
                crate::verify::lsp::facts::DiagnosticCode::String(value) => json!(value),
            }),
            "source": diagnostic.source(),
            "message": bounded_message(diagnostic.message()),
        });
        let value_bytes = value.to_string().len();
        if retained_bytes.saturating_add(value_bytes) > MAX_RESULT_DIAGNOSTIC_BYTES {
            break;
        }
        retained_bytes += value_bytes;
        serialized.push(value);
    }
    serialized
}

fn bounded_message(message: &str) -> &str {
    if message.len() <= MAX_RESULT_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_RESULT_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

/// The syntax-pipeline language key for a root-relative path, mirroring the
/// staging derivation table so `.kit/native.json` `lsp.languages` accepts the
/// same names as `SyntaxRequirement` (plus bare extensions).
fn syntax_language_key(path: &str) -> Option<&'static str> {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())?;
    Some(match extension {
        "rs" => "rust",
        "json" => "json",
        "md" | "txt" => "text",
        "sh" => "shell",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "cs" => "c-sharp",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        _ => return None,
    })
}

fn labeled_digest(label: &str, fields: &[&[u8]]) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, label.as_bytes());
    for field in fields {
        hash_field(&mut hasher, field);
    }
    ContentDigest::parse(&format!("blake3:{}", hasher.finalize().to_hex()))
        .expect("BLAKE3 produces a valid content digest")
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(
        &u64::try_from(bytes.len())
            .expect("field length fits the canonical u64 length prefix")
            .to_le_bytes(),
    );
    hasher.update(bytes);
}

fn execution_profile(wall_time: Duration) -> Option<ExecutionProfileIdentity> {
    let platform = if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Linux
    };
    let architecture = if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    };
    let wall_time_millis = u64::try_from(wall_time.as_millis()).ok()?.max(1);
    let resources = ResourceLimits::new(
        wall_time_millis,
        4 * 1024 * 1024 * 1024,
        64,
        1024 * 1024 * 1024,
        1024 * 1024 * 1024,
        1024 * 1024 * 1024,
        64 * 1024 * 1024,
        wall_time_millis,
    );
    ExecutorProfile::new(ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        platform,
        architecture,
        resources,
    ))
    .ok()
    .map(|profile| ExecutionProfileIdentity::from_profile(&profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_keys_mirror_the_staging_derivation_table() {
        assert_eq!(syntax_language_key("src/lib.rs"), Some("rust"));
        assert_eq!(syntax_language_key("notes.md"), Some("text"));
        assert_eq!(syntax_language_key("data.json"), Some("json"));
        assert_eq!(syntax_language_key("Makefile"), None);
        assert_eq!(syntax_language_key("mod.unknown"), None);
    }

    #[test]
    fn execution_profile_is_finite() {
        let profile = execution_profile(Duration::from_secs(5)).unwrap();
        assert!(profile.resources().finite());
    }

    #[test]
    fn result_messages_are_truncated_on_char_boundaries() {
        let short = "kit";
        assert_eq!(bounded_message(short), "kit");
        let long = "é".repeat(MAX_RESULT_MESSAGE_BYTES);
        let bounded = bounded_message(&long);
        assert!(bounded.len() <= MAX_RESULT_MESSAGE_BYTES);
        assert!(long.starts_with(bounded));
    }
}
