use std::{path::Path, process::Stdio, time::Duration};

use agent_client_protocol::{ByteStreams, schema::ProtocolVersion};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::{process::Command, sync::mpsc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug)]
enum Update {
    Text(String),
    Tool(String),
    Done(String),
}

#[derive(Default)]
struct State {
    transcript: String,
    input: String,
    status: String,
    pending: bool,
}

pub async fn run(root: &Path, model: &str, a2a: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--model")
        .arg(model)
        .arg("--a2a")
        .arg(a2a)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().ok_or("could not open Kit stdin")?;
    let stdout = child.stdout.take().ok_or("could not open Kit stdout")?;
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let root = root.to_path_buf();
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    let notifications = updates_tx.clone();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: agentkit_acp::SessionNotification, _cx| {
                match notification.update {
                    agentkit_acp::SessionUpdate::AgentMessageChunk(chunk) => {
                        if let agentkit_acp::ContentBlock::Text(text) = chunk.content {
                            let _ = notifications.send(Update::Text(text.text));
                        }
                    }
                    agentkit_acp::SessionUpdate::ToolCall(call) => {
                        let _ = notifications.send(Update::Tool(format!("{call:?}")));
                    }
                    _ => {}
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |connection| {
            connection
                .send_request(agentkit_acp::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(agentkit_acp::NewSessionRequest::new(root))
                .block_task()
                .await?;
            let mut terminal = ratatui::init();
            let mut state = State {
                status: "ready — Enter sends, Esc quits".into(),
                ..State::default()
            };

            let result = async {
                loop {
                    while let Ok(update) = updates_rx.try_recv() {
                        match update {
                            Update::Text(text) => state.transcript.push_str(&text),
                            Update::Tool(tool) => state.status = format!("tool: {tool}"),
                            Update::Done(status) => {
                                state.transcript.push_str("\n\n");
                                state.status = status;
                                state.pending = false;
                            }
                        }
                    }
                    terminal
                        .draw(|frame| draw(frame, &state))
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    if event::poll(Duration::from_millis(50))
                        .map_err(agent_client_protocol::Error::into_internal_error)?
                        && let Event::Key(key) = event::read()
                            .map_err(agent_client_protocol::Error::into_internal_error)?
                        && key.kind == KeyEventKind::Press
                    {
                        match key.code {
                            KeyCode::Esc => break,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break;
                            }
                            KeyCode::Backspace => {
                                state.input.pop();
                            }
                            KeyCode::Enter if !state.pending && !state.input.trim().is_empty() => {
                                let prompt = std::mem::take(&mut state.input);
                                state
                                    .transcript
                                    .push_str(&format!("\n\nyou: {prompt}\n\nkit: "));
                                state.status = "thinking…".into();
                                state.pending = true;
                                let connection = connection.clone();
                                let session_id = session.session_id.clone();
                                let done = updates_tx.clone();
                                tokio::spawn(async move {
                                    let outcome = connection
                                        .send_request(agentkit_acp::PromptRequest::new(
                                            session_id,
                                            vec![agentkit_acp::ContentBlock::Text(
                                                agentkit_acp::TextContent::new(prompt),
                                            )],
                                        ))
                                        .block_task()
                                        .await;
                                    let _ = done.send(Update::Done(match outcome {
                                        Ok(_) => "ready".into(),
                                        Err(error) => format!("error: {error}"),
                                    }));
                                });
                            }
                            KeyCode::Char(character) => state.input.push(character),
                            _ => {}
                        }
                    }
                    tokio::task::yield_now().await;
                }
                Ok(())
            }
            .await;
            ratatui::restore();
            let _ = child.kill().await;
            result
        })
        .await?;
    Ok(())
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &State) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(state.transcript.as_str())
            .block(Block::default().title("kit").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(state.input.as_str())
            .block(Block::default().title("prompt").borders(Borders::ALL)),
        rows[1],
    );
    frame.render_widget(Paragraph::new(state.status.as_str()), rows[2]);
}
