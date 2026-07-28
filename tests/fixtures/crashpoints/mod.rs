use std::collections::BTreeMap;

pub const AFTER_WAL_COMMIT: &str = "store.after_wal_commit";
pub const BEFORE_PROJECTION_UPDATE: &str = "store.before_projection_update";
pub const ISOLATION_BACKEND_UNAVAILABLE: &str = "executor.isolation_backend_unavailable";
pub const ACP_CHILD_MID_TOOL_CALL: &str = "acp.child_mid_tool_call";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrashAction {
    Terminate,
    ReturnUnavailable,
    Disconnect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashTrigger {
    pub name: String,
    pub occurrence: u32,
    pub action: CrashAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectedCrash {
    pub seed: u64,
    pub name: String,
    pub occurrence: u32,
    pub action: CrashAction,
    pub fingerprint: u64,
}

#[derive(Clone, Debug)]
pub struct CrashSchedule {
    seed: u64,
    triggers: Vec<CrashTrigger>,
    visits: BTreeMap<String, u32>,
}

impl CrashSchedule {
    pub fn new(seed: u64, triggers: Vec<CrashTrigger>) -> Self {
        Self {
            seed,
            triggers,
            visits: BTreeMap::new(),
        }
    }

    pub fn hit(&mut self, name: &str) -> Option<InjectedCrash> {
        let occurrence = self.visits.entry(name.to_owned()).or_default();
        *occurrence += 1;
        self.triggers
            .iter()
            .find(|trigger| trigger.name == name && trigger.occurrence == *occurrence)
            .map(|trigger| InjectedCrash {
                seed: self.seed,
                name: name.to_owned(),
                occurrence: *occurrence,
                action: trigger.action.clone(),
                fingerprint: self.seed.rotate_left(19) ^ (*occurrence as u64),
            })
    }
}
