#![cfg(windows)]

use std::{
    io::Read,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kit::{
    domain::{
        ids::{AttemptId, PrincipalId, ProcessId, ProjectId, RunId, TerminalId, WorkspaceId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim},
    },
    executor::{
        backends::windows_job::{
            Job, JobError, Recovery, WindowsCommand, spawn_attempt_registered,
        },
        cancel::{DurableBoundaryState, SqliteCancellationCoordinator, WorkspaceIdentity},
        process::own::{
            ProcessRecord, ProcessRegistrationContext, ProcessRegistry,
            ProcessRegistryRegistration, ProcessTerminalConfig,
        },
        process::tree::{
            BoundaryControl, BoundaryIdentity, BoundaryKind, Ownership, PersistedBoundary,
        },
        profile::{
            Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
        },
        terminal::{NativePtyDriver, PtyDriver, TerminalOwner, TerminalSize},
    },
};
use windows_sys::Win32::{
    Foundation::{GetHandleInformation, HANDLE},
    Security::SECURITY_ATTRIBUTES,
    System::{
        JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
            SetInformationJobObject,
        },
        Pipes::CreatePipe,
        SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_SET_ATTRIBUTES},
        Threading::{GetCurrentProcess, GetProcessHandleCount},
    },
};

static SERIAL: Mutex<()> = Mutex::new(());

fn ownership(fence: u64) -> Ownership {
    Ownership::new("kit-test-daemon", format!("attempt:test:fence:{fence}")).unwrap()
}

fn limits(cpu_millis: u64, memory_bytes: u64, pids: u32) -> ResourceLimits {
    ResourceLimits::new(cpu_millis, memory_bytes, pids, 1, 1, 1, 1024 * 1024, 10_000)
}

fn production_profile(resources: ResourceLimits) -> ExecutorProfile {
    ExecutorProfile::new(ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Windows,
        Architecture::X86_64,
        resources,
    ))
    .unwrap()
}

fn cmd(script: &str) -> WindowsCommand {
    WindowsCommand::new(r"C:\Windows\System32\cmd.exe")
        .arg("/d")
        .arg("/s")
        .arg("/c")
        .arg(script)
}

#[test]
fn child_is_persisted_and_job_owned_before_user_code_runs() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(1);
    let mut job = Job::create(owner.clone(), limits(5_000, 256 * 1024 * 1024, 8)).unwrap();
    let marker = std::env::temp_dir().join(format!("kit-job-suspend-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let command = cmd(&format!("echo released>\"{}\"", marker.display()));
    let mut persisted = None;
    let process = job
        .spawn(&owner, &command, None, |record: &PersistedBoundary| {
            std::thread::sleep(Duration::from_millis(50));
            assert!(!marker.exists(), "user code ran before durable persistence");
            persisted = Some(record.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        job.wait(&process, Instant::now() + Duration::from_secs(5))
            .unwrap(),
        0
    );
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(marker.exists());
    assert!(persisted.is_some());
    let _ = std::fs::remove_file(marker);
}

struct ProductionRegistry {
    calls: Arc<Mutex<Vec<&'static str>>>,
    coordinator: SqliteCancellationCoordinator,
    owner: AttemptOwnership,
}

impl ProcessRegistry for ProductionRegistry {
    fn prepared(
        &self,
        _context: ProcessRegistrationContext,
        _claim: ProcessClaim,
        boundary: &PersistedBoundary,
        _terminal: ProcessTerminalConfig,
    ) -> std::io::Result<()> {
        assert_eq!(boundary.identity.kind(), BoundaryKind::WindowsComposite);
        assert!(boundary.windows_layers().unwrap().is_some());
        assert_eq!(
            self.coordinator.boundary_state(self.owner).unwrap(),
            DurableBoundaryState::Active
        );
        self.calls.lock().unwrap().push("prepared");
        Ok(())
    }

    fn started(
        &self,
        _context: ProcessRegistrationContext,
        _record: &ProcessRecord,
    ) -> std::io::Result<()> {
        self.calls.lock().unwrap().push("started");
        Ok(())
    }

    fn exited(
        &self,
        _context: ProcessRegistrationContext,
        _record: &ProcessRecord,
    ) -> std::io::Result<()> {
        self.calls.lock().unwrap().push("exited");
        Ok(())
    }

    fn outcome_unknown(
        &self,
        _context: ProcessRegistrationContext,
        _process_id: ProcessId,
    ) -> std::io::Result<()> {
        self.calls.lock().unwrap().push("outcome_unknown");
        Ok(())
    }
}

struct ProductionContext {
    database: std::path::PathBuf,
    owner: AttemptOwnership,
    coordinator: SqliteCancellationCoordinator,
    calls: Arc<Mutex<Vec<&'static str>>>,
    registration: ProcessRegistryRegistration,
    workspace: WorkspaceIdentity,
}

fn production_context(tag: &str) -> ProductionContext {
    let database = std::env::temp_dir().join(format!(
        "kit-windows-{tag}-{}-{}",
        std::process::id(),
        AttemptId::generate().unwrap()
    ));
    let owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        FencingToken::new(1),
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE attempt_driver_claims (
               run_id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL UNIQUE,
               principal_id TEXT NOT NULL, fence INTEGER NOT NULL,
               lease_version INTEGER NOT NULL, expires_at_unix_micros INTEGER NOT NULL,
               quiescent INTEGER NOT NULL
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO attempt_driver_claims
               (run_id, attempt_id, principal_id, fence, lease_version,
                expires_at_unix_micros, quiescent)
             VALUES (?1, ?2, ?3, ?4, 1,
               CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER) + 60000000,
               0)",
            rusqlite::params![
                RunId::generate().unwrap().to_string(),
                owner.attempt_id.to_string(),
                owner.principal_id.to_string(),
                i64::try_from(owner.fencing_token.get()).unwrap(),
            ],
        )
        .unwrap();
    drop(connection);
    let coordinator = SqliteCancellationCoordinator::new(&database);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(ProductionRegistry {
        calls: calls.clone(),
        coordinator: coordinator.clone(),
        owner,
    });
    let registration = ProcessRegistryRegistration::new(
        registry,
        ProcessRegistrationContext {
            project_id: ProjectId::generate().unwrap(),
            principal_id: owner.principal_id,
        },
    );
    let workspace = WorkspaceIdentity::new(
        WorkspaceId::generate().unwrap(),
        format!("windows-{tag}-acquisition"),
        format!("windows-{tag}-revision"),
    )
    .unwrap();
    ProductionContext {
        database,
        owner,
        coordinator,
        calls,
        registration,
        workspace,
    }
}

#[test]
fn production_attempt_launch_uses_job_and_registers_cancellation_before_resume() {
    let _guard = SERIAL.lock().unwrap();
    let context = production_context("production");
    let marker = std::env::temp_dir().join(format!(
        "kit-windows-production-marker-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let command = cmd(&format!(
        "echo launched>\"{}\" & (for /L %i in (1,1,20000) do @echo stdout-flood-%i) & (for /L %i in (1,1,20000) do @echo stderr-flood-%i 1>&2)",
        marker.display()
    ));
    let mut persisted = None;
    let resources = limits(5_000, 256 * 1024 * 1024, 4);
    let profile = production_profile(resources);
    let mut process = spawn_attempt_registered(
        &profile,
        &command,
        resources,
        context.owner,
        &context.coordinator,
        context.workspace,
        context.registration,
        |boundary: &PersistedBoundary| {
            assert!(!marker.exists());
            persisted = Some(boundary.clone());
            Ok(())
        },
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    let output = process
        .wait(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(output.stdout.original_bytes() > 64 * 1024);
    assert!(output.stderr.original_bytes() > 64 * 1024);
    assert!(output.retained_bytes() <= 1024 * 1024);
    assert!(marker.exists());
    assert!(persisted.is_some());
    assert_eq!(
        *context.calls.lock().unwrap(),
        ["prepared", "started", "exited"]
    );
    assert_eq!(
        context.coordinator.boundary_state(context.owner).unwrap(),
        DurableBoundaryState::Quiescent
    );
    let _ = std::fs::remove_file(marker);
    let _ = std::fs::remove_file(context.database);
}

#[test]
fn production_timeout_terminates_job_and_terminalizes_registries() {
    let _guard = SERIAL.lock().unwrap();
    let context = production_context("timeout");
    let resources = ResourceLimits::new(30_000, 256 * 1024 * 1024, 8, 1, 1, 1, 1024 * 1024, 100);
    let profile = production_profile(resources);
    let mut process = spawn_attempt_registered(
        &profile,
        &cmd("start /b cmd /d /c ping -n 30 127.0.0.1 ^>nul & ping -n 30 127.0.0.1 >nul"),
        resources,
        context.owner,
        &context.coordinator,
        context.workspace,
        context.registration,
        |_: &PersistedBoundary| Ok(()),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();

    assert!(matches!(
        process.wait(Instant::now() + Duration::from_secs(5)),
        Err(JobError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut
    ));
    assert_eq!(process.active_processes().unwrap(), 0);
    assert_eq!(
        *context.calls.lock().unwrap(),
        ["prepared", "started", "exited"]
    );
    assert_eq!(
        context.coordinator.boundary_state(context.owner).unwrap(),
        DurableBoundaryState::Quiescent
    );
    drop(process);
    let _ = std::fs::remove_file(context.database);
}

#[test]
fn explicit_handle_list_excludes_unrelated_inheritable_handles() {
    if let Some(raw) = std::env::var_os("KIT_WINDOWS_UNRELATED_HANDLE_CHILD") {
        let handle = raw.to_string_lossy().parse::<usize>().unwrap() as HANDLE;
        let mut flags = 0;
        // The numeric value is supplied by the parent, but HANDLE_LIST must exclude the object.
        assert_eq!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    assert_ne!(
        unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) },
        0
    );
    let sentinel = unsafe { OwnedHandle::from_raw_handle(read.cast()) };
    let _peer = unsafe { OwnedHandle::from_raw_handle(write.cast()) };
    let owner = ownership(10);
    let mut job = Job::create(owner.clone(), limits(5_000, 256 * 1024 * 1024, 4)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("windows_job_limits::explicit_handle_list_excludes_unrelated_inheritable_handles")
                .arg("--nocapture")
                .env(
                    "KIT_WINDOWS_UNRELATED_HANDLE_CHILD",
                    (sentinel.as_raw_handle() as usize).to_string(),
                ),
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        job.wait(&process, Instant::now() + Duration::from_secs(10))
            .unwrap(),
        0
    );
}

#[test]
fn cancellation_and_recovery_prove_zero_active_processes() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(2);
    let mut job = Job::create(owner.clone(), limits(30_000, 256 * 1024 * 1024, 8)).unwrap();
    let mut persisted = None;
    let process = job
        .spawn(
            &owner,
            &cmd("ping -n 30 127.0.0.1 >nul"),
            None,
            |record: &PersistedBoundary| {
                persisted = Some(record.clone());
                Ok(())
            },
        )
        .unwrap();
    assert!(job.active_processes().unwrap() > 0);
    let mut recovered = match Job::recover(persisted.as_ref().unwrap(), &owner).unwrap() {
        Recovery::Reopened(job) => job,
        Recovery::OutcomeUnknown { detail } => panic!("live Job was not reopened: {detail}"),
    };
    recovered
        .kill_boundary(Instant::now() + Duration::from_secs(5))
        .unwrap();
    recovered
        .wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(recovered.active_processes().unwrap(), 0);
    let _ = process
        .wait(Instant::now() + Duration::from_secs(5))
        .unwrap();
}

#[test]
fn timeout_terminates_the_whole_job_before_quiescence() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(8);
    let mut job = Job::create(owner.clone(), limits(30_000, 256 * 1024 * 1024, 8)).unwrap();
    let process = job
        .spawn(
            &owner,
            &cmd("start /b cmd /d /c ping -n 30 127.0.0.1 ^>nul & ping -n 30 127.0.0.1 >nul"),
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        process
            .wait(Instant::now() + Duration::from_millis(20))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    job.kill_boundary(Instant::now() + Duration::from_secs(5))
        .unwrap();
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(job.active_processes().unwrap(), 0);
}

#[test]
fn stale_owner_cannot_spawn_or_recover_job() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(3);
    let stale = ownership(2);
    let mut job = Job::create(owner.clone(), limits(5_000, 256 * 1024 * 1024, 4)).unwrap();
    assert!(matches!(
        job.spawn(&stale, &cmd("exit 0"), None, |_| Ok(())),
        Err(JobError::OwnershipMismatch)
    ));
    let persisted = PersistedBoundary {
        ownership: owner,
        identity: job.identity().clone(),
    };
    assert!(matches!(
        Job::recover(&persisted, &stale),
        Err(JobError::OwnershipMismatch)
    ));
}

#[test]
fn cpu_user_time_limit_enforces_with_generic_exit_evidence() {
    if std::env::var_os("KIT_WINDOWS_CPU_LIMIT_CHILD").is_some() {
        loop {
            std::hint::spin_loop();
        }
    }
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(4);
    let mut job = Job::create(owner.clone(), limits(100, 256 * 1024 * 1024, 4)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("windows_job_limits::cpu_user_time_limit_enforces_with_generic_exit_evidence")
                .arg("--nocapture")
                .env("KIT_WINDOWS_CPU_LIMIT_CHILD", "1"),
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_ne!(
        job.wait(&process, Instant::now() + Duration::from_secs(10))
            .unwrap(),
        0
    );
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
}

#[test]
fn memory_limit_enforces_without_claiming_a_specific_cause() {
    if std::env::var_os("KIT_WINDOWS_MEMORY_LIMIT_CHILD").is_some() {
        let mut allocations = Vec::new();
        loop {
            allocations.push(vec![0xff; 1024 * 1024]);
        }
    }
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(6);
    let mut job = Job::create(owner.clone(), limits(10_000, 128 * 1024 * 1024, 4)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("windows_job_limits::memory_limit_enforces_without_claiming_a_specific_cause")
                .arg("--nocapture")
                .env("KIT_WINDOWS_MEMORY_LIMIT_CHILD", "1"),
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_ne!(
        job.wait(&process, Instant::now() + Duration::from_secs(10))
            .unwrap(),
        0,
        "memory-bound child unexpectedly succeeded"
    );
    job.terminate().unwrap();
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
}

#[test]
fn daemon_handle_loss_kills_job_and_recovery_is_outcome_unknown() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(7);
    let mut job = Job::create(owner.clone(), limits(30_000, 256 * 1024 * 1024, 4)).unwrap();
    let mut persisted = None;
    let process = job
        .spawn(
            &owner,
            &cmd("ping -n 30 127.0.0.1 >nul"),
            None,
            |record: &PersistedBoundary| {
                persisted = Some(record.clone());
                Ok(())
            },
        )
        .unwrap();
    drop(job);
    let _ = process
        .wait(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(matches!(
        Job::recover(persisted.as_ref().unwrap(), &owner).unwrap(),
        Recovery::OutcomeUnknown { .. }
    ));
}

#[test]
fn recovery_rejects_changed_limit_evidence_without_touching_live_job() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(9);
    let mut job = Job::create(owner.clone(), limits(30_000, 256 * 1024 * 1024, 4)).unwrap();
    let mut persisted = None;
    let process = job
        .spawn(
            &owner,
            &cmd("ping -n 30 127.0.0.1 >nul"),
            None,
            |record: &PersistedBoundary| {
                persisted = Some(record.clone());
                Ok(())
            },
        )
        .unwrap();
    let mut substituted = persisted.unwrap();
    let mut evidence = substituted
        .identity
        .start_identity()
        .split(':')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    evidence[4] = (evidence[4].parse::<usize>().unwrap() + 1).to_string();
    substituted.identity = BoundaryIdentity::new(
        BoundaryKind::WindowsJobObject,
        substituted.identity.locator(),
        substituted.identity.ownership_token(),
        evidence.join(":"),
    )
    .unwrap();
    assert!(matches!(
        Job::recover(&substituted, &owner),
        Err(JobError::OutcomeUnknown(_))
    ));
    assert!(job.active_processes().unwrap() > 0);
    job.terminate().unwrap();
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
    let _ = process.wait(Instant::now() + Duration::from_secs(5));
}

#[test]
fn recovery_rejects_breakaway_flag_mutation() {
    let _guard = SERIAL.lock().unwrap();
    let owner = ownership(11);
    let mut job = Job::create(owner.clone(), limits(30_000, 256 * 1024 * 1024, 4)).unwrap();
    let mut persisted = None;
    let process = job
        .spawn(
            &owner,
            &cmd("ping -n 30 127.0.0.1 >nul"),
            None,
            |record: &PersistedBoundary| {
                persisted = Some(record.clone());
                Ok(())
            },
        )
        .unwrap();
    let name = persisted
        .as_ref()
        .unwrap()
        .identity
        .locator()
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let raw = unsafe {
        OpenJobObjectW(
            JOB_OBJECT_QUERY | JOB_OBJECT_SET_ATTRIBUTES,
            0,
            name.as_ptr(),
        )
    };
    assert!(!raw.is_null());
    let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    assert_ne!(
        unsafe {
            QueryInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&mut information as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&information) as u32,
                std::ptr::null_mut(),
            )
        },
        0
    );
    information.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&information) as u32,
            )
        },
        0
    );
    assert!(matches!(
        Job::recover(persisted.as_ref().unwrap(), &owner),
        Err(JobError::OutcomeUnknown(_))
    ));
    job.terminate().unwrap();
    job.wait_and_reap(Instant::now() + Duration::from_secs(5))
        .unwrap();
    let _ = process.wait(Instant::now() + Duration::from_secs(5));
}

#[test]
fn conpty_binary_io_resize_and_teardown() {
    let _guard = SERIAL.lock().unwrap();
    let driver = NativePtyDriver::new();
    driver.ensure_available().unwrap();
    let terminal_id = TerminalId::generate().unwrap();
    let owner_record = TerminalOwner {
        project_id: ProjectId::generate().unwrap(),
        process_id: ProcessId::generate().unwrap(),
        attempt_id: AttemptId::generate().unwrap(),
        principal_id: PrincipalId::generate().unwrap(),
        process_fence: kit::domain::lifecycle::FencingToken::new(1),
        boundary_id: "windows-conpty-test".to_owned(),
    };
    driver
        .allocate(
            terminal_id,
            &owner_record,
            TerminalSize::new(80, 24).unwrap(),
        )
        .unwrap();
    let binding = driver.binding(terminal_id).unwrap();
    let mut output = binding.output_reader().unwrap();
    binding.resize(TerminalSize::new(100, 40).unwrap()).unwrap();

    let owner = ownership(5);
    let mut job = Job::create(owner.clone(), limits(5_000, 256 * 1024 * 1024, 4)).unwrap();
    let process = job
        .spawn(
            &owner,
            &WindowsCommand::new(r"C:\Windows\System32\cmd.exe")
                .arg("/d")
                .arg("/q")
                .arg("/k"),
            Some(&binding),
            |_| Ok(()),
        )
        .unwrap();
    binding.write_all(b"echo conpty-ok\r\nexit\r\n").unwrap();
    assert_eq!(
        job.wait(&process, Instant::now() + Duration::from_secs(5))
            .unwrap(),
        0
    );
    driver.interrupt(terminal_id).unwrap();
    drop(binding);
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).unwrap();
    assert!(bytes.windows(9).any(|window| window == b"conpty-ok"));
    assert_eq!(job.active_processes().unwrap(), 0);
}

#[test]
fn concurrent_conpty_resize_and_interrupt_do_not_race_handle_close() {
    let _guard = SERIAL.lock().unwrap();
    let driver = Arc::new(NativePtyDriver::new());
    driver.ensure_available().unwrap();
    let terminal_id = TerminalId::generate().unwrap();
    let owner = TerminalOwner {
        project_id: ProjectId::generate().unwrap(),
        process_id: ProcessId::generate().unwrap(),
        attempt_id: AttemptId::generate().unwrap(),
        principal_id: PrincipalId::generate().unwrap(),
        process_fence: kit::domain::lifecycle::FencingToken::new(1),
        boundary_id: "windows-conpty-resize-interrupt".to_owned(),
    };
    driver
        .allocate(terminal_id, &owner, TerminalSize::new(80, 24).unwrap())
        .unwrap();
    let resizing = driver.clone();
    let thread = std::thread::spawn(move || {
        for columns in 81..200 {
            if resizing
                .resize(terminal_id, TerminalSize::new(columns, 24).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    driver.interrupt(terminal_id).unwrap();
    thread.join().unwrap();
    assert!(driver.binding(terminal_id).is_err());
}

#[test]
fn conpty_allocation_and_teardown_do_not_leak_handles() {
    let _guard = SERIAL.lock().unwrap();
    let before = handle_count();
    let owner = TerminalOwner {
        project_id: ProjectId::generate().unwrap(),
        process_id: ProcessId::generate().unwrap(),
        attempt_id: AttemptId::generate().unwrap(),
        principal_id: PrincipalId::generate().unwrap(),
        process_fence: kit::domain::lifecycle::FencingToken::new(1),
        boundary_id: "windows-conpty-leak-test".to_owned(),
    };
    let driver = NativePtyDriver::new();
    driver.ensure_available().unwrap();
    for _ in 0..32 {
        let terminal_id = TerminalId::generate().unwrap();
        driver
            .allocate(terminal_id, &owner, TerminalSize::new(80, 24).unwrap())
            .unwrap();
        driver.interrupt(terminal_id).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle_count() != before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(handle_count(), before);
}

fn handle_count() -> u32 {
    let mut count = 0;
    // SAFETY: GetCurrentProcess returns a process pseudo-handle and count is writable.
    assert_ne!(
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
        0
    );
    count
}
