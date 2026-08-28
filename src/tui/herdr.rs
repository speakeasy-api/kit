use std::{
    ffi::OsString,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{sync::mpsc, task::JoinHandle};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
const METADATA_TIMEOUT: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_millis(250);
const RELEASE_RETRY_DELAY: Duration = Duration::from_millis(50);

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
    last_summary: Option<String>,
    metadata_initialized: bool,
    metadata_sequence: u64,
    lifecycle_work: Option<mpsc::UnboundedSender<Work>>,
    lifecycle_worker: Option<JoinHandle<()>>,
    metadata_work: Option<mpsc::UnboundedSender<Work>>,
    metadata_worker: Option<JoinHandle<()>>,
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
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        Self::with_sequence(bin, pane_id, sequence)
    }

    fn with_sequence(bin: OsString, pane_id: OsString, metadata_sequence: u64) -> Self {
        Self {
            bin,
            pane_id,
            last: None,
            last_summary: None,
            metadata_initialized: false,
            metadata_sequence,
            lifecycle_work: None,
            lifecycle_worker: None,
            metadata_work: None,
            metadata_worker: None,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(bin: impl Into<OsString>, pane_id: impl Into<OsString>) -> Self {
        Self::with_sequence(bin.into(), pane_id.into(), 0)
    }

    #[cfg(test)]
    pub(super) async fn report(&mut self, session_id: &str, state: State) {
        self.report_with_summary(session_id, state, None).await;
    }

    pub(super) async fn report_with_summary(
        &mut self,
        session_id: &str,
        state: State,
        prompt: Option<&str>,
    ) {
        if let Some(args) = self.lifecycle_command(session_id, state) {
            self.lifecycle_sender()
                .send(Work {
                    args,
                    release: false,
                })
                .ok();
        }
        if let Some(args) = self.metadata_command(prompt) {
            self.metadata_sender()
                .send(Work {
                    args,
                    release: false,
                })
                .ok();
        }
    }

    pub(super) async fn release(mut self) {
        let release = self.release_command();
        self.lifecycle_sender()
            .send(Work {
                args: release,
                release: true,
            })
            .ok();
        self.lifecycle_work.take();
        self.metadata_work.take();

        let lifecycle_worker = self.lifecycle_worker.take();
        let metadata_worker = self.metadata_worker.take();
        let clear_sequence = self.next_metadata_sequence();
        let clear = self.clear_summary_command(clear_sequence);
        let bin = self.bin.clone();
        let lifecycle = async {
            if let Some(worker) = lifecycle_worker {
                let _ = worker.await;
            }
        };
        let metadata = async {
            if let Some(worker) = metadata_worker {
                let _ = worker.await;
            }
            if !run(&bin, &clear, METADATA_TIMEOUT).await {
                tokio::time::sleep(RELEASE_RETRY_DELAY).await;
                let _ = run(&bin, &clear, METADATA_TIMEOUT).await;
            }
        };
        tokio::join!(lifecycle, metadata);
    }

    fn lifecycle_sender(&mut self) -> &mpsc::UnboundedSender<Work> {
        if self.lifecycle_work.is_none() {
            let (work, pending) = mpsc::unbounded_channel();
            self.lifecycle_worker = Some(tokio::spawn(worker(
                self.bin.clone(),
                pending,
                COMMAND_TIMEOUT,
            )));
            self.lifecycle_work = Some(work);
        }
        self.lifecycle_work
            .as_ref()
            .expect("lifecycle worker sender initialized")
    }

    fn metadata_sender(&mut self) -> &mpsc::UnboundedSender<Work> {
        if self.metadata_work.is_none() {
            let (work, pending) = mpsc::unbounded_channel();
            self.metadata_worker = Some(tokio::spawn(worker(
                self.bin.clone(),
                pending,
                METADATA_TIMEOUT,
            )));
            self.metadata_work = Some(work);
        }
        self.metadata_work
            .as_ref()
            .expect("metadata worker sender initialized")
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

    #[cfg(test)]
    pub(super) fn command(&mut self, session_id: &str, state: State) -> Option<Vec<OsString>> {
        self.lifecycle_command(session_id, state)
    }

    #[cfg(test)]
    pub(super) fn commands(
        &mut self,
        session_id: &str,
        state: State,
        prompt: Option<&str>,
    ) -> Option<Vec<Vec<OsString>>> {
        let mut commands = Vec::new();
        if let Some(command) = self.lifecycle_command(session_id, state) {
            commands.push(command);
        }
        if let Some(command) = self.metadata_command(prompt) {
            commands.push(command);
        }
        (!commands.is_empty()).then_some(commands)
    }

    fn lifecycle_command(&mut self, session_id: &str, state: State) -> Option<Vec<OsString>> {
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

    fn metadata_command(&mut self, prompt: Option<&str>) -> Option<Vec<OsString>> {
        match prompt.and_then(summary) {
            Some(summary) => {
                if self.metadata_initialized && self.last_summary.as_ref() == Some(&summary) {
                    return None;
                }
                self.metadata_initialized = true;
                self.last_summary = Some(summary.clone());
                let sequence = self.next_metadata_sequence();
                Some(self.summary_command(&summary, sequence))
            }
            None if !self.metadata_initialized || self.last_summary.is_some() => {
                self.metadata_initialized = true;
                self.last_summary = None;
                let sequence = self.next_metadata_sequence();
                Some(self.clear_summary_command(sequence))
            }
            None => None,
        }
    }

    fn next_metadata_sequence(&mut self) -> u64 {
        self.metadata_sequence = self.metadata_sequence.saturating_add(1);
        self.metadata_sequence
    }

    fn summary_command(&self, summary: &str, sequence: u64) -> Vec<OsString> {
        vec![
            "pane".into(),
            "report-metadata".into(),
            self.pane_id.clone(),
            "--source".into(),
            "custom:kit-context".into(),
            "--agent".into(),
            "kit".into(),
            "--applies-to-source".into(),
            "custom:kit".into(),
            "--token".into(),
            format!("summary={summary}").into(),
            "--seq".into(),
            sequence.to_string().into(),
        ]
    }

    fn clear_summary_command(&self, sequence: u64) -> Vec<OsString> {
        vec![
            "pane".into(),
            "report-metadata".into(),
            self.pane_id.clone(),
            "--source".into(),
            "custom:kit-context".into(),
            "--clear-token".into(),
            "summary".into(),
            "--seq".into(),
            sequence.to_string().into(),
        ]
    }
}

async fn worker(bin: OsString, mut pending: mpsc::UnboundedReceiver<Work>, timeout: Duration) {
    let Some(mut current) = pending.recv().await else {
        return;
    };
    loop {
        while let Ok(newer) = pending.try_recv() {
            current = newer;
        }
        if run(&bin, &current.args, timeout).await {
            if current.release {
                return;
            }
            let Some(next) = pending.recv().await else {
                return;
            };
            current = next;
        } else {
            if current.release || (pending.is_closed() && pending.is_empty()) {
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

async fn run(bin: &OsString, args: &[OsString], timeout: Duration) -> bool {
    let mut command = tokio::process::Command::new(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    matches!(
        tokio::time::timeout(timeout, command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

fn summary(prompt: &str) -> Option<String> {
    let summary = prompt
        .chars()
        .filter_map(|character| {
            if character.is_whitespace() {
                Some(' ')
            } else if unsafe_character(character) {
                None
            } else {
                Some(character)
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect::<String>();
    (!summary.is_empty()).then_some(summary)
}

fn unsafe_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}
