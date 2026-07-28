#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualClock {
    pub seed: u64,
    now_ms: u64,
}

impl ManualClock {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            now_ms: 1_700_000_000_000 + seed % 1_000_000,
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub fn advance_ms(&mut self, delta: u64) {
        self.now_ms = self
            .now_ms
            .checked_add(delta)
            .expect("fixture clock overflow");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub owner: String,
    pub fence: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseController {
    pub clock: ManualClock,
    next_fence: u64,
    current: Option<Lease>,
}

impl LeaseController {
    pub fn new(seed: u64) -> Self {
        Self {
            clock: ManualClock::new(seed),
            next_fence: seed.rotate_left(7),
            current: None,
        }
    }

    pub fn issue(&mut self, owner: &str, ttl_ms: u64) -> Lease {
        self.next_fence = self
            .next_fence
            .checked_add(1)
            .expect("fixture fence overflow");
        let lease = Lease {
            owner: owner.to_owned(),
            fence: self.next_fence,
            expires_at_ms: self
                .clock
                .now_ms()
                .checked_add(ttl_ms)
                .expect("fixture lease overflow"),
        };
        self.current = Some(lease.clone());
        lease
    }

    pub fn renew(&mut self, lease: &Lease, ttl_ms: u64) -> Option<Lease> {
        if !self.can_commit(lease) {
            return None;
        }
        let renewed = Lease {
            expires_at_ms: self.clock.now_ms().checked_add(ttl_ms)?,
            ..lease.clone()
        };
        self.current = Some(renewed.clone());
        Some(renewed)
    }

    pub fn can_commit(&self, lease: &Lease) -> bool {
        self.clock.now_ms() < lease.expires_at_ms
            && self
                .current
                .as_ref()
                .is_some_and(|current| current.owner == lease.owner && current.fence == lease.fence)
    }
}
