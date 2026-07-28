use std::collections::BTreeMap;

use super::contract::{
    AuthDecision, AuthDenial, AuthenticatedPrincipal, Authenticator, GrantSnapshot,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalPeerObservation {
    uid: u32,
    pid: u32,
    socket_owner_uid: u32,
}

impl LocalPeerObservation {
    pub fn from_transport(uid: u32, pid: u32, socket_owner_uid: u32) -> Self {
        Self {
            uid,
            pid,
            socket_owner_uid,
        }
    }

    pub fn uid(self) -> u32 {
        self.uid
    }

    pub fn pid(self) -> u32 {
        self.pid
    }

    pub fn socket_owner_uid(self) -> u32 {
        self.socket_owner_uid
    }
}

#[derive(Clone, Debug, Default)]
pub struct LocalPeerAuthenticator {
    grants_by_uid: BTreeMap<u32, GrantSnapshot>,
}

impl LocalPeerAuthenticator {
    pub fn new(grants_by_uid: BTreeMap<u32, GrantSnapshot>) -> Self {
        Self { grants_by_uid }
    }
}

impl Authenticator<LocalPeerObservation> for LocalPeerAuthenticator {
    fn authenticate(&self, observation: &LocalPeerObservation) -> AuthDecision {
        if observation.pid == 0 || observation.uid != observation.socket_owner_uid {
            return Err(AuthDenial::Unauthenticated);
        }
        self.grants_by_uid
            .get(&observation.uid)
            .cloned()
            .map(AuthenticatedPrincipal::from_grants)
            .ok_or(AuthDenial::Unauthenticated)
    }
}
