use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::Instant,
};

use kit::{
    cli::core::{
        AutoStart, ClientError, ClientErrorKind, ClientRequest, DiscoveryError, HttpClient,
        Invocation, Output, OutputFormat, connect_daemon, execute_with_retry, parse,
        read_discovery, render_error, render_exec_response, render_response,
    },
    runtime::daemon::{Daemon, DaemonConfig, DaemonSignal},
};

mod core_grader_worker;

fn main() -> ExitCode {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if let Some(exit) = core_grader_worker::worker_main(&arguments) {
        return exit;
    }
    if let Some(exit) = kit::executor::syntax::worker_main(&arguments) {
        return exit;
    }
    #[cfg(debug_assertions)]
    if let Some(exit) = kit::test_support::mcp_stdio_worker_main(&arguments) {
        return exit;
    }
    let output = match parse(arguments) {
        Err(error) => {
            let format = error.output_format().unwrap_or(OutputFormat::Human);
            clap_output(error.into_clap(), format)
        }
        Ok(cli) => {
            let state_root = cli
                .state_root
                .unwrap_or_else(kit::cli::core::default_state_root);
            match cli.invocation {
                Invocation::Daemon(command) => {
                    daemon(state_root, command.daemonize, cli.timeout, cli.format)
                }
                Invocation::Ui => ui(state_root, cli.auto_start, cli.timeout, cli.format),
                Invocation::Provider(command) => kit::cli::provider::execute(command, cli.format)
                    .unwrap_or_else(|error| render_error(&error, cli.format)),
                Invocation::Auth(command) => {
                    kit::cli::auth::execute(command, cli.format, cli.timeout)
                }
                Invocation::Client(request) => dispatch(
                    *request,
                    state_root,
                    cli.auto_start,
                    cli.timeout,
                    cli.format,
                ),
                Invocation::Exec(mut request) => {
                    if let Err(error) = request.read_input_source(&mut std::io::stdin().lock()) {
                        render_error(
                            &ClientError::new(ClientErrorKind::Invalid, error.to_string()),
                            cli.format,
                        )
                    } else {
                        dispatch_exec(
                            *request,
                            state_root,
                            cli.auto_start,
                            cli.timeout,
                            cli.format,
                        )
                    }
                }
                Invocation::Repo(mut request) => {
                    if let Err(error) = request.read_input_source(&mut std::io::stdin().lock()) {
                        render_error(
                            &ClientError::new(ClientErrorKind::Invalid, error.to_string()),
                            cli.format,
                        )
                    } else {
                        dispatch_repo(
                            *request,
                            state_root,
                            cli.auto_start,
                            cli.timeout,
                            cli.format,
                        )
                    }
                }
            }
        }
    };
    write_output(&output);
    ExitCode::from(output.exit_code)
}

fn clap_output(error: clap::Error, format: OutputFormat) -> Output {
    let rendered = error.to_string();
    if !error.use_stderr() {
        return Output {
            exit_code: error.exit_code() as u8,
            stdout: rendered,
            stderr: String::new(),
        };
    }
    if format == OutputFormat::Human {
        return Output {
            exit_code: error.exit_code() as u8,
            stdout: String::new(),
            stderr: rendered,
        };
    }
    let detail = rendered
        .strip_prefix("error: ")
        .unwrap_or(&rendered)
        .trim_end();
    render_error(&ClientError::new(ClientErrorKind::Invalid, detail), format)
}

fn dispatch_repo(
    request: kit::cli::repo::RepoRequest,
    state_root: PathBuf,
    auto_start: bool,
    timeout: std::time::Duration,
    format: OutputFormat,
) -> Output {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return render_error(&ClientError::internal(error.to_string()), format),
    };
    let auto_start = AutoStart {
        enabled: auto_start,
        executable,
        timeout,
    };
    let mut client = if auto_start.enabled {
        match connect_daemon(&state_root, &auto_start, |discovery| {
            HttpClient::connect(discovery, timeout)
        }) {
            Ok(connection) => connection.connection,
            Err(error) => return render_error(&error.into(), format),
        }
    } else {
        let discovery = match read_discovery(&state_root) {
            Ok(discovery) => discovery,
            Err(DiscoveryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return render_error(
                    &ClientError::unavailable("daemon is unavailable and auto-start is disabled"),
                    format,
                );
            }
            Err(error) => return render_error(&error.into(), format),
        };
        match HttpClient::connect(&discovery, timeout) {
            Ok(client) => client,
            Err(error) => return render_error(&error, format),
        }
    };
    match kit::cli::repo::execute(&mut client, request)
        .and_then(|value| render_exec_response(value, format))
    {
        Ok(output) => output,
        Err(error) => render_error(&error, format),
    }
}

fn dispatch_exec(
    request: kit::cli::exec::ExecRequest,
    state_root: PathBuf,
    auto_start: bool,
    timeout: std::time::Duration,
    format: OutputFormat,
) -> Output {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return render_error(&ClientError::internal(error.to_string()), format),
    };
    let auto_start = AutoStart {
        enabled: auto_start,
        executable,
        timeout,
    };
    let mut client = if auto_start.enabled {
        match connect_daemon(&state_root, &auto_start, |discovery| {
            HttpClient::connect(discovery, timeout)
        }) {
            Ok(connection) => connection.connection,
            Err(error) => return render_error(&error.into(), format),
        }
    } else {
        let discovery = match read_discovery(&state_root) {
            Ok(discovery) => discovery,
            Err(DiscoveryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return render_error(
                    &ClientError::unavailable("daemon is unavailable and auto-start is disabled"),
                    format,
                );
            }
            Err(error) => return render_error(&error.into(), format),
        };
        match HttpClient::connect(&discovery, timeout) {
            Ok(client) => client,
            Err(error) => return render_error(&error, format),
        }
    };
    match kit::cli::exec::execute(&mut client, request)
        .and_then(|value| render_exec_response(value, format))
    {
        Ok(output) => output,
        Err(error) => render_error(&error, format),
    }
}

fn dispatch(
    request: ClientRequest,
    state_root: PathBuf,
    auto_start: bool,
    timeout: std::time::Duration,
    format: OutputFormat,
) -> Output {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return render_error(&ClientError::internal(error.to_string()), format),
    };
    let auto_start = AutoStart {
        enabled: auto_start,
        executable,
        timeout,
    };
    let mut client = if auto_start.enabled {
        match connect_daemon(&state_root, &auto_start, |discovery| {
            HttpClient::connect(discovery, timeout)
        }) {
            Ok(connection) => connection.connection,
            Err(error) => return render_error(&error.into(), format),
        }
    } else {
        let discovery = match read_discovery(&state_root) {
            Ok(discovery) => discovery,
            Err(DiscoveryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return render_error(
                    &ClientError::unavailable("daemon is unavailable and auto-start is disabled"),
                    format,
                );
            }
            Err(error) => return render_error(&error.into(), format),
        };
        match HttpClient::connect(&discovery, timeout) {
            Ok(client) => client,
            Err(error) => return render_error(&error, format),
        }
    };
    if let ClientRequest::Query {
        query,
        stream: true,
        stream_cursor,
        ..
    } = &request
    {
        let mut stdout = std::io::stdout().lock();
        return match client.follow(query, stream_cursor.as_ref(), |line| {
            stdout.write_all(line).map_err(|error| {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    ClientError::unavailable("event stream output closed")
                } else {
                    ClientError::internal(error.to_string())
                }
            })
        }) {
            Ok(()) => success(),
            Err(error) => render_error(&error, format),
        };
    }
    match execute_with_retry(&mut client, &request, 3)
        .and_then(|response| render_response(response, format))
    {
        Ok(output) => output,
        Err(error) => render_error(&error, format),
    }
}

fn daemon(
    state_root: PathBuf,
    daemonize: bool,
    timeout: std::time::Duration,
    format: OutputFormat,
) -> Output {
    if daemonize {
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => return render_error(&ClientError::internal(error.to_string()), format),
        };
        return daemonize_process(&state_root, &executable, timeout, format);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return render_error(&ClientError::internal(error.to_string()), format),
    };
    match runtime.block_on(async {
        let signal = DaemonSignal::install().map_err(kit::runtime::daemon::DaemonError::Io)?;
        Daemon::start(DaemonConfig::new(state_root), signal)
            .await?
            .wait()
            .await
    }) {
        Ok(_) => Output {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        },
        Err(error) => render_error(&ClientError::internal(error.to_string()), format),
    }
}

fn ui(
    state_root: PathBuf,
    auto_start: bool,
    timeout: std::time::Duration,
    format: OutputFormat,
) -> Output {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return render_error(&ClientError::internal(error.to_string()), format),
    };
    let connection = match connect_daemon(
        &state_root,
        &AutoStart {
            enabled: auto_start,
            executable,
            timeout,
        },
        |discovery| HttpClient::connect(discovery, timeout),
    ) {
        Ok(connection) => connection,
        Err(error) => return render_error(&error.into(), format),
    };
    let public_url = format!("{}/ui", connection.discovery.endpoint.trim_end_matches('/'));
    let launch_url = format!("{public_url}#{}", connection.discovery.credential);
    if let Err(error) = open_browser(&launch_url) {
        return render_error(
            &ClientError::unavailable(format!("could not open the browser: {error}")),
            format,
        );
    }
    if format == OutputFormat::Human {
        Output {
            exit_code: 0,
            stdout: format!("Opened Kit UI at {public_url}\n"),
            stderr: String::new(),
        }
    } else {
        Output {
            exit_code: 0,
            stdout: format!(
                "{}\n",
                serde_json::json!({"opened": true, "url": public_url})
            ),
            stderr: String::new(),
        }
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = Command::new("xdg-open").arg(url).status()?;
    #[cfg(windows)]
    let status = Command::new("cmd")
        .args(["/C", "start", "", url])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "browser opener exited with {status}"
        )))
    }
}

fn daemonize_process(
    state_root: &Path,
    executable: &Path,
    timeout: std::time::Duration,
    format: OutputFormat,
) -> Output {
    if let Ok(discovery) = read_discovery(state_root)
        && HttpClient::connect(&discovery, timeout).is_ok()
    {
        return success();
    }

    let mut command = Command::new(executable);
    command
        .arg("daemon")
        .arg("--state-root")
        .arg(state_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: setsid is async-signal-safe and the closure performs no allocation.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return render_error(&ClientError::unavailable(error.to_string()), format),
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(discovery) = read_discovery(state_root)
            && HttpClient::connect(
                &discovery,
                timeout.min(std::time::Duration::from_millis(250)),
            )
            .is_ok()
        {
            return success();
        }
        if let Ok(Some(status)) = child.try_wait() {
            return render_error(
                &ClientError::unavailable(format!(
                    "daemonized process exited with status {:?}",
                    status.code()
                )),
                format,
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return render_error(
                &ClientError::timeout("daemon did not become ready before the timeout"),
                format,
            );
        }
        thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn success() -> Output {
    Output {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
    }
}

fn write_output(output: &Output) {
    let _ = std::io::stdout().write_all(output.stdout.as_bytes());
    let _ = std::io::stderr().write_all(output.stderr.as_bytes());
}
