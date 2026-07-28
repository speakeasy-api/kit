use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::Command;
use std::time::Duration;

use agentkit_core::{
    CancellationController, FinishReason, Item, ItemKind, Part, ToolOutput, ToolResultPart,
};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{
    Tool, ToolContext, ToolError, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use serde_json::{Value, json};

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4.6";
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn prompt_cache_key(model: &str, prompt: &str) -> String {
    let mut hasher = DefaultHasher::new();
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    model.hash(&mut hasher);
    prompt.hash(&mut hasher);
    format!("how:{}:{:016x}", env!("CARGO_PKG_VERSION"), hasher.finish())
}

// ─── Terminal guard ───────────────────────────────────────────────────────────

struct TermGuard {
    raw: bool,
    cursor_hidden: bool,
}

impl TermGuard {
    fn new() -> Self {
        Self {
            raw: false,
            cursor_hidden: false,
        }
    }

    fn raw_on(&mut self) -> io::Result<()> {
        if !self.raw {
            terminal::enable_raw_mode()?;
            self.raw = true;
        }
        Ok(())
    }

    fn raw_off(&mut self) -> io::Result<()> {
        if self.raw {
            terminal::disable_raw_mode()?;
            self.raw = false;
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        if !self.cursor_hidden {
            execute!(io::stderr(), cursor::Hide)?;
            self.cursor_hidden = true;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if self.cursor_hidden {
            execute!(io::stderr(), cursor::Show)?;
            self.cursor_hidden = false;
        }
        Ok(())
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if self.cursor_hidden {
            let _ = execute!(io::stderr(), cursor::Show);
        }
        if self.raw {
            let _ = terminal::disable_raw_mode();
        }
    }
}

// ─── Entry ────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let (model, prompt) = parse_args()?;
    let prompt = if prompt.is_empty() {
        read_stdin()?
    } else {
        prompt
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    if !prompt.is_empty() {
        if io::stderr().is_terminal() && io::stdin().is_terminal() {
            return rt.block_on(run_tui(model, prompt));
        }
        return rt.block_on(run_plain(model, prompt));
    }

    if !io::stdin().is_terminal() {
        eprintln!("no input provided");
        std::process::exit(1);
    }

    rt.block_on(run_interactive(model))
}

// ─── Arg parsing ──────────────────────────────────────────────────────────────

fn parse_args() -> Result<(String, String), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut model = DEFAULT_MODEL.to_string();
    let mut prompt_parts = Vec::new();
    let mut i = 0;
    let mut after_separator = false;

    while i < args.len() {
        if after_separator {
            prompt_parts.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => after_separator = true,
            "-m" | "--model" => {
                i += 1;
                model = args.get(i).ok_or("--model requires a value")?.clone();
            }
            other => prompt_parts.push(other.to_string()),
        }
        i += 1;
    }

    Ok((model, prompt_parts.join(" ")))
}

fn read_stdin() -> Result<String, Box<dyn std::error::Error>> {
    if io::stdin().is_terminal() {
        return Ok(String::new());
    }
    let mut buf = String::new();
    for line in io::stdin().lock().lines() {
        buf.push_str(&line?);
        buf.push('\n');
    }
    Ok(buf.trim().to_string())
}

// ─── Environment ──────────────────────────────────────────────────────────────

fn shell_info() -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".into());
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".into());
    format!("Shell: {shell}, OS: {os} ({arch}), CWD: {cwd}")
}

fn system_prompt() -> String {
    format!(
        r#"You are a terminal command assistant. Environment: {info}

The user describes a task. Respond with the shell command(s) that accomplish it.

You have one tool available: `is_available`. Use it to check whether commands exist on the system before suggesting them. Call it with a single command name as a string. It returns "yes" or "no".

After checking availability, respond with ONLY a JSON array of 1 to 3 command strings. No explanation, no markdown, no surrounding text — just the JSON array.

Examples of valid responses:
["git remote set-url origin https://example.com/repo.git"]
["find . -name '*.js' -type f"]
["rg 'pattern' src/", "grep -r 'pattern' src/", "find src/ -name '*.rs' -exec grep -l 'pattern' {{}} +"]

If you suggest multiple commands, they should be alternatives (not a sequence). Order them by preference."#,
        info = shell_info()
    )
}

// ─── Modes ────────────────────────────────────────────────────────────────────

/// Interactive mode: prompt → spinner → select.
async fn run_interactive(model: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut guard = TermGuard::new();

    // Header
    execute!(
        io::stderr(),
        SetForegroundColor(Color::DarkGrey),
        Print("how — ask your terminal\r\n\r\n"),
        ResetColor,
    )?;

    // Read prompt
    guard.raw_on()?;
    let prompt = read_prompt()?;
    guard.raw_off()?;
    eprint!("\r\n");

    if prompt.is_empty() {
        return Ok(());
    }

    run_and_select(&mut guard, model, prompt).await
}

/// Prompt given via args, terminal attached — spinner + select.
async fn run_tui(model: String, prompt: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut guard = TermGuard::new();
    run_and_select(&mut guard, model, prompt).await
}

/// Shared: spinner → select → run.
async fn run_and_select(
    guard: &mut TermGuard,
    model: String,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let cancellation = CancellationController::new();

    // Spinner phase
    guard.hide_cursor()?;
    guard.raw_on()?;
    let result = spin_while_thinking(model, prompt, cancellation.clone()).await;
    execute!(
        io::stderr(),
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
    )?;
    guard.raw_off()?;

    let commands = match result {
        Ok(cmds) => cmds,
        Err(AgentError::Cancelled) => {
            guard.show_cursor()?;
            eprint!("\r\n");
            std::process::exit(130);
        }
        Err(AgentError::Other(e)) => {
            guard.show_cursor()?;
            return Err(e);
        }
    };

    if commands.is_empty() {
        guard.show_cursor()?;
        eprintln!("could not determine commands");
        std::process::exit(1);
    }

    // Selection phase
    guard.raw_on()?;
    let selection = select_command(&commands)?;
    guard.show_cursor()?;
    guard.raw_off()?;

    if let Some(cmd) = selection {
        execute!(
            io::stderr(),
            SetForegroundColor(Color::DarkGrey),
            Print(format!("$ {cmd}\n")),
            ResetColor,
        )?;
        run_command(&cmd);
    }

    Ok(())
}

/// Plain mode: no TUI, print commands to stdout.
async fn run_plain(model: String, prompt: String) -> Result<(), Box<dyn std::error::Error>> {
    let cancel = CancellationController::new();
    let commands = run_agent(model, prompt, cancel)
        .await
        .map_err(|e| match e {
            AgentError::Cancelled => "cancelled".into(),
            AgentError::Other(e) => e,
        })?;

    if commands.is_empty() {
        eprintln!("could not parse commands from response");
        std::process::exit(1);
    }

    for cmd in &commands {
        println!("{cmd}");
    }

    Ok(())
}

// ─── Prompt input ─────────────────────────────────────────────────────────────

fn read_prompt() -> Result<String, Box<dyn std::error::Error>> {
    let mut w = io::stderr();
    let mut input = String::new();
    let mut pos: usize = 0;

    redraw_prompt(&mut w, &input, pos)?;

    loop {
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != event::KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return Ok(input),
            KeyCode::Char('c') if ctrl => return Ok(String::new()),
            KeyCode::Backspace if pos > 0 => {
                input.remove(pos - 1);
                pos -= 1;
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Delete if pos < input.len() => {
                input.remove(pos);
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Left if pos > 0 => {
                pos -= 1;
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Right if pos < input.len() => {
                pos += 1;
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Home => {
                pos = 0;
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::End => {
                pos = input.len();
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Char('a') if ctrl => {
                pos = 0;
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Char('e') if ctrl => {
                pos = input.len();
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Char('u') if ctrl => {
                input.drain(..pos);
                pos = 0;
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Char('w') if ctrl => {
                // Delete word backward
                let orig = pos;
                while pos > 0 && input.as_bytes()[pos - 1] == b' ' {
                    pos -= 1;
                }
                while pos > 0 && input.as_bytes()[pos - 1] != b' ' {
                    pos -= 1;
                }
                input.drain(pos..orig);
                redraw_prompt(&mut w, &input, pos)?;
            }
            KeyCode::Char(c) => {
                input.insert(pos, c);
                pos += 1;
                redraw_prompt(&mut w, &input, pos)?;
            }
            _ => {}
        }
    }
}

fn redraw_prompt(w: &mut impl Write, input: &str, cursor_pos: usize) -> io::Result<()> {
    execute!(
        w,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::Magenta),
        Print("❯ "),
        ResetColor,
        Print(input),
        cursor::MoveToColumn(2 + cursor_pos as u16),
    )?;
    w.flush()
}

// ─── Spinner ──────────────────────────────────────────────────────────────────

async fn spin_while_thinking(
    model: String,
    prompt: String,
    cancel: CancellationController,
) -> Result<Vec<String>, AgentError> {
    let agent_fut = run_agent(model, prompt, cancel.clone());
    tokio::pin!(agent_fut);

    let mut w = io::stderr();
    let mut frame = 0usize;

    loop {
        tokio::select! {
            result = &mut agent_fut => {
                return result;
            }
            _ = tokio::time::sleep(Duration::from_millis(80)) => {
                // Drain key events for Ctrl+C
                while event::poll(Duration::ZERO).unwrap_or(false) {
                    if let Ok(Event::Key(key)) = event::read()
                        && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            cancel.interrupt();
                        }
                }

                let dot = SPINNER[frame % SPINNER.len()];
                let _ = execute!(
                    w,
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine),
                    SetForegroundColor(Color::Cyan),
                    Print(dot),
                    Print(" "),
                    SetForegroundColor(Color::DarkGrey),
                    Print("thinking..."),
                    ResetColor,
                );
                let _ = w.flush();
                frame += 1;
            }
        }
    }
}

// ─── Agent ────────────────────────────────────────────────────────────────────

enum AgentError {
    Cancelled,
    Other(Box<dyn std::error::Error>),
}

impl From<agentkit_loop::LoopError> for AgentError {
    fn from(e: agentkit_loop::LoopError) -> Self {
        AgentError::Other(e.into())
    }
}

async fn run_agent(
    model: String,
    prompt: String,
    cancellation: CancellationController,
) -> Result<Vec<String>, AgentError> {
    let config = OpenRouterConfig::from_env()
        .map_err(|e| AgentError::Other(e.into()))?
        .with_temperature(0.0);
    let config = if model != DEFAULT_MODEL {
        OpenRouterConfig::new(config.api_key.clone(), &model)
            .with_temperature(0.0)
            .with_base_url(config.base_url.clone())
    } else {
        config
    };

    let adapter = OpenRouterAdapter::new(config).map_err(|e| AgentError::Other(e.into()))?;
    let tools = ToolRegistry::new().with(IsAvailableTool);

    let cache_key = prompt_cache_key(&model, &prompt);

    let agent = Agent::builder()
        .model(adapter)
        .add_tool_source(tools)
        .cancellation(cancellation.handle())
        .transcript(vec![Item::text(ItemKind::System, system_prompt())])
        .input(vec![Item::text(ItemKind::User, prompt)])
        .build()?;

    let mut driver = agent
        .start(
            SessionConfig::new("how").with_cache(
                PromptCacheRequest::automatic()
                    .with_retention(PromptCacheRetention::Short)
                    .with_key(cache_key),
            ),
        )
        .await?;

    loop {
        let step = driver.next().await;
        match step? {
            LoopStep::Finished(result) => {
                if result.finish_reason == FinishReason::Cancelled {
                    return Err(AgentError::Cancelled);
                }
                let text = extract_text(&result.items);
                return Ok(parse_commands(&text));
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(req)) => {
                req.approve(&mut driver)?;
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => break,
        }
    }

    Ok(Vec::new())
}

// ─── Selection ────────────────────────────────────────────────────────────────

fn select_command(commands: &[String]) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut w = io::stderr();
    let mut selected: usize = 0;
    let line_count = commands.len() + 3; // header + commands + blank + hint

    // Hide cursor for clean list
    execute!(w, cursor::Hide)?;

    draw_list(&mut w, commands, selected)?;

    loop {
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != event::KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    commands.len() - 1
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = if selected >= commands.len() - 1 {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Enter => {
                clear_list(&mut w, line_count)?;
                execute!(w, cursor::Show)?;
                return Ok(Some(commands[selected].clone()));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                clear_list(&mut w, line_count)?;
                execute!(w, cursor::Show)?;
                return Ok(None);
            }
            KeyCode::Char('c') if ctrl => {
                clear_list(&mut w, line_count)?;
                execute!(w, cursor::Show)?;
                return Ok(None);
            }
            _ => continue,
        }

        // Redraw on navigation
        execute!(w, cursor::MoveUp(line_count as u16))?;
        draw_list(&mut w, commands, selected)?;
    }
}

fn draw_list(w: &mut impl Write, commands: &[String], selected: usize) -> io::Result<()> {
    // Header
    execute!(
        w,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print("  Pick a command:"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
    )?;

    // Commands
    for (i, cmd) in commands.iter().enumerate() {
        execute!(w, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        if i == selected {
            execute!(
                w,
                SetForegroundColor(Color::Cyan),
                Print("  ❯ "),
                SetAttribute(Attribute::Bold),
                Print(cmd),
                SetAttribute(Attribute::Reset),
                Print("\r\n"),
            )?;
        } else {
            execute!(
                w,
                SetForegroundColor(Color::DarkGrey),
                Print("    "),
                Print(cmd),
                ResetColor,
                Print("\r\n"),
            )?;
        }
    }

    // Blank line
    execute!(
        w,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Print("\r\n"),
    )?;

    // Hint
    execute!(
        w,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::DarkGrey),
        Print("  ↑↓ navigate · enter select · esc cancel"),
        ResetColor,
        Print("\r\n"),
    )?;

    w.flush()
}

fn clear_list(w: &mut impl Write, line_count: usize) -> io::Result<()> {
    execute!(
        w,
        cursor::MoveUp(line_count as u16),
        Clear(ClearType::FromCursorDown),
    )
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn extract_text(items: &[Item]) -> String {
    items
        .iter()
        .filter(|item| item.kind == ItemKind::Assistant)
        .flat_map(|item| &item.parts)
        .filter_map(|part| match part {
            Part::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_commands(text: &str) -> Vec<String> {
    let text = text.trim();
    let start = text.find('[');
    let end = text.rfind(']');

    if let (Some(start), Some(end)) = (start, end)
        && end > start
    {
        let json_str = &text[start..=end];
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(json_str) {
            return arr
                .into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }

    if !text.is_empty() && !text.contains('\n') {
        return vec![text.to_string()];
    }

    Vec::new()
}

fn run_command(cmd: &str) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let status = Command::new(&shell).arg("-c").arg(cmd).status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("failed to execute: {e}");
            std::process::exit(1);
        }
    }
}

// ─── is_available tool ────────────────────────────────────────────────────────

struct IsAvailableTool;

#[async_trait::async_trait]
impl Tool for IsAvailableTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
            ToolSpec::new(
                "is_available",
                "Check if a command is available on the system. Pass a single command name (e.g. \"rg\"). Returns \"yes\" or \"no\".",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command name to check"
                        }
                    },
                    "required": ["command"]
                }),
            )
        });
        &SPEC
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let command = request
            .input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");

        let available = Command::new("which")
            .arg(command)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text(if available { "yes" } else { "no" }),
        )))
    }
}
