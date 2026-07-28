use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapKind {
    Principal,
    Global,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapError {
    ZeroLimit { cap: CapKind },
    Exhausted { cap: CapKind, limit: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapUsage {
    pub global: usize,
    pub principal: usize,
}

struct CapState<P> {
    global: usize,
    principals: HashMap<P, usize>,
}

struct CapInner<P> {
    global_limit: usize,
    principal_limit: usize,
    state: Mutex<CapState<P>>,
}

pub struct ConcurrencyCaps<P> {
    inner: Arc<CapInner<P>>,
}

impl<P> Clone for ConcurrencyCaps<P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<P: Clone + Eq + Hash> ConcurrencyCaps<P> {
    pub fn new(global_limit: usize, principal_limit: usize) -> Result<Self, CapError> {
        if principal_limit == 0 {
            return Err(CapError::ZeroLimit {
                cap: CapKind::Principal,
            });
        }
        if global_limit == 0 {
            return Err(CapError::ZeroLimit {
                cap: CapKind::Global,
            });
        }
        Ok(Self {
            inner: Arc::new(CapInner {
                global_limit,
                principal_limit,
                state: Mutex::new(CapState {
                    global: 0,
                    principals: HashMap::new(),
                }),
            }),
        })
    }

    pub fn global_limit(&self) -> usize {
        self.inner.global_limit
    }

    pub fn principal_limit(&self) -> usize {
        self.inner.principal_limit
    }

    pub fn try_acquire(&self, principal: P) -> Result<CapPermit<P>, CapError> {
        let mut state = self.lock();
        let principal_in_use = state.principals.get(&principal).copied().unwrap_or(0);
        if principal_in_use >= self.inner.principal_limit {
            return Err(CapError::Exhausted {
                cap: CapKind::Principal,
                limit: self.inner.principal_limit,
            });
        }
        if state.global >= self.inner.global_limit {
            return Err(CapError::Exhausted {
                cap: CapKind::Global,
                limit: self.inner.global_limit,
            });
        }
        state.global += 1;
        *state.principals.entry(principal.clone()).or_default() += 1;
        drop(state);
        Ok(CapPermit {
            inner: Arc::clone(&self.inner),
            principal: Some(principal),
        })
    }

    pub fn usage(&self, principal: &P) -> CapUsage {
        let state = self.lock();
        CapUsage {
            global: state.global,
            principal: state.principals.get(principal).copied().unwrap_or(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CapState<P>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct CapPermit<P: Clone + Eq + Hash> {
    inner: Arc<CapInner<P>>,
    principal: Option<P>,
}

impl<P: Clone + Eq + Hash> CapPermit<P> {
    pub fn release(mut self) {
        if let Some(principal) = self.principal.take() {
            release(&self.inner, &principal);
        }
    }
}

impl<P: Clone + Eq + Hash> Drop for CapPermit<P> {
    fn drop(&mut self) {
        if let Some(principal) = self.principal.take() {
            release(&self.inner, &principal);
        }
    }
}

fn release<P: Eq + Hash>(inner: &CapInner<P>, principal: &P) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let remove = if let Some(in_use) = state.principals.get_mut(principal) {
        *in_use -= 1;
        *in_use == 0
    } else {
        return;
    };
    if remove {
        state.principals.remove(principal);
    }
    state.global -= 1;
}
