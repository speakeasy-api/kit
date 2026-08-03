use crate::{
    capabilities::broker::{
        BrokerError, BrokerInvocation, BrokerOutcome, BrokerRuntime, invoke as broker_invoke,
    },
    capabilities::kernel::invoke::{
        AuthorizedInvocation, DispatchOutcome, InvocationEnvelope, InvocationResult, InvokeError,
    },
    capabilities::{native::NativeCatalog, schema::NormalizedSchema},
    runtime::scheduler::reserve::BudgetLedger,
    store::sqlite::append::SqliteStore,
};

/// The adapter from capability callers into the sole capability broker.
pub(crate) struct OrchestratedCapabilityInvocation<'a> {
    envelope: InvocationEnvelope<'a>,
    schema: &'a NormalizedSchema,
    store: &'a mut SqliteStore,
    budget: &'a BudgetLedger,
}

impl<'a> OrchestratedCapabilityInvocation<'a> {
    pub(crate) fn new(
        envelope: InvocationEnvelope<'a>,
        schema: &'a NormalizedSchema,
        store: &'a mut SqliteStore,
        budget: &'a BudgetLedger,
    ) -> Self {
        Self {
            envelope,
            schema,
            store,
            budget,
        }
    }

    pub(crate) fn execute(
        self,
        backend: &mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    ) -> Result<InvocationResult, InvokeError> {
        let request = if NativeCatalog::by_identity(self.envelope.capability).is_some() {
            BrokerInvocation::native(self.envelope).map_err(map_broker_error)?
        } else {
            BrokerInvocation::external(self.envelope, self.schema)
        };
        broker_invoke(
            request,
            BrokerRuntime::new(self.store, self.budget, backend),
        )
        .map_err(map_broker_error)
        .and_then(|result| match result {
            BrokerOutcome::Completed(result) => Ok(result.invocation),
            BrokerOutcome::AuthRequired(_) => Err(InvokeError::BrokerAuth),
        })
    }
}

fn map_broker_error(error: BrokerError) -> InvokeError {
    match error {
        BrokerError::NativeCapabilityBinding => InvokeError::NativeCapabilityBinding,
        BrokerError::SchemaBindingMismatch => InvokeError::SchemaBindingMismatch,
        BrokerError::UnsupportedValidation => InvokeError::UnsupportedValidation,
        BrokerError::InvalidArguments => InvokeError::InvalidArguments,
        BrokerError::AuthStore(error) => InvokeError::Store(error),
        BrokerError::Invoke(error) => error,
        BrokerError::Accounting(error) => InvokeError::Accounting(error),
        BrokerError::ToolReservationRequired => InvokeError::ToolReservationRequired,
        BrokerError::InvalidAuthRequirement
        | BrokerError::InvalidTransportOperation
        | BrokerError::AuthCredentialMismatch
        | BrokerError::AuthNotRequired
        | BrokerError::AuthResolutionCancelled
        | BrokerError::TransportAuthCancelled
        | BrokerError::AuthPrincipalMismatch
        | BrokerError::AuthScopeMismatch
        | BrokerError::AuthDenied
        | BrokerError::RepeatedAuthChallenge
        | BrokerError::ReplayNotAuthorized
        | BrokerError::ReplayPermitConsumed
        | BrokerError::TransportAlreadyCompleted
        | BrokerError::TransportOutcomeUnknown
        | BrokerError::InvalidAuthState => InvokeError::BrokerAuth,
    }
}
