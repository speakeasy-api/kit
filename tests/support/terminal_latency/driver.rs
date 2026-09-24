//! Test-only entry point: real TUI, fake ACP peer at the process boundary.
use std::{os::unix::process::CommandExt, path::Path};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("serve") {
        return Err(std::process::Command::new(std::env::var("PROBE_PYTHON")?)
            .arg(std::env::var("PROBE_SCRIPT")?)
            .arg("--agent")
            .args(std::env::args().skip(2))
            .exec()
            .into());
    }
    kit::tui::run(
        Path::new(&std::env::var("PROBE_ROOT")?),
        "latency-fixture",
        kit::ProviderKind::OpenRouter,
        None,
        None,
        &Default::default(),
        &Default::default(),
        None,
        false,
    )
    .await
}
