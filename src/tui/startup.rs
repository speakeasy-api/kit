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
    Clipboard(ClipboardRoute, ClipboardResult),
}

/// Pick an existing workspace session without creating or resuming a session.
/// Cancellation and an empty catalog return `None`.
pub async fn pick_session(
    root: &Path,
    stop: &mut Stop,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let root = root
        .canonicalize()
        .map_err(|error| Failure(format!("{}: {error}", root.display())))?;
    let filesystem =
        crate::resilient_fs::Fs::new(std::sync::Arc::new(crate::resilient_fs::DiskBackend));
    let Some(entries) = scan_catalog(root.clone(), filesystem, stop).await? else {
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
                    let root = root.clone();
                    let updates = updates_tx.clone();
                    tokio::spawn(async move {
                        let id = session_id.clone();
                        let name = display_name.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            crate::session::set_display_name_and_title(&root, &id, name.as_deref())
                        })
                        .await
                        .map_err(|error| format!("session rename worker failed: {error}"))
                        .and_then(|result| result);
                        let _ = updates.send(PickerUpdate::Session(Update::SessionRenamed {
                            session_id,
                            display_name: display_name.map(|name| name.trim().to_string()),
                            result,
                        }));
                    });
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
    result
}

// This worker only reads disk, with its own empty filesystem namespace: it
// cannot migrate sessions, recover accepted writes, or hold global recovery
// locks. Dropping its JoinHandle intentionally detaches it on cancellation;
// unlike spawn_blocking, it does not delay Tokio runtime teardown.
async fn scan_catalog(
    root: std::path::PathBuf,
    filesystem: crate::resilient_fs::Fs,
    stop: &mut Stop,
) -> Result<Option<Vec<crate::session::CatalogEntry>>, Box<dyn std::error::Error>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("session-catalog".into())
        .spawn(move || {
            let result = crate::session::catalog_with(&filesystem, &root).map_err(|error| {
                format!("could not list sessions for {}: {error}", root.display())
            });
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
