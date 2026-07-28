use std::{
    fmt, io,
    time::{Duration, Instant},
};

const IDENTITY_FIELD_LIMIT: usize = 4 * 1024;
const START_IDENTITY_LIMIT: usize = 8 * 1024;
const PERSISTED_RECORD_LIMIT: usize = 32 * 1024;

/// The daemon service and attempt that are allowed to control a process boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ownership {
    daemon_service: String,
    attempt: String,
}

impl Ownership {
    pub fn new(
        daemon_service: impl Into<String>,
        attempt: impl Into<String>,
    ) -> Result<Self, Error> {
        let ownership = Self {
            daemon_service: daemon_service.into(),
            attempt: attempt.into(),
        };
        if valid_value(&ownership.daemon_service) && valid_value(&ownership.attempt) {
            Ok(ownership)
        } else {
            Err(Error::InvalidIdentity)
        }
    }

    pub fn daemon_service(&self) -> &str {
        &self.daemon_service
    }

    pub fn attempt(&self) -> &str {
        &self.attempt
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryKind {
    LinuxCgroupV2,
    Container,
    MacOsProcessGroup,
    WindowsJobObject,
    WindowsContainerOrVm,
    WindowsComposite,
}

/// Durable identity supplied and verified by the boundary owner.
///
/// `start_identity` is a kernel/runtime identity (for example cgroup device+inode,
/// container invocation digest, or process start token), not merely a PID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryIdentity {
    kind: BoundaryKind,
    locator: String,
    ownership_token: String,
    start_identity: String,
}

impl BoundaryIdentity {
    pub fn new(
        kind: BoundaryKind,
        locator: impl Into<String>,
        ownership_token: impl Into<String>,
        start_identity: impl Into<String>,
    ) -> Result<Self, Error> {
        let identity = Self {
            kind,
            locator: locator.into(),
            ownership_token: ownership_token.into(),
            start_identity: start_identity.into(),
        };
        let (field_limit, start_limit) = if matches!(
            identity.kind,
            BoundaryKind::WindowsJobObject
                | BoundaryKind::WindowsContainerOrVm
                | BoundaryKind::WindowsComposite
        ) {
            (IDENTITY_FIELD_LIMIT, START_IDENTITY_LIMIT)
        } else {
            (1024, 1024)
        };
        if valid_identity_field(&identity.locator, field_limit)
            && valid_identity_field(&identity.ownership_token, field_limit)
            && valid_identity_field(&identity.start_identity, start_limit)
        {
            Ok(identity)
        } else {
            Err(Error::InvalidIdentity)
        }
    }

    pub const fn kind(&self) -> BoundaryKind {
        self.kind
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn ownership_token(&self) -> &str {
        &self.ownership_token
    }

    pub fn start_identity(&self) -> &str {
        &self.start_identity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedBoundary {
    pub ownership: Ownership,
    pub identity: BoundaryIdentity,
}

impl PersistedBoundary {
    /// A strict, line-safe record suitable for writing before a child is released.
    pub fn encode(&self) -> String {
        format!(
            "v1\n{}\n{}\n{}\n{}\n{}\n{}\n",
            hex(self.ownership.daemon_service.as_bytes()),
            hex(self.ownership.attempt.as_bytes()),
            kind_name(self.identity.kind),
            hex(self.identity.locator.as_bytes()),
            hex(self.identity.ownership_token.as_bytes()),
            hex(self.identity.start_identity.as_bytes()),
        )
    }

    pub fn decode(record: &str) -> Result<Self, Error> {
        if record.len() > PERSISTED_RECORD_LIMIT {
            return Err(Error::InvalidIdentity);
        }
        let fields = record.lines().collect::<Vec<_>>();
        if fields.len() != 7 || fields[0] != "v1" {
            return Err(Error::InvalidIdentity);
        }
        let ownership = Ownership::new(unhex(fields[1])?, unhex(fields[2])?)?;
        let kind = match fields[3] {
            "linux-cgroup-v2" => BoundaryKind::LinuxCgroupV2,
            "container" => BoundaryKind::Container,
            "macos-process-group" => BoundaryKind::MacOsProcessGroup,
            "windows-job-object" => BoundaryKind::WindowsJobObject,
            "windows-container-or-vm" => BoundaryKind::WindowsContainerOrVm,
            "windows-composite" => BoundaryKind::WindowsComposite,
            _ => return Err(Error::InvalidIdentity),
        };
        let identity = BoundaryIdentity::new(
            kind,
            unhex(fields[4])?,
            unhex(fields[5])?,
            unhex(fields[6])?,
        )?;
        let persisted = Self {
            ownership,
            identity,
        };
        persisted.windows_layers()?;
        Ok(persisted)
    }

    pub fn windows_composite(
        ownership: Ownership,
        job: BoundaryIdentity,
        isolation: BoundaryIdentity,
    ) -> Result<Self, Error> {
        if job.kind() != BoundaryKind::WindowsJobObject
            || isolation.kind() != BoundaryKind::WindowsContainerOrVm
        {
            return Err(Error::InvalidIdentity);
        }
        let layers = encode_windows_layers(&job, &isolation);
        let token = windows_composite_token(&ownership, &layers);
        Ok(Self {
            ownership,
            identity: BoundaryIdentity::new(
                BoundaryKind::WindowsComposite,
                format!("{}+{}", job.locator(), isolation.locator()),
                token,
                layers,
            )?,
        })
    }

    /// Returns both authenticated layers. A malformed, partial, or modified
    /// composite is rejected rather than being treated as a Job-only boundary.
    pub fn windows_layers(&self) -> Result<Option<(BoundaryIdentity, BoundaryIdentity)>, Error> {
        if self.identity.kind() != BoundaryKind::WindowsComposite {
            return Ok(None);
        }
        let layers = self.identity.start_identity();
        if self.identity.ownership_token() != windows_composite_token(&self.ownership, layers) {
            return Err(Error::InvalidIdentity);
        }
        let fields = decode_canonical_fields(layers, "v2", 8)?;
        let job = decode_nested_identity(&fields[..4])?;
        let isolation = decode_nested_identity(&fields[4..])?;
        if job.kind() != BoundaryKind::WindowsJobObject
            || isolation.kind() != BoundaryKind::WindowsContainerOrVm
        {
            return Err(Error::InvalidIdentity);
        }
        Ok(Some((job, isolation)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Containment {
    /// A cgroup, container, or equivalent boundary that contains session escape.
    Complete,
    /// A macOS process group. Descendants can escape it with `setsid`.
    ProcessGroupOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostSupport {
    Supported {
        kind: BoundaryKind,
        containment: Containment,
    },
    Unavailable {
        reason: String,
    },
    Delegated {
        implementation_unit: &'static str,
    },
}

impl HostSupport {
    /// Reports only primitives this module can honestly enforce by itself.
    pub fn trusted_local(require_complete_containment: bool) -> Self {
        if cfg!(target_os = "macos") {
            if require_complete_containment {
                Self::Unavailable {
                    reason: "macOS process groups cannot contain descendants that call setsid"
                        .to_owned(),
                }
            } else {
                Self::Supported {
                    kind: BoundaryKind::MacOsProcessGroup,
                    containment: Containment::ProcessGroupOnly,
                }
            }
        } else if cfg!(target_os = "linux") {
            Self::Unavailable {
                reason: "Linux execution requires a probed cgroup-v2 or container helper boundary"
                    .to_owned(),
            }
        } else if cfg!(target_os = "windows") {
            Self::Supported {
                kind: BoundaryKind::WindowsJobObject,
                containment: Containment::Complete,
            }
        } else {
            Self::Unavailable {
                reason: "no whole-process boundary is implemented for this host".to_owned(),
            }
        }
    }

    /// Called only after the trusted backend has probed its cgroup/container primitive.
    pub fn trusted_helper(identity: &BoundaryIdentity) -> Result<Self, Error> {
        match identity.kind {
            BoundaryKind::LinuxCgroupV2
            | BoundaryKind::Container
            | BoundaryKind::WindowsContainerOrVm => Ok(Self::Supported {
                kind: identity.kind,
                containment: Containment::Complete,
            }),
            _ => Err(Error::Unavailable {
                detail: "trusted helper must own a cgroup-v2 or container boundary".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inspection {
    pub identity: BoundaryIdentity,
    /// Exact count from the owning primitive. `None` is not proof of quiescence.
    pub survivors: Option<u32>,
    /// True only when the primitive says no process can remain in the boundary.
    pub quiescent: bool,
}

/// Implemented by a trusted cgroup/container helper or the macOS implementation below.
/// Killing a PID list is not a valid implementation of this contract.
pub trait BoundaryControl: Send {
    fn identity(&self) -> &BoundaryIdentity;
    fn containment(&self) -> Containment;
    fn release(&mut self, deadline: Instant) -> io::Result<()>;
    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()>;
    fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()>;
    fn inspect(&mut self, deadline: Instant) -> io::Result<Inspection>;

    /// Binds a suspended Windows Job root into a container or VM boundary.
    fn bind_windows_root(&mut self, _job: &BoundaryIdentity, _deadline: Instant) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "boundary has no trusted Windows root-binding contract",
        ))
    }
}

impl<T: BoundaryControl + ?Sized> BoundaryControl for Box<T> {
    fn identity(&self) -> &BoundaryIdentity {
        (**self).identity()
    }

    fn containment(&self) -> Containment {
        (**self).containment()
    }

    fn release(&mut self, deadline: Instant) -> io::Result<()> {
        (**self).release(deadline)
    }

    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()> {
        (**self).kill_boundary(deadline)
    }

    fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()> {
        (**self).wait_and_reap(deadline)
    }

    fn inspect(&mut self, deadline: Instant) -> io::Result<Inspection> {
        (**self).inspect(deadline)
    }

    fn bind_windows_root(&mut self, job: &BoundaryIdentity, deadline: Instant) -> io::Result<()> {
        (**self).bind_windows_root(job, deadline)
    }
}

/// Success means both the record and its parent-directory entry are durable.
pub trait BoundaryPersistence {
    fn persist(&mut self, boundary: &PersistedBoundary) -> io::Result<()>;
}

impl<F> BoundaryPersistence for F
where
    F: FnMut(&PersistedBoundary) -> io::Result<()>,
{
    fn persist(&mut self, boundary: &PersistedBoundary) -> io::Result<()> {
        self(boundary)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Running,
    Cancelling,
    Quiescent,
    OutcomeUnknown,
    NotQuiescent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cancellation {
    pub survivors: u32,
    pub state: LifecycleState,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    InvalidIdentity,
    OwnershipMismatch,
    BoundaryIdentityMismatch,
    Unavailable {
        detail: String,
    },
    OutcomeUnknown {
        detail: String,
    },
    NotQuiescent {
        survivors: Option<u32>,
        detail: String,
    },
    StartFailed {
        detail: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity => formatter.write_str("invalid process-boundary identity"),
            Self::OwnershipMismatch => formatter.write_str("process-boundary ownership mismatch"),
            Self::BoundaryIdentityMismatch => {
                formatter.write_str("process-boundary start identity or token mismatch")
            }
            Self::Unavailable { detail } => {
                write!(formatter, "process boundary unavailable: {detail}")
            }
            Self::OutcomeUnknown { detail } => write!(formatter, "outcome_unknown: {detail}"),
            Self::NotQuiescent { survivors, detail } => {
                write!(
                    formatter,
                    "not_quiescent ({survivors:?} survivors): {detail}"
                )
            }
            Self::StartFailed { detail } => {
                write!(formatter, "process boundary start failed: {detail}")
            }
        }
    }
}

impl std::error::Error for Error {}

pub struct ProcessTree<C: BoundaryControl> {
    persisted: PersistedBoundary,
    control: C,
    state: LifecycleState,
}

impl<C: BoundaryControl> ProcessTree<C> {
    pub fn start(
        ownership: Ownership,
        mut control: C,
        mut persistence: impl BoundaryPersistence,
        deadline: Instant,
    ) -> Result<Self, Error> {
        if control.containment() != Containment::Complete {
            return Err(Error::Unavailable {
                detail: "durable process ownership requires complete containment".to_owned(),
            });
        }
        let persisted = PersistedBoundary {
            ownership,
            identity: control.identity().clone(),
        };
        if let Err(error) = persistence.persist(&persisted) {
            cleanup_failed_start(&mut control, deadline)?;
            return Err(Error::StartFailed {
                detail: format!("durable boundary persistence failed: {error}"),
            });
        }
        if let Err(error) = control.release(deadline) {
            cleanup_failed_start(&mut control, deadline)?;
            return Err(Error::StartFailed {
                detail: format!("persisted boundary release failed: {error}"),
            });
        }
        Ok(Self {
            persisted,
            control,
            state: LifecycleState::Running,
        })
    }

    /// Rebinds a daemon after a crash only when the persisted start identity and token match.
    pub fn recover(persisted: PersistedBoundary, control: C) -> Result<Self, Error> {
        if control.containment() != Containment::Complete
            || persisted.identity.kind() == BoundaryKind::MacOsProcessGroup
        {
            return Err(Error::Unavailable {
                detail: "persisted recovery requires a complete reconstructible boundary"
                    .to_owned(),
            });
        }
        if &persisted.identity != control.identity() {
            return Err(Error::BoundaryIdentityMismatch);
        }
        Ok(Self {
            persisted,
            control,
            state: LifecycleState::Running,
        })
    }

    pub fn persisted(&self) -> &PersistedBoundary {
        &self.persisted
    }

    pub const fn state(&self) -> LifecycleState {
        self.state
    }

    pub fn finish(
        &mut self,
        ownership: &Ownership,
        deadline: Instant,
    ) -> Result<Cancellation, Error> {
        if ownership != &self.persisted.ownership {
            return Err(Error::OwnershipMismatch);
        }
        if self.state == LifecycleState::Quiescent {
            return Ok(Cancellation {
                survivors: 0,
                state: self.state,
            });
        }
        let waited = self.control.wait_and_reap(deadline);
        let inspection = checked_inspection(
            self.control.inspect(deadline),
            &self.persisted.identity,
            &mut self.state,
        )?;
        if inspection.survivors != Some(0) || !inspection.quiescent {
            self.state = LifecycleState::NotQuiescent;
            return Err(Error::NotQuiescent {
                survivors: inspection.survivors,
                detail: "boundary completion did not prove zero survivors".to_owned(),
            });
        }
        if let Err(error) = waited {
            self.state = LifecycleState::OutcomeUnknown;
            return Err(Error::OutcomeUnknown {
                detail: format!("boundary completion wait was not confirmed: {error}"),
            });
        }
        self.state = LifecycleState::Quiescent;
        Ok(Cancellation {
            survivors: 0,
            state: self.state,
        })
    }

    pub fn cancel(
        &mut self,
        ownership: &Ownership,
        deadline: Instant,
    ) -> Result<Cancellation, Error> {
        if ownership != &self.persisted.ownership {
            return Err(Error::OwnershipMismatch);
        }
        if self.state == LifecycleState::Quiescent {
            return Ok(Cancellation {
                survivors: 0,
                state: self.state,
            });
        }
        self.state = LifecycleState::Cancelling;

        // Always attempt kill, reap, and inspection. A clean scan cannot turn an
        // unconfirmed kill/reap into success.
        let killed = self.control.kill_boundary(deadline);
        let reaped = self.control.wait_and_reap(deadline);
        let inspected = self.control.inspect(deadline);

        let inspection = checked_inspection(inspected, &self.persisted.identity, &mut self.state)?;

        if inspection.survivors != Some(0) || !inspection.quiescent {
            self.state = LifecycleState::NotQuiescent;
            return Err(Error::NotQuiescent {
                survivors: inspection.survivors,
                detail: "boundary inspection did not prove zero survivors".to_owned(),
            });
        }
        if let Err(error) = killed {
            self.state = LifecycleState::OutcomeUnknown;
            return Err(Error::OutcomeUnknown {
                detail: format!("whole-boundary kill was not confirmed: {error}"),
            });
        }
        if let Err(error) = reaped {
            self.state = LifecycleState::OutcomeUnknown;
            return Err(Error::OutcomeUnknown {
                detail: format!("boundary wait/reap was not confirmed: {error}"),
            });
        }

        self.state = LifecycleState::Quiescent;
        Ok(Cancellation {
            survivors: 0,
            state: self.state,
        })
    }

    pub fn reconcile_after_daemon_crash(
        persisted: PersistedBoundary,
        control: C,
        deadline: Instant,
    ) -> Result<Cancellation, Error> {
        let ownership = persisted.ownership.clone();
        let mut tree = Self::recover(persisted, control)?;
        tree.cancel(&ownership, deadline)
    }
}

impl<C: BoundaryControl> Drop for ProcessTree<C> {
    fn drop(&mut self) {
        if self.state != LifecycleState::Quiescent {
            let ownership = self.persisted.ownership.clone();
            let _ = self.cancel(&ownership, Instant::now() + Duration::from_secs(5));
        }
    }
}

fn checked_inspection(
    inspected: io::Result<Inspection>,
    identity: &BoundaryIdentity,
    state: &mut LifecycleState,
) -> Result<Inspection, Error> {
    match inspected {
        Ok(inspection) if &inspection.identity == identity => Ok(inspection),
        Ok(_) => {
            *state = LifecycleState::OutcomeUnknown;
            Err(Error::OutcomeUnknown {
                detail: "post-operation inspection identity mismatch".to_owned(),
            })
        }
        Err(error) => {
            *state = LifecycleState::OutcomeUnknown;
            Err(Error::OutcomeUnknown {
                detail: format!("post-operation inspection failed: {error}"),
            })
        }
    }
}

fn cleanup_failed_start(
    control: &mut impl BoundaryControl,
    deadline: Instant,
) -> Result<(), Error> {
    let killed = control.kill_boundary(deadline);
    let reaped = control.wait_and_reap(deadline);
    let inspected = control.inspect(deadline);
    let inspection = inspected.map_err(|error| Error::OutcomeUnknown {
        detail: format!("failed-start boundary inspection failed: {error}"),
    })?;
    if inspection.identity != *control.identity() {
        return Err(Error::OutcomeUnknown {
            detail: "failed-start boundary inspection identity mismatch".to_owned(),
        });
    }
    if inspection.survivors != Some(0) || !inspection.quiescent {
        return Err(Error::NotQuiescent {
            survivors: inspection.survivors,
            detail: "failed-start cleanup did not prove zero survivors".to_owned(),
        });
    }
    killed.map_err(|error| Error::OutcomeUnknown {
        detail: format!("failed-start boundary kill was not confirmed: {error}"),
    })?;
    reaped.map_err(|error| Error::OutcomeUnknown {
        detail: format!("failed-start boundary reap was not confirmed: {error}"),
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::{process::Child, thread, time::Duration};

    pub struct ProcessGroup {
        identity: BoundaryIdentity,
        child: Option<Child>,
        pgid: Option<i32>,
        killed: bool,
    }

    impl ProcessGroup {
        /// Spawns a cooperative trusted-local tree. This does not contain `setsid` descendants.
        #[cfg(test)]
        fn spawn(command: &mut Command, ownership_token: impl Into<String>) -> Result<Self, Error> {
            // SAFETY: setpgid is async-signal-safe and touches no Rust-managed state.
            unsafe {
                command.pre_exec(|| {
                    if setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    }
                });
            }
            let child = command.spawn().map_err(|error| Error::Unavailable {
                detail: format!("cannot spawn macOS process-group leader: {error}"),
            })?;
            let pid = child.id();
            let identity = BoundaryIdentity::new(
                BoundaryKind::MacOsProcessGroup,
                pid.to_string(),
                ownership_token,
                // Holding this unreaped Child reserves the PID until the group kill.
                format!("unreaped-child:{pid}"),
            )?;
            Ok(Self {
                identity,
                child: Some(child),
                pgid: Some(pid as i32),
                killed: false,
            })
        }

        /// A process group cannot be safely reconstructed from a persisted PID after daemon loss.
        pub fn recover(_identity: &BoundaryIdentity) -> Result<Self, Error> {
            Err(Error::Unavailable {
                detail: "macOS process-group recovery cannot bind a persisted PID to its old Child handle"
                    .to_owned(),
            })
        }
    }

    impl BoundaryControl for ProcessGroup {
        fn identity(&self) -> &BoundaryIdentity {
            &self.identity
        }

        fn containment(&self) -> Containment {
            Containment::ProcessGroupOnly
        }

        fn release(&mut self, _deadline: Instant) -> io::Result<()> {
            Ok(())
        }

        fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()> {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process-group kill deadline elapsed",
                ));
            }
            let pgid = self.pgid.ok_or_else(|| {
                io::Error::other("process-group identity was retired after leader reap")
            })?;
            // The leader is deliberately unreaped, so its PID/PGID cannot have been reused.
            let result = unsafe { kill(-pgid, SIGKILL) };
            let error = io::Error::last_os_error();
            if result == 0 || error.raw_os_error() == Some(ESRCH) {
                self.killed = true;
                Ok(())
            } else {
                Err(error)
            }
        }

        fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()> {
            if !self.killed {
                return Err(io::Error::other("process group was not killed"));
            }
            loop {
                let child = self
                    .child
                    .as_mut()
                    .ok_or_else(|| io::Error::other("process-group leader was already reaped"))?;
                if child.try_wait()?.is_some() {
                    self.child = None;
                    self.pgid = None;
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "process group did not become empty",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
        }

        fn inspect(&mut self, _deadline: Instant) -> io::Result<Inspection> {
            Err(io::Error::other(
                "macOS process-group inspection cannot safely retarget a reaped PGID",
            ))
        }
    }

    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(1);
            if !self.killed
                && let Some(pgid) = self.pgid
            {
                // The Child is still unreaped here, so this retry cannot hit a reused PGID.
                if unsafe { kill(-pgid, SIGKILL) } == 0 {
                    self.killed = true;
                }
            }
            if self.killed {
                while Instant::now() < deadline {
                    let Some(child) = self.child.as_mut() else {
                        break;
                    };
                    match child.try_wait() {
                        Ok(Some(_)) => {
                            self.child = None;
                            self.pgid = None;
                            break;
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(2)),
                        Err(_) => break,
                    }
                }
            }
        }
    }

    const SIGKILL: i32 = 9;
    const ESRCH: i32 = 3;

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    #[cfg(test)]
    use std::{os::unix::process::CommandExt, process::Command};

    #[cfg(test)]
    unsafe extern "C" {
        fn setpgid(pid: i32, pgid: i32) -> i32;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reaped_leader_retires_pgid_before_retry() {
            let mut command = Command::new("/bin/sleep");
            command.arg("30");
            let mut group = ProcessGroup::spawn(&mut command, "a".repeat(64)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            group.kill_boundary(deadline).unwrap();
            group.wait_and_reap(deadline).unwrap();
            assert!(group.pgid.is_none());
            assert!(group.kill_boundary(deadline).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::ProcessGroup as MacOsProcessGroup;

fn valid_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains(['\0', '\n', '\r'])
}

fn valid_identity_field(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.contains(['\0', '\n', '\r'])
}

fn kind_name(kind: BoundaryKind) -> &'static str {
    match kind {
        BoundaryKind::LinuxCgroupV2 => "linux-cgroup-v2",
        BoundaryKind::Container => "container",
        BoundaryKind::MacOsProcessGroup => "macos-process-group",
        BoundaryKind::WindowsJobObject => "windows-job-object",
        BoundaryKind::WindowsContainerOrVm => "windows-container-or-vm",
        BoundaryKind::WindowsComposite => "windows-composite",
    }
}

fn encode_windows_layers(job: &BoundaryIdentity, isolation: &BoundaryIdentity) -> String {
    encode_canonical_fields(
        "v2",
        &[
            kind_name(job.kind()),
            job.locator(),
            job.ownership_token(),
            job.start_identity(),
            kind_name(isolation.kind()),
            isolation.locator(),
            isolation.ownership_token(),
            isolation.start_identity(),
        ],
    )
}

fn decode_nested_identity(fields: &[&str]) -> Result<BoundaryIdentity, Error> {
    if fields.len() != 4 {
        return Err(Error::InvalidIdentity);
    }
    let kind = match fields[0] {
        "windows-job-object" => BoundaryKind::WindowsJobObject,
        "windows-container-or-vm" => BoundaryKind::WindowsContainerOrVm,
        _ => return Err(Error::InvalidIdentity),
    };
    BoundaryIdentity::new(kind, fields[1], fields[2], fields[3])
}

pub(crate) fn encode_canonical_fields(version: &str, fields: &[&str]) -> String {
    let mut encoded = String::from(version);
    encoded.push('|');
    for field in fields {
        use fmt::Write as _;
        write!(encoded, "{}:{field},", field.len()).expect("writing to a string cannot fail");
    }
    let digest = blake3::hash(encoded.as_bytes());
    encoded.push('#');
    encoded.push_str(&digest.to_hex());
    encoded
}

pub(crate) fn decode_canonical_fields<'a>(
    value: &'a str,
    version: &str,
    count: usize,
) -> Result<Vec<&'a str>, Error> {
    if value.len() > START_IDENTITY_LIMIT
        || value
            .strip_prefix(version)
            .and_then(|value| value.strip_prefix('|'))
            .is_none()
    {
        return Err(Error::InvalidIdentity);
    }
    let mut offset = version.len() + 1;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let rest = value.get(offset..).ok_or(Error::InvalidIdentity)?;
        let colon = rest.find(':').ok_or(Error::InvalidIdentity)?;
        let length = rest[..colon]
            .parse::<usize>()
            .map_err(|_| Error::InvalidIdentity)?;
        if length == 0 || length > IDENTITY_FIELD_LIMIT {
            return Err(Error::InvalidIdentity);
        }
        let start = offset + colon + 1;
        let end = start.checked_add(length).ok_or(Error::InvalidIdentity)?;
        let field = value.get(start..end).ok_or(Error::InvalidIdentity)?;
        if value.as_bytes().get(end) != Some(&b',')
            || !valid_identity_field(field, IDENTITY_FIELD_LIMIT)
        {
            return Err(Error::InvalidIdentity);
        }
        fields.push(field);
        offset = end + 1;
    }
    let digest = value
        .get(offset..)
        .and_then(|value| value.strip_prefix('#'))
        .ok_or(Error::InvalidIdentity)?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || blake3::hash(&value.as_bytes()[..offset]).to_hex().as_str() != digest
        || encode_canonical_fields(version, &fields) != value
    {
        return Err(Error::InvalidIdentity);
    }
    Ok(fields)
}

fn windows_composite_token(ownership: &Ownership, layers: &str) -> String {
    let mut input = Vec::with_capacity(
        ownership.daemon_service().len() + ownership.attempt().len() + layers.len() + 32,
    );
    input.extend_from_slice(b"kit-windows-composite-v1\0");
    input.extend_from_slice(ownership.daemon_service().as_bytes());
    input.push(0);
    input.extend_from_slice(ownership.attempt().as_bytes());
    input.push(0);
    input.extend_from_slice(layers.as_bytes());
    hex(blake3::hash(&input).as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 15) as usize] as char);
    }
    encoded
}

fn unhex(value: &str) -> Result<String, Error> {
    if value.len() > START_IDENTITY_LIMIT * 2 || !value.len().is_multiple_of(2) {
        return Err(Error::InvalidIdentity);
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = digit(pair[0])?;
        let low = digit(pair[1])?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).map_err(|_| Error::InvalidIdentity)
}

fn digit(value: u8) -> Result<u8, Error> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(Error::InvalidIdentity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_layer(kind: BoundaryKind, name: &str, start: &str) -> BoundaryIdentity {
        BoundaryIdentity::new(kind, name, "a".repeat(64), start).unwrap()
    }

    #[test]
    fn windows_composite_round_trips_both_authenticated_layers() {
        let ownership = Ownership::new("daemon-with-fence:7", "process-1").unwrap();
        let job = windows_layer(
            BoundaryKind::WindowsJobObject,
            "Local\\kit-job-a",
            "v1:42:1337:100:200:3",
        );
        let runtime = windows_layer(
            BoundaryKind::WindowsContainerOrVm,
            "container-a",
            "v1|hyper_v|plan|helper|runtime|generation|root|42|1337",
        );
        let composite =
            PersistedBoundary::windows_composite(ownership, job.clone(), runtime.clone()).unwrap();

        let decoded = PersistedBoundary::decode(&composite.encode()).unwrap();
        assert_eq!(decoded.windows_layers().unwrap(), Some((job, runtime)));
    }

    #[test]
    fn windows_composite_rejects_partial_or_modified_layer_records() {
        let ownership = Ownership::new("daemon-with-fence:8", "process-2").unwrap();
        let job = windows_layer(
            BoundaryKind::WindowsJobObject,
            "Local\\kit-job-b",
            "v1:43:1338:100:200:3",
        );
        let runtime = windows_layer(
            BoundaryKind::WindowsContainerOrVm,
            "container-b",
            "v1|container|plan|helper|runtime|generation|root|43|1338",
        );
        let mut composite = PersistedBoundary::windows_composite(ownership, job, runtime).unwrap();
        composite.identity.start_identity.push_str("|extra");
        assert_eq!(composite.windows_layers(), Err(Error::InvalidIdentity));

        let job_only = PersistedBoundary {
            ownership: composite.ownership,
            identity: windows_layer(
                BoundaryKind::WindowsJobObject,
                "Local\\kit-job-b",
                "v1:43:1338:100:200:3",
            ),
        };
        assert_eq!(job_only.windows_layers().unwrap(), None);
    }

    #[test]
    fn windows_composite_round_trips_production_sized_fields_at_the_limit() {
        let ownership = Ownership::new("d".repeat(1024), "a".repeat(1024)).unwrap();
        let job = BoundaryIdentity::new(
            BoundaryKind::WindowsJobObject,
            "j".repeat(1024),
            "1".repeat(64),
            "s".repeat(1024),
        )
        .unwrap();
        let runtime = BoundaryIdentity::new(
            BoundaryKind::WindowsContainerOrVm,
            "r".repeat(1024),
            "2".repeat(64),
            "i".repeat(3072),
        )
        .unwrap();

        let composite =
            PersistedBoundary::windows_composite(ownership, job.clone(), runtime.clone()).unwrap();
        assert_eq!(
            PersistedBoundary::decode(&composite.encode())
                .unwrap()
                .windows_layers()
                .unwrap(),
            Some((job, runtime))
        );
    }

    #[test]
    fn canonical_identity_rejects_limit_overflow_corruption_and_noncanonical_lengths() {
        assert!(
            BoundaryIdentity::new(
                BoundaryKind::WindowsContainerOrVm,
                "x",
                "y",
                "z".repeat(START_IDENTITY_LIMIT + 1),
            )
            .is_err()
        );

        let encoded = encode_canonical_fields("v2", &["job", "locator"]);
        let mut corrupted = encoded.clone();
        corrupted.replace_range(4..5, "4");
        assert_eq!(
            decode_canonical_fields(&corrupted, "v2", 2),
            Err(Error::InvalidIdentity)
        );

        let mut corrupted = encoded;
        let last = corrupted.len() - 1;
        let replacement = if corrupted.as_bytes()[last] == b'0' {
            "1"
        } else {
            "0"
        };
        corrupted.replace_range(last.., replacement);
        assert_eq!(
            decode_canonical_fields(&corrupted, "v2", 2),
            Err(Error::InvalidIdentity)
        );
    }
}
