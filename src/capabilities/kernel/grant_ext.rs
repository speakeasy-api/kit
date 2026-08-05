use std::collections::BTreeSet;

use crate::domain::{
    egress::{CredentialHandle as EgressCredentialHandle, DestinationGrant, Scheme},
    secret::SecretHandle,
};

use super::identity::put_bytes;

const MAX_EXTENSION_ITEMS: usize = 64;
const MAX_DELEGATION_DEPTH: u16 = 64;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EgressConstraint {
    scheme: Scheme,
    host: String,
    port: u16,
    credential: SecretHandle,
}

impl EgressConstraint {
    pub fn new(
        scheme: &str,
        host: &str,
        port: u16,
        credential: SecretHandle,
    ) -> Result<Self, GrantExtensionError> {
        let normalized = DestinationGrant::new(
            scheme,
            host,
            port,
            EgressCredentialHandle::new(credential.identifier())
                .map_err(|_| GrantExtensionError::InvalidEgress)?,
        )
        .map_err(|_| GrantExtensionError::InvalidEgress)?;
        Ok(Self {
            scheme: normalized.destination().scheme(),
            host: normalized.destination().host(),
            port: normalized.destination().port(),
            credential,
        })
    }

    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub const fn credential(&self) -> &SecretHandle {
        &self.credential
    }

    fn write_canonical(&self, output: &mut Vec<u8>) {
        output.push(match self.scheme {
            Scheme::Http => 0,
            Scheme::Https => 1,
        });
        put_bytes(output, self.host.as_bytes());
        output.extend_from_slice(&self.port.to_be_bytes());
        put_bytes(output, self.credential.identifier().as_bytes());
    }
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct GrantExtension {
    egress: BTreeSet<EgressConstraint>,
    credentials: BTreeSet<SecretHandle>,
    maximum_delegation_depth: u16,
}

impl GrantExtension {
    pub fn new(
        egress: impl IntoIterator<Item = EgressConstraint>,
        credentials: impl IntoIterator<Item = SecretHandle>,
        maximum_delegation_depth: u16,
    ) -> Result<Self, GrantExtensionError> {
        if maximum_delegation_depth > MAX_DELEGATION_DEPTH {
            return Err(GrantExtensionError::LimitExceeded);
        }
        let extension = Self {
            egress: collect_bounded(egress)?,
            credentials: collect_bounded(credentials)?,
            maximum_delegation_depth,
        };
        Ok(extension)
    }

    pub fn egress(&self) -> &BTreeSet<EgressConstraint> {
        &self.egress
    }

    pub fn credentials(&self) -> &BTreeSet<SecretHandle> {
        &self.credentials
    }

    pub const fn maximum_delegation_depth(&self) -> u16 {
        self.maximum_delegation_depth
    }

    pub(super) fn allows_except_depth(&self, request: &RequestExtension) -> bool {
        request.egress.as_ref().is_none_or(|egress| {
            self.egress.contains(egress) && self.credentials.contains(egress.credential())
        }) && request.egresses.is_subset(&self.egress)
            && request
                .egresses
                .iter()
                .all(|egress| self.credentials.contains(egress.credential()))
            && request.credentials.is_subset(&self.credentials)
            && request.credential.as_ref().is_none_or(|credential| {
                self.credentials.contains(credential)
                    && request
                        .egress
                        .as_ref()
                        .is_none_or(|egress| egress.credential() == credential)
            })
    }

    pub(super) fn write_canonical(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&(self.egress.len() as u64).to_be_bytes());
        for egress in &self.egress {
            egress.write_canonical(output);
        }
        output.extend_from_slice(&(self.credentials.len() as u64).to_be_bytes());
        for credential in &self.credentials {
            put_bytes(output, credential.identifier().as_bytes());
        }
        output.extend_from_slice(&self.maximum_delegation_depth.to_be_bytes());
    }
}

fn collect_bounded<T: Ord>(
    values: impl IntoIterator<Item = T>,
) -> Result<BTreeSet<T>, GrantExtensionError> {
    let mut bounded = BTreeSet::new();
    for (index, value) in values.into_iter().enumerate() {
        if index == MAX_EXTENSION_ITEMS {
            return Err(GrantExtensionError::LimitExceeded);
        }
        bounded.insert(value);
    }
    Ok(bounded)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequestExtension {
    egress: Option<EgressConstraint>,
    egresses: BTreeSet<EgressConstraint>,
    credential: Option<SecretHandle>,
    credentials: BTreeSet<SecretHandle>,
    workspace_revision: Option<String>,
}

impl RequestExtension {
    pub fn new(egress: Option<EgressConstraint>, credential: Option<SecretHandle>) -> Self {
        Self {
            egress,
            egresses: BTreeSet::new(),
            credential,
            credentials: BTreeSet::new(),
            workspace_revision: None,
        }
    }

    pub fn with_credentials(
        mut self,
        credentials: impl IntoIterator<Item = SecretHandle>,
    ) -> Result<Self, GrantExtensionError> {
        self.credentials = collect_bounded(credentials)?;
        if let Some(credential) = &self.credential {
            self.credentials.remove(credential);
        }
        if self.credentials.len() + usize::from(self.credential.is_some()) > MAX_EXTENSION_ITEMS {
            return Err(GrantExtensionError::LimitExceeded);
        }
        Ok(self)
    }

    pub fn with_egresses(
        mut self,
        egresses: impl IntoIterator<Item = EgressConstraint>,
    ) -> Result<Self, GrantExtensionError> {
        self.egresses = collect_bounded(egresses)?;
        if self.egress.is_none() && !self.egresses.is_empty() {
            return Err(GrantExtensionError::InvalidEgress);
        }
        if let Some(initial) = &self.egress {
            self.egresses.remove(initial);
        }
        if self.egresses.len() + usize::from(self.egress.is_some()) > MAX_EXTENSION_ITEMS {
            return Err(GrantExtensionError::LimitExceeded);
        }
        Ok(self)
    }

    pub fn with_workspace_revision(mut self, revision: impl Into<String>) -> Self {
        self.workspace_revision = Some(revision.into());
        self
    }

    pub const fn egress(&self) -> Option<&EgressConstraint> {
        self.egress.as_ref()
    }

    pub fn egresses(&self) -> &BTreeSet<EgressConstraint> {
        &self.egresses
    }

    pub const fn credential(&self) -> Option<&SecretHandle> {
        self.credential.as_ref()
    }

    pub fn credentials(&self) -> &BTreeSet<SecretHandle> {
        &self.credentials
    }

    pub fn workspace_revision(&self) -> Option<&str> {
        self.workspace_revision.as_deref()
    }

    pub(super) fn write_canonical(&self, output: &mut Vec<u8>) {
        match &self.egress {
            Some(egress) => {
                output.push(1);
                egress.write_canonical(output);
            }
            None => output.push(0),
        }
        output.extend_from_slice(&(self.egresses.len() as u64).to_be_bytes());
        for egress in &self.egresses {
            egress.write_canonical(output);
        }
        match &self.credential {
            Some(credential) => {
                output.push(1);
                put_bytes(output, credential.identifier().as_bytes());
            }
            None => output.push(0),
        }
        match &self.workspace_revision {
            Some(revision) => {
                output.push(1);
                put_bytes(output, revision.as_bytes());
            }
            None => output.push(0),
        }
        if !self.credentials.is_empty() {
            output.extend_from_slice(&(self.credentials.len() as u64).to_be_bytes());
            for credential in &self.credentials {
                put_bytes(output, credential.identifier().as_bytes());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantExtensionError {
    InvalidEgress,
    LimitExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_extension_requires_every_requested_credential() {
        let first = SecretHandle::parse("env:FIRST").unwrap();
        let second = SecretHandle::parse("env:SECOND").unwrap();
        let request = RequestExtension::new(None, None)
            .with_credentials([first.clone(), second.clone()])
            .unwrap();
        assert!(
            GrantExtension::new([], [first.clone(), second], 0)
                .unwrap()
                .allows_except_depth(&request)
        );
        assert!(
            !GrantExtension::new([], [first], 0)
                .unwrap()
                .allows_except_depth(&request)
        );
    }

    #[test]
    fn secondary_egresses_require_a_primary_destination() {
        let secondary = EgressConstraint::new(
            "https",
            "redirect.example",
            443,
            SecretHandle::parse("env:REDIRECT_TOKEN").unwrap(),
        )
        .unwrap();
        assert_eq!(
            RequestExtension::new(None, None).with_egresses([secondary]),
            Err(GrantExtensionError::InvalidEgress)
        );
    }
}
