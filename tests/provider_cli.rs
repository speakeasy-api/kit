use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

const CANARY: &str = "kit-provider-cli-canary";

fn temporary(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kit-provider-cli-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn kit(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kit"));
    command
        .env("KIT_CONFIG_FILE", config)
        .env_remove("KIT_PROVIDER")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENROUTER_API_KEY");
    command
}

fn success(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn path_help_and_list_are_local_and_do_not_create_state() {
    let root = temporary("local");
    let config = root.join("config/config.json");
    let state = root.join("state");

    let path = success(
        kit(&config)
            .args(["provider", "path", "--auto-start", "--state-root"])
            .arg(&state),
    );
    assert_eq!(
        String::from_utf8(path.stdout).unwrap().trim(),
        config.display().to_string()
    );
    assert!(!root.exists());

    let list = success(
        kit(&config)
            .args(["--json", "provider", "list", "--auto-start", "--state-root"])
            .arg(&state),
    );
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["items"], serde_json::json!([]));
    assert!(!root.exists());

    let help = success(kit(&config).args(["provider", "add", "--help"]));
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--api-key-env"));
    assert!(help.contains("OPENAI_API_KEY"));
    assert!(help.contains("OPENROUTER_API_KEY"));
    assert!(help.contains("ANTHROPIC_AUTH_TOKEN"));
    assert!(!help.contains("--api-key <"));
    assert!(!root.exists());
}

#[test]
fn standard_openai_and_openrouter_credentials_are_used_by_default_without_leaking() {
    let root = temporary("standard-defaults");
    let config = root.join("kit/config.json");
    let openai = "synthetic-openai-default";
    let openrouter = "synthetic-openrouter-default";

    let output = success(kit(&config).env("OPENAI_API_KEY", openai).args([
        "provider",
        "add",
        "openai",
        "--provider",
        "openai",
    ]));
    assert!(!String::from_utf8(output.stdout).unwrap().contains(openai));

    let output = success(kit(&config).env("OPENROUTER_API_KEY", openrouter).args([
        "provider",
        "add",
        "openrouter",
        "--provider",
        "openrouter",
    ]));
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains(openrouter)
    );

    let persisted: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(persisted["providers"]["openai"]["api_key"], openai);
    assert_eq!(persisted["providers"]["openrouter"]["api_key"], openrouter);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_api_key_environment_overrides_standard_variables() {
    let root = temporary("credential-overrides");
    let config = root.join("kit/config.json");
    let standard = "synthetic-standard-credential";
    let override_value = "synthetic-override-credential";

    for (name, provider, standard_variable) in [
        ("openai", "openai", "OPENAI_API_KEY"),
        ("openrouter", "openrouter", "OPENROUTER_API_KEY"),
    ] {
        let output = success(
            kit(&config)
                .env(standard_variable, standard)
                .env("KIT_TEST_API_KEY_OVERRIDE", override_value)
                .args([
                    "provider",
                    "add",
                    name,
                    "--provider",
                    provider,
                    "--api-key-env",
                    "KIT_TEST_API_KEY_OVERRIDE",
                ]),
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(!output.contains(standard));
        assert!(!output.contains(override_value));
    }

    let persisted: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(persisted["providers"]["openai"]["api_key"], override_value);
    assert_eq!(
        persisted["providers"]["openrouter"]["api_key"],
        override_value
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn anthropic_prefers_standard_auth_token_and_explicit_flags_override_the_choice() {
    let root = temporary("anthropic-precedence");
    let config = root.join("kit/config.json");
    let standard_token = "synthetic-anthropic-standard-token";
    let standard_key = "synthetic-anthropic-standard-key";
    let override_key = "synthetic-anthropic-override-key";
    let override_token = "synthetic-anthropic-override-token";
    let common = [
        "--provider",
        "anthropic",
        "--model",
        "claude-test",
        "--max-tokens",
        "10",
    ];

    let output = success(
        kit(&config)
            .env("ANTHROPIC_AUTH_TOKEN", standard_token)
            .env("ANTHROPIC_API_KEY", standard_key)
            .args(["provider", "add", "preferred"])
            .args(common),
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains(standard_token)
    );

    success(
        kit(&config)
            .env("ANTHROPIC_AUTH_TOKEN", standard_token)
            .env("ANTHROPIC_API_KEY", standard_key)
            .env("KIT_TEST_ANTHROPIC_KEY", override_key)
            .args(["provider", "add", "explicit-key"])
            .args(common)
            .args(["--api-key-env", "KIT_TEST_ANTHROPIC_KEY"]),
    );
    success(
        kit(&config)
            .env("ANTHROPIC_AUTH_TOKEN", standard_token)
            .env("ANTHROPIC_API_KEY", standard_key)
            .env("KIT_TEST_ANTHROPIC_TOKEN", override_token)
            .args(["provider", "add", "explicit-token"])
            .args(common)
            .args(["--auth-token-env", "KIT_TEST_ANTHROPIC_TOKEN"]),
    );
    success(
        kit(&config)
            .env("ANTHROPIC_API_KEY", standard_key)
            .args(["provider", "add", "fallback-key"])
            .args(common),
    );

    let persisted: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(
        persisted["providers"]["preferred"]["auth_token"],
        standard_token
    );
    assert!(persisted["providers"]["preferred"].get("api_key").is_none());
    assert_eq!(
        persisted["providers"]["explicit-key"]["api_key"],
        override_key
    );
    assert_eq!(
        persisted["providers"]["explicit-token"]["auth_token"],
        override_token
    );
    assert_eq!(
        persisted["providers"]["fallback-key"]["api_key"],
        standard_key
    );

    let rejected = kit(&config)
        .env("KIT_TEST_ANTHROPIC_KEY", override_key)
        .env("KIT_TEST_ANTHROPIC_TOKEN", override_token)
        .args(["provider", "add", "both"])
        .args(common)
        .args([
            "--api-key-env",
            "KIT_TEST_ANTHROPIC_KEY",
            "--auth-token-env",
            "KIT_TEST_ANTHROPIC_TOKEN",
        ])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    let rejected = String::from_utf8(rejected.stderr).unwrap();
    assert!(rejected.contains("cannot both be specified"));
    assert!(!rejected.contains(override_key));
    assert!(!rejected.contains(override_token));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_standard_credentials_name_the_expected_environment_variables() {
    let root = temporary("missing-standard-credentials");
    let config = root.join("kit/config.json");
    for (arguments, expected) in [
        (
            vec!["provider", "add", "openai", "--provider", "openai"],
            "OPENAI_API_KEY",
        ),
        (
            vec!["provider", "add", "openrouter", "--provider", "openrouter"],
            "OPENROUTER_API_KEY",
        ),
        (
            vec![
                "provider",
                "add",
                "anthropic",
                "--provider",
                "anthropic",
                "--model",
                "claude-test",
                "--max-tokens",
                "10",
            ],
            "ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY",
        ),
    ] {
        let output = kit(&config).args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains(CANARY));
    }
    assert!(!config.exists());
    let _ = fs::remove_dir_all(root);
}

#[cfg(not(windows))]
#[test]
fn path_resolution_prefers_override_then_xdg_then_home_without_using_cwd() {
    let root = temporary("path-resolution");
    let xdg = root.join("xdg");
    let home = root.join("home");
    let cwd = root.join("cwd");
    fs::create_dir_all(&cwd).unwrap();

    let output = success(
        Command::new(env!("CARGO_BIN_EXE_kit"))
            .env_remove("KIT_CONFIG_FILE")
            .env("XDG_CONFIG_HOME", &xdg)
            .env("HOME", &home)
            .current_dir(&cwd)
            .args(["provider", "path"]),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        xdg.join("kit/config.json").display().to_string()
    );

    let output = success(
        Command::new(env!("CARGO_BIN_EXE_kit"))
            .env_remove("KIT_CONFIG_FILE")
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", &home)
            .current_dir(&cwd)
            .args(["--json", "provider", "path"]),
    );
    let output: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        output["path"],
        home.join(".config/kit/config.json").display().to_string()
    );
    assert!(!cwd.join(".kit").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn add_list_switch_replace_and_redaction_work_for_named_profiles() {
    let root = temporary("management");
    let config = root.join("kit/config.json");

    let first = success(kit(&config).args([
        "provider",
        "add",
        "local",
        "--provider",
        "ollama",
        "--model",
        "llama-test",
    ]));
    assert!(
        String::from_utf8(first.stdout)
            .unwrap()
            .contains("selected it")
    );

    success(kit(&config).env("KIT_TEST_OPENAI_KEY", CANARY).args([
        "provider",
        "add",
        "work",
        "--provider",
        "openai",
        "--api-key-env",
        "KIT_TEST_OPENAI_KEY",
    ]));
    let list = success(kit(&config).args(["--json", "provider", "list"]));
    let rendered = String::from_utf8(list.stdout).unwrap();
    assert!(!rendered.contains(CANARY));
    let list: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    assert_eq!(list["items"][0]["name"], "local");
    assert_eq!(list["items"][0]["current"], true);
    assert_eq!(list["items"][1]["name"], "work");

    let switched = success(kit(&config).args(["--json", "provider", "use", "work"]));
    let switched: Value = serde_json::from_slice(&switched.stdout).unwrap();
    assert_eq!(switched["daemon_restart_required"], true);

    let refused = kit(&config)
        .env("KIT_TEST_OPENAI_KEY", CANARY)
        .args([
            "provider",
            "add",
            "work",
            "--provider",
            "openai",
            "--api-key-env",
            "KIT_TEST_OPENAI_KEY",
        ])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    let refused = String::from_utf8(refused.stderr).unwrap();
    assert!(refused.contains("--replace"));
    assert!(!refused.contains(CANARY));

    success(kit(&config).args([
        "provider",
        "add",
        "work",
        "--provider",
        "ollama",
        "--model",
        "replacement",
        "--replace",
    ]));
    let persisted: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(persisted["current"], "work");
    assert_eq!(persisted["providers"]["work"]["provider"], "ollama");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(config.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn all_provider_add_options_and_credential_failures_are_validated() {
    let root = temporary("providers");
    let config = root.join("kit/config.json");

    for (name, arguments) in [
        (
            "openai",
            vec![
                "--provider",
                "openai",
                "--api-key-env",
                "KIT_TEST_CREDENTIAL",
                "--base-url",
                "http://openai.test/chat",
            ],
        ),
        (
            "anthropic",
            vec![
                "--provider",
                "anthropic",
                "--auth-token-env",
                "KIT_TEST_CREDENTIAL",
                "--model",
                "claude-test",
                "--max-tokens",
                "321",
                "--version",
                "test-version",
                "--beta",
                "one,two",
            ],
        ),
        (
            "openrouter",
            vec![
                "--provider",
                "openrouter",
                "--api-key-env",
                "KIT_TEST_CREDENTIAL",
                "--app-name",
                "kit-test",
                "--site-url",
                "https://kit.test",
                "--max-completion-tokens",
                "654",
                "--temperature",
                "0.5",
                "--reasoning-effort",
                "high",
            ],
        ),
        (
            "ollama",
            vec!["--provider", "ollama", "--model", "llama-test"],
        ),
    ] {
        let mut command = kit(&config);
        command
            .env("KIT_TEST_CREDENTIAL", CANARY)
            .args(["provider", "add", name])
            .args(arguments);
        success(&mut command);
    }

    let persisted: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(persisted["providers"]["openai"]["model"], "gpt-4o");
    assert_eq!(
        persisted["providers"]["openrouter"]["model"],
        "openrouter/auto"
    );
    assert_eq!(persisted["providers"]["anthropic"]["max_tokens"], 321);

    let missing = kit(&config)
        .args([
            "provider",
            "add",
            "missing",
            "--provider",
            "openai",
            "--api-key-env",
            "KIT_DEFINITELY_MISSING_CREDENTIAL",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let error = String::from_utf8(missing.stderr).unwrap();
    assert!(error.contains("KIT_DEFINITELY_MISSING_CREDENTIAL"));
    assert!(!error.contains(CANARY));

    let inapplicable = kit(&config)
        .env("KIT_TEST_CREDENTIAL", CANARY)
        .args([
            "provider",
            "add",
            "bad",
            "--provider",
            "openai",
            "--api-key-env",
            "KIT_TEST_CREDENTIAL",
            "--max-tokens",
            "1",
        ])
        .output()
        .unwrap();
    assert_eq!(inapplicable.status.code(), Some(2));
    let error = String::from_utf8(inapplicable.stderr).unwrap();
    assert!(error.contains("only valid for anthropic"));
    assert!(!error.contains(CANARY));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn list_rejects_duplicate_oversize_symlink_and_world_readable_configs() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = temporary("unsafe");
    fs::create_dir_all(&root).unwrap();
    let config = root.join("config.json");
    let valid = br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m"}}}"#;
    let cases = [
        br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m"},"a":{"provider":"ollama","model":"n"}}}"#.as_slice(),
        br#"{"current":"a","providers":{"a":{"provider":"ollama","model":"m","unknown":1}}}"#,
        b"{".as_slice(),
    ];
    for bytes in cases {
        fs::write(&config, bytes).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let output = kit(&config).args(["provider", "list"]).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }

    fs::write(&config, vec![b' '; 64 * 1024 + 1]).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let output = kit(&config).args(["provider", "list"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr).unwrap().contains("64 KiB"));

    fs::write(&config, valid).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let output = kit(&config).args(["provider", "list"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("chmod 600")
    );

    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.join("link.json");
    symlink(&config, &link).unwrap();
    let output = kit(&link).args(["provider", "list"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("regular file")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn kit_provider_environment_override_ignores_an_invalid_persistent_registry() {
    let root = temporary("precedence");
    fs::create_dir_all(&root).unwrap();
    let config = root.join("config.json");
    fs::write(&config, b"not-json").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let state = root.join("state");
    let output = kit(&config)
        .env("KIT_PROVIDER", "not-a-provider")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OLLAMA_MODEL")
        .args(["daemon", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("model adapter unavailable"), "{error}");
    assert!(
        !error.contains("persistent provider configuration"),
        "{error}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_persistent_registry_is_an_actionable_daemon_setup_error_before_state_creation() {
    let root = temporary("daemon-invalid");
    fs::create_dir_all(&root).unwrap();
    let config = root.join("config.json");
    fs::write(&config, b"not-json").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let state = root.join("state");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let output = kit(&config)
        .env("KIT_PROJECT_ROOT", &project)
        .args(["daemon", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("daemon setup failed"), "{error}");
    assert!(
        error.contains("persistent provider configuration"),
        "{error}"
    );
    assert!(error.contains("invalid provider config"), "{error}");
    assert!(!state.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_registry_keeps_the_typed_model_adapter_unavailable_startup() {
    let root = temporary("daemon-missing");
    let config = root.join("missing/config.json");
    let state = root.join("state");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let output = kit(&config)
        .env("KIT_PROJECT_ROOT", &project)
        .args(["daemon", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("model adapter unavailable"), "{error}");
    assert!(!error.contains("invalid provider config"), "{error}");
    fs::remove_dir_all(root).unwrap();
}
