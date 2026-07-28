use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use super::budget::{Exhaustion, RunBudget};
use super::limits::Spend;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ReservationId(u128);

impl ReservationId {
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u128 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationStatus {
    Reserved,
    Debited,
    Released,
    Reconciled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservationSnapshot {
    id: ReservationId,
    spend: Spend,
    status: ReservationStatus,
}

impl ReservationSnapshot {
    pub const fn new(id: ReservationId, spend: Spend, status: ReservationStatus) -> Self {
        Self { id, spend, status }
    }

    pub const fn id(self) -> ReservationId {
        self.id
    }

    pub const fn spend(self) -> Spend {
        self.spend
    }

    pub const fn status(self) -> ReservationStatus {
        self.status
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetTotals {
    pub committed: Spend,
    pub reserved: Spend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
    Exhausted(Exhaustion),
    ReservationConflict {
        id: ReservationId,
    },
    UnknownReservation {
        id: ReservationId,
    },
    InvalidTransition {
        id: ReservationId,
        from: ReservationStatus,
        to: ReservationStatus,
    },
}

#[derive(Default)]
struct LedgerState {
    committed: Spend,
    reserved: Spend,
    reservations: BTreeMap<ReservationId, ReservationSnapshot>,
}

pub struct BudgetLedger {
    budget: RunBudget,
    state: Mutex<LedgerState>,
}

impl BudgetLedger {
    pub fn new(budget: RunBudget) -> Self {
        Self {
            budget,
            state: Mutex::new(LedgerState::default()),
        }
    }

    pub fn from_snapshots(
        budget: RunBudget,
        snapshots: impl IntoIterator<Item = ReservationSnapshot>,
    ) -> Result<Self, BudgetError> {
        let ledger = Self::new(budget);
        for snapshot in snapshots {
            ledger.reconcile(snapshot)?;
        }
        Ok(ledger)
    }

    pub const fn budget(&self) -> RunBudget {
        self.budget
    }

    pub fn reserve(
        &self,
        id: ReservationId,
        spend: Spend,
    ) -> Result<ReservationSnapshot, BudgetError> {
        let mut state = self.lock();
        if let Some(existing) = state.reservations.get(&id).copied() {
            return if existing.spend == spend {
                Ok(existing)
            } else {
                Err(BudgetError::ReservationConflict { id })
            };
        }

        self.budget
            .check(state.committed, state.reserved, spend)
            .map_err(BudgetError::Exhausted)?;
        state.reserved = state
            .reserved
            .checked_add(spend)
            .expect("budget check overflowed");
        let snapshot = ReservationSnapshot::new(id, spend, ReservationStatus::Reserved);
        state.reservations.insert(id, snapshot);
        Ok(snapshot)
    }

    pub fn commit(&self, id: ReservationId) -> Result<ReservationSnapshot, BudgetError> {
        let mut state = self.lock();
        let snapshot = state
            .reservations
            .get(&id)
            .copied()
            .ok_or(BudgetError::UnknownReservation { id })?;
        match snapshot.status {
            ReservationStatus::Debited | ReservationStatus::Reconciled => Ok(snapshot),
            ReservationStatus::Released => Err(BudgetError::InvalidTransition {
                id,
                from: snapshot.status,
                to: ReservationStatus::Debited,
            }),
            ReservationStatus::Reserved => {
                state.reserved = state
                    .reserved
                    .checked_sub(snapshot.spend)
                    .expect("reservation total is inconsistent");
                state.committed = state
                    .committed
                    .checked_add(snapshot.spend)
                    .expect("committed total overflowed");
                let committed = ReservationSnapshot::new(
                    snapshot.id,
                    snapshot.spend,
                    ReservationStatus::Debited,
                );
                state.reservations.insert(id, committed);
                Ok(committed)
            }
        }
    }

    pub fn release(&self, id: ReservationId) -> Result<ReservationSnapshot, BudgetError> {
        let mut state = self.lock();
        let snapshot = state
            .reservations
            .get(&id)
            .copied()
            .ok_or(BudgetError::UnknownReservation { id })?;
        match snapshot.status {
            ReservationStatus::Released => Ok(snapshot),
            ReservationStatus::Debited | ReservationStatus::Reconciled => {
                Err(BudgetError::InvalidTransition {
                    id,
                    from: snapshot.status,
                    to: ReservationStatus::Released,
                })
            }
            ReservationStatus::Reserved => {
                state.reserved = state
                    .reserved
                    .checked_sub(snapshot.spend)
                    .expect("reservation total is inconsistent");
                let released = ReservationSnapshot::new(
                    snapshot.id,
                    snapshot.spend,
                    ReservationStatus::Released,
                );
                state.reservations.insert(id, released);
                Ok(released)
            }
        }
    }

    pub fn reconcile(
        &self,
        incoming: ReservationSnapshot,
    ) -> Result<ReservationSnapshot, BudgetError> {
        let mut state = self.lock();
        let Some(existing) = state.reservations.get(&incoming.id).copied() else {
            if incoming.status != ReservationStatus::Released {
                self.budget
                    .check(state.committed, state.reserved, incoming.spend)
                    .map_err(BudgetError::Exhausted)?;
                match incoming.status {
                    ReservationStatus::Reserved => {
                        state.reserved = state
                            .reserved
                            .checked_add(incoming.spend)
                            .expect("budget check overflowed");
                    }
                    ReservationStatus::Debited | ReservationStatus::Reconciled => {
                        state.committed = state
                            .committed
                            .checked_add(incoming.spend)
                            .expect("budget check overflowed");
                    }
                    ReservationStatus::Released => unreachable!(),
                }
            }
            state.reservations.insert(incoming.id, incoming);
            return Ok(incoming);
        };

        if existing.spend != incoming.spend {
            return Err(BudgetError::ReservationConflict { id: incoming.id });
        }
        if existing.status == incoming.status {
            return Ok(existing);
        }

        match (existing.status, incoming.status) {
            (
                ReservationStatus::Reserved,
                ReservationStatus::Debited | ReservationStatus::Reconciled,
            ) => {
                state.reserved = state
                    .reserved
                    .checked_sub(existing.spend)
                    .expect("reservation total is inconsistent");
                state.committed = state
                    .committed
                    .checked_add(existing.spend)
                    .expect("committed total overflowed");
                state.reservations.insert(incoming.id, incoming);
                Ok(incoming)
            }
            (ReservationStatus::Reserved, ReservationStatus::Released) => {
                state.reserved = state
                    .reserved
                    .checked_sub(existing.spend)
                    .expect("reservation total is inconsistent");
                state.reservations.insert(incoming.id, incoming);
                Ok(incoming)
            }
            (
                ReservationStatus::Debited | ReservationStatus::Reconciled,
                ReservationStatus::Reserved,
            )
            | (ReservationStatus::Released, ReservationStatus::Reserved) => Ok(existing),
            _ => Err(BudgetError::InvalidTransition {
                id: incoming.id,
                from: existing.status,
                to: incoming.status,
            }),
        }
    }

    pub fn snapshot(&self, id: ReservationId) -> Option<ReservationSnapshot> {
        self.lock().reservations.get(&id).copied()
    }

    pub fn snapshots(&self) -> Vec<ReservationSnapshot> {
        self.lock().reservations.values().copied().collect()
    }

    pub fn totals(&self) -> BudgetTotals {
        let state = self.lock();
        BudgetTotals {
            committed: state.committed,
            reserved: state.reserved,
        }
    }

    pub fn remaining(&self) -> Spend {
        let state = self.lock();
        self.budget.remaining(state.committed, state.reserved)
    }

    fn lock(&self) -> MutexGuard<'_, LedgerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
