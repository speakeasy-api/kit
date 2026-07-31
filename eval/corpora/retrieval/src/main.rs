use kit_retrieval_eval::{
    cleanup_failed_run, prepare, refresh_frozen, run_canary, run_local, run_trusted, run_worker,
    run_worker_startup_probe, verify_with_vendor,
};
use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let command = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or(
            "usage: w07-retrieval <prepare VENDOR_DIR|verify|run-local VENDOR_DIR|run-trusted>",
        )?;
    match command.as_str() {
        "prepare" => {
            let vendor = arguments
                .next()
                .map(PathBuf::from)
                .ok_or("prepare requires a temporary cargo vendor --versioned-dirs directory")?;
            if arguments.next().is_some() {
                return Err("prepare accepts exactly one vendor directory".into());
            }
            prepare(&vendor)?;
        }
        "verify" => {
            let vendor = arguments.next().map(PathBuf::from);
            if arguments.next().is_some() {
                return Err("verify accepts at most one measured VENDOR_DIR".into());
            }
            verify_with_vendor(vendor.as_deref())?;
        }
        "refresh-frozen" => {
            if arguments.next().is_some() {
                return Err("refresh-frozen accepts no arguments".into());
            }
            refresh_frozen()?;
        }
        "cleanup-failed" => {
            if arguments.next().is_some() {
                return Err("cleanup-failed accepts no arguments".into());
            }
            cleanup_failed_run()?;
        }
        "run-local" => {
            let vendor = arguments
                .next()
                .map(PathBuf::from)
                .ok_or("run-local requires a cargo vendor --locked --versioned-dirs directory")?;
            if arguments.next().is_some() {
                return Err("run-local accepts exactly one vendor directory".into());
            }
            run_local(&vendor)?;
        }
        "canary" => {
            let vendor = arguments
                .next()
                .map(PathBuf::from)
                .ok_or("canary requires a cargo vendor --locked --versioned-dirs directory")?;
            if arguments.next().is_some() {
                return Err("canary accepts exactly one vendor directory".into());
            }
            run_canary(&vendor)?;
        }
        "run-trusted" => {
            if arguments.next().is_some() {
                return Err("run-trusted accepts no arguments".into());
            }
            run_trusted()?;
        }
        "worker" => {
            let paths = arguments.map(PathBuf::from).collect::<Vec<_>>();
            if paths.len() != 6 {
                return Err("invalid hidden worker invocation".into());
            }
            run_worker(
                &paths[0], &paths[1], &paths[2], &paths[3], &paths[4], &paths[5],
            )?;
        }
        "worker-startup-probe" => {
            let paths = arguments.map(PathBuf::from).collect::<Vec<_>>();
            if paths.len() != 2 {
                return Err("invalid hidden worker startup probe invocation".into());
            }
            run_worker_startup_probe(&paths[0], &paths[1])?;
        }
        _ => return Err(format!("unknown command: {command}").into()),
    }
    Ok(())
}
