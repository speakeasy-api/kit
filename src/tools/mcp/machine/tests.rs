use std::sync::{Arc, Mutex};

use axum::http::Response;
use base64::Engine;
use rmcp::transport::auth::{
    OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRedirectPolicy, OAuthHttpRequest,
};
use serde_json::{Value, json};

use super::*;

struct Issuer {
    forms: Mutex<Vec<std::collections::BTreeMap<String, String>>>,
    rejected: bool,
}

impl OAuthHttpClient for Issuer {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let path = request.request.uri().path();
            let body = if path.contains("oauth-protected-resource") {
                json!({"resource":"https://mcp.example/mcp", "authorization_servers":["https://issuer.example"]})
            } else if path.contains("oauth-authorization-server") {
                json!({"issuer":"https://issuer.example", "authorization_endpoint":"https://issuer.example/authorize", "token_endpoint":"https://issuer.example/token", "token_endpoint_auth_methods_supported":["client_secret_post", "private_key_jwt"], "token_endpoint_auth_signing_alg_values_supported":["ES256"]})
            } else if path == "/token" {
                assert!(matches!(
                    request.redirect_policy,
                    OAuthHttpRedirectPolicy::Stop
                ));
                let form = url::form_urlencoded::parse(request.request.body())
                    .into_owned()
                    .collect();
                self.forms.lock().unwrap().push(form);
                if self.rejected {
                    return Ok(Response::builder()
                        .status(401)
                        .header("content-type", "application/json")
                        .body(
                            br#"{"error":"invalid_client","error_description":"DO-NOT-EXPOSE"}"#
                                .to_vec(),
                        )
                        .unwrap());
                }
                json!({"access_token":"test-access", "token_type":"Bearer", "expires_in":3600,"vendor_secret":"vendor-secret-canary"})
            } else {
                return Err("unexpected fake issuer endpoint".into());
            };
            Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).unwrap())
                .unwrap())
        })
    }
}

fn config(path: &std::path::Path, jwt: bool) -> Config {
    Config {
        client_id: "service-client".into(),
        scopes: vec!["read".into()],
        resource: "https://mcp.example/mcp".into(),
        expected_issuer: "https://issuer.example".into(),
        method: if jwt {
            Method::PrivateKeyJwt {
                private_key_file: path.into(),
                signing_algorithm: Algorithm::ES256,
                token_endpoint_audience: Some("https://issuer.example/token".into()),
            }
        } else {
            Method::ClientSecret {
                client_secret_file: path.into(),
            }
        },
    }
}

async fn manager(issuer: Arc<Issuer>) -> AuthorizationManager {
    AuthorizationManager::new_with_oauth_http_client("https://mcp.example/mcp", issuer)
        .await
        .unwrap()
}

#[tokio::test]
async fn secret_rotation_uses_client_credentials_and_preserves_parameters() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    let issuer = Arc::new(Issuer {
        forms: Mutex::new(Vec::new()),
        rejected: false,
    });
    for secret in ["first-secret", "rotated-secret"] {
        std::fs::write(&path, format!("{secret}\n")).unwrap();
        let (token, manager) =
            authenticate_manager(manager(issuer.clone()).await, &config(&path, false))
                .await
                .unwrap();
        assert_eq!(token, "test-access");
        assert!(expiry(&manager, now()).await.unwrap() > now());
        let forms = issuer.forms.lock().unwrap();
        let form = forms.last().unwrap();
        assert_eq!(form["grant_type"], "client_credentials");
        assert_eq!(form["client_id"], "service-client");
        assert_eq!(form["client_secret"], secret);
        assert_eq!(form["resource"], "https://mcp.example/mcp");
        assert_eq!(form["scope"], "read");
        assert!(!form.contains_key("refresh_token"));
    }
}

#[tokio::test]
async fn jwt_assertion_has_explicit_audience_and_no_secret_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("key.pem");
    std::fs::write(
        &path,
        include_bytes!("../../../../fixtures/mcp-auth/test-only-key.pem"),
    )
    .unwrap();
    let issuer = Arc::new(Issuer {
        forms: Mutex::new(Vec::new()),
        rejected: false,
    });
    let (token, _) = authenticate_manager(manager(issuer.clone()).await, &config(&path, true))
        .await
        .unwrap();
    assert_eq!(token, "test-access");
    let forms = issuer.forms.lock().unwrap();
    let form = forms.last().unwrap();
    assert_eq!(form["grant_type"], "client_credentials");
    assert!(!form.contains_key("client_secret"));
    assert!(!form.contains_key("client_id"));
    let assertion = &form["client_assertion"];
    let claims: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(assertion.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["aud"], "https://issuer.example/token");
    assert_eq!(claims["iss"], "service-client");
    assert_eq!(claims["sub"], "service-client");
    assert!(claims["jti"].as_str().is_some());
    assert!(claims["exp"].as_u64().unwrap() > now());
}

#[tokio::test]
async fn issuer_mismatch_precedes_file_read_and_exchange() {
    let issuer = Arc::new(Issuer {
        forms: Mutex::new(Vec::new()),
        rejected: false,
    });
    let mut config = config(std::path::Path::new("/does/not/exist"), false);
    config.expected_issuer = "https://different.example".into();
    let error = authenticate_manager(manager(issuer.clone()).await, &config)
        .await
        .err()
        .unwrap();
    assert_eq!(error, "machine OAuth issuer does not match expected_issuer");
    assert!(issuer.forms.lock().unwrap().is_empty());
}

#[tokio::test]
async fn issuer_failure_does_not_expose_response_or_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    std::fs::write(&path, "secret-canary").unwrap();
    let issuer = Arc::new(Issuer {
        forms: Mutex::new(Vec::new()),
        rejected: true,
    });
    let error = authenticate_manager(manager(issuer).await, &config(&path, false))
        .await
        .err()
        .unwrap();
    assert_eq!(error, "machine OAuth token exchange failed");
}

#[test]
fn configuration_is_explicit_and_strict() {
    let valid = json!({"type":"client_credentials", "client_id":"service", "resource":"https://mcp.example/mcp", "expected_issuer":"https://issuer.example", "method":{"type":"client_secret", "client_secret_file":"/run/secret"}});
    let parsed: crate::tools::mcp::auth::Config = serde_json::from_value(valid.clone()).unwrap();
    parsed.machine().unwrap().validate().unwrap();
    for (key, value) in [
        ("client_secret", json!("inline-secret")),
        ("clientId", json!("ambiguous")),
        ("unexpected", json!(true)),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        assert!(serde_json::from_value::<crate::tools::mcp::auth::Config>(invalid).is_err());
    }
    assert!(validate_transport_url("http://mcp.example/mcp").is_err());
    assert!(validate_transport_url("https://user:pass@mcp.example/mcp").is_err());
}

#[tokio::test]
async fn credential_files_are_bounded_and_never_cached() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    assert!(read_secret(&path).await.is_err());
    std::fs::write(&path, []).unwrap();
    assert!(read_secret(&path).await.is_err());
    std::fs::write(&path, vec![b'x'; 65537]).unwrap();
    assert!(read_secret(&path).await.is_err());
    assert!(read_secret(directory.path()).await.is_err());
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn vendor_token_fields_never_enter_debug_logs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    std::fs::write(&path, "file-secret-canary").unwrap();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    async {
        tracing::debug!("outer-calibration");
        let issuer = Arc::new(Issuer {
            forms: Mutex::new(Vec::new()),
            rejected: false,
        });
        authenticate_manager(manager(issuer).await, &config(&path, false))
            .await
            .unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(output.contains("outer-calibration"));
    assert!(!output.contains("file-secret-canary"));
    assert!(!output.contains("vendor-secret-canary"));
    assert!(!output.contains("test-access"));
}

#[cfg(unix)]
#[tokio::test]
async fn provisioned_fifo_is_rejected_without_waiting_for_a_writer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        read_secret(&path).await.err().unwrap(),
        "machine OAuth credential must be a regular file"
    );
}
