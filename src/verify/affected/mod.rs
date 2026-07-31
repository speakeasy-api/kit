use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::Deserialize;

use crate::{
    executor::check::safe_id,
    verify::profiles::{
        CheckClass, DeclaredCheck, MAX_CHECKS, ProfileSelection, VerificationRegistry,
    },
    workspace::edit::ir::RootRelativePath,
};

pub const AFFECTED_SELECTOR_VERSION: u16 = 1;
const MAX_PACKAGE_BYTES: usize = 256;
const HARD_MAX_MODEL_BYTES: usize = 64 * 1024;
const HARD_MAX_POLICY_CHECKS: usize = 256;
const HARD_MAX_REGISTRY_CHECKS: usize = 256;
const HARD_MAX_CHANGED_PATHS: usize = 4096;
const HARD_MAX_EVIDENCE_CHECKS: usize = 4096;
const HARD_MAX_PACKAGES: usize = 4096;
const HARD_MAX_PACKAGE_LINKS: usize = 4096;
const HARD_MAX_CHANGED_PATH_BYTES: usize = 16 * 1024 * 1024;
const HARD_MAX_PREFIX_BYTES: usize = 1024 * 1024;
const HARD_MAX_MATCH_WORK: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckSelectionPolicy {
    Critical,
    RequiredWhenAffected,
    Optional,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AffectedCheck {
    id: String,
    policy: CheckSelectionPolicy,
    packages: BTreeSet<String>,
}

impl AffectedCheck {
    pub fn new(
        id: impl Into<String>,
        policy: CheckSelectionPolicy,
        packages: BTreeSet<String>,
    ) -> Result<Self, AffectedError> {
        let id = id.into();
        if !safe_id(&id)
            || packages
                .iter()
                .any(|package| !valid_name(package, MAX_PACKAGE_BYTES))
        {
            return Err(AffectedError::InvalidPolicy);
        }
        Ok(Self {
            id,
            policy,
            packages,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SelectionReason {
    CriticalPolicy,
    Explicit,
    ChangedPath,
    Package,
    SymbolTest,
    BuildDependency,
    History,
    PriorFailure,
    Coverage,
    ModelProposal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRejection {
    TooLarge,
    Malformed,
    UnsupportedVersion,
    TooManyChecks,
    InvalidCheckId,
    UnknownCheck,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelProposalDisposition {
    Absent,
    Accepted,
    Rejected(ModelRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AffectedLimits {
    pub max_checks: usize,
    pub max_model_bytes: usize,
    pub max_policy_checks: usize,
    pub max_changed_paths: usize,
    pub max_evidence_checks: usize,
    pub max_match_work: usize,
}

impl Default for AffectedLimits {
    fn default() -> Self {
        Self {
            max_checks: MAX_CHECKS,
            max_model_bytes: 16 * 1024,
            max_policy_checks: 256,
            max_changed_paths: 4096,
            max_evidence_checks: 4096,
            max_match_work: 1_000_000,
        }
    }
}

pub struct AffectedInput<'a> {
    pub changed_paths: &'a BTreeSet<RootRelativePath>,
    pub changed_packages: &'a BTreeSet<String>,
    pub symbol_tests: &'a BTreeSet<String>,
    pub build_dependents: &'a BTreeSet<String>,
    pub historical_checks: &'a BTreeSet<String>,
    pub prior_failure_checks: &'a BTreeSet<String>,
    pub coverage_checks: &'a BTreeSet<String>,
    pub explicit_checks: &'a BTreeSet<String>,
    pub model_proposal: Option<&'a [u8]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AffectedSelection {
    exact_targets: BTreeSet<String>,
    protected_floor: BTreeSet<String>,
    reasons: BTreeMap<String, BTreeSet<SelectionReason>>,
    model_disposition: ModelProposalDisposition,
}

impl AffectedSelection {
    pub fn exact_targets(&self) -> &BTreeSet<String> {
        &self.exact_targets
    }

    pub fn protected_floor(&self) -> &BTreeSet<String> {
        &self.protected_floor
    }

    pub fn reasons(&self) -> &BTreeMap<String, BTreeSet<SelectionReason>> {
        &self.reasons
    }

    pub const fn model_disposition(&self) -> ModelProposalDisposition {
        self.model_disposition
    }

    pub fn into_profile_selection(self) -> Option<ProfileSelection> {
        (!self.exact_targets.is_empty()).then_some(ProfileSelection::Targeted {
            exact_targets: self.exact_targets,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AffectedError {
    InvalidLimits,
    InvalidPolicy,
    DuplicateCheck,
    UnknownEvidenceCheck,
    ProtectedFloorExceedsLimit,
    SelectionExceedsLimit,
}

impl fmt::Display for AffectedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "affected-check limits are invalid",
            Self::InvalidPolicy => "affected-check policy is invalid",
            Self::DuplicateCheck => "affected-check policy contains a duplicate check",
            Self::UnknownEvidenceCheck => "affected-check evidence names an unknown check",
            Self::ProtectedFloorExceedsLimit => "affected-check protected floor exceeds the limit",
            Self::SelectionExceedsLimit => "affected-check selection exceeds the limit",
        })
    }
}

impl std::error::Error for AffectedError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelProposal {
    version: u16,
    select: Vec<String>,
}

pub fn select_affected(
    registry: &VerificationRegistry,
    checks: &[AffectedCheck],
    input: AffectedInput<'_>,
    limits: AffectedLimits,
) -> Result<AffectedSelection, AffectedError> {
    if limits.max_checks == 0
        || limits.max_checks > MAX_CHECKS
        || limits.max_model_bytes == 0
        || limits.max_model_bytes > HARD_MAX_MODEL_BYTES
        || limits.max_policy_checks == 0
        || limits.max_policy_checks > HARD_MAX_POLICY_CHECKS
        || limits.max_changed_paths == 0
        || limits.max_changed_paths > HARD_MAX_CHANGED_PATHS
        || limits.max_evidence_checks == 0
        || limits.max_evidence_checks > HARD_MAX_EVIDENCE_CHECKS
        || limits.max_match_work == 0
        || limits.max_match_work > HARD_MAX_MATCH_WORK
    {
        return Err(AffectedError::InvalidLimits);
    }
    if checks.len() > limits.max_policy_checks
        || input.changed_paths.len() > limits.max_changed_paths
        || registry.checks().len() > HARD_MAX_REGISTRY_CHECKS
        || input.changed_packages.len() > HARD_MAX_PACKAGES
    {
        return Err(AffectedError::SelectionExceedsLimit);
    }
    let evidence_count = [
        input.symbol_tests.len(),
        input.build_dependents.len(),
        input.historical_checks.len(),
        input.prior_failure_checks.len(),
        input.coverage_checks.len(),
        input.explicit_checks.len(),
    ]
    .into_iter()
    .try_fold(0_usize, usize::checked_add)
    .ok_or(AffectedError::SelectionExceedsLimit)?;
    if evidence_count > limits.max_evidence_checks {
        return Err(AffectedError::SelectionExceedsLimit);
    }
    let changed_path_bytes = input
        .changed_paths
        .iter()
        .try_fold(0_usize, |total, path| {
            total.checked_add(path.as_str().len())
        })
        .ok_or(AffectedError::SelectionExceedsLimit)?;
    let package_links = checks
        .iter()
        .try_fold(0_usize, |total, check| {
            total.checked_add(check.packages.len())
        })
        .ok_or(AffectedError::SelectionExceedsLimit)?;
    if changed_path_bytes > HARD_MAX_CHANGED_PATH_BYTES || package_links > HARD_MAX_PACKAGE_LINKS {
        return Err(AffectedError::SelectionExceedsLimit);
    }

    let targeted = registry
        .checks()
        .iter()
        .filter(|check| check.class() == CheckClass::Targeted)
        .map(|check| (check.command().id(), check))
        .collect::<BTreeMap<_, _>>();
    if targeted.len() != checks.len() {
        return Err(AffectedError::InvalidPolicy);
    }
    let mut declared = BTreeMap::<&str, (&AffectedCheck, &DeclaredCheck)>::new();
    for check in checks {
        let declaration = targeted
            .get(check.id.as_str())
            .ok_or(AffectedError::InvalidPolicy)?;
        if declared
            .insert(check.id.as_str(), (check, *declaration))
            .is_some()
        {
            return Err(AffectedError::DuplicateCheck);
        }
    }
    if input
        .changed_packages
        .iter()
        .any(|package| !valid_name(package, MAX_PACKAGE_BYTES))
    {
        return Err(AffectedError::InvalidPolicy);
    }
    let changed_paths = input
        .changed_paths
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let prefix_count = declared
        .values()
        .try_fold(0_usize, |total, (_, check)| {
            total.checked_add(check.changed_path_prefix_count())
        })
        .ok_or(AffectedError::SelectionExceedsLimit)?;
    let prefix_bytes = declared
        .values()
        .try_fold(0_usize, |total, (_, check)| {
            total.checked_add(check.changed_path_prefix_bytes())
        })
        .ok_or(AffectedError::SelectionExceedsLimit)?;
    let match_work = prefix_count
        .saturating_mul(changed_paths.len())
        .saturating_add(package_links.saturating_mul(input.changed_packages.len()));
    if prefix_bytes > HARD_MAX_PREFIX_BYTES || match_work > limits.max_match_work {
        return Err(AffectedError::SelectionExceedsLimit);
    }

    let mut candidates = BTreeSet::new();
    let mut floor = BTreeSet::new();
    let mut reasons = BTreeMap::<String, BTreeSet<SelectionReason>>::new();
    for (check, declaration) in declared.values().copied() {
        if check.policy == CheckSelectionPolicy::Critical {
            add(
                check,
                SelectionReason::CriticalPolicy,
                true,
                &mut candidates,
                &mut floor,
                &mut reasons,
            );
        }
        if declaration.affected(&changed_paths) {
            add_affected(
                check,
                SelectionReason::ChangedPath,
                &mut candidates,
                &mut floor,
                &mut reasons,
            );
        }
        if !check.packages.is_disjoint(input.changed_packages) {
            add_affected(
                check,
                SelectionReason::Package,
                &mut candidates,
                &mut floor,
                &mut reasons,
            );
        }
    }

    for (ids, reason) in [
        (input.symbol_tests, SelectionReason::SymbolTest),
        (input.build_dependents, SelectionReason::BuildDependency),
        (input.historical_checks, SelectionReason::History),
        (input.prior_failure_checks, SelectionReason::PriorFailure),
        (input.coverage_checks, SelectionReason::Coverage),
    ] {
        add_evidence(
            ids,
            reason,
            &declared,
            &mut candidates,
            &mut floor,
            &mut reasons,
        )?;
    }
    for id in input.explicit_checks {
        if !safe_id(id) {
            return Err(AffectedError::UnknownEvidenceCheck);
        }
        let (check, _) = declared
            .get(id.as_str())
            .ok_or(AffectedError::UnknownEvidenceCheck)?;
        add(
            check,
            SelectionReason::Explicit,
            true,
            &mut candidates,
            &mut floor,
            &mut reasons,
        );
    }

    if floor.len() > limits.max_checks {
        return Err(AffectedError::ProtectedFloorExceedsLimit);
    }

    let (proposal, mut disposition) = decode_model(input.model_proposal, &declared, limits);
    let selected = match proposal {
        Some(ids) => {
            let mut selected = floor.clone();
            selected.extend(ids.iter().cloned());
            if selected.len() > limits.max_checks {
                disposition = ModelProposalDisposition::Rejected(ModelRejection::TooManyChecks);
                candidates
            } else {
                for id in ids {
                    reasons
                        .entry(id)
                        .or_default()
                        .insert(SelectionReason::ModelProposal);
                }
                selected
            }
        }
        None => candidates,
    };
    if selected.len() > limits.max_checks {
        return Err(AffectedError::SelectionExceedsLimit);
    }
    reasons.retain(|id, _| selected.contains(id));

    Ok(AffectedSelection {
        exact_targets: selected,
        protected_floor: floor,
        reasons,
        model_disposition: disposition,
    })
}

fn add_evidence(
    ids: &BTreeSet<String>,
    reason: SelectionReason,
    declared: &BTreeMap<&str, (&AffectedCheck, &DeclaredCheck)>,
    candidates: &mut BTreeSet<String>,
    floor: &mut BTreeSet<String>,
    reasons: &mut BTreeMap<String, BTreeSet<SelectionReason>>,
) -> Result<(), AffectedError> {
    for id in ids {
        if !safe_id(id) {
            return Err(AffectedError::UnknownEvidenceCheck);
        }
        let (check, _) = declared
            .get(id.as_str())
            .ok_or(AffectedError::UnknownEvidenceCheck)?;
        add_affected(check, reason, candidates, floor, reasons);
    }
    Ok(())
}

fn add_affected(
    check: &AffectedCheck,
    reason: SelectionReason,
    candidates: &mut BTreeSet<String>,
    floor: &mut BTreeSet<String>,
    reasons: &mut BTreeMap<String, BTreeSet<SelectionReason>>,
) {
    add(
        check,
        reason,
        check.policy != CheckSelectionPolicy::Optional,
        candidates,
        floor,
        reasons,
    );
}

fn add(
    check: &AffectedCheck,
    reason: SelectionReason,
    protect: bool,
    candidates: &mut BTreeSet<String>,
    floor: &mut BTreeSet<String>,
    reasons: &mut BTreeMap<String, BTreeSet<SelectionReason>>,
) {
    candidates.insert(check.id.clone());
    if protect {
        floor.insert(check.id.clone());
    }
    reasons.entry(check.id.clone()).or_default().insert(reason);
}

fn decode_model(
    bytes: Option<&[u8]>,
    declared: &BTreeMap<&str, (&AffectedCheck, &DeclaredCheck)>,
    limits: AffectedLimits,
) -> (Option<BTreeSet<String>>, ModelProposalDisposition) {
    let Some(bytes) = bytes else {
        return (None, ModelProposalDisposition::Absent);
    };
    if bytes.len() > limits.max_model_bytes {
        return (
            None,
            ModelProposalDisposition::Rejected(ModelRejection::TooLarge),
        );
    }
    let Ok(proposal) = serde_json::from_slice::<ModelProposal>(bytes) else {
        return (
            None,
            ModelProposalDisposition::Rejected(ModelRejection::Malformed),
        );
    };
    if proposal.version != AFFECTED_SELECTOR_VERSION {
        return (
            None,
            ModelProposalDisposition::Rejected(ModelRejection::UnsupportedVersion),
        );
    }
    if proposal.select.len() > limits.max_checks {
        return (
            None,
            ModelProposalDisposition::Rejected(ModelRejection::TooManyChecks),
        );
    }
    let mut selected = BTreeSet::new();
    for id in proposal.select {
        if !safe_id(&id) {
            return (
                None,
                ModelProposalDisposition::Rejected(ModelRejection::InvalidCheckId),
            );
        }
        if !declared.contains_key(id.as_str()) {
            return (
                None,
                ModelProposalDisposition::Rejected(ModelRejection::UnknownCheck),
            );
        }
        selected.insert(id);
    }
    (Some(selected), ModelProposalDisposition::Accepted)
}

fn valid_name(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
        && !value.chars().any(char::is_whitespace)
}
