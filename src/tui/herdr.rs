use std::{ffi::OsString, process::Stdio, time::Duration};

use tokio::{sync::mpsc, task::JoinHandle};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
const RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Idle,
    Working,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    session_id: String,
    state: State,
}

struct Work {
    args: Vec<OsString>,
    release: bool,
}

pub(super) struct Integration {
    bin: OsString,
    pane_id: OsString,
    last: Option<Snapshot>,
    work: Option<mpsc::UnboundedSender<Work>>,
    worker: Option<JoinHandle<()>>,
}

impl Integration {
    pub(super) fn from_values(
        herdr_env: Option<&str>,
        bin: Option<&str>,
        pane_id: Option<&str>,
    ) -> Option<Self> {
        if herdr_env != Some("1") {
            return None;
        }
        Some(Self::new(bin?.into(), pane_id?.into()))
    }

    fn new(bin: OsString, pane_id: OsString) -> Self {
        Self {
            bin,
            pane_id,
            last: None,
            work: None,
            worker: None,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(bin: impl Into<OsString>, pane_id: impl Into<OsString>) -> Self {
        Self::new(bin.into(), pane_id.into())
    }

    pub(super) async fn report(&mut self, session_id: &str, state: State) {
        let Some(args) = self.command(session_id, state) else {
            return;
        };
        self.sender()
            .send(Work {
                args,
                release: false,
            })
            .ok();
    }

    pub(super) async fn release(mut self) {
        let args = self.release_command();
        self.sender()
            .send(Work {
                args,
                release: true,
            })
            .ok();
        self.work.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }

    fn sender(&mut self) -> &mpsc::UnboundedSender<Work> {
        if self.work.is_none() {
            let (work, pending) = mpsc::unbounded_channel();
            self.worker = Some(tokio::spawn(worker(self.bin.clone(), pending)));
            self.work = Some(work);
        }
        self.work.as_ref().expect("worker sender initialized")
    }

    pub(super) fn release_command(&self) -> Vec<OsString> {
        vec![
            "pane".into(),
            "release-agent".into(),
            self.pane_id.clone(),
            "--source".into(),
            "custom:kit".into(),
            "--agent".into(),
            "kit".into(),
        ]
    }

    pub(super) fn command(&mut self, session_id: &str, state: State) -> Option<Vec<OsString>> {
        let next = Snapshot {
            session_id: session_id.to_owned(),
            state,
        };
        if self.last.as_ref() == Some(&next) {
            return None;
        }
        self.last = Some(next);
        Some(vec![
            "pane".into(),
            "report-agent".into(),
            self.pane_id.clone(),
            "--source".into(),
            "custom:kit".into(),
            "--agent".into(),
            "kit".into(),
            "--state".into(),
            state.as_str().into(),
            "--agent-session-id".into(),
            session_id.into(),
        ])
    }
}

async fn worker(bin: OsString, mut pending: mpsc::UnboundedReceiver<Work>) {
    let Some(mut current) = pending.recv().await else {
        return;
    };
    loop {
        while let Ok(newer) = pending.try_recv() {
            current = newer;
        }
        if run(&bin, &current.args).await {
            if current.release {
                return;
            }
            let Some(next) = pending.recv().await else {
                return;
            };
            current = next;
        } else {
            if current.release {
                return;
            }
            tokio::select! {
                Some(newer) = pending.recv() => current = newer,
                () = tokio::time::sleep(RETRY_DELAY) => {}
                else => return,
            }
        }
    }
}

async fn run(bin: &OsString, args: &[OsString]) -> bool {
    let mut command = tokio::process::Command::new(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    matches!(
        tokio::time::timeout(COMMAND_TIMEOUT, command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}
