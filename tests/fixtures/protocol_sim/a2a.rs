use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMessage {
    pub remote_id: String,
    pub sequence: u64,
    pub digest: String,
    pub delegation_path: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageDecision {
    Dispatched { trace_id: u64 },
    DuplicateDropped,
    DelegationRejected,
}

#[derive(Clone, Debug)]
pub struct A2aSimulator {
    seed: u64,
    max_depth: usize,
}

impl A2aSimulator {
    pub fn new(seed: u64, max_depth: usize) -> Self {
        Self { seed, max_depth }
    }

    pub fn replay(&self, messages: &[RemoteMessage]) -> Vec<MessageDecision> {
        let mut seen = BTreeSet::new();
        messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let path = message.delegation_path.iter().collect::<BTreeSet<_>>();
                if message.delegation_path.len() > self.max_depth
                    || path.len() != message.delegation_path.len()
                {
                    MessageDecision::DelegationRejected
                } else if !seen.insert((message.remote_id.clone(), message.digest.clone())) {
                    MessageDecision::DuplicateDropped
                } else {
                    MessageDecision::Dispatched {
                        trace_id: self.seed.rotate_left(31) ^ index as u64,
                    }
                }
            })
            .collect()
    }
}
