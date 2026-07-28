use crate::{
    capabilities::kernel::invoke::{
        AuthorizedInvocation, DispatchOutcome, InvocationEnvelope, InvocationResult,
        InvocationRuntime, InvokeError, invoke,
    },
    runtime::scheduler::reserve::BudgetLedger,
    store::sqlite::append::SqliteStore,
};

/// The sole kernel entry point used by agent and public native invocations.
pub(crate) struct OrchestratedNativeInvocation<'a> {
    envelope: InvocationEnvelope<'a>,
    store: &'a mut SqliteStore,
    budget: &'a BudgetLedger,
}

impl<'a> OrchestratedNativeInvocation<'a> {
    pub(crate) fn new(
        envelope: InvocationEnvelope<'a>,
        store: &'a mut SqliteStore,
        budget: &'a BudgetLedger,
    ) -> Self {
        Self {
            envelope,
            store,
            budget,
        }
    }

    pub(crate) fn execute(
        self,
        backend: &mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    ) -> Result<InvocationResult, InvokeError> {
        invoke(
            self.envelope,
            InvocationRuntime::new(self.store, self.budget, backend),
        )
    }
}
