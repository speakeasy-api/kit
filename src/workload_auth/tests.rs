#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::disallowed_methods,
        clippy::disallowed_macros
    )
)]
use super::*;

fn config() -> Config {
    Config {
        token_file: PathBuf::from("/run/identity/token"),
        expected_issuer: "https://issuer.example/tenant".into(),
        token_endpoint: "https://issuer.example/tenant/token".into(),
        resource: "https://resource.example/mcp".into(),
        ..Default::default()
    }
}

#[test]
fn rejects_unsafe_configuration_before_credentials() {
    assert!(config().validate().is_ok());
    for endpoint in [
        "http://issuer.example/token",
        "https://other.example/token",
        "https://secret@issuer.example/token",
        "https://issuer.example/token#fragment",
    ] {
        let mut candidate = config();
        candidate.token_endpoint = endpoint.into();
        assert!(Session::new(candidate).is_err());
    }
    let mut candidate = config();
    candidate.token_file = PathBuf::from("relative/token");
    assert!(Session::new(candidate).is_err());
}

#[tokio::test]
async fn file_reopens_and_malformed_tokens_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    assert!(read_assertion(&path).await.is_err());
    for value in ["aaa.bbb.ccc", "ddd.eee.fff"] {
        std::fs::write(&path, format!("{value}\n")).unwrap();
        assert_eq!(read_assertion(&path).await.unwrap().as_str(), value);
    }
    for value in ["", "secret", "a..b", "a.b.c.d", "a. b.c", "a.b.c\nsecret"] {
        std::fs::write(&path, value).unwrap();
        assert_eq!(
            read_assertion(&path).await.err().unwrap(),
            "invalid workload assertion"
        );
    }
    std::fs::write(&path, vec![b'a'; MAX_BYTES + 1]).unwrap();
    assert_eq!(
        read_assertion(&path).await.err().unwrap(),
        "workload assertion exceeds size limit"
    );
    assert!(read_assertion(dir.path()).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn projected_symlink_rotation_is_supported() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    for (version, value) in [("v1", "aaa.bbb.ccc"), ("v2", "ddd.eee.fff")] {
        std::fs::create_dir(dir.path().join(version)).unwrap();
        std::fs::write(dir.path().join(version).join("token"), value).unwrap();
    }
    symlink("v1", dir.path().join("..data")).unwrap();
    symlink("..data/token", dir.path().join("token")).unwrap();
    assert_eq!(
        read_assertion(&dir.path().join("token"))
            .await
            .unwrap()
            .as_str(),
        "aaa.bbb.ccc"
    );
    symlink("v2", dir.path().join("..data-next")).unwrap();
    std::fs::rename(dir.path().join("..data-next"), dir.path().join("..data")).unwrap();
    assert_eq!(
        read_assertion(&dir.path().join("token"))
            .await
            .unwrap()
            .as_str(),
        "ddd.eee.fff"
    );
}

#[test]
fn response_requires_bearer_fields_and_unsigned_expiry() {
    for body in [
        r#"{"access_token":"secret","expires_in":60}"#,
        r#"{"access_token":"secret","token_type":"Bearer","expires_in":-1}"#,
    ] {
        assert!(serde_json::from_str::<Token>(body).is_err());
    }
}

#[tokio::test]
async fn expiry_and_clock_rollback_reacquire_without_stale_fallback() {
    for expired in [false, true] {
        let mut session = Session::new(config()).unwrap();
        let now = SystemTime::now();
        *session.cached.lock().await = Some(Cached {
            value: Acquisition(Arc::new(Zeroizing::new("must-not-escape".into()))),
            acquired: if expired {
                now - Duration::from_secs(60)
            } else {
                now + Duration::from_secs(60)
            },
            refresh_at: if expired {
                now
            } else {
                now + Duration::from_secs(60)
            },
        });
        // A failed discovery must not expose a stale cache. This invalid URL
        // exercises the real pre-network validation boundary without an issuer.
        session.config.expected_issuer = "http://issuer.example".into();
        assert!(session.token().await.is_err());
        assert!(session.cached.lock().await.is_none());
    }
}

#[tokio::test]
async fn caches_are_resource_local_and_invalidatable() {
    let first = Session::new(config()).unwrap();
    let mut second_config = config();
    second_config.resource = "https://resource.example/other-mcp".into();
    let second = Session::new(second_config).unwrap();
    let now = SystemTime::now();
    *first.cached.lock().await = Some(Cached {
        value: Acquisition(Arc::new(Zeroizing::new("first-resource-token".into()))),
        acquired: now,
        refresh_at: now + Duration::from_secs(60),
    });
    assert!(second.cached.lock().await.is_none());
    let (a, b) = tokio::join!(first.token(), first.token());
    assert_eq!(a.unwrap(), "first-resource-token");
    assert_eq!(b.unwrap(), "first-resource-token");
    first
        .invalidate_acquisition(&first.acquisition().await.unwrap())
        .await;
    assert!(first.cached.lock().await.is_none());
}

#[test]
fn explicit_configuration_and_extensions_are_strict() {
    let mut candidate = config();
    candidate.endpoint_mode = EndpointMode::Explicit;
    candidate.expected_issuer.clear();
    candidate.resource = "urn:example:inference".into();
    assert!(candidate.validate().is_ok());
    candidate
        .extensions
        .insert("federation_policy_id".into(), "example".into());
    assert!(candidate.validate().is_ok());
    for key in [
        "grant_type",
        "subject_token",
        "assertion",
        "requested_token_type",
        "client_secret",
        "client_assertion",
        "resource",
        "scope",
        "Authorization",
        "actor_token",
    ] {
        candidate.extensions.insert(key.into(), "no".into());
        assert!(candidate.validate().is_err(), "{key}");
        candidate.extensions.remove(key);
    }
    for endpoint in [
        "http://issuer.example/token",
        "https://issuer.example/token?secret=value",
        "https://user@issuer.example/token",
        "https://issuer.example/token#part",
    ] {
        candidate.token_endpoint = endpoint.into();
        assert!(candidate.validate().is_err());
    }
    let decoded: Config = serde_json::from_value(serde_json::json!({
        "tokenFile":"/run/identity/token", "tokenEndpoint":"https://issuer.example/token",
        "endpointMode":"explicit", "grant":"tokenExchange"
    }))
    .unwrap();
    assert!(decoded.validate().is_ok());
    assert!(decoded.grant == Grant::TokenExchange);
    assert!(config().grant == Grant::JwtBearer);
    candidate = decoded;
    candidate
        .extensions
        .insert("custom".into(), "x".repeat(1025));
    assert!(candidate.validate().is_err());
}
