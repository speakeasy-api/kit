use std::collections::BTreeMap;

use jsonwebtoken::{
    Algorithm, EncodingKey, Header, encode,
    jwk::{Jwk, JwkSet, KeyOperations, PublicKeyUse},
};
use kit::{
    api::auth::{
        contract::{AuthDenial, Authenticator, GrantSnapshot},
        remote::{
            FAKE_PKI_EVIDENCE_CLASSIFICATION,
            mtls::{MtlsAuthenticator, MtlsObservation, RevocationPolicy, SubjectAltName},
            oidc::{OidcAlgorithm, OidcAuthenticator, OidcObservation, OidcRevocations},
        },
    },
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
    },
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber, date_time_ymd,
};
use serde::Serialize;

const NOW: u64 = 1_800_000_000;
const ISSUER: &str = "https://idp.example.test";
const AUDIENCE: &str = "kit-api";
const SUBJECT: &str = "subject-a";
const SAN: &str = "spiffe://example.test/principal-a";
const KID: &str = "key-1";

const CA_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIKeNHErj5C8Kd2vPw8bNpfigIbZzfEkMLkDsB5y8sja3\n-----END PRIVATE KEY-----\n";
const OTHER_CA_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIJG1l3kOGcu+c3IgerJl9B+Qwh5ZNyo9RjAxKGQWjQ/9\n-----END PRIVATE KEY-----\n";
const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEILpEEIXnp+Ocrwgn7qAHG/jMWDhxlgbNqYaW54HtV3s6\n-----END PRIVATE KEY-----\n";
const JWT_RSA_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC3UQBTeVjOtSY4\nHaZHjSpQPlIIUXSiIq+WRInLoWwYEXmloR41HMmwsCQVV1WFZ7z0wUj14vD3/Bl6\nJG2JTU8ur+RvJojm1gXxg/etp4DG2HVtXong4QE7BKqJufHITMVuEhojkTulHIbW\nXfQQjaxQpGsOIuWcRz3YVB7zpAL7yoeHhvFd7RV+IqG9i4fjN4pzlCTv/TQig+s7\n539MsNx1ZakBfeBhx62JUPhFe6pXdPS2hXVUiTQRPMBm3GimDzyuA3WkVKzPyNMB\n2h+BALRFLslqPaFpul7NIifX36KgUPaimntpvFRxahqyDvJ9ATtq6oMeHaRUMZf5\nkRxjLIXLAgMBAAECggEAAIIV+SVDTMINyrwHo6J4NTlnACTm/jK7FTSNbpC8/E1t\nbpBwGqpAw4pJdKcFqAADSGkSFbRnrJhN+HEKE1uxK3+gp3o43kLw80bFX1Lb4DE7\nahkyp/qXsUfbB9S0dIoEm2srbWElWYN8ZYhkeSNGEKx+q3mx9JPx+kaJa2159flh\nis34maBeEr97gwjAvMjLbdVEpoaEIRC/hmem2ckT5jsDd4HS7RKNXwk/S8O7/PQW\n42xKAvL0APk5J53CDoW4DT78y7t4Rj/dVeRZAhdjDUFP+idZ1r9k6PM8vs5tl1P0\njzcOMzUBFmhnb5MKFvBLc4MKJQYzTT06/qdfAV0M2QKBgQDsoUF+pNQuERUKCI9T\nZey7rFgsbBkK2t0XvgpLwwMbF548HgL+QJhAAONaLe5+2GlZSgb5OgoYYspiuzQT\noz2mqeN2MSMnUtntyUt+Y6IzPEEPg6bVGdoCP3FSvz1L/JDJuJq4cqb3OGPe4yEt\nZDymqUJCDTO52vT0GLdZ6S70twKBgQDGUoYrbButBHX5nwE5XnrjgENGT4RauI76\nQ158MuFmRmpgaWlc37ByVyzMG7x9qxcad4Ry19hsG5KYnL/PNs31a2i/BdfLZyFF\nY0dfNExz6tKf4PWxZhhFhX94f7qseSzXLx8eMQqdds4WQsA13JI9qQJ0pVSNyb/V\nXM/n9XMrjQKBgDsKgSz4M3jLClTWjexhIhAxkE6FKjprIX8rC6abocrAudqGInkN\n5O8TSabWjwtXM/HzZooI0TwEajr4OqYrtNZAzWBQIlVNdtK9xvhiI7Zk8lbMonPJ\nX3vwGHZtAP5Upkuuo+whr0c/6qtSQJTyza9HzCBu6tkUqMm+4QCuDelBAoGAax8S\nF4w6WrcJHj7Tg3BUAmQ6clTrEbGUkPsoov88nmi0dsUZQzAT93681La6lkp+nS4n\nXXzXCnXONh6cwElC8CgHGP8H83cOEpOwbm0qSoZxJCh3rU2PGKYmFyku5JBDNyvd\nrAojSLBuWrnNZopwd1u91tGinT93HcEXD5yVi9UCgYBq1sjl5jlliyHzPWMeV3dn\nkJWDLMpCwrpmQzrhkA02PaZO1BB7QgZeIKTYkzECHT44wHflalVOEEsVZpEn2Ivd\nJz6j2JwX7Ke23MA0MDaV6+7syAwPKx3+pOGwdun2uZNgvS74IWeBEfdMhGrGncX0\nQegKxe+skNhLjXJ5SUTdZg==\n-----END PRIVATE KEY-----\n";
const JWT_ES256_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgDiK9mmFgf/o/86cX\n6bNpL6VbT2sUxQQ+vXrkWILoGm2hRANCAAQsxPFM/MWE25lZ1GibWpun7M5DIjIm\nGvbcKR1lsjhr2kvMmRhuYA+HG9Qb+C2BYXGKthPQahHDDIEQWJVOWPed\n-----END PRIVATE KEY-----\n";

fn snapshot() -> GrantSnapshot {
    GrantSnapshot::new(
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        ProjectId::parse("project_00000000000000000000000002").unwrap(),
        [Grant::WorkspaceRead],
    )
}

fn ca(private_key: &str) -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::default();
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2035, 1, 1);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(SerialNumber::from_slice(&[1]));
    CertifiedIssuer::self_signed(params, KeyPair::from_pem(private_key).unwrap()).unwrap()
}

fn client_certificate(
    issuer: &CertifiedIssuer<'_, KeyPair>,
    serial: u8,
    expired: bool,
    san: &str,
) -> Vec<u8> {
    let mut params = CertificateParams::default();
    params.not_before = date_time_ymd(if expired { 2020 } else { 2025 }, 1, 1);
    params.not_after = date_time_ymd(if expired { 2021 } else { 2030 }, 1, 1);
    params.serial_number = Some(SerialNumber::from_slice(&[serial]));
    params.subject_alt_names = vec![SanType::URI(san.try_into().unwrap())];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params
        .signed_by(&KeyPair::from_pem(CLIENT_KEY).unwrap(), issuer)
        .unwrap()
        .der()
        .to_vec()
}

fn mtls(
    trusted_root: Vec<u8>,
    revoked_serials: impl IntoIterator<Item = Vec<u8>>,
) -> MtlsAuthenticator {
    MtlsAuthenticator::new(
        [trusted_root],
        revoked_serials,
        RevocationPolicy::PresentedChain,
        BTreeMap::from([(SubjectAltName::Uri(SAN.to_owned()), snapshot())]),
    )
    .unwrap()
}

#[derive(Clone, Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'a str,
    sub: &'a str,
    exp: u64,
    nbf: u64,
    jti: &'a str,
    sid: &'a str,
}

fn jwt_keys_and_jwks() -> (EncodingKey, EncodingKey, EncodingKey, String) {
    let rsa = EncodingKey::from_rsa_pem(JWT_RSA_KEY.as_bytes()).unwrap();
    let es256 = EncodingKey::from_ec_pem(JWT_ES256_KEY.as_bytes()).unwrap();
    let eddsa = EncodingKey::from_ed_pem(CLIENT_KEY.as_bytes()).unwrap();
    let mut rsa_jwk = Jwk::from_encoding_key(&rsa, Algorithm::RS256).unwrap();
    rsa_jwk.common.key_id = Some(KID.to_owned());
    rsa_jwk.common.public_key_use = Some(PublicKeyUse::Signature);
    rsa_jwk.common.key_operations = Some(vec![KeyOperations::Verify]);
    let es256_jwk = serde_json::from_value(serde_json::json!({
        "kty": "EC",
        "crv": "P-256",
        "x": "LMTxTPzFhNuZWdRom1qbp-zOQyIyJhr23CkdZbI4a9o",
        "y": "S8yZGG5gD4cb1Bv4LYFhcYq2E9BqEcMMgRBYlU5Y950",
        "alg": "ES256",
        "kid": "key-es256",
        "use": "sig",
        "key_ops": ["verify"]
    }))
    .unwrap();
    let eddsa_jwk = serde_json::from_value(serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "x": "J_Rfbd6hYKw07fzYeOQ0-FYudVTinSlW3L3icNj1RC0",
        "alg": "EdDSA",
        "kid": "key-eddsa",
        "use": "sig",
        "key_ops": ["verify"]
    }))
    .unwrap();
    let jwks = serde_json::to_string(&JwkSet {
        keys: vec![rsa_jwk, es256_jwk, eddsa_jwk],
    })
    .unwrap();
    (rsa, es256, eddsa, jwks)
}

fn token(key: &EncodingKey, algorithm: Algorithm, key_id: &str, claims: &Claims<'_>) -> String {
    let mut header = Header::new(algorithm);
    header.kid = Some(key_id.to_owned());
    encode(&header, claims, key).unwrap()
}

fn oidc(jwks: &str, revocations: OidcRevocations) -> OidcAuthenticator {
    OidcAuthenticator::new(
        ISSUER,
        AUDIENCE,
        jwks,
        [
            OidcAlgorithm::Rs256,
            OidcAlgorithm::Es256,
            OidcAlgorithm::EdDsa,
        ],
        revocations,
        BTreeMap::from([(SUBJECT.to_owned(), snapshot())]),
    )
    .unwrap()
}

fn authenticate_oidc(
    authenticator: &OidcAuthenticator,
    token: &str,
) -> kit::api::auth::contract::AuthDecision {
    let authorization = format!("Bearer {token}");
    authenticator.authenticate(&OidcObservation::from_transport(&authorization, NOW))
}

#[test]
fn operational_fake_pki_denies_exactly_seven_required_cases_and_accepts_valid_peers() {
    assert_eq!(
        FAKE_PKI_EVIDENCE_CLASSIFICATION,
        "operational fake-PKI (O), not external conformance (C)"
    );

    let trusted_ca = ca(CA_KEY);
    let other_ca = ca(OTHER_CA_KEY);
    let root = trusted_ca.der().to_vec();
    let valid_chain = vec![client_certificate(&trusted_ca, 2, false, SAN)];
    let valid_mtls = mtls(root.clone(), []);
    assert_eq!(
        valid_mtls
            .authenticate(&MtlsObservation::from_tls_transport(&valid_chain, NOW))
            .unwrap()
            .principal_id(),
        snapshot().principal_id()
    );

    let expired_chain = vec![client_certificate(&trusted_ca, 3, true, SAN)];
    let expired_cert = mtls(root.clone(), [])
        .authenticate(&MtlsObservation::from_tls_transport(&expired_chain, NOW));
    let wrong_ca_chain = vec![client_certificate(&other_ca, 4, false, SAN)];
    let wrong_ca = mtls(root.clone(), [])
        .authenticate(&MtlsObservation::from_tls_transport(&wrong_ca_chain, NOW));
    let revoked_chain = vec![client_certificate(&trusted_ca, 5, false, SAN)];
    let revoked_cert = mtls(root, [vec![5]])
        .authenticate(&MtlsObservation::from_tls_transport(&revoked_chain, NOW));

    let (key, es256_key, eddsa_key, jwks) = jwt_keys_and_jwks();
    let valid_claims = Claims {
        iss: ISSUER,
        aud: AUDIENCE,
        sub: SUBJECT,
        exp: NOW + 100,
        nbf: NOW - 100,
        jti: "valid-token",
        sid: "valid-session",
    };
    let valid_jwt = token(&key, Algorithm::RS256, KID, &valid_claims);
    let valid_oidc = oidc(&jwks, OidcRevocations::default());
    assert_eq!(
        authenticate_oidc(&valid_oidc, &valid_jwt)
            .unwrap()
            .principal_id(),
        snapshot().principal_id()
    );
    for (key, algorithm, key_id) in [
        (&es256_key, Algorithm::ES256, "key-es256"),
        (&eddsa_key, Algorithm::EdDSA, "key-eddsa"),
    ] {
        assert!(
            authenticate_oidc(&valid_oidc, &token(key, algorithm, key_id, &valid_claims)).is_ok()
        );
    }

    let bad_audience = token(
        &key,
        Algorithm::RS256,
        KID,
        &Claims {
            aud: "other-api",
            ..valid_claims.clone()
        },
    );
    let bad_issuer = token(
        &key,
        Algorithm::RS256,
        KID,
        &Claims {
            iss: "https://attacker.invalid",
            ..valid_claims.clone()
        },
    );
    let expired_jwt = token(
        &key,
        Algorithm::RS256,
        KID,
        &Claims {
            exp: NOW,
            ..valid_claims
        },
    );
    let claims_segment = valid_jwt.split('.').nth(1).unwrap();
    let alg_none =
        format!("eyJhbGciOiJub25lIiwia2lkIjoia2V5LTEiLCJ0eXAiOiJKV1QifQ.{claims_segment}.");

    let denials = [
        expired_cert,
        wrong_ca,
        revoked_cert,
        authenticate_oidc(&valid_oidc, &bad_audience),
        authenticate_oidc(&valid_oidc, &bad_issuer),
        authenticate_oidc(&valid_oidc, &expired_jwt),
        authenticate_oidc(&valid_oidc, &alg_none),
    ];
    assert_eq!(denials.len(), 7);
    assert!(
        denials
            .into_iter()
            .all(|denial| denial == Err(AuthDenial::Unauthenticated))
    );
}

#[test]
fn remote_identity_mapping_and_revocation_fail_closed() {
    let trusted_ca = ca(CA_KEY);
    let root = trusted_ca.der().to_vec();
    let unmapped_chain = vec![client_certificate(
        &trusted_ca,
        6,
        false,
        "spiffe://example.test/unmapped",
    )];
    assert_eq!(
        mtls(root, []).authenticate(&MtlsObservation::from_tls_transport(&unmapped_chain, NOW)),
        Err(AuthDenial::Unauthenticated)
    );

    let (key, _, _, jwks) = jwt_keys_and_jwks();
    let claims = Claims {
        iss: ISSUER,
        aud: AUDIENCE,
        sub: SUBJECT,
        exp: NOW + 100,
        nbf: NOW - 100,
        jti: "revocable-token",
        sid: "revocable-session",
    };
    let encoded = token(&key, Algorithm::RS256, KID, &claims);
    let auth = oidc(&jwks, OidcRevocations::default());
    assert!(authenticate_oidc(&auth, &encoded).is_ok());
    auth.revoke_token(claims.jti).unwrap();
    assert_eq!(
        authenticate_oidc(&auth, &encoded),
        Err(AuthDenial::Unauthenticated)
    );

    let session_token = token(
        &key,
        Algorithm::RS256,
        KID,
        &Claims {
            jti: "other-token",
            ..claims
        },
    );
    auth.revoke_session("revocable-session").unwrap();
    assert_eq!(
        authenticate_oidc(&auth, &session_token),
        Err(AuthDenial::Unauthenticated)
    );
}
