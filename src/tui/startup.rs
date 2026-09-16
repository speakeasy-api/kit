//! Session selection before launching an agent or opening a persisted session.

use std::path::Path;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{
    StreamExt,
    future::{Either, select},
};

use super::{
    Action, App, ClipboardResult, ClipboardRoute, Failure, Stop, Update, enter, handle, leave,
    read_clipboard, ui,
};

enum PickerUpdate {
    Tick,
    Session(Update),
    RenamePrepared {
        session_id: String,
        display_name: Option<String>,
        result: Result<crate::session::PreparedDisplayName, String>,
    },
    Clipboard(ClipboardRoute, ClipboardResult),
}

/// Pick an existing workspace session without creating or resuming a session.
/// Cancellation and an empty catalog return `None`.
/// The caller must await this future to completion and signal cancellation via
/// `Stop`: dropping it cannot asynchronously drain admitted storage commits.
pub async fn pick_session(
    root: &Path,
    stop: &mut Stop,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let backend = std::sync::Arc::new(crate::resilient_fs::DiskBackend);
    let Some((root, entries)) = scan_catalog(root.to_path_buf(), backend, stop).await? else {
        return Ok(None);
    };
    if entries.is_empty() {
        eprintln!("no resumable sessions for workspace {}", root.display());
        return Ok(None);
    }

    let mut app = App::new(root.clone(), String::new(), String::new(), String::new());
    app.apply(Update::SessionCatalog(Ok(entries)));
    // The caller retains signal handlers across selection and startup. Restore the
    // terminal on both successful cancellation and fallible reads/draws.
    let (mut terminal, mut images) = enter()?;
    let mut renames = RenameCommits::default();
    let result = async {
        let mut events = EventStream::new();
        let mut clipboard_pending = false;
        let mut ticker = tokio::time::interval(super::TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
        loop {
            terminal.draw(|frame| ui::draw(frame, &mut app, &mut images))?;
            let action = {
                let event = std::pin::pin!(events.next());
                let redraw = app.needs_redraw_tick() || images.pending();
                let update = std::pin::pin!(async {
                    if !redraw {
                        return updates_rx.recv().await;
                    }
                    let update = std::pin::pin!(updates_rx.recv());
                    let tick = std::pin::pin!(ticker.tick());
                    match select(tick, update).await {
                        Either::Left(_) => Some(PickerUpdate::Tick),
                        Either::Right((update, _)) => update,
                    }
                });
                let stopped = std::pin::pin!(stop.requested());
                match select(stopped, select(event, update)).await {
                    Either::Left(_) => return Ok(None),
                    Either::Right((Either::Left((event, _)), _)) => match event {
                        Some(Ok(event)) => handle_picker_event(&mut app, event),
                        Some(Err(error)) => return Err(error.into()),
                        None => return Ok(None),
                    },
                    Either::Right((Either::Right((update, _)), _)) => match update {
                        Some(PickerUpdate::Tick) => {
                            app.tick();
                            Action::None
                        }
                        Some(PickerUpdate::Session(update)) => {
                            app.apply(update);
                            Action::None
                        }
                        Some(PickerUpdate::RenamePrepared {
                            session_id,
                            display_name,
                            result,
                        }) => {
                            match result {
                                Ok(prepared) => {
                                    // This branch is the write-admission boundary. Only the
                                    // picker owns prepared values; a stopped picker drops them.
                                    renames.admit(
                                        prepared,
                                        session_id,
                                        display_name,
                                        updates_tx.clone(),
                                    );
                                }
                                Err(error) => app.apply(Update::SessionRenamed {
                                    session_id,
                                    display_name,
                                    result: Err(error),
                                }),
                            }
                            Action::None
                        }
                        Some(PickerUpdate::Clipboard(route, result)) => {
                            clipboard_pending = false;
                            // A cancelled or submitted rename must not receive an
                            // earlier clipboard read when another dialog opens.
                            if app.clipboard_route() == route {
                                match result {
                                    ClipboardResult::Text(text) => {
                                        super::handle_paste(&mut app, &text);
                                    }
                                    ClipboardResult::Error(error) => app.note(error),
                                    _ => {}
                                }
                            }
                            Action::None
                        }
                        None => return Ok(None),
                    },
                }
            };
            match action {
                Action::Quit => return Ok(None),
                Action::Resume(id) => return Ok(Some(id)),
                Action::RenameSession {
                    session_id,
                    display_name,
                } => {
                    start_rename_preparation(
                        root.clone(),
                        session_id,
                        display_name,
                        crate::resilient_fs::best_effort_global().clone(),
                        updates_tx.clone(),
                    )?;
                }
                Action::ReadClipboard(route, mode) => {
                    if clipboard_pending {
                        app.toast("clipboard is busy; try again");
                        continue;
                    }
                    clipboard_pending = true;
                    let updates = updates_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = read_clipboard(&route, mode);
                        let _ = updates.send(PickerUpdate::Clipboard(route, result));
                    });
                }
                _ => {}
            }
        }
    }
    .await;
    leave(&mut terminal);
    // No preparation can submit writes after the receiver is dropped above.
    // Accepted commits remain owned: drain them before the caller can recover
    // global storage. An indefinitely stalled accepted write is not cancellable.
    renames.drain().await?;
    result
}

/// Admission is owned by the picker, not the preparation thread. This owner is
/// drained after terminal restoration on every ordinary picker exit/error path.
#[derive(Default)]
struct RenameCommits {
    workers: Vec<tokio::task::JoinHandle<Result<Option<String>, String>>>,
}
impl RenameCommits {
    fn admit(
        &mut self,
        prepared: crate::session::PreparedDisplayName,
        session_id: String,
        display_name: Option<String>,
        updates: tokio::sync::mpsc::UnboundedSender<PickerUpdate>,
    ) {
        self.workers.push(tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || prepared.commit())
                .await
                .map_err(|error| format!("session rename worker failed: {error}"))
                .and_then(|result| result);
            let _ = updates.send(PickerUpdate::Session(Update::SessionRenamed {
                session_id,
                display_name,
                result: result.clone(),
            }));
            result
        }));
    }

    async fn drain(self) -> Result<(), String> {
        let mut failure = None;
        for worker in self.workers {
            match worker.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    // A closed stderr must not unwind past still-owned commits.
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr(), "{error}");
                }
                Err(error) => {
                    failure.get_or_insert_with(|| format!("session rename worker failed: {error}"));
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn start_rename_preparation(
    root: std::path::PathBuf,
    session_id: String,
    display_name: Option<String>,
    filesystem: crate::resilient_fs::Fs,
    updates: tokio::sync::mpsc::UnboundedSender<PickerUpdate>,
) -> std::io::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = session_id.clone();
    let name = display_name.clone();
    std::thread::Builder::new()
        .name("session-rename-prepare".into())
        .spawn(move || {
            let result =
                crate::session::prepare_display_name(&filesystem, &root, &id, name.as_deref());
            let _ = tx.send(result);
        })?;
    tokio::spawn(async move {
        let result = rx
            .await
            .map_err(|error| format!("session rename preparation worker failed: {error}"))
            .and_then(|result| result);
        let _ = updates.send(PickerUpdate::RenamePrepared {
            session_id,
            display_name: display_name.map(|name| name.trim().to_string()),
            result,
        });
    });
    Ok(())
}

// This worker only reads disk, with its own empty filesystem namespace: it
// cannot migrate sessions, recover accepted writes, or hold global recovery
// locks. Dropping its JoinHandle intentionally detaches it on cancellation;
// unlike spawn_blocking, it does not delay Tokio runtime teardown.
async fn scan_catalog(
    root: std::path::PathBuf,
    backend: std::sync::Arc<dyn crate::resilient_fs::Backend>,
    stop: &mut Stop,
) -> Result<
    Option<(std::path::PathBuf, Vec<crate::session::CatalogEntry>)>,
    Box<dyn std::error::Error>,
> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("session-catalog".into())
        .spawn(move || {
            let result = (|| {
                let root = backend
                    .canonicalize(&root)
                    .map_err(|error| format!("{}: {error}", root.display()))?;
                let filesystem = crate::resilient_fs::Fs::new(backend);
                let entries =
                    crate::session::catalog_with(&filesystem, &root).map_err(|error| {
                        format!("could not list sessions for {}: {error}", root.display())
                    })?;
                Ok::<_, String>((root, entries))
            })();
            let _ = tx.send(result);
        })?;
    let Some(result) = stop.until(rx).await else {
        return Ok(None);
    };
    // The sender has finished scanning; joining here cannot wait on disk I/O.
    worker
        .join()
        .map_err(|_| Failure("session catalog worker panicked".into()))?;
    Ok(Some(result?.map_err(Failure)?))
}

fn handle_picker_event(app: &mut App, event: Event) -> Action {
    if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Action::Quit;
    }
    let action = handle(app, event);
    if app.session_dialog.is_none() && !matches!(action, Action::Resume(_)) {
        Action::Quit
    } else {
        action
    }
}

#[cfg(all(test, unix))]
#[path = "startup_tests.rs"]
mod tests;
