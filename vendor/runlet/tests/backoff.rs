use runlet::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn descriptor(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        summary: String::new(),
        input: CallSchema::positional(vec![]),
        output: Schema::INTEGER,
        execution: ExecutionPolicy::Pure,
        schema_version: "1".into(),
    }
}

/// Builds a runtime with a single `flaky` tool that fails retryably
/// `failures` times (with an optional `retry_after` hint) and then returns 7.
fn flaky_runtime(
    failures: usize,
    retry_after: Option<Duration>,
    configure: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
) -> Runtime {
    let mut registry = ToolRegistry::new();
    registry.register(descriptor("flaky")).unwrap();
    let calls = Arc::new(Mutex::new(0usize));
    let builder = Runtime::builder().registry(registry).tool("flaky", {
        move |_: &[CanonicalValue], _: &ToolContext| {
            let mut n = calls.lock().unwrap();
            *n += 1;
            if *n <= failures {
                let mut error = ToolError::new("TEMP", "try again").retryable(true);
                if let Some(after) = retry_after {
                    error = error.with_retry_after(after);
                }
                Err(error)
            } else {
                Ok(7.into())
            }
        }
    });
    configure(builder).build().unwrap()
}

fn run_boundary(runtime: &Runtime) -> (CanonicalValue, Duration) {
    let source =
        "result = boundary retry 2 { return flaky() } catch err { return -1 }\nreturn result";
    let program = runtime.compile(source).unwrap();
    let start = Instant::now();
    let execution = runtime.run(&program).unwrap();
    (execution.value, start.elapsed())
}

#[test]
fn configured_backoff_delays_each_retry_attempt() {
    // Two retryable failures: re-attempt 1 waits base, re-attempt 2 waits
    // base * factor, so the run takes at least 10ms + 20ms.
    let runtime = flaky_runtime(2, None, |b| {
        b.retry_backoff(Duration::from_millis(10), 2.0, Duration::from_secs(1))
    });
    let (value, elapsed) = run_boundary(&runtime);
    assert_eq!(value, 7.into());
    assert!(
        elapsed >= Duration::from_millis(30),
        "expected >= 30ms of backoff, got {elapsed:?}"
    );
}

#[test]
fn retry_after_overrides_computed_backoff() {
    // Computed backoff would be 1ms; the error's retry_after of 25ms wins.
    let runtime = flaky_runtime(1, Some(Duration::from_millis(25)), |b| {
        b.retry_backoff(Duration::from_millis(1), 2.0, Duration::from_secs(1))
    });
    let (value, elapsed) = run_boundary(&runtime);
    assert_eq!(value, 7.into());
    assert!(
        elapsed >= Duration::from_millis(25),
        "expected retry_after of 25ms to be honored, got {elapsed:?}"
    );
}

#[test]
fn retry_after_is_capped_by_configured_backoff_cap() {
    // A 200ms retry_after is clamped to the 5ms cap, so the run stays fast.
    let runtime = flaky_runtime(1, Some(Duration::from_millis(200)), |b| {
        b.retry_backoff(Duration::from_millis(1), 2.0, Duration::from_millis(5))
    });
    let (value, elapsed) = run_boundary(&runtime);
    assert_eq!(value, 7.into());
    assert!(
        elapsed >= Duration::from_millis(5),
        "expected the 5ms cap to still delay the retry, got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "expected retry_after to be capped to 5ms, got {elapsed:?}"
    );
}

#[test]
fn retry_after_is_honored_as_is_without_configured_backoff() {
    let runtime = flaky_runtime(1, Some(Duration::from_millis(15)), |b| b);
    let (value, elapsed) = run_boundary(&runtime);
    assert_eq!(value, 7.into());
    assert!(
        elapsed >= Duration::from_millis(15),
        "expected retry_after of 15ms to be honored, got {elapsed:?}"
    );
}

#[test]
fn unconfigured_runtime_retries_without_delay() {
    let runtime = flaky_runtime(2, None, |b| b);
    let (value, elapsed) = run_boundary(&runtime);
    assert_eq!(value, 7.into());
    assert!(
        elapsed < Duration::from_millis(50),
        "expected immediate retries, got {elapsed:?}"
    );
}
