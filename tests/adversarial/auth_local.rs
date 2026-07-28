use std::{collections::BTreeMap, net::IpAddr};

use hmac::{Hmac, Mac};
use kit::{
    api::auth::{
        contract::{
            AuthDenial, AuthReadiness, Authenticator, Authorizer, GrantSnapshot, ResourceScope,
            ScopedAuthorizer,
        },
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        loopback::{
            LoopbackAuthenticator, LoopbackObservation, LoopbackReplayPolicy, LoopbackRequestTime,
        },
    },
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
        secret::SecretHandle,
    },
};
use sha2::{Digest, Sha256};

const UID: u32 = 501;
const PID: u32 = 4242;
const HOST: &str = "127.0.0.1:41820";
const ORIGIN: &str = "http://127.0.0.1:41820";

fn grants(principal: PrincipalId, project: ProjectId) -> GrantSnapshot {
    GrantSnapshot::new(
        principal,
        project,
        [Grant::WorkspaceRead, Grant::WorkspaceWrite],
    )
}

fn local(grants: GrantSnapshot) -> LocalPeerAuthenticator {
    LocalPeerAuthenticator::new(BTreeMap::from([(UID, grants)]))
}

fn loopback(
    grants: GrantSnapshot,
    issued_at: u64,
    expires_at: u64,
) -> (LoopbackAuthenticator, String) {
    let (authenticator, token) = LoopbackAuthenticator::issue(
        SecretHandle::parse("memory:loopback/session").unwrap(),
        grants,
        [HOST],
        [ORIGIN],
        issued_at,
        expires_at,
        LoopbackReplayPolicy::new(5, 128),
    )
    .unwrap();
    assert_eq!(
        authenticator.token_handle().identifier(),
        "memory:loopback/session"
    );
    let authorization = format!("Bearer {}", std::str::from_utf8(token.expose()).unwrap());
    (authenticator, authorization)
}

fn request<'a>(
    authorization: &'a str,
    host: &'a str,
    origin: &'a str,
    nonce: &'a [u8],
    now: u64,
) -> LoopbackObservation<'a> {
    request_at(authorization, host, origin, nonce, now, now)
}

fn request_at<'a>(
    authorization: &'a str,
    host: &'a str,
    origin: &'a str,
    nonce: &'a [u8],
    timestamp: u64,
    now: u64,
) -> LoopbackObservation<'a> {
    let signature = signature(authorization, nonce, timestamp);
    LoopbackObservation::from_transport(
        IpAddr::from([127, 0, 0, 1]),
        authorization,
        host,
        origin,
        nonce,
        &signature,
        LoopbackRequestTime::new(timestamp, now),
    )
}

fn signature(authorization: &str, nonce: &[u8], timestamp: u64) -> Vec<u8> {
    let token = authorization.strip_prefix("Bearer ").unwrap().as_bytes();
    let key = Sha256::digest(token);
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
    mac.update(b"KIT-LOOPBACK-REQUEST-V1\0");
    mac.update(&timestamp.to_be_bytes());
    mac.update(nonce);
    mac.finalize()
        .into_bytes()
        .iter()
        .flat_map(|byte| format!("{byte:02x}").into_bytes())
        .collect()
}

#[test]
fn nonce_replay_cache_stays_bounded_and_rejects_at_capacity() {
    let snapshot = grants(
        PrincipalId::generate().unwrap(),
        ProjectId::generate().unwrap(),
    );
    let (authenticator, token) = LoopbackAuthenticator::issue(
        SecretHandle::parse("memory:loopback/bounded").unwrap(),
        snapshot,
        [HOST],
        [ORIGIN],
        100,
        1_000,
        LoopbackReplayPolicy::new(10, 128),
    )
    .unwrap();
    let authorization = format!("Bearer {}", std::str::from_utf8(token.expose()).unwrap());

    for nonce in 0_u64..128 {
        assert!(
            authenticator
                .authenticate(&request(
                    &authorization,
                    HOST,
                    ORIGIN,
                    &nonce.to_be_bytes(),
                    100,
                ))
                .is_ok()
        );
    }
    assert_eq!(authenticator.replay_cache_len(), 128);
    assert_eq!(
        authenticator.authenticate(&request(&authorization, HOST, ORIGIN, b"capacity", 100,)),
        Err(AuthDenial::Unauthenticated)
    );
    assert_eq!(authenticator.replay_cache_len(), 128);
    assert_eq!(
        authenticator.authenticate(&request(
            &authorization,
            HOST,
            ORIGIN,
            &0_u64.to_be_bytes(),
            100,
        )),
        Err(AuthDenial::Unauthenticated)
    );

    assert!(
        authenticator
            .authenticate(&request(&authorization, HOST, ORIGIN, b"after-window", 111,))
            .is_ok()
    );
    assert_eq!(authenticator.replay_cache_len(), 1);
}

#[test]
fn timestamp_window_and_oversized_nonces_are_rejected() {
    let snapshot = grants(
        PrincipalId::generate().unwrap(),
        ProjectId::generate().unwrap(),
    );
    let (authenticator, token) = loopback(snapshot, 100, 200);
    let oversized = vec![b'x'; 1_000_000];
    assert_eq!(
        authenticator.authenticate(&request_at(&token, HOST, ORIGIN, &oversized, 100, 100)),
        Err(AuthDenial::Unauthenticated)
    );
    assert_eq!(authenticator.replay_cache_len(), 0);
    assert_eq!(
        authenticator.authenticate(&request_at(&token, HOST, ORIGIN, b"stale", 100, 106)),
        Err(AuthDenial::Unauthenticated)
    );
}

#[test]
fn exact_seven_required_denial_cases_are_closed() {
    let principal = PrincipalId::generate().unwrap();
    let other_principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let snapshot = grants(principal, project);
    let local = local(snapshot.clone());
    let peer = LocalPeerObservation::from_transport(UID, PID, UID);
    assert_eq!(
        (peer.uid(), peer.pid(), peer.socket_owner_uid()),
        (UID, PID, UID)
    );
    let authenticated = local.authenticate(&peer).unwrap();

    let unauthenticated =
        local.authenticate(&LocalPeerObservation::from_transport(UID + 1, PID, UID + 1));
    let cross_principal = ScopedAuthorizer.authorize(
        &authenticated,
        ResourceScope::new(other_principal, project),
        Grant::WorkspaceRead,
    );

    let (bad_origin_auth, bad_origin_token) = loopback(snapshot.clone(), 10, 20);
    let bad_origin = bad_origin_auth.authenticate(&request(
        &bad_origin_token,
        HOST,
        "http://attacker.invalid",
        b"bad-origin",
        11,
    ));
    let (bad_host_auth, bad_host_token) = loopback(snapshot.clone(), 10, 20);
    let bad_host = bad_host_auth.authenticate(&request(
        &bad_host_token,
        "attacker.invalid",
        ORIGIN,
        b"bad-host",
        11,
    ));

    let (replay_auth, replay_token) = loopback(snapshot.clone(), 10, 20);
    assert!(
        replay_auth
            .authenticate(&request(&replay_token, HOST, ORIGIN, b"nonce", 11))
            .is_ok()
    );
    let replayed_token =
        replay_auth.authenticate(&request(&replay_token, HOST, ORIGIN, b"nonce", 11));

    let (expired_auth, expired_token) = loopback(snapshot.clone(), 10, 20);
    let expired_token =
        expired_auth.authenticate(&request(&expired_token, HOST, ORIGIN, b"expired", 20));

    let (revoked_auth, revoked_token) = loopback(snapshot, 10, 20);
    revoked_auth.revoke();
    let revoked_token =
        revoked_auth.authenticate(&request(&revoked_token, HOST, ORIGIN, b"revoked", 11));

    let denials = [
        ("unauthenticated", unauthenticated.unwrap_err()),
        ("cross-principal", cross_principal.unwrap_err()),
        ("bad Origin", bad_origin.unwrap_err()),
        ("bad Host", bad_host.unwrap_err()),
        ("replayed token", replayed_token.unwrap_err()),
        ("expired token", expired_token.unwrap_err()),
        ("revoked token", revoked_token.unwrap_err()),
    ];
    assert_eq!(denials.len(), 7);
    assert_eq!(denials[0].1, AuthDenial::Unauthenticated);
    assert_eq!(denials[1].1, AuthDenial::Unauthorized);
    assert!(
        denials[2..]
            .iter()
            .all(|(_, denial)| *denial == AuthDenial::Unauthenticated)
    );
}

#[test]
fn authorization_denials_do_not_disclose_cross_resource_state() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let authenticated = local(grants(principal, project))
        .authenticate(&LocalPeerObservation::from_transport(UID, PID, UID))
        .unwrap();
    assert_eq!(authenticated.principal_id(), principal);

    let denials = [
        ScopedAuthorizer.authorize(
            &authenticated,
            ResourceScope::new(PrincipalId::generate().unwrap(), project),
            Grant::WorkspaceRead,
        ),
        ScopedAuthorizer.authorize(
            &authenticated,
            ResourceScope::new(principal, ProjectId::generate().unwrap()),
            Grant::WorkspaceRead,
        ),
        ScopedAuthorizer.authorize(
            &authenticated,
            ResourceScope::new(principal, project),
            Grant::ProcessSpawn,
        ),
    ];
    assert!(
        denials
            .into_iter()
            .all(|decision| decision == Err(AuthDenial::Unauthorized))
    );
}

#[test]
fn readiness_requires_both_components_in_every_boot_order() {
    let snapshot = grants(
        PrincipalId::generate().unwrap(),
        ProjectId::generate().unwrap(),
    );
    let authenticator = local(snapshot);
    let authorizer = ScopedAuthorizer;

    let authn_first = AuthReadiness::new();
    assert!(!authn_first.is_ready());
    authn_first.install_authenticator(&authenticator);
    assert!(!authn_first.is_ready());
    authn_first.install_authorizer(&authorizer);
    assert!(authn_first.is_ready());

    let authz_first = AuthReadiness::new();
    assert!(!authz_first.is_ready());
    authz_first.install_authorizer(&authorizer);
    assert!(!authz_first.is_ready());
    authz_first.install_authenticator(&authenticator);
    assert!(authz_first.is_ready());
}
