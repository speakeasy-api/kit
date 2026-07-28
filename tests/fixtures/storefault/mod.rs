use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StorePoint {
    BeforeWalCommit,
    AfterWalCommit,
    BeforeProjectionUpdate,
    AfterUploadConfirm,
    BeforeHashVerification,
    BackupRead,
    CommitSerialization,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreFault {
    Crash,
    CorruptBytes,
    WithholdBytes,
    Partition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledFault {
    pub point: StorePoint,
    pub occurrence: u32,
    pub fault: StoreFault,
}

#[derive(Clone, Debug)]
pub struct StoreFaultSchedule {
    pub seed: u64,
    faults: Vec<ScheduledFault>,
    visits: BTreeMap<StorePoint, u32>,
}

impl StoreFaultSchedule {
    pub fn new(seed: u64, faults: Vec<ScheduledFault>) -> Self {
        Self {
            seed,
            faults,
            visits: BTreeMap::new(),
        }
    }

    pub fn at(&mut self, point: StorePoint) -> Option<StoreFault> {
        let occurrence = self.visits.entry(point).or_default();
        *occurrence += 1;
        self.faults
            .iter()
            .find(|fault| fault.point == point && fault.occurrence == *occurrence)
            .map(|fault| fault.fault.clone())
    }

    pub fn apply_to_bytes(&self, fault: &StoreFault, bytes: &[u8]) -> Option<Vec<u8>> {
        match fault {
            StoreFault::WithholdBytes => None,
            StoreFault::CorruptBytes => {
                let mut corrupted = bytes.to_vec();
                if !corrupted.is_empty() {
                    let index = (self.seed as usize) % corrupted.len();
                    corrupted[index] ^= 0x80;
                }
                Some(corrupted)
            }
            StoreFault::Crash | StoreFault::Partition => Some(bytes.to_vec()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Committed,
    Faulted(StoreFault),
}

#[derive(Clone, Debug)]
pub struct StoreHarness {
    pub wal: Vec<String>,
    pub projection: Vec<String>,
    pub schedule: StoreFaultSchedule,
}

impl StoreHarness {
    pub fn new(seed: u64, faults: Vec<ScheduledFault>) -> Self {
        Self {
            wal: Vec::new(),
            projection: Vec::new(),
            schedule: StoreFaultSchedule::new(seed, faults),
        }
    }

    pub fn append(&mut self, event: &str) -> AppendOutcome {
        if let Some(fault) = self.schedule.at(StorePoint::BeforeWalCommit) {
            return AppendOutcome::Faulted(fault);
        }
        self.wal.push(event.to_owned());
        if let Some(fault) = self.schedule.at(StorePoint::AfterWalCommit) {
            return AppendOutcome::Faulted(fault);
        }
        if let Some(fault) = self.schedule.at(StorePoint::BeforeProjectionUpdate) {
            return AppendOutcome::Faulted(fault);
        }
        self.projection.push(event.to_owned());
        AppendOutcome::Committed
    }

    pub fn recover_projection(&mut self) {
        self.projection.clone_from(&self.wal);
    }
}
