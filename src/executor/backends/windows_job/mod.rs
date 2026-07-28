//! Windows Job Object execution boundary.
//!
//! A Job Object contains and accounts for a process tree, but is not a filesystem,
//! network, privilege, container, or VM boundary. Consequently this module never
//! advertises a Job Object by itself as restricted or hostile isolation.

use std::collections::BTreeSet;

use crate::executor::profile::{
    BackendId, BackendPrimitive, BackendProbe, ExecutionLabel, ExecutorProfile, Platform,
    ProbeStatus,
};
use crate::executor::secrets::ExecutorSecretBroker;

#[cfg(windows)]
mod native;
pub mod runtime;
#[cfg(windows)]
pub use native::{
    Job, JobError, JobProcess, ProductionProcess, Recovery, WindowsCommand,
    spawn_attempt_registered, spawn_attempt_registered_with_secret_broker,
};
#[cfg(windows)]
pub(crate) use native::{
    spawn_attempt_registered_sequence, spawn_composite_suspended, spawn_daemon_observed,
    spawn_daemon_observed_with_secret_broker,
};
pub use runtime::{
    HELPER_PATH, HELPER_PROTOCOL, RuntimeBoundary, RuntimeEvidence, RuntimeMode,
    RuntimeUnavailable, UnavailableReason,
};

pub fn capabilities() -> BTreeSet<BackendPrimitive> {
    use BackendPrimitive as P;

    BTreeSet::from([
        P::WindowsJobObject,
        P::ProcessBoundary,
        P::WholeProcessTreeControl,
        P::CpuLimit,
        P::MemoryLimit,
        P::PidLimit,
    ])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionSpawnBackend {
    WindowsJob,
    WindowsComposite,
}

pub fn required_production_spawn_backend(
    profile: &ExecutorProfile,
) -> Option<ProductionSpawnBackend> {
    use BackendPrimitive as P;

    if profile.platform() != Platform::Windows {
        None
    } else if profile.requirements().contains(&P::ContainerOrVmBoundary)
        || profile.requirements().contains(&P::VmTenantBoundary)
    {
        Some(ProductionSpawnBackend::WindowsComposite)
    } else if profile.requirements().contains(&P::WindowsJobObject) {
        Some(ProductionSpawnBackend::WindowsJob)
    } else {
        None
    }
}

/// Production profiles that require a Windows container or VM are never routed
/// to a Job-only launcher. The trusted helper probe must also succeed.
pub fn production_spawn_backend(
    profile: &ExecutorProfile,
) -> Result<Option<ProductionSpawnBackend>, RuntimeUnavailable> {
    let required = required_production_spawn_backend(profile);
    if required == Some(ProductionSpawnBackend::WindowsComposite) {
        composite_probe(profile)?;
    }
    Ok(required)
}

pub fn production_spawn_backend_with_secret_broker(
    profile: &ExecutorProfile,
    broker: &dyn ExecutorSecretBroker,
) -> Result<Option<ProductionSpawnBackend>, RuntimeUnavailable> {
    let required = required_production_spawn_backend(profile);
    if required == Some(ProductionSpawnBackend::WindowsComposite) {
        composite_probe_with_secret_broker(profile, broker)?;
    }
    Ok(required)
}

pub fn composite_probe(profile: &ExecutorProfile) -> Result<BackendProbe, RuntimeUnavailable> {
    composite_probe_for_terminal(profile, false)
}

pub fn composite_probe_with_secret_broker(
    profile: &ExecutorProfile,
    broker: &dyn ExecutorSecretBroker,
) -> Result<BackendProbe, RuntimeUnavailable> {
    composite_probe_for_terminal_with_secret_broker(profile, false, broker)
}

pub(crate) fn composite_probe_for_terminal_with_secret_broker(
    profile: &ExecutorProfile,
    require_conpty: bool,
    broker: &dyn ExecutorSecretBroker,
) -> Result<BackendProbe, RuntimeUnavailable> {
    let evidence = runtime::probe_for_terminal_with_secret_broker(profile, require_conpty, broker)?;
    #[cfg(windows)]
    Job::probe_operations(profile.resources(), require_conpty).map_err(|error| {
        RuntimeUnavailable {
            reason: UnavailableReason::RuntimeProbeFailed,
            detail: error.to_string(),
        }
    })?;
    Ok(BackendProbe {
        backend: BackendId::new("windows-job-container-vm").expect("static backend ID is valid"),
        label: profile.label(),
        platform: Platform::Windows,
        architecture: profile.architecture(),
        capabilities: runtime::composite_capabilities(&evidence),
        status: ProbeStatus::Available,
    })
}

pub fn composite_probe_for_terminal(
    profile: &ExecutorProfile,
    require_conpty: bool,
) -> Result<BackendProbe, RuntimeUnavailable> {
    let evidence = runtime::probe_for_terminal(profile, require_conpty)?;
    #[cfg(windows)]
    Job::probe_operations(profile.resources(), require_conpty).map_err(|error| {
        RuntimeUnavailable {
            reason: UnavailableReason::RuntimeProbeFailed,
            detail: error.to_string(),
        }
    })?;
    Ok(BackendProbe {
        backend: BackendId::new("windows-job-container-vm").expect("static backend ID is valid"),
        label: profile.label(),
        platform: Platform::Windows,
        architecture: profile.architecture(),
        capabilities: runtime::composite_capabilities(&evidence),
        status: ProbeStatus::Available,
    })
}

/// Reports only the Job Object layer. A separate, probed container/Hyper-V
/// backend must advertise the remaining primitives before an isolation profile
/// can be selected.
pub fn probe(profile: &ExecutorProfile) -> BackendProbe {
    BackendProbe {
        backend: BackendId::new("windows-job-object").expect("static backend ID is valid"),
        label: ExecutionLabel::TrustedLocal,
        platform: Platform::Windows,
        architecture: profile.architecture(),
        capabilities: capabilities(),
        status: probe_status(profile),
    }
}

#[cfg(windows)]
fn probe_status(profile: &ExecutorProfile) -> ProbeStatus {
    let host_architecture = if cfg!(target_arch = "x86_64") {
        crate::executor::profile::Architecture::X86_64
    } else if cfg!(target_arch = "aarch64") {
        crate::executor::profile::Architecture::Aarch64
    } else {
        return ProbeStatus::Unavailable {
            reason: "unsupported Windows host architecture".to_owned(),
        };
    };
    if profile.platform() != Platform::Windows || profile.architecture() != host_architecture {
        return ProbeStatus::Unavailable {
            reason: "Windows Job profile platform or architecture does not match this host"
                .to_owned(),
        };
    }
    match Job::probe_operations(profile.resources(), false) {
        Ok(()) => ProbeStatus::Available,
        Err(error) => ProbeStatus::Unavailable {
            reason: error.to_string(),
        },
    }
}

#[cfg(not(windows))]
fn probe_status(_profile: &ExecutorProfile) -> ProbeStatus {
    ProbeStatus::Unavailable {
        reason: "Windows Job Objects are unavailable on this host".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::profile::{
        Architecture, BackendPrimitive as P, ExecutorProfile, Platform, ProfileSpec,
        ResourceLimits, TrustTier, select_backend,
    };

    #[test]
    fn job_capabilities_do_not_claim_hostile_isolation() {
        let capabilities = capabilities();
        assert!(capabilities.contains(&P::WindowsJobObject));
        for primitive in [
            P::ContainerOrVmBoundary,
            P::VmTenantBoundary,
            P::FilesystemBoundary,
            P::NetworkDeny,
            P::PrivilegeBoundary,
            P::OutputLimit,
            P::WallTimeLimit,
        ] {
            assert!(!capabilities.contains(&primitive));
        }
    }

    #[test]
    fn job_only_probe_cannot_select_restricted_or_hostile_profiles() {
        for tier in [TrustTier::Restricted, TrustTier::Hostile] {
            let profile = ExecutorProfile::new(ProfileSpec::isolated(
                tier,
                Platform::Windows,
                Architecture::X86_64,
                ResourceLimits::new(1, 1, 1, 1, 1, 1, 1, 1),
            ))
            .unwrap();
            assert!(select_backend(&profile, [probe(&profile)]).is_err());
        }
    }

    #[test]
    fn production_selector_never_routes_isolated_windows_to_job_only() {
        let profile = ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Windows,
            Architecture::X86_64,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 64, 1),
        ))
        .unwrap();
        if !cfg!(windows) {
            assert!(matches!(
                production_spawn_backend(&profile),
                Err(RuntimeUnavailable { .. })
            ));
        }
        assert_eq!(
            required_production_spawn_backend(&profile),
            Some(ProductionSpawnBackend::WindowsComposite)
        );

        let unix = ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Linux,
            Architecture::X86_64,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 64, 1),
        ))
        .unwrap();
        assert_eq!(production_spawn_backend(&unix).unwrap(), None);
    }
}
