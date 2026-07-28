use std::{fs, process::Command};

#[test]
fn runs_a_core_program_from_a_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_runlet"))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/02_operators.rnlt"
        ))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"total\":57"));
    assert!(stdout.ends_with('\n'));
}

#[test]
fn streams_graph_updates_in_graph_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_runlet"))
        .arg("graph")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/02_operators.rnlt"
        ))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("succeeded"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"total\":57"));
}

#[test]
fn reports_source_diagnostics_and_fails() {
    let path = std::env::temp_dir().join(format!("runlet-invalid-{}.rnlt", std::process::id()));
    fs::write(&path, "return missing_name\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_runlet"))
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error[RL2101]"));
    assert!(stderr.contains(":1:8:"));
}

#[test]
fn requires_a_program_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_runlet")).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr).unwrap().contains("usage:"));
}
