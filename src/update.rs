//! Conservative, read-only discovery followed by an explicit update plan.
use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug)]
enum Plan {
    Release(PathBuf),
    Cargo(Vec<OsString>),
}

fn refuse(message: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::other(message.into()).into()
}

pub(crate) fn run(dry_run: bool) -> Result<()> {
    let executable = env::current_exe()?.canonicalize()?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let install_dir = env::var_os("KIT_INSTALL_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|p| p.join(".local/bin")));
    let mise_installs = mise_installs_dir(
        home.as_deref(),
        env::var_os("MISE_DATA_DIR").map(PathBuf::from),
        env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        env::var_os("MISE_INSTALLS_DIR").map(PathBuf::from),
    );
    let plan = detect(
        &executable,
        install_dir.as_deref(),
        option_env!("KIT_RELEASE_BINARY") == Some("1"),
        mise_installs.as_deref(),
    )?;
    println!("Running executable: {}", executable.display());
    let mut command = match plan {
        Plan::Release(directory) => {
            println!(
                "Method: release installer\nInstall latest release into {} (checksum verified; KIT_VERSION ignored)",
                directory.display()
            );
            let mut command = Command::new("sh");
            command.args(["-c", include_str!("../install.sh")]);
            command
                .env("KIT_INSTALL_DIR", directory)
                .env("KIT_VERSION", "latest");
            command
        }
        Plan::Cargo(args) => {
            println!(
                "Method: Cargo\nCommand: cargo {}",
                args.iter()
                    .map(|s| format!("{:?}", s))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let mut command = Command::new("cargo");
            command.args(args);
            command
        }
    };
    if dry_run {
        println!("Dry run: no downloads or changes made.");
        return Ok(());
    }
    let status = command.status()?;
    if !status.success() {
        return Err(refuse(format!(
            "update command failed ({status}); installation may be unchanged; inspect the output above"
        )));
    }
    Ok(())
}

// mise v2026.7.7 uses XDG_DATA_HOME/mise even on macOS, with explicit
// MISE_DATA_DIR and MISE_INSTALLS_DIR overrides (upstream src/env.rs).
fn mise_installs_dir(
    home: Option<&Path>,
    data: Option<PathBuf>,
    xdg_data: Option<PathBuf>,
    installs: Option<PathBuf>,
) -> Option<PathBuf> {
    let directory = installs.or_else(|| {
        data.or_else(|| xdg_data.map(|p| p.join("mise")))
            .or_else(|| home.map(|p| p.join(".local/share/mise")))
            .map(|p| p.join("installs"))
    })?;
    // mise expands a leading ~/ in path environment variables.
    if let Ok(relative) = directory.strip_prefix("~") {
        home.map(|home| home.join(relative))
    } else {
        Some(directory)
    }
}

fn detect(
    executable: &Path,
    install_dir: Option<&Path>,
    release_binary: bool,
    mise_installs: Option<&Path>,
) -> Result<Plan> {
    // Managed installations take precedence over an explicit KIT_INSTALL_DIR.
    // Never overwrite a versioned mise installation or choose a config scope.
    if let Some(installs) = mise_installs.and_then(|p| p.canonicalize().ok())
        && let Ok(relative) = executable.strip_prefix(installs)
    {
        let mut components = relative.components();
        let tool = components.next();
        let version = components.next();
        if tool.is_some_and(|p| p.as_os_str() == "github-speakeasy-api-kit")
            && version.is_some()
            && components.next().is_some()
        {
            return Err(refuse(
                "Kit is managed by mise (github:speakeasy-api/kit). No changes made. In the directory whose mise config selects Kit, inspect `mise ls github:speakeasy-api/kit`, then run `mise upgrade github:speakeasy-api/kit` to update within its configured version range. An exact pin will stay pinned; use `mise upgrade --bump github:speakeasy-api/kit` only if you intend to change that config's pin. KIT_INSTALL_DIR cannot override mise ownership.",
            ));
        }
        return Err(refuse(
            "Kit is inside mise's installs directory. No changes made. Inspect `mise ls --installed --json` to verify the owning tool and config, then use `mise upgrade <owning-tool>` in that config's directory; do not overwrite a versioned installation with install.sh.",
        ));
    }
    if executable
        .components()
        .any(|p| p.as_os_str().to_string_lossy().ends_with(".app"))
    {
        return Err(refuse(
            "Kit is inside a desktop app. Update the Kit app, not its bundled executable.",
        ));
    }
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return Err(refuse(
            "Kit is running in a container. Pull the latest Kit image and recreate the container.",
        ));
    }
    // Inspect metadata adjacent to the actual executable, not whichever Cargo is on PATH.
    if let Some(bin) = executable
        .parent()
        .filter(|p| p.file_name().is_some_and(|s| s == "bin"))
        && let Some(root) = bin.parent()
        && (root.join(".crates.toml").exists() || root.join(".crates2.json").exists())
    {
        return cargo_plan(root, executable);
    }
    if executable.components().any(|p| p.as_os_str() == "Cellar") {
        return Err(refuse(
            "Kit appears to be Homebrew-managed. Verify ownership with brew list, then use brew upgrade <owning-formula>. Automatic Homebrew updates are not supported.",
        ));
    }
    // A release build marker plus an exact canonical installer destination avoids
    // treating arbitrary source builds or unrelated PATH entries as installer copies.
    if release_binary
        && let Some(directory) = install_dir
        && directory.join("kit").canonicalize().ok().as_deref() == Some(executable)
        && directory.canonicalize().ok().as_deref() == executable.parent()
    {
        return Ok(Plan::Release(directory.canonicalize()?));
    }
    Err(refuse(
        "Cannot safely determine this Kit installation method. For a source build, update the checkout and run mise run install. For a release installer, rerun install.sh (or set KIT_INSTALL_DIR to its original directory). For a desktop app or container, update the app or image. No changes made.",
    ))
}

fn cargo_plan(root: &Path, executable: &Path) -> Result<Plan> {
    let metadata = root.join(".crates.toml");
    let richer = root.join(".crates2.json");
    let richer_value: Option<serde_json::Value> = if richer.exists() {
        Some(serde_json::from_str(&fs::read_to_string(&richer)?)?)
    } else {
        None
    };
    let entries: serde_json::Map<String, serde_json::Value> = if metadata.exists() {
        let value: toml::Value = toml::from_str(&fs::read_to_string(&metadata)?)?;
        let table = value
            .get("v1")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| refuse("Invalid Cargo provenance: missing v1 table"))?;
        table
            .iter()
            .map(|(key, value)| Ok((key.clone(), serde_json::to_value(value)?)))
            .collect::<Result<_>>()?
    } else {
        richer_value
            .as_ref()
            .and_then(|v| v.get("installs"))
            .and_then(|v| v.as_object())
            .ok_or_else(|| refuse("Invalid Cargo provenance: missing installs table"))?
            .iter()
            .map(|(key, value)| (key.clone(), value.get("bins").cloned().unwrap_or_default()))
            .collect()
    };
    let owners: Vec<_> = entries
        .iter()
        .filter(|(_, bins)| {
            bins.as_array().is_some_and(|bins| {
                bins.iter().any(|b| {
                    b.as_str()
                        .is_some_and(|b| Some(std::ffi::OsStr::new(b)) == executable.file_name())
                })
            })
        })
        .collect();
    let [(package, _)] = owners.as_slice() else {
        return Err(refuse(
            "Cargo metadata does not identify a unique owner for this executable; use the original cargo install command",
        ));
    };
    let source = package
        .strip_prefix("kit ")
        .and_then(|s| s.split_once(" (").map(|(_, s)| s))
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| refuse("Cargo executable is not owned by package kit"))?;
    let mut args: Vec<OsString> = ["install", "--force", "--locked", "--root"]
        .into_iter()
        .map(Into::into)
        .collect();
    args.push(root.into());
    args.extend(["--bin".into(), "kit".into()]);
    if let Some(path) = source.strip_prefix("path+") {
        let path = url::Url::parse(path)?
            .to_file_path()
            .map_err(|()| refuse("Unsupported Cargo path URL"))?;
        if !path.join("Cargo.toml").is_file() {
            return Err(refuse(format!(
                "Original Cargo source checkout is missing: {}. Restore it or explicitly reinstall from another source.",
                path.display()
            )));
        }
        args.extend(["--path".into(), path.canonicalize()?.into_os_string()]);
    } else if let Some(git) = source.strip_prefix("git+") {
        let mut url = url::Url::parse(git)?;
        let queries: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        for (key, value) in queries {
            if !["branch", "tag", "rev"].contains(&key.as_str()) {
                return Err(refuse(
                    "Unsupported Cargo git source selector; use the original cargo install command",
                ));
            }
            args.extend([format!("--{key}").into(), value.into()]);
        }
        url.set_query(None);
        url.set_fragment(None); // Cargo's resolved commit is not an explicit revision pin.
        args.extend(["--git".into(), url.to_string().into(), "kit".into()]);
    } else if source == "registry+https://github.com/rust-lang/crates.io-index"
        || source == "registry+sparse+https://index.crates.io/"
    {
        args.extend(["--registry".into(), "crates-io".into(), "kit".into()]);
    } else {
        return Err(refuse(
            "Unsupported Cargo registry/source. Use the original cargo install command to preserve its source.",
        ));
    }
    // Cargo's richer metadata records build options. Preserve them when present.
    if let Some(value) = richer_value {
        let entry = value
            .get("installs")
            .and_then(|v| v.get(package.as_str()))
            .ok_or_else(|| {
                refuse("Cargo metadata files disagree; use the original cargo install command")
            })?;
        if entry.get("bins") != Some(&entries[package.as_str()]) {
            return Err(refuse(
                "Cargo metadata files disagree about executable ownership",
            ));
        }
        for key in ["all_features", "no_default_features"] {
            if entry.get(key).is_some_and(|value| !value.is_boolean()) {
                return Err(refuse(format!("Invalid Cargo {key} metadata")));
            }
        }
        for key in ["profile", "target"] {
            if entry.get(key).is_some_and(|value| !value.is_string()) {
                return Err(refuse(format!("Invalid Cargo {key} metadata")));
            }
        }
        if entry.get("features").is_some_and(|value| !value.is_array()) {
            return Err(refuse("Invalid Cargo features metadata"));
        }
        for (key, flag) in [
            ("all_features", "--all-features"),
            ("no_default_features", "--no-default-features"),
        ] {
            if entry.get(key).and_then(|v| v.as_bool()) == Some(true) {
                args.push(flag.into());
            }
        }
        if let Some(features) = entry.get("features").and_then(|v| v.as_array()) {
            for feature in features {
                let feature = feature
                    .as_str()
                    .ok_or_else(|| refuse("Invalid Cargo features metadata"))?;
                args.extend(["--features".into(), feature.into()]);
            }
        }
        for (key, flag) in [("profile", "--profile"), ("target", "--target")] {
            if let Some(value) = entry.get(key).and_then(|v| v.as_str()) {
                args.extend([flag.into(), value.into()]);
            }
        }
    }
    Ok(Plan::Cargo(args))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;

    fn cargo_root(source: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(root.path().join("bin/kit"), "binary").unwrap();
        fs::write(
            root.path().join(".crates.toml"),
            format!("[v1]\n'kit 0.2.1 ({source})' = ['kit']\n"),
        )
        .unwrap();
        root
    }

    fn args(root: &Path) -> Vec<OsString> {
        let Plan::Cargo(args) = cargo_plan(root, &root.join("bin/kit")).unwrap() else {
            panic!("not Cargo")
        };
        args
    }

    #[test]
    fn cargo_path_stays_local_and_missing_checkout_refuses() {
        let checkout = tempfile::tempdir().unwrap();
        fs::write(checkout.path().join("Cargo.toml"), "[package]").unwrap();
        let url = url::Url::from_directory_path(checkout.path()).unwrap();
        let root = cargo_root(&format!("path+{url}"));
        let plan = args(root.path());
        assert!(plan.windows(2).any(|a| a
            == [
                OsString::from("--path"),
                checkout.path().canonicalize().unwrap().into_os_string()
            ]));
        assert!(!plan.contains(&OsString::from("--git")));
        fs::remove_file(checkout.path().join("Cargo.toml")).unwrap();
        assert!(cargo_plan(root.path(), &root.path().join("bin/kit")).is_err());
    }

    #[test]
    fn cargo_git_preserves_selectors_not_resolved_commit() {
        for selector in ["", "?branch=release", "?tag=v1", "?rev=abc"] {
            let root = cargo_root(&format!("git+https://example.com/kit{selector}#resolved"));
            let plan = args(root.path());
            assert!(plan.contains(&OsString::from("https://example.com/kit")));
            assert!(
                !plan
                    .iter()
                    .any(|s| s.to_string_lossy().contains("resolved"))
            );
            if !selector.is_empty() {
                let (key, value) = selector[1..].split_once('=').unwrap();
                assert!(
                    plan.windows(2)
                        .any(|a| a == [OsString::from(format!("--{key}")), OsString::from(value)])
                );
            }
        }
    }

    #[test]
    fn cargo_custom_registry_and_unowned_binary_refuse() {
        let root = cargo_root("registry+https://example.com/index");
        assert!(cargo_plan(root.path(), &root.path().join("bin/kit")).is_err());
        fs::write(
            root.path().join(".crates.toml"),
            "[v1]\n'other 1.0 (registry+https://github.com/rust-lang/crates.io-index)' = ['kit']",
        )
        .unwrap();
        assert!(cargo_plan(root.path(), &root.path().join("bin/kit")).is_err());
    }

    #[test]
    fn cargo_preserves_build_options() {
        let source = "registry+https://github.com/rust-lang/crates.io-index";
        let root = cargo_root(source);
        fs::write(root.path().join(".crates2.json"), serde_json::json!({"installs": {format!("kit 0.2.1 ({source})"): {"bins": ["kit"], "features": ["tui"], "no_default_features": true, "profile": "release", "target": "aarch64-apple-darwin"}}}).to_string()).unwrap();
        let plan = args(root.path());
        fs::remove_file(root.path().join(".crates.toml")).unwrap();
        assert_eq!(plan, args(root.path()));
        for flag in [
            "--no-default-features",
            "--features",
            "tui",
            "--profile",
            "release",
            "--target",
            "aarch64-apple-darwin",
            "crates-io",
        ] {
            assert!(plan.contains(&OsString::from(flag)));
        }
    }

    #[test]
    fn source_build_and_app_refuse_even_with_explicit_destination() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("kit");
        fs::write(&executable, "source build").unwrap();
        assert!(detect(&executable, Some(root.path()), false, None).is_err());
        assert!(
            detect(
                Path::new("/Applications/Kit.app/Contents/MacOS/kit"),
                None,
                true,
                None
            )
            .unwrap_err()
            .to_string()
            .contains("desktop app")
        );
    }

    #[test]
    fn cargo_conflicting_and_malformed_metadata_refuse() {
        let source = "registry+https://github.com/rust-lang/crates.io-index";
        let root = cargo_root(source);
        for entry in [
            serde_json::json!({"bins": ["other"]}),
            serde_json::json!({"bins": ["kit"], "no_default_features": "true"}),
            serde_json::json!({"bins": ["kit"], "features": "tui"}),
        ] {
            fs::write(
                root.path().join(".crates2.json"),
                serde_json::json!({"installs": {format!("kit 0.2.1 ({source})"): entry}})
                    .to_string(),
            )
            .unwrap();
            assert!(cargo_plan(root.path(), &root.path().join("bin/kit")).is_err());
        }
    }

    #[test]
    fn release_marker_requires_exact_installer_destination() {
        if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
            return; // Container policy intentionally takes precedence.
        }
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("kit"), "release").unwrap();
        let executable = root.path().join("kit").canonicalize().unwrap();
        assert!(matches!(
            detect(&executable, Some(root.path()), true, None).unwrap(),
            Plan::Release(_)
        ));
        assert!(
            detect(
                &executable,
                Some(&root.path().join("elsewhere")),
                true,
                None
            )
            .is_err()
        );
        assert!(detect(&executable, Some(root.path()), false, None).is_err());
        #[cfg(unix)]
        {
            // A destination whose kit is merely a link to elsewhere is not an
            // installer-owned destination; it must not be silently replaced.
            let links = root.path().join("links");
            fs::create_dir(&links).unwrap();
            std::os::unix::fs::symlink(&executable, links.join("kit")).unwrap();
            assert!(detect(&executable, Some(&links), true, None).is_err());
        }
    }

    #[test]
    fn mise_directory_resolution_preserves_override_precedence() {
        let home = Path::new("/home/test");
        assert_eq!(
            mise_installs_dir(Some(home), None, None, None),
            Some(home.join(".local/share/mise/installs"))
        );
        assert_eq!(
            mise_installs_dir(Some(home), None, Some("/xdg".into()), None),
            Some("/xdg/mise/installs".into())
        );
        assert_eq!(
            mise_installs_dir(Some(home), Some("/data".into()), Some("/xdg".into()), None),
            Some("/data/installs".into())
        );
        assert_eq!(
            mise_installs_dir(
                Some(home),
                Some("/data".into()),
                None,
                Some("/installs".into())
            ),
            Some("/installs".into())
        );
        assert_eq!(
            mise_installs_dir(Some(home), Some("~/custom".into()), None, None),
            Some(home.join("custom/installs"))
        );
        assert_eq!(mise_installs_dir(None, None, None, None), None);
    }

    #[test]
    fn mise_release_binaries_cannot_be_reclassified_by_installer_override() {
        let root = tempfile::tempdir().unwrap();
        let installs = root.path().join("custom mise data/installs");
        for suffix in ["0.2.1/kit", "0.2.2/bin/kit", "0.2.3/archive/bin/kit"] {
            let executable = installs.join("github-speakeasy-api-kit").join(suffix);
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, "release binary").unwrap();
            let executable = executable.canonicalize().unwrap();
            for marker in [false, true] {
                let error = detect(&executable, executable.parent(), marker, Some(&installs))
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("managed by mise"), "{error}");
                assert!(error.contains("mise upgrade github:speakeasy-api/kit"));
                assert!(error.contains("exact pin will stay pinned"));
            }
        }
    }

    #[test]
    fn mise_detection_requires_actual_canonical_install_root() {
        let root = tempfile::tempdir().unwrap();
        let installs = root.path().join("installs");
        fs::create_dir(&installs).unwrap();
        let unrelated = root
            .path()
            .join("not-mise/github-speakeasy-api-kit/0.2.1/kit");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "source build").unwrap();
        let unrelated = unrelated.canonicalize().unwrap();
        let error = detect(&unrelated, None, false, Some(&installs))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("managed by mise"));
        #[cfg(unix)]
        {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&installs, &alias).unwrap();
            let executable = installs.join("github-speakeasy-api-kit/0.2.1/kit");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, "release").unwrap();
            let executable = executable.canonicalize().unwrap();
            assert!(
                detect(&executable, executable.parent(), true, Some(&alias))
                    .unwrap_err()
                    .to_string()
                    .contains("managed by mise")
            );
        }
    }
}
