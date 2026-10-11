//! Read canonical AHP registrations without changing user-owned files.
use super::{AhpConfig, supported};
use agenthooksprotocol::{
    generated::boundary::ALL_BOUNDARIES,
    hooks::{EventGrant, Hooks, HooksOptions},
};
#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fs, path::Path};

impl AhpConfig {
    /// Load user hooks, then workspace hooks. Workspace definitions replace user
    /// definitions with the same id in place; new ids are appended. Unknown
    /// top-level extension fields are preserved, with workspace values winning.
    /// Invalid or unreadable files are warned about and skipped independently.
    pub fn load(root: &Path) -> Result<Option<Self>, String> {
        let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
        load_with_home(home.as_deref().map(Path::new), root)
    }
}

fn grants() -> Result<BTreeMap<String, Value>, String> {
    supported()
        .into_iter()
        .map(|(name, capabilities)| {
            // Canonical wire authority is narrower than the set of boundaries Kit
            // consumes: observation-only events must not receive interception.
            let descriptor = ALL_BOUNDARIES
                .iter()
                .find(|entry| entry.name == name)
                .ok_or_else(|| format!("Kit supports an unknown canonical AHP event: {name}"))?;
            let intercept = descriptor.modes.contains(&"intercept")
                && capabilities.as_value()["effects"]
                    .as_array()
                    .is_some_and(|effects| !effects.is_empty());
            let modes: Vec<_> = descriptor
                .modes
                .iter()
                .copied()
                .filter(|mode| *mode != "intercept" || intercept)
                .collect();
            let mut declaration = Value::Object(Map::from_iter([(
                "modes".to_owned(),
                Value::Array(modes.into_iter().map(Value::from).collect()),
            )]));
            if intercept {
                declaration["capabilities"] = capabilities.as_value().clone();
            }
            Ok((name, declaration))
        })
        .collect()
}

fn read(path: &Path, grants: &BTreeMap<String, Value>) -> Result<Option<Value>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let validate = || -> Result<Value, String> {
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let capabilities = grants
            .iter()
            .map(|(name, declaration)| {
                EventGrant::from_declaration(declaration).map(|grant| (name.clone(), grant))
            })
            .collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?;
        // The SDK validates the canonical schema, duplicate backend ids and
        // registration routes. Its registered transports are lazy: construction
        // does not spawn a process, send a request or start a worker/runtime.
        Hooks::new(
            &value,
            HooksOptions::new("urn:kit:ahp:config", capabilities),
        )
        .map_err(|error| error.to_string())?;
        Ok(value)
    };
    validate()
        .map(Some)
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn warn_config(path: &Path, error: &str) {
    tracing::warn!(path = %path.display(), %error, "Skipping invalid AHP hooks configuration");
    if let Some(transport) = crate::diagnostic_transport::global() {
        transport.publish_line(&format!(
            "Warning: skipping invalid AHP hooks configuration {}: {error}",
            path.display()
        ));
    }
}

pub(super) fn load_with_home(
    home: Option<&Path>,
    root: &Path,
) -> Result<Option<AhpConfig>, String> {
    let grants = grants()?;
    let local_path = root.join(".agents/hooks.json");
    let global_path = home
        .filter(|path| !path.as_os_str().is_empty())
        .map(|home| home.join(".agents/hooks.json"));
    let global = global_path
        .as_deref()
        .and_then(|path| match read(path, &grants) {
            Ok(value) => value,
            Err(error) => {
                warn_config(path, &error);
                None
            }
        });
    let same_path = global_path.as_ref().is_some_and(|path| {
        path == &local_path
            || matches!((fs::canonicalize(path), fs::canonicalize(&local_path)),
            (Ok(global), Ok(local)) if global == local)
    });
    let local = if same_path {
        None
    } else {
        match read(&local_path, &grants) {
            Ok(value) => value,
            Err(error) => {
                warn_config(&local_path, &error);
                None
            }
        }
    };
    let registration = match (global, local) {
        (None, None) => return Ok(None),
        (Some(value), None) | (None, Some(value)) => value,
        (Some(mut global), Some(local)) => {
            let global_path = global_path
                .as_ref()
                .ok_or_else(|| "AHP global registration has no source path".to_owned())?;
            if global["protocolVersion"] != local["protocolVersion"] {
                let error = format!(
                    "protocolVersion is incompatible with {}",
                    global_path.display()
                );
                warn_config(&local_path, &error);
                return Ok(Some(AhpConfig {
                    registration: global,
                    grants,
                }));
            }
            let hooks = global["hooks"].as_array_mut().ok_or_else(|| {
                format!(
                    "{}: SDK-validated registration has no hooks array",
                    global_path.display()
                )
            })?;
            let local_hooks = local["hooks"].as_array().ok_or_else(|| {
                format!(
                    "{}: SDK-validated registration has no hooks array",
                    local_path.display()
                )
            })?;
            for hook in local_hooks {
                if let Some(index) = hooks
                    .iter()
                    .position(|existing| existing["id"] == hook["id"])
                {
                    hooks[index] = hook.clone();
                } else {
                    hooks.push(hook.clone());
                }
            }
            let local_fields = local.as_object().ok_or_else(|| {
                format!(
                    "{}: SDK-validated registration is not an object",
                    local_path.display()
                )
            })?;
            for (key, value) in local_fields {
                if key != "hooks" {
                    global[key] = value.clone();
                }
            }
            global
        }
    };
    Ok(Some(AhpConfig {
        registration,
        grants,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
    use super::*;
    use crate::ahp::AhpHost;
    use tempfile::TempDir;

    fn registration(ids: &[&str]) -> Value {
        json!({"protocolVersion":"draft","hooks":ids.iter().map(|id| json!({
            "id":id,"transport":{"type":"http","url":"https://example.com/hooks"},
            "subscriptions":[{"events":["tool.before"],"mode":"intercept","timeoutMs":1000,
                "failurePolicy":"fail-closed","content":{"default":"metadata"}}]
        })).collect::<Vec<_>>()})
    }
    fn write(root: &Path, value: &Value) -> std::path::PathBuf {
        let path = root.join(".agents/hooks.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn absent_global_local_and_disabled_home() {
        let home = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        assert!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .is_none()
        );
        let global = registration(&["org.example.global"]);
        write(home.path(), &global);
        assert_eq!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            global
        );
        for home in [None, Some(Path::new(""))] {
            assert!(load_with_home(home, root.path()).unwrap().is_none());
        }
        let local = registration(&["org.example.local"]);
        write(root.path(), &local);
        for home in [None, Some(Path::new(""))] {
            assert_eq!(
                load_with_home(home, root.path())
                    .unwrap()
                    .unwrap()
                    .registration,
                local
            );
        }
    }

    #[test]
    fn merges_whole_hooks_in_order_and_preserves_extensions_without_rewriting() {
        let home = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let mut global = registration(&[
            "org.example.first",
            "org.example.replace",
            "org.example.last",
        ]);
        global["globalExtension"] = json!({"keep":true});
        global["sharedExtension"] = json!({"global":true});
        global["hooks"][1]["globalOnly"] = json!(true);
        let mut local = registration(&["org.example.replace", "org.example.append"]);
        local["hooks"][0]["transport"]["url"] = json!("https://example.com/local");
        local["sharedExtension"] = json!({"local":true});
        local["localExtension"] = json!(42);
        let global_path = write(home.path(), &global);
        let local_path = write(root.path(), &local);
        let before = [
            fs::read(&global_path).unwrap(),
            fs::read(&local_path).unwrap(),
        ];
        let config = load_with_home(Some(home.path()), root.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            config.registration["hooks"],
            json!([
                global["hooks"][0],
                local["hooks"][0],
                global["hooks"][2],
                local["hooks"][1]
            ])
        );
        assert_eq!(
            config.registration["globalExtension"],
            global["globalExtension"]
        );
        assert_eq!(
            config.registration["sharedExtension"],
            local["sharedExtension"]
        );
        assert_eq!(
            config.registration["localExtension"],
            local["localExtension"]
        );
        assert_eq!(
            before,
            [
                fs::read(&global_path).unwrap(),
                fs::read(&local_path).unwrap()
            ]
        );
    }

    #[test]
    fn invalid_files_are_path_qualified_and_not_rewritten() {
        let root = TempDir::new().unwrap();
        let path = write(root.path(), &registration(&["org.example.test"]));
        let mut unsupported = registration(&["org.example.test"]);
        unsupported["protocolVersion"] = json!("future");
        let mut invalid = registration(&["org.example.test"]);
        invalid["hooks"][0]["subscriptions"][0]["timeoutMs"] = json!(0);
        let cases = [
            b"{broken".to_vec(),
            serde_json::to_vec(&unsupported).unwrap(),
            serde_json::to_vec(&invalid).unwrap(),
            serde_json::to_vec(&registration(&[])).unwrap(),
            serde_json::to_vec(
                &json!({"registration":registration(&["org.example.test"]),"grants":{}}),
            )
            .unwrap(),
        ];
        for bytes in cases {
            fs::write(&path, &bytes).unwrap();
            let error = read(&path, &grants().unwrap()).unwrap_err();
            assert!(load_with_home(None, root.path()).unwrap().is_none());
            assert!(error.contains(&path.display().to_string()), "{error}");
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn duplicate_ids_are_rejected_in_each_file_before_merging() {
        let home = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let duplicate = registration(&["org.example.duplicate", "org.example.duplicate"]);
        let global_path = write(home.path(), &duplicate);
        write(root.path(), &registration(&["org.example.duplicate"]));
        let error = read(&global_path, &grants().unwrap()).unwrap_err();
        assert!(error.contains("duplicate backend id"), "{error}");
        assert!(error.contains(&global_path.display().to_string()));
        assert_eq!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            registration(&["org.example.duplicate"])
        );
        write(home.path(), &registration(&["org.example.duplicate"]));
        let local_path = write(root.path(), &duplicate);
        let error = read(&local_path, &grants().unwrap()).unwrap_err();
        assert!(error.contains("duplicate backend id"), "{error}");
        assert!(error.contains(&local_path.display().to_string()));
        assert_eq!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            registration(&["org.example.duplicate"])
        );
    }

    #[test]
    fn unreadable_file_is_not_treated_as_missing() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(".agents/hooks.json");
        fs::create_dir_all(&path).unwrap();
        let error = read(&path, &grants().unwrap()).unwrap_err();
        assert!(load_with_home(None, root.path()).unwrap().is_none());
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn valid_source_survives_invalid_other_source_and_both_invalid_disable_hooks() {
        let home = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let global = registration(&["org.example.global"]);
        let local = registration(&["org.example.local"]);
        let global_path = write(home.path(), &global);
        let local_path = write(root.path(), &local);
        fs::write(&global_path, b"{broken global").unwrap();
        assert_eq!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            local
        );
        assert_eq!(fs::read(&global_path).unwrap(), b"{broken global");
        write(home.path(), &global);
        fs::write(&local_path, b"{broken local").unwrap();
        assert_eq!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            global
        );
        assert_eq!(fs::read(&local_path).unwrap(), b"{broken local");
        fs::write(&global_path, b"{broken global").unwrap();
        assert!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(&global_path).unwrap(), b"{broken global");
        assert_eq!(fs::read(&local_path).unwrap(), b"{broken local");
    }

    #[test]
    fn identical_invalid_global_and_local_source_is_skipped() {
        let root = TempDir::new().unwrap();
        let path = write(root.path(), &registration(&["org.example.once"]));
        fs::write(&path, b"{broken").unwrap();
        assert!(
            load_with_home(Some(root.path()), root.path())
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
    }

    #[test]
    fn identical_global_and_local_paths_are_not_duplicated() {
        let root = TempDir::new().unwrap();
        let value = registration(&["org.example.once"]);
        write(root.path(), &value);
        assert_eq!(
            load_with_home(Some(root.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            value
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_global_and_local_paths_are_not_duplicated() {
        let home = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let value = registration(&["org.example.once"]);
        let path = write(home.path(), &value);
        fs::create_dir(root.path().join(".agents")).unwrap();
        std::os::unix::fs::symlink(path, root.path().join(".agents/hooks.json")).unwrap();
        assert_eq!(
            load_with_home(Some(home.path()), root.path())
                .unwrap()
                .unwrap()
                .registration,
            value
        );
    }

    #[test]
    fn automatic_grants_construct_a_real_host_with_narrow_modes() {
        let root = TempDir::new().unwrap();
        let mut value = registration(&["org.example.host"]);
        value["hooks"][0]["subscriptions"].as_array_mut().unwrap().push(
            json!({"events":["session.start","model.switch.after"],"mode":"observe","content":{"default":"metadata"}}));
        write(root.path(), &value);
        let config = load_with_home(None, root.path()).unwrap().unwrap();
        assert_eq!(config.grants["session.start"], json!({"modes":["observe"]}));
        assert_eq!(
            config.grants["model.switch.after"],
            json!({"modes":["observe"]})
        );
        assert!(
            config.grants["tool.before"]["modes"]
                .as_array()
                .unwrap()
                .contains(&json!("intercept"))
        );
        assert_eq!(
            config.grants["tool.before"]["capabilities"],
            supported()["tool.before"].as_value().clone()
        );
        AhpHost::new(config).unwrap();
    }
}
