use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agent::adapters::grammar_edit::{
        EditOrchestrator, EditPathTrace, GrammarEditContext, NativeEditOutcome, NativeEditServices,
    },
    api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
    capabilities::kernel::invoke::{AuthorizedInvocation, CanonicalOutput, DispatchOutcome},
    domain::config::{Executor as ConfigExecutor, Grant, RunConfigSnapshot},
    executor::{
        backends::local_os::{LocalCommand, LocalOsBackend, SandboxPaths},
        cancel::{SqliteCancellationCoordinator, WorkspaceIdentity},
        check::CheckRunner,
        process::own::ProcessRegistryRegistration,
        profile::{
            Architecture, CompatibilityOptIn, ExecutorProfile, Platform, ProfileSpec,
            ResourceLimits, TrustTier,
        },
    },
    store::artifacts::{ArtifactRetention, ArtifactStore},
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
    verify::profiles::{ProfileSelection, VerificationRegistry},
    workspace::{
        acquire::AcquisitionResult,
        index::meta::{IndexOptions, MetadataIndex},
        read::{ArtifactContext, ReadOptions, ReadRange, ReadRequest, read},
        revision::{ManagedWorkspace, RevisionId, RevisionOptions},
        search::{
            discover::{DiscoverCursor, DiscoverOptions, DiscoverQuery, discover},
            lexical::{SearchCursor, SearchMode, SearchOptions, SearchQuery, search},
        },
    },
};

pub(crate) struct NativeFormatterRuntime {
    pub descriptor: crate::workspace::edit::format::FormatterDescriptor,
    pub executor: crate::executor::formatter::FormatterExecutor,
}

#[derive(Clone)]
pub(crate) struct NativeFeedbackRuntime {
    pub database: PathBuf,
    pub adapters: BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
    pub limits: crate::verify::feedback::FeedbackLimits,
}

use super::{MAX_NATIVE_OUTPUT_BYTES, NativeCatalog, NativeTool};

const MAX_RUN_CPU_MILLIS: u64 = 60_000;
const MAX_RUN_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RUN_PIDS: u32 = 512;
const MAX_RUN_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RUN_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_RUN_IO_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_RUN_OUTPUT_BYTES: u64 = 1024 * 1024;
const MAX_RUN_WALL_TIME_MILLIS: u64 = 10 * 60 * 1000;
const MAX_NATIVE_WORKSPACE_SCAN_TIME: std::time::Duration = std::time::Duration::from_secs(20);
pub(crate) const MAX_EDIT_VALIDATION_TIME: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

pub(crate) struct NativeRuntime {
    pub workspace_id: crate::domain::ids::WorkspaceId,
    pub process_registration: Option<ProcessRegistryRegistration>,
    pub cancellation: SqliteCancellationCoordinator,
    pub live_cancellation: Arc<AtomicBool>,
    pub container_image: Option<String>,
    pub verification_registry: VerificationRegistry,
    pub check_runner: Option<CheckRunner>,
    pub secrets: Vec<crate::domain::secret::SecretLease>,
    pub syntax_executors: Vec<crate::executor::syntax::SyntaxExecutor>,
    pub formatter_required: bool,
    pub formatter: Option<NativeFormatterRuntime>,
    pub feedback: Option<NativeFeedbackRuntime>,
    pub edit_validation_time: std::time::Duration,
    #[cfg(test)]
    pub run_runner: Option<CheckRunner>,
}

pub(crate) struct NativeDispatcher {
    root: PathBuf,
    workspace: Option<ManagedWorkspace>,
    index: Option<MetadataIndex>,
    build: PathBuf,
    temp: PathBuf,
    artifacts: Arc<ArtifactStore>,
    authenticated: AuthenticatedPrincipal,
    grants: GrantSnapshot,
    config: RunConfigSnapshot,
    acquisition: Option<AcquisitionResult>,
    workspace_id: crate::domain::ids::WorkspaceId,
    process_registration: Option<ProcessRegistryRegistration>,
    cancellation: SqliteCancellationCoordinator,
    live_cancellation: Arc<AtomicBool>,
    container_image: Option<String>,
    verification_registry: VerificationRegistry,
    check_runner: Option<CheckRunner>,
    secrets: Vec<crate::domain::secret::SecretLease>,
    syntax_executors: Vec<crate::executor::syntax::SyntaxExecutor>,
    formatter_required: bool,
    formatter: Option<NativeFormatterRuntime>,
    feedback: Option<NativeFeedbackRuntime>,
    edit_validation_time: std::time::Duration,
    #[cfg(test)]
    run_runner: Option<CheckRunner>,
}

impl NativeDispatcher {
    pub(crate) fn open(
        root: PathBuf,
        scratch: &Path,
        artifacts: Arc<ArtifactStore>,
        authenticated: AuthenticatedPrincipal,
        config: RunConfigSnapshot,
        acquisition: Option<AcquisitionResult>,
        runtime: NativeRuntime,
    ) -> Result<Self, String> {
        if runtime.edit_validation_time.is_zero()
            || runtime.edit_validation_time > MAX_EDIT_VALIDATION_TIME
        {
            return Err("native edit validation policy is outside the trusted bound".to_owned());
        }
        let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
        if !root.is_dir() {
            return Err("trusted project root is not a directory".to_owned());
        }
        let build = scratch.join("build");
        let temp = scratch.join("tmp");
        std::fs::create_dir_all(&build).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&temp).map_err(|error| error.to_string())?;
        let grants = authenticated.grant_snapshot().clone();
        Ok(Self {
            root,
            workspace: None,
            index: None,
            build,
            temp,
            artifacts,
            authenticated,
            grants,
            config,
            acquisition,
            workspace_id: runtime.workspace_id,
            process_registration: runtime.process_registration,
            cancellation: runtime.cancellation,
            live_cancellation: runtime.live_cancellation,
            container_image: runtime.container_image,
            verification_registry: runtime.verification_registry,
            check_runner: runtime.check_runner,
            secrets: runtime.secrets,
            syntax_executors: runtime.syntax_executors,
            formatter_required: runtime.formatter_required,
            formatter: runtime.formatter,
            feedback: runtime.feedback,
            edit_validation_time: runtime.edit_validation_time,
            #[cfg(test)]
            run_runner: runtime.run_runner,
        })
    }

    pub(crate) fn revision(&mut self) -> Result<String, String> {
        self.revision_state().map(|(revision, _)| revision)
    }

    pub(crate) fn revision_state(&mut self) -> Result<(String, String), String> {
        let workspace = self.ensure_workspace()?;
        workspace.mark_dirty();
        workspace
            .current_revision()
            .map(|revision| (revision.id().to_string(), revision.digest().to_string()))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn bind_authority(
        &mut self,
        authenticated: AuthenticatedPrincipal,
        config: RunConfigSnapshot,
        attempt: crate::domain::lifecycle::AttemptOwnership,
        cancellation: Arc<AtomicBool>,
    ) {
        self.grants = authenticated.grant_snapshot().clone();
        self.authenticated = authenticated;
        self.config = config;
        self.live_cancellation = cancellation;
        if let Some(runner) = &mut self.check_runner {
            runner.bind_attempt(attempt);
        }
        if let Some(formatter) = &mut self.formatter {
            formatter.executor.bind_attempt(attempt);
        }
    }

    pub(crate) fn dispatch(&mut self, invocation: &AuthorizedInvocation) -> DispatchOutcome {
        if self.cancelled() {
            return failed("cancelled");
        }
        let Some(descriptor) = NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.identity() == invocation.capability())
        else {
            return failed("native_tool_binding_unknown");
        };
        if descriptor.schema().normalized_digest() != invocation.schema_digest()
            || descriptor.effect() != invocation.effect()
        {
            return failed("native_tool_binding_mismatch");
        }
        if descriptor.tool() == NativeTool::Edit {
            return match self.edit(invocation.arguments(), invocation.attempt()) {
                Ok((data, artifacts, true)) => committed_output(data, artifacts),
                Ok((data, artifacts, false)) => output(data, artifacts),
                Err(code) => failed(&code),
            };
        }
        let result = match descriptor.tool() {
            NativeTool::Discover => self.discover(invocation.arguments()),
            NativeTool::Search => self.search(invocation.arguments()),
            NativeTool::Read => self.read(invocation.arguments()),
            NativeTool::Edit => unreachable!(),
            NativeTool::Run => self.run(invocation.arguments(), invocation.attempt()),
            NativeTool::Check => self.check(invocation.arguments(), invocation.attempt()),
        };
        match result {
            Ok((_data, _artifacts)) if self.cancelled() => failed("cancelled_after_dispatch"),
            Ok((data, artifacts)) => output(data, artifacts),
            Err(code) => failed(&code),
        }
    }

    fn cancelled(&self) -> bool {
        self.live_cancellation.load(Ordering::Acquire)
    }

    fn ensure_workspace(&mut self) -> Result<&ManagedWorkspace, String> {
        if self.workspace.is_none() {
            let defaults = RevisionOptions::default();
            let options = RevisionOptions {
                max_scan_time: defaults.max_scan_time.max(
                    self.edit_validation_time
                        .min(MAX_NATIVE_WORKSPACE_SCAN_TIME),
                ),
                ..defaults
            };
            self.workspace = Some(
                ManagedWorkspace::open_with_options(&self.root, options)
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(self.workspace.as_ref().expect("workspace was initialized"))
    }

    fn workspace_index(
        &mut self,
        expected: &str,
    ) -> Result<(ManagedWorkspace, MetadataIndex), String> {
        let expected = revision(expected)?;
        let workspace = self.ensure_workspace()?.clone();
        workspace.mark_dirty();
        let current = workspace
            .current_revision()
            .map_err(code("workspace_revision_failed"))?;
        if current.id() != expected {
            return Err("stale_revision".to_owned());
        }
        if let Some(index) = &self.index
            && index.revision() == expected
        {
            return Ok((workspace, index.clone()));
        }
        let index_options = IndexOptions {
            max_build_time: std::time::Duration::from_secs(60),
            ..IndexOptions::default()
        };
        let index = MetadataIndex::build(&workspace, expected, &index_options)
            .map_err(code("workspace_index_failed"))?;
        self.index = Some(index.clone());
        Ok((workspace, index))
    }

    fn discover(&mut self, bytes: &[u8]) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: DiscoverInput = decode(bytes)?;
        let (workspace, index) = self.workspace_index(&input.expected_revision)?;
        let response = discover(
            &workspace,
            &index,
            &DiscoverQuery {
                terms: input.terms,
                roots: input.roots.into_iter().map(PathBuf::from).collect(),
                languages: input.languages,
            },
            &bounded_discover_options(),
            input.cursor.as_ref(),
        )
        .map_err(code("discover_failed"))?;
        serde_json::to_value(response)
            .map(|value| (value, Vec::new()))
            .map_err(code("serialization_failed"))
    }

    fn search(&mut self, bytes: &[u8]) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: SearchInput = decode(bytes)?;
        let (workspace, index) = self.workspace_index(&input.expected_revision)?;
        let response = search(
            &workspace,
            &index,
            &SearchQuery {
                text: input.text,
                mode: input.mode.into(),
            },
            &SearchOptions {
                path_prefixes: input.path_prefixes.into_iter().map(PathBuf::from).collect(),
                languages: input.languages,
                max_result_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
                max_time: std::time::Duration::from_secs(30),
                ..SearchOptions::default()
            },
            input.cursor.as_ref(),
        )
        .map_err(code("search_failed"))?;
        serde_json::to_value(response)
            .map(|value| (value, Vec::new()))
            .map_err(code("serialization_failed"))
    }

    fn read(&mut self, bytes: &[u8]) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: ReadInput = decode(bytes)?;
        let (workspace, index) = self.workspace_index(&input.expected_revision)?;
        let response = read(
            &workspace,
            &index,
            &self.artifacts,
            &ArtifactContext {
                principal: self.authenticated.principal_id().to_string(),
                project: self.config.project_id().to_string(),
                retention: ArtifactRetention::Forever,
            },
            &ReadRequest {
                expected_revision: revision(&input.expected_revision)?,
                path: PathBuf::from(input.path),
                range: input.range.into(),
            },
            &ReadOptions {
                max_inline_bytes: 32 * 1024,
                max_result_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
                max_time: std::time::Duration::from_secs(30),
                ..ReadOptions::default()
            },
        )
        .map_err(code("read_failed"))?;
        let artifacts = response
            .artifact
            .as_ref()
            .map(|artifact| vec![artifact.id.clone()])
            .unwrap_or_default();
        serde_json::to_value(response)
            .map(|value| (value, artifacts))
            .map_err(code("serialization_failed"))
    }

    fn edit(
        &mut self,
        bytes: &[u8],
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>, bool), String> {
        self.ensure_not_cancelled()?;
        if self.verification_registry.is_empty() {
            return Err("trusted_edit_registry_unavailable".to_owned());
        }
        if self.syntax_executors.is_empty() {
            return Err("trusted_edit_syntax_unavailable".to_owned());
        }
        if self.formatter_required && self.formatter.is_none() {
            return Err("trusted_edit_formatter_unavailable".to_owned());
        }
        if self
            .feedback
            .as_ref()
            .is_none_or(|feedback| feedback.adapters.is_empty())
        {
            return Err("trusted_edit_feedback_unavailable".to_owned());
        }
        let input: Value = serde_json::from_slice(bytes).map_err(code("invalid_arguments"))?;
        let expected = input
            .get("expected_revision")
            .and_then(Value::as_str)
            .ok_or_else(|| "invalid_arguments".to_owned())?;
        let workspace = self.ensure_workspace()?.clone();
        let limits = crate::workspace::edit::ir::EditLimits {
            max_authorization_time: std::time::Duration::from_secs(30),
            max_validation_time: self.edit_validation_time,
            ..crate::workspace::edit::ir::EditLimits::default()
        };
        let context = GrammarEditContext::from_workspace(workspace, self.root.clone(), limits)
            .map_err(code("edit_context_failed"))?;
        if context.expected_revision().as_str() != expected {
            return Err("stale_revision".to_owned());
        }
        let mut trace = EditPathTrace::default();
        let runner = self
            .check_runner
            .as_mut()
            .ok_or_else(|| "trusted_edit_runner_unavailable".to_owned())?;
        let mut syntax_executors = self.syntax_executors.iter_mut().collect::<Vec<_>>();
        let feedback = self
            .feedback
            .as_ref()
            .expect("trusted feedback was checked above");
        let formatter = self
            .formatter
            .as_mut()
            .map(|formatter| (&formatter.descriptor, &mut formatter.executor));
        let outcome = EditOrchestrator::execute_native(
            bytes,
            &context,
            &self.authenticated,
            &self.grants,
            &self.config,
            &self.artifacts,
            &self.live_cancellation,
            &self.verification_registry,
            runner,
            &self.secrets,
            &mut syntax_executors,
            NativeEditServices {
                workspace_id: self.workspace_id.to_string(),
                attempt,
                feedback_database: &feedback.database,
                build: &self.build,
                temp: &self.temp,
                diagnostic_adapters: &feedback.adapters,
                feedback_limits: feedback.limits.clone(),
                formatter,
            },
            &mut trace,
        )
        .map_err(native_edit_error)?;
        match outcome {
            NativeEditOutcome::Aborted { receipt, feedback } => {
                let artifacts = vec![
                    receipt.result_artifact.reference.clone(),
                    feedback.payload_artifact.reference.clone(),
                    feedback.report_artifact.reference.clone(),
                ];
                Ok((
                    json!({
                        "outcome": "aborted",
                        "feedback": feedback.payload,
                        "feedback_artifacts": {
                            "payload_artifact": feedback.payload_artifact.reference,
                            "report_artifact": feedback.report_artifact.reference,
                        },
                        "events": feedback.events,
                        "trace": trace.ids(),
                        "verification": receipt,
                    }),
                    artifacts,
                    false,
                ))
            }
            NativeEditOutcome::Committed { edit, feedback } => {
                self.index = None;
                let receipt = edit.verification_receipt();
                let diff_artifact = json!({
                    "reference": edit.diff_artifact_reference().to_string(),
                    "digest": edit.diff_artifact_digest().to_string(),
                    "media_type": "text/x-diff; charset=utf-8",
                    "class": "diff",
                    "provenance": {
                        "principal_id": self.grants.principal_id(),
                        "project_id": self.grants.project_id(),
                        "transaction_id": edit.transaction_id(),
                        "revision_id": edit.revision().id(),
                    },
                });
                let artifacts = vec![
                    edit.diff_artifact_reference().to_string(),
                    receipt.result_artifact.reference.clone(),
                    feedback.payload_artifact.reference.clone(),
                    feedback.report_artifact.reference.clone(),
                ];
                Ok((
                    json!({
                        "outcome": if edit.committed_with_cancel_race() {
                            "committed_with_cancel_race"
                        } else {
                            "committed"
                        },
                        "diff_artifact": diff_artifact,
                        "diff_preview": edit.diff_preview(),
                        "feedback": feedback.payload,
                        "feedback_artifacts": {
                            "payload_artifact": feedback.payload_artifact.reference,
                            "report_artifact": feedback.report_artifact.reference,
                        },
                        "events": feedback.events,
                        "revision": {
                            "digest": edit.revision().digest().to_string(),
                            "epoch": edit.revision().epoch().to_string(),
                            "id": edit.revision().id().to_string(),
                        },
                        "trace": trace.ids(),
                        "transaction_id": edit.transaction_id(),
                        "verification": receipt,
                    }),
                    artifacts,
                    true,
                ))
            }
        }
    }

    fn run(
        &mut self,
        bytes: &[u8],
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: RunInput = decode(bytes)?;
        if input.working_directory != "."
            || input.argv.is_empty()
            || input.mounts != RunMounts::required()
            || !trusted_run_limits(input.limits)
        {
            return Err("run_request_rejected".to_owned());
        }
        if input.network == NetworkPolicy::ProfileGrants
            && !self
                .config
                .effective_authority()
                .contains(&Grant::NetworkEgress)
        {
            return Err("network_grant_required".to_owned());
        }
        if !input.host_compatibility
            && self.config.effective().executor == ConfigExecutor::IsolatedVm
        {
            return Err("configured_vm_executor_unavailable".to_owned());
        }
        let spec = if input.host_compatibility {
            if !self
                .authenticated
                .grant_snapshot()
                .grants()
                .contains(&Grant::HostProcessCompatibility)
                || !self
                    .config
                    .effective_authority()
                    .contains(&Grant::HostProcessCompatibility)
            {
                return Err("host_compatibility_grant_required".to_owned());
            }
            ProfileSpec::host_compatibility(
                host_platform()?,
                host_architecture()?,
                input.limits,
                CompatibilityOptIn::trusted_local("native run policy grant")
                    .map_err(code("executor_profile_rejected"))?,
            )
        } else {
            ProfileSpec::isolated(
                if self.config.effective().executor == ConfigExecutor::RestrictedContainer {
                    TrustTier::Restricted
                } else {
                    TrustTier::TrustedLocal
                },
                host_platform()?,
                host_architecture()?,
                input.limits,
            )
        };
        let profile = ExecutorProfile::new(spec).map_err(code("executor_profile_rejected"))?;
        #[cfg(test)]
        if self.run_runner.is_some() {
            return self.run_conformance(input, attempt);
        }
        if self.config.effective().executor == ConfigExecutor::RestrictedContainer {
            let acquisition = self
                .acquisition
                .as_ref()
                .ok_or_else(|| "workspace_acquisition_unavailable".to_owned())?;
            let registration = self
                .process_registration
                .as_ref()
                .ok_or_else(|| "attempt_executor_unavailable".to_owned())?;
            let image = self
                .container_image
                .as_deref()
                .ok_or_else(|| "trusted_run_image_unavailable".to_owned())?;
            let argv = input.argv;
            let command_digest = digest(&serde_json::to_vec(&argv).expect("argv serializes"));
            let config_digest = format!("sha256:{}", hex(&self.config.digest()));
            let plan = crate::executor::backends::container::prepare_captured(
                &profile,
                acquisition,
                &self.build,
                &self.temp,
                "native-run",
                image,
                argv.clone(),
                &input.environment,
                crate::executor::backends::container::CheckExecutionRequest {
                    program: &argv[0],
                    arguments: &argv[1..],
                    binary_digest: &command_digest,
                    config_digest: &config_digest,
                },
            )
            .map_err(code("executor_isolation_unavailable"))?;
            let report = plan
                .run_registered(
                    crate::domain::lifecycle::ProcessOwnership::Attempt(attempt),
                    &self.cancellation,
                    WorkspaceIdentity::from_acquisition(self.workspace_id, acquisition),
                    registration.clone(),
                    false,
                )
                .map_err(code("attempt_executor_unavailable"))?;
            let child = report
                .child_output
                .ok_or_else(|| "executor_output_unavailable".to_owned())?;
            let stdout = self.persist_log(&child.stdout.bytes)?;
            let stderr = self.persist_log(&child.stderr.bytes)?;
            let evidence = json!({
                "boundary_id": report.evidence.boundary_id,
                "boundary_absent": report.evidence.boundary_absent,
                "helper_identity": report.evidence.helper_identity,
                "image_digest": report.evidence.resolved_image_digest,
                "inspected": report.evidence.inspected,
                "invocation_digest": report.evidence.invocation_digest,
                "kill_attempted": report.evidence.kill_attempted,
                "plan_digest": report.evidence.plan_digest,
                "process_id": report.evidence.process_id,
                "quiescent": report.evidence.quiescent,
                "reaped": report.evidence.reaped,
                "runtime_identity": report.evidence.runtime_identity,
                "survivors": report.evidence.survivors,
            });
            let process =
                self.persist_report(&serde_json::to_vec(&evidence).expect("evidence serializes"))?;
            return Ok((
                json!({
                    "outcome": match report.outcome {
                        crate::executor::backends::container::ExecutionOutcome::Success => json!({"status": "success", "exit_code": 0}),
                        crate::executor::backends::container::ExecutionOutcome::Exit(code) => json!({"status": "exit", "exit_code": code}),
                        crate::executor::backends::container::ExecutionOutcome::Signal(signal) => json!({"status": "signal", "signal": signal}),
                    },
                    "process_artifact": process.clone(),
                    "stderr_artifact": stderr.clone(),
                    "stdout_artifact": stdout.clone(),
                }),
                vec![stdout, stderr, process],
            ));
        }
        let paths = SandboxPaths::new(&self.root, &self.build, &self.temp)
            .map_err(code("executor_paths_rejected"))?;
        let backend = LocalOsBackend::select(&profile, &paths)
            .map_err(code("executor_isolation_unavailable"))?;
        let mut command = LocalCommand::new(&input.argv[0], &self.root);
        for argument in input.argv.into_iter().skip(1) {
            command = command.arg(argument);
        }
        for (key, value) in input.environment {
            command = command.env(key, value);
        }
        let _prepared = backend
            .prepare(&profile, &paths, command)
            .map_err(code("executor_prepare_failed"))?;
        // M003 currently exposes no attempt-owned local launch authority. Never
        // fall back to an unowned host process from a model tool.
        Err("attempt_executor_unavailable".to_owned())
    }

    #[cfg(test)]
    fn run_conformance(
        &mut self,
        input: RunInput,
        _attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>), String> {
        let command = crate::executor::check::CheckCommand::new(
            "native-run",
            input.argv[0].clone(),
            input.argv[1..].to_vec(),
            format!("example.invalid/native-run@sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64)),
            format!("sha256:{}", "c".repeat(64)),
            input.limits,
        )
        .map_err(code("run_request_rejected"))?;
        let source_digest = crate::executor::check::immutable_tree_digest(&self.root)
            .map_err(code("run_tree_failed"))?;
        let completion = self
            .run_runner
            .as_mut()
            .expect("test run runner was checked")
            .execute(crate::executor::check::CheckExecutionRequest {
                command: &command,
                immutable_source: &self.root,
                source_digest: &source_digest,
                build: &self.build,
                temp: &self.temp,
                max_preview_bytes: 16 * 1024,
                artifacts: &self.artifacts,
                principal: &self.authenticated.principal_id().to_string(),
                project: &self.config.project_id().to_string(),
                retention: ArtifactRetention::Forever,
                stored_at_unix_micros: crate::store::artifacts::now_unix_micros()
                    .map_err(code("artifact_clock_unavailable"))?,
                secrets: &self.secrets,
                more_boundaries: false,
            })
            .map_err(|_| {
                if self.cancelled() {
                    "cancelled".to_owned()
                } else {
                    "attempt_executor_unavailable".to_owned()
                }
            })?;
        let stdout = completion.stdout_artifact.reference().to_owned();
        let stderr = completion.stderr_artifact.reference().to_owned();
        let process = completion.process_artifact.reference().to_owned();
        Ok((
            json!({
                "outcome": match completion.status {
                    crate::executor::check::CheckStatus::Pass => json!({"status": "success", "exit_code": 0}),
                    crate::executor::check::CheckStatus::Exit(code) => json!({"status": "exit", "exit_code": code}),
                },
                "process": completion.process,
                "process_artifact": process,
                "stderr_artifact": stderr,
                "stdout_artifact": stdout,
            }),
            vec![stdout, stderr, process],
        ))
    }

    fn check(
        &mut self,
        bytes: &[u8],
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<(Value, Vec<String>), String> {
        self.ensure_not_cancelled()?;
        let input: CheckInput = decode(bytes)?;
        if input.profile == CheckProfile::Targeted && input.targets.is_empty() {
            return Err("check_targets_required".to_owned());
        }
        if input.profile == CheckProfile::Full
            && !self
                .config
                .effective_authority()
                .contains(&Grant::VerificationFull)
        {
            return Err("verification_full_grant_required".to_owned());
        }
        if self.verification_registry.is_empty() {
            return Err("trusted_check_registry_unavailable".to_owned());
        }
        if self.check_runner.is_none() {
            return Err("trusted_check_runner_unavailable".to_owned());
        }
        let feedback = self
            .feedback
            .as_ref()
            .ok_or_else(|| "trusted_check_feedback_unavailable".to_owned())?
            .clone();
        if feedback.adapters.is_empty() {
            return Err("trusted_check_feedback_unavailable".to_owned());
        }
        let selection = match input.profile {
            CheckProfile::Syntax => ProfileSelection::Syntax,
            CheckProfile::Fast => ProfileSelection::Fast,
            CheckProfile::Targeted => ProfileSelection::Targeted {
                exact_targets: input.targets.into_iter().collect(),
            },
            CheckProfile::Full => ProfileSelection::Full,
        };
        if self
            .verification_registry
            .select_native(&selection, &self.grants, &self.config)
            .map_err(code("check_profile_rejected"))?
            .is_empty()
        {
            return Err("check_profile_empty".to_owned());
        }
        let revision = self
            .ensure_workspace()?
            .current_revision()
            .map_err(code("check_tree_failed"))?;
        let plan_digest = format!("blake3:{}", blake3::hash(bytes).to_hex());
        let context = crate::workspace::edit::validate::EditOperationContext::current(
            revision.id().to_string(),
            revision.epoch().to_string(),
            revision.digest().to_string(),
            plan_digest,
        );
        let authority = crate::verify::feedback::FeedbackAuthority::issue(
            &self.authenticated,
            self.workspace_id.to_string(),
            self.config.run_id().to_string(),
            context.selected_plan_digest(),
            attempt.fencing_token.get(),
        )
        .map_err(code("check_feedback_authority_unavailable"))?;
        let mut events = crate::verify::feedback::FeedbackEventStore::open(&feedback.database)
            .map_err(code("check_feedback_store_unavailable"))?;
        let mut observer = crate::verify::feedback::FeedbackVerificationObserver::from_context(
            &mut events,
            &authority,
            &context,
            context.base_workspace_digest(),
        );
        let runner = self
            .check_runner
            .as_mut()
            .ok_or_else(|| "trusted_check_runner_unavailable".to_owned())?;
        let result = crate::verify::profiles::verify_current(
            &context,
            &self.root,
            &self.build,
            &self.temp,
            BTreeSet::new(),
            crate::verify::profiles::VerificationRequest {
                selection,
                registry: &self.verification_registry,
                authenticated: &self.authenticated,
                grants: &self.grants,
                config: &self.config,
                runner: Some(runner),
                observer: Some(&mut observer),
                artifacts: &self.artifacts,
                secrets: &self.secrets,
                on_check_failure: crate::verify::profiles::CheckFailureBehavior::Abort,
                model_outcome: None,
                cancellation: Some(&self.live_cancellation),
            },
            false,
        )
        .map_err(code("check_execution_failed"))?;
        drop(observer);
        let feedback_output = {
            let mut pipeline = crate::verify::feedback::FeedbackPipeline::new(
                &self.artifacts,
                &mut events,
                &self.authenticated,
                self.workspace_id.to_string(),
                ArtifactRetention::Forever,
                crate::store::artifacts::now_unix_micros()
                    .map_err(code("artifact_clock_unavailable"))?,
                &self.secrets,
                feedback.limits.clone(),
            )
            .map_err(code("check_feedback_unavailable"))?;
            let baseline = pipeline
                .capture_baseline(&authority, &context, &result, &feedback.adapters)
                .map_err(code("check_feedback_unavailable"))?;
            pipeline
                .process_result(
                    &authority,
                    Some(&baseline),
                    &context,
                    context.base_workspace_digest(),
                    &result,
                    &crate::verify::feedback::EditMapping::default(),
                    &feedback.adapters,
                )
                .map_err(code("check_feedback_unavailable"))?
        };
        let artifacts = vec![
            result.receipt().result_artifact.reference.clone(),
            feedback_output.payload_artifact.reference.clone(),
            feedback_output.report_artifact.reference.clone(),
        ];
        Ok((
            json!({
                "feedback": feedback_output.payload,
                "events": feedback_output.events,
                "verification": result,
            }),
            artifacts,
        ))
    }

    fn persist_log(&self, bytes: &[u8]) -> Result<String, String> {
        self.persist_artifact(
            bytes,
            "application/octet-stream",
            crate::store::artifacts::ArtifactClass::Log,
        )
    }

    fn persist_report(&self, bytes: &[u8]) -> Result<String, String> {
        self.persist_artifact(
            bytes,
            "application/json",
            crate::store::artifacts::ArtifactClass::Report,
        )
    }

    fn persist_artifact(
        &self,
        bytes: &[u8],
        media_type: &str,
        class: crate::store::artifacts::ArtifactClass,
    ) -> Result<String, String> {
        self.ensure_not_cancelled()?;
        let capture =
            CaptureRedactor::new(&self.secrets).sanitize(CaptureBoundary::Artifact, bytes);
        let bytes = capture.bytes().map_err(code("artifact_redaction_failed"))?;
        self.artifacts
            .put(
                bytes,
                crate::store::artifacts::ArtifactMetadata::new(
                    media_type,
                    class,
                    self.authenticated.principal_id().to_string(),
                    self.config.project_id().to_string(),
                    ArtifactRetention::Forever,
                    crate::store::artifacts::now_unix_micros()
                        .map_err(code("artifact_clock_unavailable"))?,
                )
                .map_err(code("artifact_metadata_failed"))?,
            )
            .map(|artifact| artifact.reference().to_string())
            .map_err(code("artifact_persistence_failed"))
    }

    fn ensure_not_cancelled(&self) -> Result<(), String> {
        if self.cancelled() {
            Err("cancelled".to_owned())
        } else {
            Ok(())
        }
    }
}

fn native_edit_error(
    error: crate::agent::adapters::grammar_edit::EditOrchestrationError,
) -> String {
    use crate::agent::adapters::grammar_edit::EditOrchestrationError;
    match error {
        EditOrchestrationError::Grammar(_) => "edit_input_rejected",
        EditOrchestrationError::Validation(error) => {
            return format!("edit_validation_failed:{}", validation_error_detail(&error));
        }
        EditOrchestrationError::Stage(_) => "edit_stage_failed",
        EditOrchestrationError::Verification(_) | EditOrchestrationError::VerificationRejected => {
            "edit_verification_failed"
        }
        EditOrchestrationError::Cancelled => "cancelled",
        EditOrchestrationError::Recovery(_) => "edit_recovery_failed",
        EditOrchestrationError::Feedback(_) => "edit_feedback_failed",
    }
    .to_owned()
}

fn validation_error_detail(
    error: &crate::workspace::edit::validate::ValidationError,
) -> &'static str {
    use crate::workspace::edit::validate::{ValidationError, ValidationLimit};
    match error {
        ValidationError::IdentityPolicyMismatch => "identity_policy_mismatch",
        ValidationError::StaleRevision => "stale_revision",
        ValidationError::ExternalEdit => "external_edit",
        ValidationError::AmbiguousAnchor(_) => "ambiguous_anchor",
        ValidationError::AnchorMismatch(_) => "anchor_mismatch",
        ValidationError::BaseDigestMismatch(_) => "base_digest_mismatch",
        ValidationError::InvalidUnicode(_) => "invalid_unicode",
        ValidationError::NewlineMismatch(_) => "newline_mismatch",
        ValidationError::FinalNewlineMismatch(_) => "final_newline_mismatch",
        ValidationError::BinaryFile(_) => "binary_file",
        ValidationError::RangeOutsideFile(_) => "range_outside_file",
        ValidationError::UnsafePath(_) => "unsafe_path",
        ValidationError::PathStateMismatch => "path_state_mismatch",
        ValidationError::LimitExceeded(limit) => match limit {
            ValidationLimit::Operations => "operations_limit",
            ValidationLimit::Path => "path_limit",
            ValidationLimit::Content => "content_limit",
            ValidationLimit::ReadBytes => "read_bytes_limit",
            ValidationLimit::Memory => "memory_limit",
            ValidationLimit::Time => "time_limit",
            ValidationLimit::Authorization => "authorization_limit",
        },
        ValidationError::Unavailable => "unavailable",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoverInput {
    expected_revision: String,
    terms: Vec<String>,
    roots: Vec<String>,
    languages: Vec<String>,
    #[serde(default)]
    cursor: Option<DiscoverCursor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    expected_revision: String,
    text: String,
    mode: SearchModeInput,
    path_prefixes: Vec<String>,
    languages: Vec<String>,
    #[serde(default)]
    cursor: Option<SearchCursor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchModeInput {
    Path,
    Content,
    PathAndContent,
}

impl From<SearchModeInput> for SearchMode {
    fn from(value: SearchModeInput) -> Self {
        match value {
            SearchModeInput::Path => Self::Path,
            SearchModeInput::Content => Self::Content,
            SearchModeInput::PathAndContent => Self::PathAndContent,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    expected_revision: String,
    path: String,
    range: ReadRangeInput,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ReadRangeInput {
    Full,
    Bytes { start: usize, end: usize },
    Lines { start: usize, end: usize },
}

impl From<ReadRangeInput> for ReadRange {
    fn from(value: ReadRangeInput) -> Self {
        match value {
            ReadRangeInput::Full => Self::Full,
            ReadRangeInput::Bytes { start, end } => Self::Bytes { start, end },
            ReadRangeInput::Lines { start, end } => Self::Lines { start, end },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    argv: Vec<String>,
    working_directory: String,
    mounts: RunMounts,
    environment: BTreeMap<String, String>,
    network: NetworkPolicy,
    limits: ResourceLimits,
    host_compatibility: bool,
    #[serde(rename = "background")]
    _background: RunBackground,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunBackground {
    Foreground,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RunMounts {
    source: MountPolicy,
    build: MountPolicy,
    temp: MountPolicy,
}

impl RunMounts {
    const fn required() -> Self {
        Self {
            source: MountPolicy::ReadOnly,
            build: MountPolicy::ReadWrite,
            temp: MountPolicy::ReadWrite,
        }
    }
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum MountPolicy {
    ReadOnly,
    ReadWrite,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum NetworkPolicy {
    Deny,
    ProfileGrants,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckInput {
    profile: CheckProfile,
    targets: Vec<String>,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CheckProfile {
    Syntax,
    Fast,
    Targeted,
    Full,
}

fn bounded_discover_options() -> DiscoverOptions {
    DiscoverOptions {
        max_result_bytes: MAX_NATIVE_OUTPUT_BYTES / 2,
        max_time: std::time::Duration::from_secs(30),
        ..DiscoverOptions::default()
    }
}

fn trusted_run_limits(limits: ResourceLimits) -> bool {
    limits.finite()
        && limits.cpu_millis <= MAX_RUN_CPU_MILLIS
        && limits.memory_bytes <= MAX_RUN_MEMORY_BYTES
        && limits.pids <= MAX_RUN_PIDS
        && limits.file_bytes <= MAX_RUN_FILE_BYTES
        && limits.disk_bytes <= MAX_RUN_DISK_BYTES
        && limits.io_bytes <= MAX_RUN_IO_BYTES
        && limits.output_bytes <= MAX_RUN_OUTPUT_BYTES
        && limits.wall_time_millis <= MAX_RUN_WALL_TIME_MILLIS
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|_| "invalid_arguments".to_owned())
}

fn revision(value: &str) -> Result<RevisionId, String> {
    RevisionId::parse(value).ok_or_else(|| "invalid_revision".to_owned())
}

fn code<E: std::fmt::Display>(prefix: &'static str) -> impl FnOnce(E) -> String {
    move |_| prefix.to_owned()
}

fn output(data: Value, artifacts: Vec<String>) -> DispatchOutcome {
    let body = serde_json::to_vec(&json!({
        "artifacts": artifacts,
        "data": data,
        "truncated": false,
        "version": 1,
    }))
    .expect("native output serializes");
    if body.len() > MAX_NATIVE_OUTPUT_BYTES {
        failed("native_output_too_large")
    } else {
        DispatchOutcome::Succeeded(CanonicalOutput {
            media_type: "application/json".to_owned(),
            body,
        })
    }
}

fn committed_output(data: Value, artifacts: Vec<String>) -> DispatchOutcome {
    match output(data, artifacts) {
        DispatchOutcome::Succeeded(output) => DispatchOutcome::DurablyCommitted(output),
        outcome => outcome,
    }
}

fn failed(code: &str) -> DispatchOutcome {
    DispatchOutcome::Failed {
        code: code.to_owned(),
    }
}

fn digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn host_platform() -> Result<Platform, String> {
    if cfg!(target_os = "macos") {
        Ok(Platform::MacOs)
    } else if cfg!(target_os = "linux") {
        Ok(Platform::Linux)
    } else if cfg!(target_os = "windows") {
        Ok(Platform::Windows)
    } else {
        Err("executor_host_unsupported".to_owned())
    }
}

fn host_architecture() -> Result<Architecture, String> {
    if cfg!(target_arch = "x86_64") {
        Ok(Architecture::X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(Architecture::Aarch64)
    } else {
        Err("executor_architecture_unsupported".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        io::{Read as _, Write as _},
        net::TcpListener,
        sync::atomic::AtomicU64,
        thread,
        time::Duration,
    };

    use agentkit_core::{Item, ItemKind};
    use agentkit_loop::{Agent, LoopInterrupt, LoopStep, ModelAdapter, SessionConfig};

    use crate::{
        agent::adapters::tool::{ToolBinding, ToolExecutorAdapter, ToolKernelContext},
        api::auth::contract::GrantSnapshot,
        capabilities::kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot},
            identity::DigestAlgorithm,
        },
        domain::{
            config::{LayerStack, RunConfigContext},
            ids::{AttemptId, PrincipalId, ProjectId, RunId, WorkspaceId},
            lifecycle::{AttemptOwnership, FencingToken},
        },
        executor::{
            check::{CheckCommand, ConformanceCheck},
            profile::ResourceLimits,
        },
        runtime::scheduler::{budget::RunBudget, reserve::BudgetLedger},
        test_support,
        verify::profiles::{CheckClass, CheckRequirement, DeclaredCheck, VerificationRegistry},
    };

    use super::*;

    fn dispatcher(runner: Option<CheckRunner>) -> (PathBuf, NativeDispatcher) {
        let directory = std::env::temp_dir().join(format!(
            "kit-native-check-{}",
            crate::domain::ids::RunId::generate().unwrap()
        ));
        let root = directory.join("source");
        let scratch = directory.join("scratch");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("format.json"), "{}\n").unwrap();
        let artifacts = Arc::new(ArtifactStore::open(directory.join("artifacts")).unwrap());
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let grants = BTreeSet::from([
            Grant::ProcessSpawn,
            Grant::VerificationTargeted,
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
        ]);
        let config = LayerStack::safe_defaults()
            .materialize(
                RunConfigContext {
                    principal_id: principal,
                    project_id: project,
                    run_id: RunId::generate().unwrap(),
                },
                &grants,
            )
            .unwrap();
        let authenticated =
            AuthenticatedPrincipal::from_grants(GrantSnapshot::new(principal, project, grants));
        let command = CheckCommand::new(
            "diagnostics",
            "cargo",
            vec!["check".to_owned()],
            "example.invalid/check@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ResourceLimits::new(1_000, 1024 * 1024, 8, 1024, 1024, 1024, 1024, 1_000),
        )
        .unwrap();
        let typecheck = CheckCommand::new(
            "typecheck",
            "cargo",
            vec!["check".to_owned()],
            "example.invalid/check@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ResourceLimits::new(1_000, 1024 * 1024, 8, 1024, 1024, 1024, 1024, 1_000),
        )
        .unwrap();
        let registry = VerificationRegistry::new(vec![
            DeclaredCheck::new(
                CheckClass::Diagnostics,
                command,
                CheckRequirement::Required,
                BTreeSet::new(),
                false,
            )
            .unwrap(),
            DeclaredCheck::new(
                CheckClass::Typecheck,
                typecheck,
                CheckRequirement::Required,
                BTreeSet::new(),
                false,
            )
            .unwrap(),
        ])
        .unwrap();
        let dispatcher = NativeDispatcher::open(
            root,
            &scratch,
            artifacts,
            authenticated,
            config,
            None,
            NativeRuntime {
                workspace_id: WorkspaceId::generate().unwrap(),
                process_registration: None,
                cancellation: SqliteCancellationCoordinator::new(directory.join("state.sqlite3")),
                live_cancellation: Arc::new(AtomicBool::new(false)),
                container_image: None,
                verification_registry: registry,
                check_runner: runner,
                secrets: Vec::new(),
                syntax_executors: vec![
                    crate::executor::syntax::SyntaxExecutor::debug(
                        "text",
                        crate::workspace::edit::format::NATIVE_TEXT_VERSION,
                        crate::executor::syntax::DebugSyntaxAction::Pass(None),
                    ),
                    crate::executor::syntax::SyntaxExecutor::debug(
                        "json",
                        crate::workspace::edit::format::NATIVE_JSON_VERSION,
                        crate::executor::syntax::DebugSyntaxAction::Pass(None),
                    ),
                    crate::executor::syntax::SyntaxExecutor::debug(
                        "rust",
                        crate::workspace::edit::format::RUST_GRAMMAR_VERSION,
                        crate::executor::syntax::DebugSyntaxAction::Pass(None),
                    ),
                ],
                formatter_required: false,
                formatter: None,
                feedback: Some(NativeFeedbackRuntime {
                    database: directory.join("feedback.sqlite3"),
                    adapters: BTreeMap::from([
                        (
                            "diagnostics".to_owned(),
                            crate::verify::feedback::DiagnosticAdapter::NormalizedJsonLinesV1,
                        ),
                        (
                            "typecheck".to_owned(),
                            crate::verify::feedback::DiagnosticAdapter::NormalizedJsonLinesV1,
                        ),
                    ]),
                    limits: crate::verify::feedback::FeedbackLimits::default(),
                }),
                edit_validation_time: crate::workspace::edit::ir::EditLimits::default()
                    .max_validation_time,
                run_runner: None,
            },
        )
        .unwrap();
        (directory, dispatcher)
    }

    fn attempt(dispatcher: &NativeDispatcher) -> crate::domain::lifecycle::AttemptOwnership {
        crate::domain::lifecycle::AttemptOwnership::new(
            crate::domain::ids::AttemptId::generate().unwrap(),
            dispatcher.authenticated.principal_id(),
            crate::domain::lifecycle::FencingToken::new(1),
        )
    }

    fn protocol_server(
        responses: Vec<String>,
    ) -> (String, thread::JoinHandle<Vec<serde_json::Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0);
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap();
                while bytes.len() - header_end < length {
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0);
                    bytes.extend_from_slice(&buffer[..read]);
                }
                requests
                    .push(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn native_inputs(revision: &str) -> Vec<(String, Value)> {
        let original = b"{}\n";
        vec![
            (
                "kit_discover".to_owned(),
                json!({"expected_revision":revision,"terms":["main"],"roots":[],"languages":["rust"]}),
            ),
            (
                "kit_search".to_owned(),
                json!({"expected_revision":revision,"text":"main","mode":"content","path_prefixes":[],"languages":["rust"]}),
            ),
            (
                "kit_read".to_owned(),
                json!({"expected_revision":revision,"path":"lib.rs","range":{"kind":"full"}}),
            ),
            (
                "kit_edit".to_owned(),
                json!({
                    "version":1,
                    "expected_revision":revision,
                    "operations":[{
                        "op":"replace_range",
                        "path":"format.json",
                        "base_digest":format!("blake3:{}", blake3::hash(original).to_hex()),
                        "range":{"start":0,"end":original.len()},
                        "expected":{"encoding":"utf8","newline":"lf","text":"{}","final_newline":true},
                        "replacement":{"encoding":"utf8","newline":"lf","text":"{\"x\":1}","final_newline":true},
                        "executable":"preserve"
                    }]
                }),
            ),
            (
                "kit_run".to_owned(),
                json!({
                    "argv":["cargo","metadata"],
                    "working_directory":".",
                    "mounts":{"source":"read_only","build":"read_write","temp":"read_write"},
                    "environment":{},
                    "network":"deny",
                    "host_compatibility":false,
                    "background":"foreground",
                    "limits":{"cpu_millis":1000,"memory_bytes":1048576,"pids":8,"file_bytes":1048576,"disk_bytes":1048576,"io_bytes":1048576,"output_bytes":65536,"wall_time_millis":1000}
                }),
            ),
            (
                "kit_check".to_owned(),
                json!({"profile":"fast","targets":[]}),
            ),
        ]
    }

    fn anthropic_tool_stream(inputs: &[(String, Value)]) -> String {
        let mut stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-tools\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
        )
        .to_owned();
        for (index, (name, input)) in inputs.iter().enumerate() {
            stream.push_str(&format!(
                "event: content_block_start\ndata: {}\n\n",
                json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("anthropic-call-{index}"),"name":name,"input":{}}})
            ));
            stream.push_str(&format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}})
            ));
            stream.push_str(&format!(
                "event: content_block_stop\ndata: {}\n\n",
                json!({"type":"content_block_stop","index":index})
            ));
        }
        stream.push_str(concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        ));
        stream
    }

    fn anthropic_completion_stream() -> String {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-done\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"complete\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .to_owned()
    }

    fn completions_tool_stream(inputs: &[(String, Value)]) -> String {
        let calls = inputs
            .iter()
            .enumerate()
            .map(|(index, (name, input))| {
                json!({"index":index,"id":format!("completion-call-{index}"),"type":"function","function":{"name":name,"arguments":input.to_string()}})
            })
            .collect::<Vec<_>>();
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chatcmpl-tools","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
        )
    }

    fn completions_completion_stream() -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chatcmpl-done","model":"gpt-test","choices":[{"index":0,"delta":{"content":"complete"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
        )
    }

    fn configure_formatter(dispatcher: &mut NativeDispatcher) {
        let path =
            crate::workspace::edit::ir::RootRelativePath::parse("format.json", 4096).unwrap();
        dispatcher.formatter_required = true;
        dispatcher.formatter = Some(NativeFormatterRuntime {
            descriptor: crate::workspace::edit::format::FormatterDescriptor::new(
                "route-formatter",
                "v1",
                vec![path],
            )
            .unwrap(),
            executor: test_support::formatter_executor(test_support::FormatterTestAction::Rewrite(
                "format.json".to_owned(),
                b"{\n  \"x\": 1\n}\n".to_vec(),
            )),
        });
    }

    async fn exercise_provider_route<M: ModelAdapter>(
        model: M,
        captured: thread::JoinHandle<Vec<Value>>,
        directory: PathBuf,
        mut dispatcher: NativeDispatcher,
    ) {
        dispatcher.run_runner = Some(CheckRunner::conformance([ConformanceCheck::pass(
            b"run-output",
            b"",
        )]));
        configure_formatter(&mut dispatcher);
        let revision = dispatcher.revision().unwrap();
        let inputs = native_inputs(&revision);
        let principal = dispatcher.authenticated.principal_id();
        let project = dispatcher.config.project_id();
        let run = dispatcher.config.run_id();
        let workspace = dispatcher.workspace_id;
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(1),
        );
        let configured = NativeCatalog::all()
            .iter()
            .map(|descriptor| {
                let constraints = ArgumentConstraints::new([format!(
                    "native={}@{}",
                    descriptor.tool().short_name(),
                    descriptor.identity().version().as_str()
                )
                .into_bytes()]);
                (descriptor, constraints)
            })
            .collect::<Vec<_>>();
        let grants = CapabilityGrantSnapshot::new(
            &dispatcher.config,
            configured.iter().map(|(descriptor, constraints)| {
                CapabilityGrant::new(
                    principal,
                    project,
                    workspace,
                    descriptor.identity().clone(),
                    descriptor.schema().normalized_digest(),
                    descriptor.effect(),
                    constraints.clone(),
                )
            }),
            DigestAlgorithm::Sha256,
        );
        let bindings = configured
            .iter()
            .map(|(descriptor, constraints)| {
                let binding = ToolBinding::new(
                    descriptor.spec().clone(),
                    descriptor.identity().clone(),
                    descriptor.schema().normalized_digest(),
                    descriptor.schema().normalized_digest(),
                    descriptor.effect(),
                    constraints.clone(),
                    descriptor.reservation(),
                    descriptor.retry_safety(),
                    descriptor.approval(),
                );
                if descriptor.tool() == NativeTool::Check {
                    binding.with_cost_estimator(|_| {
                        Ok(crate::runtime::scheduler::limits::Spend::new(0, 0, 0, 1, 2))
                    })
                } else {
                    binding
                }
            })
            .collect::<Vec<_>>();
        let database = directory.join("route.sqlite3");
        let mut store = test_support::open_sqlite_store(&database).unwrap();
        let claim = store
            .install_driver_claim_for_test(crate::api::service::AttemptDriverClaim {
                run_id: run,
                attempt_id: attempt.attempt_id,
                principal_id: principal,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();
        let budget = Arc::new(BudgetLedger::new(RunBudget::new(0, 0, 0, 256, 256)));
        let snapshot = dispatcher.config.clone();
        let tool = ToolExecutorAdapter::new(
            bindings,
            ToolKernelContext {
                authenticated: dispatcher.authenticated.clone(),
                config: dispatcher.config.clone(),
                grants,
                delegation: None,
                workspace_id: workspace,
                project_id: project,
                attempt,
                claim,
                current_fence: Arc::new(AtomicU64::new(1)),
                cancellation: Arc::new(AtomicBool::new(false)),
                cancellation_coordinator: Arc::new(SqliteCancellationCoordinator::new(&database)),
                budget: Arc::clone(&budget),
            },
            store,
            move |invocation| dispatcher.dispatch(invocation),
        )
        .unwrap();
        let agent = Agent::builder()
            .model(model)
            .tool_executor(tool)
            .input(vec![Item::text(ItemKind::User, "exercise native tools")])
            .build()
            .unwrap();
        let mut driver = agent
            .start(SessionConfig::new(run.to_string()))
            .await
            .unwrap();
        loop {
            match driver.next().await.unwrap() {
                LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                    pending.approve(&mut driver).unwrap();
                }
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {}
                LoopStep::Interrupt(other) => panic!("unexpected interrupt: {other:?}"),
                LoopStep::Finished(result) => {
                    assert_eq!(result.finish_reason, agentkit_core::FinishReason::Completed);
                    break;
                }
            }
        }

        let requests = captured.join().unwrap();
        assert_eq!(requests.len(), 2);
        let registered = requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool.get("name")
                    .or_else(|| tool.pointer("/function/name"))
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered,
            NativeTool::ALL
                .into_iter()
                .map(NativeTool::provider_alias)
                .collect()
        );
        let events = test_support::open_sqlite_store(&database)
            .unwrap()
            .events()
            .unwrap();
        let payloads = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event.event_type.as_str(),
                    "capability.invocation_intent" | "capability.invocation_outcome"
                )
            })
            .map(|event| serde_json::from_slice::<Value>(&event.event.payload).unwrap())
            .collect::<Vec<_>>();
        for (alias, _) in &inputs {
            let descriptor = NativeCatalog::by_tool_name(alias).unwrap();
            assert!(payloads.iter().any(|payload| {
                payload["capability"]["name"] == descriptor.tool().short_name()
            }));
        }
        let outputs = payloads
            .iter()
            .filter_map(|payload| payload["result"]["output"]["body"].as_array())
            .map(|bytes| {
                serde_json::from_slice::<Value>(
                    &bytes
                        .iter()
                        .map(|byte| byte.as_u64().unwrap() as u8)
                        .collect::<Vec<_>>(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 6);
        assert!(outputs.iter().all(|output| {
            output["version"] == 1 && output["truncated"] == false && output["artifacts"].is_array()
        }));
        assert_eq!(budget.totals().committed.tools(), 6);
        assert_eq!(budget.totals().committed.processes(), 3);
        let restarted =
            crate::agent::executor::tool_budget_from_events(&events, &snapshot).unwrap();
        assert_eq!(restarted.remaining().tools(), 250);
        assert_eq!(restarted.remaining().processes(), 253);
        assert!(
            restarted
                .reserve(
                    crate::runtime::scheduler::reserve::ReservationId::new(1),
                    crate::runtime::scheduler::limits::Spend::new(0, 0, 0, 251, 0),
                )
                .is_err()
        );
        assert!(
            restarted
                .reserve(
                    crate::runtime::scheduler::reserve::ReservationId::new(2),
                    crate::runtime::scheduler::limits::Spend::new(0, 0, 0, 0, 254),
                )
                .is_err()
        );
        let durable = serde_json::to_string(&outputs).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join("source/format.json")).unwrap(),
            "{\n  \"x\": 1\n}\n",
            "{durable}"
        );
        for evidence in [
            "diff_artifact",
            "feedback",
            "process_artifact",
            "verification",
        ] {
            assert!(
                durable.contains(evidence),
                "missing route evidence: {evidence}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn anthropic_streamed_aliases_reach_native_dispatch_through_the_agent_loop() {
        let checks = (0..6).map(|_| ConformanceCheck::pass(b"", b""));
        let (directory, mut dispatcher) = dispatcher(Some(CheckRunner::conformance(checks)));
        let revision = dispatcher.revision().unwrap();
        let inputs = native_inputs(&revision);
        let (url, captured) = protocol_server(vec![
            anthropic_tool_stream(&inputs),
            anthropic_completion_stream(),
        ]);
        let mut config =
            agentkit_provider_anthropic::AnthropicConfig::new("test", "claude-test", 1024)
                .unwrap()
                .with_base_url(url);
        config.tool_choice = None;
        exercise_provider_route(
            agentkit_provider_anthropic::AnthropicAdapter::new(config).unwrap(),
            captured,
            directory,
            dispatcher,
        )
        .await;
    }

    #[tokio::test]
    async fn completions_streamed_aliases_reach_native_dispatch_through_the_agent_loop() {
        let checks = (0..6).map(|_| ConformanceCheck::pass(b"", b""));
        let (directory, mut dispatcher) = dispatcher(Some(CheckRunner::conformance(checks)));
        let revision = dispatcher.revision().unwrap();
        let inputs = native_inputs(&revision);
        let (url, captured) = protocol_server(vec![
            completions_tool_stream(&inputs),
            completions_completion_stream(),
        ]);
        exercise_provider_route(
            agentkit_provider_openai::OpenAIAdapter::new(
                agentkit_provider_openai::OpenAIConfig::new("test", "gpt-test").with_base_url(url),
            )
            .unwrap(),
            captured,
            directory,
            dispatcher,
        )
        .await;
    }

    #[test]
    fn trusted_check_runner_returns_bounded_artifacts() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"ok", b""),
            ConformanceCheck::pass(b"ok", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let owner = attempt(&dispatcher);
        let (value, artifacts) = dispatcher
            .check(br#"{"profile":"fast","targets":[]}"#, owner)
            .unwrap();
        assert_eq!(value["verification"]["checks"][0]["status"], "pass");
        assert_eq!(artifacts.len(), 3);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn absent_trusted_check_runner_is_typed_unavailable() {
        let (directory, mut dispatcher) = dispatcher(None);
        let owner = attempt(&dispatcher);
        assert_eq!(
            dispatcher.check(br#"{"profile":"fast","targets":[]}"#, owner),
            Err("trusted_check_runner_unavailable".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn all_bytes(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                bytes.extend(all_bytes(&entry.unwrap().path()));
            }
        } else if let Ok(file) = std::fs::read(path) {
            bytes.extend(file);
        }
        bytes
    }

    #[test]
    fn cancellation_during_native_run_reaps_the_protocol_service() {
        let (directory, mut dispatcher) = dispatcher(None);
        let cancellation = Arc::clone(&dispatcher.live_cancellation);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        dispatcher.run_runner = Some(CheckRunner::conformance([
            ConformanceCheck::CancelWhenSignalled {
                entered: entered_tx,
                cancellation: Arc::clone(&cancellation),
            },
        ]));
        let owner = attempt(&dispatcher);
        let result = thread::scope(|scope| {
            let worker = scope.spawn(move || {
                dispatcher.run(
                    br#"{"argv":["long-running"],"working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},"environment":{},"network":"deny","host_compatibility":false,"background":"foreground","limits":{"cpu_millis":1000,"memory_bytes":1048576,"pids":8,"file_bytes":1048576,"disk_bytes":1048576,"io_bytes":1048576,"output_bytes":65536,"wall_time_millis":1000}}"#,
                    owner,
                )
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            cancellation.store(true, Ordering::Release);
            worker.join().unwrap()
        });
        assert_eq!(result, Err("cancelled".to_owned()));
        let evidence = String::from_utf8_lossy(&all_bytes(&directory)).into_owned();
        assert!(evidence.contains("\"kill_attempted\":true"));
        assert!(evidence.contains("\"reaped\":true"));
        assert!(evidence.contains("\"survivors\":0"));
        assert!(evidence.contains("\"phase\":\"quiescent\""));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_during_each_native_check_child_reaps_with_zero_survivors() {
        for child in 0..2 {
            let (directory, mut dispatcher) = dispatcher(None);
            let cancellation = Arc::clone(&dispatcher.live_cancellation);
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let mut checks = Vec::new();
            if child == 1 {
                checks.push(ConformanceCheck::pass(b"first", b""));
            }
            checks.push(ConformanceCheck::CancelWhenSignalled {
                entered: entered_tx,
                cancellation: Arc::clone(&cancellation),
            });
            dispatcher.check_runner = Some(CheckRunner::conformance(checks));
            let owner = attempt(&dispatcher);
            let result = thread::scope(|scope| {
                let worker = scope
                    .spawn(move || dispatcher.check(br#"{"profile":"fast","targets":[]}"#, owner));
                entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                cancellation.store(true, Ordering::Release);
                worker.join().unwrap()
            });
            let (result, artifacts) = result.unwrap();
            assert_eq!(result["verification"]["decision"], "abort");
            assert_eq!(
                result["verification"]["checks"][child]["status"],
                "cancelled"
            );
            assert!(result["verification"]["checks"][child]["process_artifact"].is_string());
            assert_eq!(artifacts.len(), 3);
            let evidence = String::from_utf8_lossy(&all_bytes(&directory)).into_owned();
            assert!(
                evidence.contains("\"kill_attempted\":true"),
                "child {child}"
            );
            assert!(evidence.contains("\"reaped\":true"), "child {child}");
            assert!(evidence.contains("\"survivors\":0"), "child {child}");
            assert!(
                evidence.contains("\"phase\":\"quiescent\""),
                "child {child}"
            );
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn native_edit_aborts_without_required_verification_services() {
        let (directory, mut dispatcher) = dispatcher(None);
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {
                    "encoding": "utf8",
                    "newline": "lf",
                    "text": "never materialized",
                    "final_newline": true
                },
                "executable": false
            }]
        }))
        .unwrap();
        assert_eq!(
            dispatcher.edit(&input, attempt(&dispatcher)),
            Err("trusted_edit_runner_unavailable".to_owned())
        );
        assert!(!dispatcher.root.join("created.txt").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_edit_uses_configured_check_runner_before_materialization() {
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
            ConformanceCheck::pass(b"diagnostics", b""),
            ConformanceCheck::pass(b"typecheck", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {
                    "encoding": "utf8",
                    "newline": "lf",
                    "text": "verified",
                    "final_newline": true
                },
                "executable": false
            }]
        }))
        .unwrap();
        let owner = attempt(&dispatcher);
        let (result, artifacts, committed) = dispatcher.edit(&input, owner).unwrap();
        assert_eq!(
            std::fs::read_to_string(dispatcher.root.join("created.txt")).unwrap(),
            "verified\n"
        );
        assert!(!result["verification"].is_null());
        assert!(artifacts.len() >= 2);
        assert!(committed);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_edit_returns_feedback_and_preserves_revision_on_new_required_diagnostic() {
        let diagnostic = serde_json::to_vec(&json!({
            "schema_version": 1,
            "path": "created.txt",
            "range": {"start_line": 1, "start_column": 1, "end_line": 1, "end_column": 2},
            "code": "E1",
            "message": "new diagnostic",
            "severity": "error",
            "tool": "test"
        }))
        .unwrap();
        let runner = CheckRunner::conformance([
            ConformanceCheck::pass(b"", b""),
            ConformanceCheck::pass(b"", b""),
            ConformanceCheck::exit(1, diagnostic, b"failed"),
            ConformanceCheck::pass(b"", b""),
        ]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {
                    "encoding": "utf8",
                    "newline": "lf",
                    "text": "rejected",
                    "final_newline": true
                },
                "executable": false
            }]
        }))
        .unwrap();
        let owner = attempt(&dispatcher);
        let (result, artifacts, committed) = dispatcher.edit(&input, owner).unwrap();
        assert_eq!(result["outcome"], "aborted");
        assert!(!result["feedback"]["items"].as_array().unwrap().is_empty());
        assert_eq!(result["events"].as_array().unwrap().len(), 6);
        assert_eq!(artifacts.len(), 3);
        assert!(!committed);
        assert!(!dispatcher.root.join("created.txt").exists());
        assert_eq!(dispatcher.revision().unwrap(), revision);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn native_edit_rejects_missing_required_trusted_services_before_staging() {
        let runner = CheckRunner::conformance([]);
        let (directory, mut dispatcher) = dispatcher(Some(runner));
        let revision = dispatcher.revision().unwrap();
        let input = serde_json::to_vec(&json!({
            "version": 1,
            "expected_revision": revision,
            "operations": [{
                "op": "add_file",
                "path": "created.txt",
                "content": {"encoding": "utf8", "newline": "lf", "text": "x", "final_newline": true},
                "executable": false
            }]
        }))
        .unwrap();
        let owner = attempt(&dispatcher);
        dispatcher.feedback = None;
        assert_eq!(
            dispatcher.edit(&input, owner),
            Err("trusted_edit_feedback_unavailable".to_owned())
        );
        assert!(!dispatcher.root.join("created.txt").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_and_hard_run_limits_stop_before_effects() {
        let (directory, mut dispatcher) = dispatcher(None);
        dispatcher.live_cancellation.store(true, Ordering::Release);
        assert_eq!(
            dispatcher.run(
                br#"{"argv":["true"],"working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},"environment":{},"network":"deny","host_compatibility":false,"background":"foreground","limits":{"cpu_millis":1,"memory_bytes":1,"pids":1,"file_bytes":1,"disk_bytes":1,"io_bytes":1,"output_bytes":1,"wall_time_millis":1}}"#,
                crate::domain::lifecycle::AttemptOwnership::new(
                    crate::domain::ids::AttemptId::generate().unwrap(),
                    dispatcher.authenticated.principal_id(),
                    crate::domain::lifecycle::FencingToken::new(1),
                ),
            ),
            Err("cancelled".to_owned())
        );
        dispatcher.live_cancellation.store(false, Ordering::Release);
        assert_eq!(
            dispatcher.run(
                br#"{"argv":["true"],"working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},"environment":{},"network":"deny","host_compatibility":false,"background":"foreground","limits":{"cpu_millis":60001,"memory_bytes":1,"pids":1,"file_bytes":1,"disk_bytes":1,"io_bytes":1,"output_bytes":1,"wall_time_millis":1}}"#,
                crate::domain::lifecycle::AttemptOwnership::new(
                    crate::domain::ids::AttemptId::generate().unwrap(),
                    dispatcher.authenticated.principal_id(),
                    crate::domain::lifecycle::FencingToken::new(1),
                ),
            ),
            Err("run_request_rejected".to_owned())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
