use std::{collections::BTreeSet, path::PathBuf};

use kit::executor::profile::{
    Architecture, BackendId, BackendPrimitive, BackendProbe, BackendSelectionError,
    CompatibilityOptIn, CredentialHandle, CredentialInjection, CredentialInjectionMode,
    EgressGrant, EgressTransport, ExecutionLabel, ExecutorProfile, MountAccess, MountRole,
    Platform, ProbeStatus, ProfileError, ProfileSpec, RepositoryCodePolicy, ResourceLimits,
    SourceWriteMode, TrustTier, select_backend,
};

fn limits() -> ResourceLimits {
    ResourceLimits::new(
        60_000,
        512 * 1024 * 1024,
        128,
        64 * 1024 * 1024,
        2 * 1024 * 1024 * 1024,
        4 * 1024 * 1024 * 1024,
        16 * 1024 * 1024,
        300_000,
    )
}

fn spec_for(label: ExecutionLabel, platform: Platform) -> ProfileSpec {
    match label {
        ExecutionLabel::TrustedLocal => ProfileSpec::isolated(
            TrustTier::TrustedLocal,
            platform,
            Architecture::X86_64,
            limits(),
        ),
        ExecutionLabel::Restricted => ProfileSpec::isolated(
            TrustTier::Restricted,
            platform,
            Architecture::X86_64,
            limits(),
        ),
        ExecutionLabel::Hostile => {
            ProfileSpec::isolated(TrustTier::Hostile, platform, Architecture::X86_64, limits())
        }
        ExecutionLabel::HostCompatibility => ProfileSpec::host_compatibility(
            platform,
            Architecture::X86_64,
            limits(),
            CompatibilityOptIn::trusted_local("explicit compatibility mode").unwrap(),
        ),
    }
}

fn profile_for(label: ExecutionLabel, platform: Platform) -> ExecutorProfile {
    ExecutorProfile::new(spec_for(label, platform)).unwrap()
}

fn profile(tier: TrustTier) -> ExecutorProfile {
    profile_for(tier.into(), Platform::Linux)
}

fn probe_with(
    profile: &ExecutorProfile,
    id: impl Into<String>,
    capabilities: BTreeSet<BackendPrimitive>,
) -> BackendProbe {
    BackendProbe {
        backend: BackendId::new(id).unwrap(),
        label: profile.label(),
        platform: profile.platform(),
        architecture: profile.architecture(),
        capabilities,
        status: ProbeStatus::Available,
    }
}

fn expected_requirements(label: ExecutionLabel, platform: Platform) -> BTreeSet<BackendPrimitive> {
    use BackendPrimitive as P;

    let mut expected = BTreeSet::new();
    match label {
        ExecutionLabel::TrustedLocal => {
            expected.extend([
                P::OsSandbox,
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ScrubbedEnvironment,
                P::NetworkDeny,
                P::OutputLimit,
                P::WallTimeLimit,
                P::WholeProcessTreeControl,
                P::ReadOnlySource,
                P::RepositoryCodeSandbox,
            ]);
            match platform {
                Platform::Linux | Platform::MacOs => {}
                Platform::Windows => {
                    expected.extend([P::WindowsJobObject, P::ContainerOrVmBoundary]);
                }
            }
        }
        ExecutionLabel::Restricted => {
            expected.extend([
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::PrivilegeBoundary,
                P::SyscallPolicy,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ScrubbedEnvironment,
                P::NetworkDeny,
                P::WholeProcessTreeControl,
                P::CpuLimit,
                P::MemoryLimit,
                P::PidLimit,
                P::FileSizeLimit,
                P::DiskLimit,
                P::IoLimit,
                P::OutputLimit,
                P::WallTimeLimit,
                P::ReadOnlySource,
                P::RepositoryCodeDisabled,
            ]);
            match platform {
                Platform::Linux => expected.extend([P::UserNamespace, P::RootlessBoundary]),
                Platform::MacOs => expected.extend([P::OsSandbox]),
                Platform::Windows => {
                    expected.extend([P::WindowsJobObject, P::ContainerOrVmBoundary]);
                }
            }
        }
        ExecutionLabel::Hostile => {
            expected.extend([
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::TenantBoundary,
                P::IsolatedStorage,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ScrubbedEnvironment,
                P::NetworkDeny,
                P::WholeProcessTreeControl,
                P::CpuLimit,
                P::MemoryLimit,
                P::PidLimit,
                P::FileSizeLimit,
                P::DiskLimit,
                P::IoLimit,
                P::OutputLimit,
                P::WallTimeLimit,
                P::ReadOnlySource,
                P::RepositoryCodeDisabled,
            ]);
            match platform {
                Platform::Linux => expected.extend([P::UserKernelOrVmTenantBoundary]),
                Platform::MacOs => expected.extend([P::VmTenantBoundary]),
                Platform::Windows => {
                    expected.extend([P::WindowsJobObject, P::VmTenantBoundary]);
                }
            }
        }
        ExecutionLabel::HostCompatibility => expected.extend([
            P::ScrubbedEnvironment,
            P::ProcessGroup,
            P::OutputLimit,
            P::WallTimeLimit,
        ]),
    }
    expected
}

#[test]
fn all_tiers_and_host_compatibility_have_truthful_distinct_labels() {
    assert_eq!(
        TrustTier::ALL.map(ExecutionLabel::from),
        [
            ExecutionLabel::TrustedLocal,
            ExecutionLabel::Restricted,
            ExecutionLabel::Hostile,
        ]
    );
    for label in ExecutionLabel::ALL {
        assert_eq!(
            label.is_isolation(),
            label != ExecutionLabel::HostCompatibility
        );
        assert_eq!(label.trust_tier().is_some(), label.is_isolation());
    }
    for tier in TrustTier::ALL {
        assert_eq!(profile(tier).label(), ExecutionLabel::from(tier));
    }

    let compatibility = ExecutorProfile::new(ProfileSpec::host_compatibility(
        Platform::MacOs,
        Architecture::Aarch64,
        limits(),
        CompatibilityOptIn::trusted_local("legacy compiler requires host access").unwrap(),
    ))
    .unwrap();
    assert!(!compatibility.label().is_isolation());
    assert_eq!(
        compatibility.compatibility().unwrap().weaker_than(),
        TrustTier::TrustedLocal
    );
    assert!(matches!(
        ExecutorProfile::new(ProfileSpec {
            compatibility: None,
            ..ProfileSpec::host_compatibility(
                Platform::MacOs,
                Architecture::Aarch64,
                limits(),
                CompatibilityOptIn::trusted_local("explicit").unwrap(),
            )
        }),
        Err(ProfileError::MissingCompatibilityOptIn)
    ));
}

#[test]
fn every_label_and_platform_requires_each_mandatory_primitive_and_fails_closed() {
    for platform in [Platform::Linux, Platform::MacOs, Platform::Windows] {
        for label in ExecutionLabel::ALL {
            let profile = profile_for(label, platform);
            let expected = expected_requirements(label, platform);
            assert_eq!(
                profile.requirements(),
                &expected,
                "{label:?} on {platform:?}"
            );

            let selected = select_backend(
                &profile,
                [probe_with(&profile, "complete", expected.clone())],
            )
            .unwrap();
            assert_eq!(selected.label(), label);

            for missing in &expected {
                let mut incomplete = expected.clone();
                incomplete.remove(missing);
                for attempt in 0..100 {
                    let unavailable = BackendProbe {
                        backend: BackendId::new(format!("offline-{attempt}")).unwrap(),
                        status: ProbeStatus::Unavailable {
                            reason: "runtime absent".into(),
                        },
                        ..probe_with(&profile, "offline-template", expected.clone())
                    };
                    assert!(matches!(
                        select_backend(
                            &profile,
                            [
                                probe_with(
                                    &profile,
                                    format!("missing-{missing:?}-{attempt}"),
                                    incomplete.clone(),
                                ),
                                unavailable,
                            ],
                        ),
                        Err(BackendSelectionError::RequiredBackendUnavailable {
                            label: rejected_label,
                            ..
                        }) if rejected_label == label
                    ));
                }
            }
        }
    }
}

#[test]
fn profile_covers_mount_credentials_egress_repository_policy_and_finite_bounds() {
    let mut spec = ProfileSpec::isolated(
        TrustTier::Hostile,
        Platform::Linux,
        Architecture::Aarch64,
        limits(),
    );
    spec.source_write = SourceWriteMode::MutationOverlay;
    spec.mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Source)
        .unwrap()
        .access = MountAccess::CopyOnWrite;
    spec.credentials = vec![
        CredentialInjection {
            handle: CredentialHandle::new("registry-token").unwrap(),
            mode: CredentialInjectionMode::FileDescriptor,
        },
        CredentialInjection {
            handle: CredentialHandle::new("signing-key").unwrap(),
            mode: CredentialInjectionMode::MemoryFile,
        },
        CredentialInjection {
            handle: CredentialHandle::new("api-token").unwrap(),
            mode: CredentialInjectionMode::ScopedEnvironment {
                variable: "KIT_SCOPED_TOKEN".into(),
            },
        },
    ];
    spec.egress =
        BTreeSet::from([EgressGrant::new("crates.io", 443, EgressTransport::Tcp).unwrap()]);
    spec.repository.hooks = RepositoryCodePolicy::Sandboxed;
    spec.repository.submodules = RepositoryCodePolicy::Sandboxed;
    let profile = ExecutorProfile::new(spec).unwrap();

    assert!(profile.resources().finite());
    for primitive in [
        BackendPrimitive::TenantBoundary,
        BackendPrimitive::UserKernelOrVmTenantBoundary,
        BackendPrimitive::IsolatedStorage,
        BackendPrimitive::WholeProcessTreeControl,
        BackendPrimitive::SourceMutationOverlay,
        BackendPrimitive::CredentialFileDescriptor,
        BackendPrimitive::CredentialMemoryFile,
        BackendPrimitive::CredentialScopedEnvironment,
        BackendPrimitive::DestinationEgress,
        BackendPrimitive::RebindingSafeEgress,
        BackendPrimitive::CpuLimit,
        BackendPrimitive::MemoryLimit,
        BackendPrimitive::PidLimit,
        BackendPrimitive::FileSizeLimit,
        BackendPrimitive::DiskLimit,
        BackendPrimitive::IoLimit,
        BackendPrimitive::OutputLimit,
        BackendPrimitive::WallTimeLimit,
        BackendPrimitive::RepositoryCodeSandbox,
    ] {
        assert!(
            profile.requirements().contains(&primitive),
            "missing {primitive:?}"
        );
    }
}

#[test]
fn contradictory_and_unsafe_profiles_are_rejected() {
    let mut writable_root = ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        Platform::Linux,
        Architecture::X86_64,
        limits(),
    );
    writable_root
        .mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Root)
        .unwrap()
        .access = MountAccess::ReadWrite;
    assert!(matches!(
        ExecutorProfile::new(writable_root),
        Err(ProfileError::InvalidMountAccess {
            role: MountRole::Root,
            ..
        })
    ));

    let mut unbounded = ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        Architecture::X86_64,
        limits(),
    );
    unbounded.resources.output_bytes = 0;
    assert_eq!(
        ExecutorProfile::new(unbounded),
        Err(ProfileError::UnboundedResource("output"))
    );
    assert!(EgressGrant::new("*", 443, EgressTransport::Tcp).is_err());
}

#[test]
fn scoped_secrets_reject_loader_and_runtime_environment_names() {
    for variable in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "HOME",
        "PATH",
        "DOCKER_HOST",
        "HTTP_PROXY",
    ] {
        let mut spec = spec_for(ExecutionLabel::Restricted, Platform::Linux);
        spec.credentials.push(CredentialInjection {
            handle: CredentialHandle::new(format!("secret-{variable}")).unwrap(),
            mode: CredentialInjectionMode::ScopedEnvironment {
                variable: variable.to_owned(),
            },
        });
        assert!(matches!(
            ExecutorProfile::new(spec),
            Err(ProfileError::InvalidEnvironmentVariable(rejected)) if rejected == variable
        ));
    }

    let mut spec = spec_for(ExecutionLabel::Restricted, Platform::Linux);
    spec.credentials.push(CredentialInjection {
        handle: CredentialHandle::new("ordinary-secret").unwrap(),
        mode: CredentialInjectionMode::ScopedEnvironment {
            variable: "KIT_SCOPED_SECRET".to_owned(),
        },
    });
    ExecutorProfile::new(spec).unwrap();
}

#[test]
fn egress_only_accepts_public_addresses_and_requires_rebinding_safe_enforcement() {
    for destination in [
        "0.0.0.0",
        "10.0.0.1",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.168.0.1",
        "192.0.2.1",
        "198.18.0.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "fc00::1",
        "fe80::1",
        "ff02::1",
        "64:ff9b:1::1",
        "100::1",
        "2001::1",
        "2002::1",
        "2001:db8::1",
        "3fff::1",
        "5f00::1",
        "::ffff:10.0.0.1",
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "::10.0.0.1",
        "64:ff9b::a9fe:a9fe",
        "fec0::1",
    ] {
        assert!(
            matches!(
                EgressGrant::new(destination, 443, EgressTransport::Tcp),
                Err(ProfileError::UnsafeEgressDestination(rejected)) if rejected == destination
            ),
            "accepted unsafe destination {destination}"
        );
    }
    for destination in [
        "1.1.1.1",
        "8.8.8.8",
        "64:ff9b::808:808",
        "2001:3::1",
        "2001:20::1",
        "2606:4700:4700::1111",
        "crates.io",
    ] {
        assert!(
            EgressGrant::new(destination, 443, EgressTransport::Tcp).is_ok(),
            "rejected public destination {destination}"
        );
    }

    let mut spec = spec_for(ExecutionLabel::Restricted, Platform::Linux);
    spec.egress =
        BTreeSet::from([EgressGrant::new("crates.io", 443, EgressTransport::Tcp).unwrap()]);
    let profile = ExecutorProfile::new(spec).unwrap();
    assert!(
        profile
            .requirements()
            .contains(&BackendPrimitive::DestinationEgress)
    );
    assert!(
        profile
            .requirements()
            .contains(&BackendPrimitive::RebindingSafeEgress)
    );

    for missing in [
        BackendPrimitive::DestinationEgress,
        BackendPrimitive::RebindingSafeEgress,
    ] {
        let mut capabilities = profile.requirements().clone();
        capabilities.remove(&missing);
        assert!(matches!(
            select_backend(
                &profile,
                [probe_with(&profile, "unsafe-egress", capabilities)]
            ),
            Err(BackendSelectionError::RequiredBackendUnavailable { .. })
        ));
    }
}

#[test]
fn windows_virtual_mounts_reject_aliases_and_use_canonical_identity() {
    let mut lower_drive_root = spec_for(ExecutionLabel::Restricted, Platform::Windows);
    lower_drive_root
        .mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Root)
        .unwrap()
        .target = r"c:\".into();
    ExecutorProfile::new(lower_drive_root).unwrap();

    let mut duplicate = spec_for(ExecutionLabel::Restricted, Platform::Windows);
    duplicate
        .mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Build)
        .unwrap()
        .target = r"c:\WORKSPACE".into();
    assert!(matches!(
        ExecutorProfile::new(duplicate),
        Err(ProfileError::DuplicateMountTarget(_))
    ));

    let mut overlap = spec_for(ExecutionLabel::Restricted, Platform::Windows);
    overlap
        .mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Build)
        .unwrap()
        .target = r"c:\WORKSPACE\child".into();
    assert!(matches!(
        ExecutorProfile::new(overlap),
        Err(ProfileError::OverlappingMountTargets(_, _))
    ));

    for target in [
        r"C:/workspace",
        r"C:\workspace:stream",
        r"C:\workspace\\child",
        r"C:\workspace\.\child",
        r"C:\workspace\..\child",
        r"C:\workspace\child\",
        r"C:\workspace?",
        r"C:\CON",
        r"C:\safe\con.txt",
        r"C:\safe\CON .txt",
        r"C:\PRN",
        r"C:\AUX.log",
        r"C:\NUL",
        r"C:\CONIN$",
        r"C:\COM1",
        r"C:\COM¹.txt",
        r"C:\LPT9.txt",
        r"C:\workspace.",
        r"C:\workspace ",
        r"C:\WORKSP~1",
    ] {
        let mut spec = spec_for(ExecutionLabel::Restricted, Platform::Windows);
        spec.mounts
            .iter_mut()
            .find(|mount| mount.role == MountRole::Source)
            .unwrap()
            .target = target.into();
        assert!(
            matches!(
                ExecutorProfile::new(spec),
                Err(ProfileError::UnsafeMountTarget(_))
            ),
            "accepted unsafe Windows mount {target}"
        );
    }

    let mut mac_case_distinct = spec_for(ExecutionLabel::Restricted, Platform::MacOs);
    mac_case_distinct
        .mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Build)
        .unwrap()
        .target = "/WORKSPACE".into();
    let mac_case_distinct = ExecutorProfile::new(mac_case_distinct).unwrap();
    assert!(mac_case_distinct.mounts().iter().any(|mount| {
        mount.role == MountRole::Build
            && mount.target.as_path() == std::path::Path::new("/WORKSPACE")
    }));
}

#[test]
fn host_compatibility_canonicalizes_effective_host_policy() {
    let profile = profile_for(ExecutionLabel::HostCompatibility, Platform::Linux);
    assert_eq!(profile.schema_version(), 1);
    assert_eq!(profile.source_write(), SourceWriteMode::Direct);
    assert!(
        profile.mounts().iter().any(|mount| {
            mount.role == MountRole::Root && mount.access == MountAccess::ReadWrite
        })
    );
    assert_eq!(
        profile.repository().hooks,
        RepositoryCodePolicy::Unrestricted
    );

    let canonical: serde_json::Value = serde_json::from_slice(profile.canonical_bytes()).unwrap();
    assert_eq!(canonical["effective"]["filesystem"], "host_read_write");
    assert_eq!(canonical["effective"]["network"], "host");
    assert_eq!(canonical["effective"]["source_write"], "direct");
    assert_eq!(
        canonical["effective"]["resources"],
        serde_json::json!(["output_limit", "wall_time_limit"])
    );
    assert_eq!(
        canonical["effective"]["repository"]["hooks"],
        "unrestricted"
    );
}

#[test]
fn canonical_digest_is_order_independent_and_version_bound() {
    let mut first = ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        Architecture::X86_64,
        limits(),
    );
    first.mounts.reverse();
    first.credentials = vec![
        CredentialInjection {
            handle: CredentialHandle::new("z-token").unwrap(),
            mode: CredentialInjectionMode::FileDescriptor,
        },
        CredentialInjection {
            handle: CredentialHandle::new("a-token").unwrap(),
            mode: CredentialInjectionMode::MemoryFile,
        },
    ];
    let mut second = first.clone();
    second.mounts.reverse();
    second.credentials.reverse();

    let first = ExecutorProfile::new(first).unwrap();
    let second = ExecutorProfile::new(second).unwrap();
    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(first.digest(), second.digest());

    let mut windows_upper = spec_for(ExecutionLabel::Restricted, Platform::Windows);
    let mut windows_lower = windows_upper.clone();
    for mount in &mut windows_upper.mounts {
        mount.target = PathBuf::from(mount.target.to_string_lossy().to_ascii_uppercase());
    }
    for mount in &mut windows_lower.mounts {
        mount.target = PathBuf::from(mount.target.to_string_lossy().to_ascii_lowercase());
    }
    assert_eq!(
        ExecutorProfile::new(windows_upper).unwrap().digest(),
        ExecutorProfile::new(windows_lower).unwrap().digest()
    );
    assert_eq!(
        first.digest().to_string(),
        "blake3:6d1f19ec49a315b96c2e4760d2586b5620201e820db55ca1b7fc6d0bb9d2d0e9"
    );

    let mut unsupported = ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        Architecture::X86_64,
        limits(),
    );
    unsupported.schema_version += 1;
    assert!(matches!(
        ExecutorProfile::new(unsupported),
        Err(ProfileError::UnsupportedSchemaVersion(_))
    ));
}
