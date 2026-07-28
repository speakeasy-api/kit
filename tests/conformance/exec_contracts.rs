use std::collections::{BTreeSet, HashSet};

use kit::executor::overlay::{
    ChangeKind, DeclaredChange, MutationFence, MutationOverlay, OVERLAY_CONTRACT_VERSION,
    OVERLAY_SCHEMA_VERSION, OverlayContract, OverlayContractError, OverlayDisposition, OverlaySpec,
    OverlayTransitionError, SourceAccess, WritableLayerMode,
};
use kit::executor::vm_iface::{
    IsolatedVm, SecretHandle, VM_CONTRACT_VERSION, VM_SCHEMA_VERSION, VmCompletion,
    VmContractError, VmFence, VmNetworkPolicy, VmOutcome, VmOutcomeAttestation, VmResourceProfile,
    VmRunContract, VmRunSpec, VmStorageMode, VmTransitionError,
};

const BLAKE3_A: &str = "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BLAKE3_B: &str = "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA256_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA256_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn overlay_contract(layer: &str, fence: u64) -> OverlayContract {
    OverlayContract::new(OverlaySpec {
        schema_version: OVERLAY_SCHEMA_VERSION,
        contract_version: OVERLAY_CONTRACT_VERSION,
        overlay_id: format!("overlay-{fence}"),
        base_revision: 41,
        base_digest: BLAKE3_A.into(),
        source_access: SourceAccess::ReadOnly,
        writable_layer_id: layer.into(),
        writable_layer_mode: WritableLayerMode::CopyOnWrite,
        mutation_lock_id: "workspace-main".into(),
        fence: MutationFence::new(fence).unwrap(),
        declared_diff: BTreeSet::from([
            DeclaredChange {
                path: "src/a.rs".into(),
                kind: ChangeKind::Modify,
                base_digest: Some(BLAKE3_A.into()),
                result_digest: Some(BLAKE3_B.into()),
            },
            DeclaredChange {
                path: "src/b.rs".into(),
                kind: ChangeKind::Add,
                base_digest: None,
                result_digest: Some(BLAKE3_B.into()),
            },
        ]),
    })
    .unwrap()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverlayState {
    Active,
    Finalized(OverlayDisposition),
    Quiescent(OverlayDisposition),
}

#[derive(Clone, Default)]
struct FakeOverlay {
    current: Option<(OverlayContract, OverlayState)>,
    used_layers: HashSet<String>,
    greatest_fence: u64,
}

impl FakeOverlay {
    fn snapshot(&self) -> Self {
        self.clone()
    }

    fn restore(snapshot: Self) -> Self {
        snapshot
    }

    fn current_mut(
        &mut self,
        fence: MutationFence,
    ) -> Result<&mut (OverlayContract, OverlayState), OverlayTransitionError> {
        let current = self
            .current
            .as_mut()
            .ok_or(OverlayTransitionError::NotActive)?;
        if current.0.spec().fence != fence {
            return Err(OverlayTransitionError::StaleFence);
        }
        Ok(current)
    }

    fn finalize(
        &mut self,
        fence: MutationFence,
        disposition: OverlayDisposition,
    ) -> Result<(), OverlayTransitionError> {
        let current = self.current_mut(fence)?;
        match current.1 {
            OverlayState::Active => {
                current.1 = OverlayState::Finalized(disposition);
                Ok(())
            }
            OverlayState::Finalized(existing) => {
                Err(OverlayTransitionError::AlreadyFinalized(existing))
            }
            OverlayState::Quiescent(existing) => {
                Err(OverlayTransitionError::AlreadyFinalized(existing))
            }
        }
    }
}

impl MutationOverlay for FakeOverlay {
    fn start(&mut self, contract: OverlayContract) -> Result<(), OverlayTransitionError> {
        if self
            .current
            .as_ref()
            .is_some_and(|(_, state)| !matches!(state, OverlayState::Quiescent(_)))
        {
            return Err(OverlayTransitionError::NotQuiescent);
        }
        if contract.spec().fence.get() <= self.greatest_fence {
            return Err(OverlayTransitionError::StaleFence);
        }
        if !self
            .used_layers
            .insert(contract.spec().writable_layer_id.clone())
        {
            return Err(OverlayTransitionError::WritableLayerReused);
        }
        self.greatest_fence = contract.spec().fence.get();
        self.current = Some((contract, OverlayState::Active));
        Ok(())
    }

    fn promote(&mut self, fence: MutationFence) -> Result<(), OverlayTransitionError> {
        self.finalize(fence, OverlayDisposition::Promoted)
    }

    fn discard(&mut self, fence: MutationFence) -> Result<(), OverlayTransitionError> {
        self.finalize(fence, OverlayDisposition::Discarded)
    }

    fn attest_quiescence(&mut self, fence: MutationFence) -> Result<(), OverlayTransitionError> {
        let current = self.current_mut(fence)?;
        match current.1 {
            OverlayState::Finalized(disposition) => {
                current.1 = OverlayState::Quiescent(disposition);
                Ok(())
            }
            OverlayState::Active => Err(OverlayTransitionError::NotFinalized),
            OverlayState::Quiescent(_) => Ok(()),
        }
    }
}

fn vm_contract(run: u64, instance: &str, rootfs: &str, fence: u64) -> VmRunContract {
    VmRunContract::new(VmRunSpec {
        schema_version: VM_SCHEMA_VERSION,
        contract_version: VM_CONTRACT_VERSION,
        run_id: format!("run-{run}"),
        fence: VmFence::new(fence).unwrap(),
        image_digest: SHA256_A.into(),
        instance_id: instance.into(),
        rootfs_layer_id: rootfs.into(),
        storage_mode: VmStorageMode::CopyOnWrite,
        network: VmNetworkPolicy::Deny,
        default_grants: BTreeSet::new(),
        secret_handles: BTreeSet::from([
            SecretHandle::new("secret/api-token").unwrap(),
            SecretHandle::new("secret/git-token").unwrap(),
        ]),
        resources: VmResourceProfile {
            cpu_millis: 1_000,
            memory_bytes: 256 << 20,
            disk_bytes: 1 << 30,
            pids: 64,
            wall_time_millis: 30_000,
        },
    })
    .unwrap()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VmState {
    Running,
    Terminated(VmOutcome),
    Quiescent(VmOutcome),
    Attested(VmOutcome),
}

#[derive(Default)]
struct FakeVm {
    current: Option<(VmRunContract, VmState)>,
    instances: HashSet<String>,
    rootfs_layers: HashSet<String>,
    greatest_fence: u64,
}

impl FakeVm {
    fn current_mut(
        &mut self,
        fence: VmFence,
    ) -> Result<&mut (VmRunContract, VmState), VmTransitionError> {
        let current = self.current.as_mut().ok_or(VmTransitionError::NotRunning)?;
        if current.0.spec().fence != fence {
            return Err(VmTransitionError::StaleFence);
        }
        Ok(current)
    }
}

impl IsolatedVm for FakeVm {
    fn start(&mut self, contract: VmRunContract) -> Result<(), VmTransitionError> {
        if self
            .current
            .as_ref()
            .is_some_and(|(_, state)| !matches!(state, VmState::Attested(_)))
        {
            return Err(VmTransitionError::NotQuiescent);
        }
        if contract.spec().fence.get() <= self.greatest_fence {
            return Err(VmTransitionError::StaleFence);
        }
        if self.instances.contains(&contract.spec().instance_id) {
            return Err(VmTransitionError::InstanceReused);
        }
        if self
            .rootfs_layers
            .contains(&contract.spec().rootfs_layer_id)
        {
            return Err(VmTransitionError::RootfsReused);
        }
        self.instances.insert(contract.spec().instance_id.clone());
        self.rootfs_layers
            .insert(contract.spec().rootfs_layer_id.clone());
        self.greatest_fence = contract.spec().fence.get();
        self.current = Some((contract, VmState::Running));
        Ok(())
    }

    fn complete(
        &mut self,
        fence: VmFence,
        completion: VmCompletion,
    ) -> Result<(), VmTransitionError> {
        let current = self.current_mut(fence)?;
        if current.1 != VmState::Running {
            return Err(VmTransitionError::NotRunning);
        }
        current.1 = VmState::Terminated(completion.into());
        Ok(())
    }

    fn kill(&mut self, fence: VmFence) -> Result<(), VmTransitionError> {
        let current = self.current_mut(fence)?;
        if current.1 != VmState::Running {
            return Err(VmTransitionError::NotRunning);
        }
        current.1 = VmState::Terminated(VmOutcome::Killed);
        Ok(())
    }

    fn attest_quiescence(&mut self, fence: VmFence) -> Result<(), VmTransitionError> {
        let current = self.current_mut(fence)?;
        match current.1 {
            VmState::Terminated(outcome) => {
                current.1 = VmState::Quiescent(outcome);
                Ok(())
            }
            _ => Err(VmTransitionError::NotTerminated),
        }
    }

    fn attest_outcome(
        &mut self,
        fence: VmFence,
        attestation: VmOutcomeAttestation,
    ) -> Result<(), VmTransitionError> {
        let current = self.current_mut(fence)?;
        match current.1 {
            VmState::Quiescent(outcome) if attestation.validates_outcome(&current.0, outcome) => {
                current.1 = VmState::Attested(outcome);
                Ok(())
            }
            VmState::Attested(_) => Err(VmTransitionError::OutcomeAlreadyAttested),
            VmState::Quiescent(outcome)
                if attestation.validates(&current.0) && attestation.outcome != outcome =>
            {
                Err(VmTransitionError::OutcomeMismatch)
            }
            VmState::Quiescent(_) => Err(VmTransitionError::InvalidAttestation),
            _ => Err(VmTransitionError::NotQuiescent),
        }
    }
}

#[test]
fn exec_contracts_overlay_schema_is_canonical_versioned_and_fail_closed() {
    let contract = overlay_contract("layer-a", 7);
    assert_eq!(contract.digest().as_bytes().len(), 32);
    assert_eq!(
        OverlayContract::from_canonical_bytes(contract.canonical_bytes())
            .unwrap()
            .digest(),
        contract.digest()
    );
    let mut noncanonical = contract.canonical_bytes().to_vec();
    noncanonical.push(b'\n');
    assert_eq!(
        OverlayContract::from_canonical_bytes(&noncanonical),
        Err(OverlayContractError::NonCanonicalEncoding)
    );

    for field in ["schema_version", "contract_version"] {
        let mut value: serde_json::Value =
            serde_json::from_slice(contract.canonical_bytes()).unwrap();
        value[field] = serde_json::json!(2);
        let error = OverlayContract::from_canonical_bytes(&serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(matches!(
            (field, error),
            (
                "schema_version",
                OverlayContractError::UnsupportedSchemaVersion(2)
            ) | (
                "contract_version",
                OverlayContractError::UnsupportedContractVersion(2)
            )
        ));
    }
}

#[test]
fn exec_contracts_overlay_paths_are_cross_platform_canonical() {
    for path in [
        "/absolute",
        "C:/drive",
        "a\\b",
        "a//b",
        "a/./b",
        "a/../b",
        "a/control\0",
        "src/café.rs",
        "CON",
        "aux.txt",
        "dir/COM1.log",
        "dir/LPT9",
        "trailing.",
        "trailing ",
    ] {
        let mut spec = overlay_contract("path-layer", 70).spec().clone();
        spec.declared_diff = BTreeSet::from([DeclaredChange {
            path: path.into(),
            kind: ChangeKind::Add,
            base_digest: None,
            result_digest: Some(BLAKE3_B.into()),
        }]);
        assert!(matches!(
            OverlayContract::new(spec),
            Err(OverlayContractError::InvalidPath(rejected)) if rejected == path
        ));
    }
}

#[test]
fn exec_contracts_overlay_fake_fences_finalization_and_reuse() {
    let mut fake = FakeOverlay::default();
    fake.start(overlay_contract("layer-a", 7)).unwrap();
    assert_eq!(
        fake.promote(MutationFence::new(6).unwrap()),
        Err(OverlayTransitionError::StaleFence)
    );
    fake.promote(MutationFence::new(7).unwrap()).unwrap();
    assert_eq!(
        fake.discard(MutationFence::new(7).unwrap()),
        Err(OverlayTransitionError::AlreadyFinalized(
            OverlayDisposition::Promoted
        ))
    );
    assert_eq!(
        fake.start(overlay_contract("layer-b", 8)),
        Err(OverlayTransitionError::NotQuiescent)
    );
    fake.attest_quiescence(MutationFence::new(7).unwrap())
        .unwrap();
    let mut restarted = FakeOverlay::restore(fake.snapshot());
    assert_eq!(
        restarted.discard(MutationFence::new(7).unwrap()),
        Err(OverlayTransitionError::AlreadyFinalized(
            OverlayDisposition::Promoted
        ))
    );
    assert_eq!(
        fake.start(overlay_contract("layer-a", 8)),
        Err(OverlayTransitionError::WritableLayerReused)
    );
    fake.start(overlay_contract("layer-b", 8)).unwrap();
}

#[test]
fn exec_contracts_vm_schema_is_canonical_versioned_and_has_no_ambient_authority() {
    let contract = vm_contract(1, "instance-a", "rootfs-a", 10);
    assert_eq!(contract.digest().as_bytes().len(), 32);
    assert_eq!(
        VmRunContract::from_canonical_bytes(contract.canonical_bytes())
            .unwrap()
            .digest(),
        contract.digest()
    );
    let mut unsupported = contract.spec().clone();
    unsupported.contract_version = 2;
    assert_eq!(
        VmRunContract::new(unsupported),
        Err(VmContractError::UnsupportedContractVersion(2))
    );
    let mut unsupported = contract.spec().clone();
    unsupported.schema_version = 2;
    assert_eq!(
        VmRunContract::new(unsupported),
        Err(VmContractError::UnsupportedSchemaVersion(2))
    );
    let mut granted = contract.spec().clone();
    granted.default_grants.insert("host-filesystem".into());
    assert_eq!(
        VmRunContract::new(granted),
        Err(VmContractError::DefaultGrantForbidden)
    );
    assert!(!String::from_utf8_lossy(contract.canonical_bytes()).contains("secret-value"));
}

#[test]
fn exec_contracts_vm_fake_requires_termination_quiescence_exact_attestation_and_freshness() {
    let mut fake = FakeVm::default();
    let first = vm_contract(1, "instance-a", "rootfs-a", 10);
    fake.start(first.clone()).unwrap();
    assert_eq!(
        fake.kill(VmFence::new(9).unwrap()),
        Err(VmTransitionError::StaleFence)
    );
    assert_eq!(
        fake.attest_quiescence(VmFence::new(10).unwrap()),
        Err(VmTransitionError::NotTerminated)
    );
    fake.kill(VmFence::new(10).unwrap()).unwrap();
    assert_eq!(
        fake.complete(VmFence::new(10).unwrap(), VmCompletion::Exit(0)),
        Err(VmTransitionError::NotRunning)
    );
    assert_eq!(
        fake.start(vm_contract(2, "instance-b", "rootfs-b", 11)),
        Err(VmTransitionError::NotQuiescent)
    );
    fake.attest_quiescence(VmFence::new(10).unwrap()).unwrap();
    let wrong_attestation =
        VmOutcomeAttestation::new(&first, VmOutcome::Exit(0), SHA256_B).unwrap();
    assert_eq!(
        fake.attest_outcome(VmFence::new(10).unwrap(), wrong_attestation),
        Err(VmTransitionError::OutcomeMismatch)
    );
    let attestation = VmOutcomeAttestation::new(&first, VmOutcome::Killed, SHA256_B).unwrap();
    fake.attest_outcome(VmFence::new(10).unwrap(), attestation)
        .unwrap();
    assert_eq!(
        fake.start(vm_contract(2, "instance-a", "rootfs-b", 11)),
        Err(VmTransitionError::InstanceReused)
    );
    assert_eq!(
        fake.start(vm_contract(2, "instance-b", "rootfs-a", 11)),
        Err(VmTransitionError::RootfsReused)
    );
    fake.start(vm_contract(2, "instance-b", "rootfs-b", 11))
        .unwrap();
    let second = fake.current.as_ref().unwrap().0.clone();
    fake.complete(VmFence::new(11).unwrap(), VmCompletion::Exit(17))
        .unwrap();
    fake.attest_quiescence(VmFence::new(11).unwrap()).unwrap();
    fake.attest_outcome(
        VmFence::new(11).unwrap(),
        VmOutcomeAttestation::new(&second, VmOutcome::Exit(17), SHA256_B).unwrap(),
    )
    .unwrap();
}
