//! Session persistence example.
//!
//! Demonstrates the agentkit persistence contract end-to-end:
//!
//! - **Restore**: load prior `Item`s from sqlite and pass them to
//!   [`AgentBuilder::transcript`].
//! - **Incremental write**: register a [`TranscriptObserver`] that writes
//!   each newly-appended item to sqlite as the loop runs.
//! - **Resume**: the first `next()` call yields `AwaitingInput`, so a fresh
//!   process can pick up exactly where the previous one stopped — the
//!   transcript carries the prior turns, and the host supplies the next
//!   user message in response to the interrupt.
//!
//! Run twice in a row with the same `--session` flag to see resumption:
//!
//! ```bash
//! cargo run -p openrouter-session-persistence -- --session demo
//! # ^C or /exit, then:
//! cargo run -p openrouter-session-persistence -- --session demo
//! ```

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agentkit_core::{CancellationController, Item, ItemKind, Part};
use agentkit_loop::{
    Agent, InputRequest, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention,
    SessionConfig, TranscriptEvent, TranscriptObserver,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use rusqlite::{Connection, params};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let (session_id, db_path) = parse_args();
    println!("session: {session_id} (db: {})", db_path.display());

    let store = Arc::new(SqliteSessionStore::open(&db_path)?);
    let prior = store.load(&session_id)?;
    if !prior.is_empty() {
        println!("resuming with {} item(s) from prior run", prior.len());
    }

    let observer = SqliteTranscriptObserver {
        store: Arc::clone(&store),
        session_id: session_id.clone(),
    };

    let config = OpenRouterConfig::from_env()?;
    let adapter = OpenRouterAdapter::new(config)?;
    let cancellation = CancellationController::new();

    let mut builder = Agent::builder()
        .model(adapter)
        .cancellation(cancellation.handle())
        .transcript_observer(observer);

    if prior.is_empty() {
        builder = builder.transcript(vec![Item::text(
            ItemKind::System,
            "You are a helpful assistant. Keep replies short.",
        )]);
    } else {
        builder = builder.transcript(prior);
    }

    let agent = builder.build()?;
    let mut driver = agent
        .start(SessionConfig::new(session_id.clone()).with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    println!(
        "Type a prompt and press enter. Use /exit to quit. Press Ctrl-C to cancel the current turn."
    );

    let pending = run_until_input(&mut driver, &cancellation).await?;
    drive_repl(driver, pending, cancellation).await
}

fn parse_args() -> (String, PathBuf) {
    let mut session_id = "default".to_string();
    let mut db_path: PathBuf = "agent.db".into();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => {
                if let Some(value) = iter.next() {
                    session_id = value;
                }
            }
            "--db" => {
                if let Some(value) = iter.next() {
                    db_path = value.into();
                }
            }
            _ => {}
        }
    }
    (session_id, db_path)
}

async fn drive_repl<S>(
    mut driver: agentkit_loop::LoopDriver<S>,
    mut pending: InputRequest,
    cancellation: CancellationController,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: agentkit_loop::ModelSession,
{
    loop {
        let Some(prompt) = read_prompt()? else {
            break;
        };
        pending.submit(&mut driver, vec![Item::text(ItemKind::User, prompt)])?;
        pending = run_until_input(&mut driver, &cancellation).await?;
    }
    Ok(())
}

fn read_prompt() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut line = String::new();
    loop {
        print!("you> ");
        io::stdout().flush()?;
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/exit" || prompt == "/quit" {
            return Ok(None);
        }
        return Ok(Some(prompt.to_string()));
    }
}

async fn run_until_input<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
    cancellation: &CancellationController,
) -> Result<InputRequest, Box<dyn std::error::Error>>
where
    S: agentkit_loop::ModelSession,
{
    let interrupt = cancellation.clone();
    let ctrl_c = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        interrupt.interrupt();
    });

    let request = loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                if result.finish_reason == agentkit_core::FinishReason::Cancelled {
                    eprintln!("turn cancelled");
                }
                for item in result.items {
                    if item.kind == ItemKind::Assistant {
                        render_assistant(&item.parts);
                    }
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => break req,
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                ctrl_c.abort();
                return Err("approval interrupt unhandled in this example".into());
            }
        }
    };
    ctrl_c.abort();
    Ok(request)
}

fn render_assistant(parts: &[Part]) {
    print!("assistant> ");
    for part in parts {
        if let Part::Text(text) = part {
            print!("{}", text.text);
        }
    }
    println!();
}

/// Sqlite-backed durable store for the agent transcript. Two tables —
/// `sessions` for session metadata and `items` for the transcript stream.
/// Items are appended in monotonic `seq` order so loading reproduces the
/// transcript exactly as the loop appended it.
struct SqliteSessionStore {
    conn: Mutex<Connection>,
}

impl SqliteSessionStore {
    fn open(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS items (
                 session_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 json TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq),
                 FOREIGN KEY (session_id) REFERENCES sessions(id)
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn load(&self, session_id: &str) -> Result<Vec<Item>, Box<dyn std::error::Error>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, created_at) VALUES (?, strftime('%s','now'))",
            params![session_id],
        )?;
        let mut stmt =
            conn.prepare("SELECT json FROM items WHERE session_id = ? ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![session_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            let item: Item = serde_json::from_str(&json)?;
            out.push(item);
        }
        Ok(out)
    }

    fn append(&self, session_id: &str, item: &Item) -> Result<(), Box<dyn std::error::Error>> {
        let conn = self.conn.lock().unwrap();
        let next_seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM items WHERE session_id = ?",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let json = serde_json::to_string(item)?;
        conn.execute(
            "INSERT INTO items (session_id, seq, json) VALUES (?, ?, ?)",
            params![session_id, next_seq, json],
        )?;
        Ok(())
    }
}

/// `TranscriptObserver` impl that mirrors every appended item into sqlite.
///
/// `on_transcript_event` is called synchronously by the loop and is the single
/// mutation point for the transcript -- every push funnels through here. The
/// observer must NOT block (sqlite writes are fast and local; for remote
/// stores, use a buffered channel and persist on a background task).
///
/// Compaction-driven rewrites do **not** fire `on_transcript_event`. A
/// compaction-aware persistor would also subscribe to
/// `AgentEvent::CompactionFinished` via a `LoopObserver` and replace the
/// stored transcript when it sees that event. This example skips
/// compaction.
struct SqliteTranscriptObserver {
    store: Arc<SqliteSessionStore>,
    session_id: String,
}

impl TranscriptObserver for SqliteTranscriptObserver {
    fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
        if let Err(error) = self.store.append(&self.session_id, event.item) {
            eprintln!("[persistence] failed to append item: {error}");
        }
    }
}
