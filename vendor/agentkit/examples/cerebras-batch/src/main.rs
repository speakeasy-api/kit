//! One-shot CLI covering the Cerebras Files + Batch API surface.
//!
//! Not a turn-loop demo — batch is submit-and-poll bulk
//! `/v1/chat/completions`. Every verb mirrors the API:
//!
//! ```text
//! cerebras-batch files   upload <path> [--purpose batch]
//! cerebras-batch files   list
//! cerebras-batch files   get <id>
//! cerebras-batch files   content <id>
//! cerebras-batch files   delete <id>
//! cerebras-batch batches create --input-file-id <id> [--metadata k=v]…
//! cerebras-batch batches submit <prompts.json> [--show-jsonl]
//! cerebras-batch batches list
//! cerebras-batch batches get <id>
//! cerebras-batch batches cancel <id>
//! cerebras-batch batches wait <id> [--poll-secs <n>]
//! cerebras-batch run     <prompts.json>   # submit → wait → dump outputs
//! ```
//!
//! `prompts.json` is a JSON array of `{custom_id, messages, overrides?}`
//! entries. Overrides layer onto the adapter's `CerebrasConfig` and are fed
//! through the same `request::build_chat_body` the turn loop uses, proving
//! the chat-request builder is the single source of truth for interactive
//! and bulk inference alike.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use agentkit_core::{
    CancellationController, Item, ItemKind, MetadataMap, SessionId, TurnCancellation, TurnId,
};
use agentkit_loop::TurnRequest;
use agentkit_provider_cerebras::{
    BatchOutcome, BatchStatus, CerebrasAdapter, CerebrasConfig, ChatOverrides, FilePurpose,
    OutputFormat, ReasoningConfig, ReasoningEffort, ReasoningFormat,
};
use futures_util::StreamExt;
use serde_json::Value;

type BoxErr = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    dotenvy::dotenv().ok();

    let mut args = std::env::args().skip(1);
    let command = match args.next() {
        Some(c) => c,
        None => {
            print_help();
            return Ok(());
        }
    };

    let adapter = build_adapter()?;

    match command.as_str() {
        "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        "files" => run_files(&adapter, args.collect()).await,
        "batches" => run_batches(&adapter, args.collect()).await,
        "run" => run_chain(&adapter, args.collect()).await,
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            Err("unknown command".into())
        }
    }
}

fn build_adapter() -> Result<CerebrasAdapter, BoxErr> {
    let config = CerebrasConfig::from_env()?;
    Ok(CerebrasAdapter::new(config)?)
}

fn print_help() {
    println!(
        "cerebras-batch — exercise Files + Batch API surfaces\n\n\
         USAGE:\n    cerebras-batch <command> [OPTIONS]\n\n\
         FILES:\n    \
             files upload <path> [--purpose batch]\n    \
             files list\n    \
             files get <id>\n    \
             files content <id>        Streams raw bytes to stdout\n    \
             files delete <id>\n\n\
         BATCHES:\n    \
             batches create --input-file-id <id> [--metadata k=v]…\n    \
             batches submit <prompts.json> [--show-jsonl]\n    \
             batches list\n    \
             batches get <id>\n    \
             batches cancel <id>\n    \
             batches wait <id> [--poll-secs <n>]\n\n\
         CHAIN:\n    \
             run <prompts.json>         submit → wait → dump outputs + errors\n\n\
         Env: CEREBRAS_API_KEY, CEREBRAS_MODEL (+ anything CerebrasConfig::from_env reads)\n"
    );
}

// --- files ----------------------------------------------------------------

async fn run_files(adapter: &CerebrasAdapter, args: Vec<String>) -> Result<(), BoxErr> {
    let mut iter = args.into_iter();
    let Some(sub) = iter.next() else {
        return Err("files: expected subcommand".into());
    };
    let client = adapter.files();
    match sub.as_str() {
        "upload" => {
            let path = iter.next().ok_or("files upload: expected <path>")?;
            let mut purpose = FilePurpose::Batch;
            while let Some(flag) = iter.next() {
                match flag.as_str() {
                    "--purpose" => {
                        let raw = iter.next().ok_or("--purpose requires a value")?;
                        purpose = match raw.as_str() {
                            "batch" => FilePurpose::Batch,
                            _ => return Err(format!("unknown purpose: {raw}").into()),
                        };
                    }
                    other => return Err(format!("unknown flag: {other}").into()),
                }
            }
            let bytes = std::fs::read(&path)?;
            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("upload.jsonl")
                .to_string();
            let file = client.upload(&filename, bytes, purpose).await?;
            println!("{}", serde_json::to_string_pretty(&file)?);
        }
        "list" => {
            let files = client.list().await?;
            println!("{}", serde_json::to_string_pretty(&files)?);
        }
        "get" => {
            let id = iter.next().ok_or("files get: expected <id>")?;
            let file = client.retrieve(&id).await?;
            println!("{}", serde_json::to_string_pretty(&file)?);
        }
        "content" => {
            let id = iter.next().ok_or("files content: expected <id>")?;
            let mut stream = client.content(&id).await?;
            let mut stdout = std::io::stdout().lock();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk?;
                stdout.write_all(&bytes)?;
            }
            stdout.flush()?;
        }
        "delete" => {
            let id = iter.next().ok_or("files delete: expected <id>")?;
            client.delete(&id).await?;
            println!("[deleted {id}]");
        }
        other => return Err(format!("unknown files subcommand: {other}").into()),
    }
    Ok(())
}

// --- batches --------------------------------------------------------------

async fn run_batches(adapter: &CerebrasAdapter, args: Vec<String>) -> Result<(), BoxErr> {
    let mut iter = args.into_iter();
    let Some(sub) = iter.next() else {
        return Err("batches: expected subcommand".into());
    };
    let client = adapter.batches();
    match sub.as_str() {
        "create" => {
            let mut input_file_id = None;
            let mut metadata: BTreeMap<String, String> = BTreeMap::new();
            while let Some(flag) = iter.next() {
                match flag.as_str() {
                    "--input-file-id" => {
                        input_file_id =
                            Some(iter.next().ok_or("--input-file-id requires a value")?);
                    }
                    "--metadata" => {
                        let kv = iter.next().ok_or("--metadata requires k=v")?;
                        let (k, v) = kv.split_once('=').ok_or("--metadata expects key=value")?;
                        metadata.insert(k.into(), v.into());
                    }
                    other => return Err(format!("unknown flag: {other}").into()),
                }
            }
            let input = input_file_id.ok_or("--input-file-id is required")?;
            let job = client.create(&input, metadata).await?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        "submit" => {
            let path = iter
                .next()
                .ok_or("batches submit: expected <prompts.json>")?;
            let mut show_jsonl = false;
            let mut meta: BTreeMap<String, String> = BTreeMap::new();
            while let Some(flag) = iter.next() {
                match flag.as_str() {
                    "--show-jsonl" => show_jsonl = true,
                    "--metadata" => {
                        let kv = iter.next().ok_or("--metadata requires k=v")?;
                        let (k, v) = kv.split_once('=').ok_or("--metadata expects key=value")?;
                        meta.insert(k.into(), v.into());
                    }
                    other => return Err(format!("unknown flag: {other}").into()),
                }
            }
            let prompts = load_prompts(&path)?;
            if show_jsonl {
                render_jsonl_preview(adapter, &prompts)?;
            }
            let items = prompts
                .into_iter()
                .map(|entry| (entry.custom_id, entry.turn_request, entry.overrides));
            let job = client.submit_chat_batch(items, meta).await?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        "list" => {
            let jobs = client.list().await?;
            println!("{}", serde_json::to_string_pretty(&jobs)?);
        }
        "get" => {
            let id = iter.next().ok_or("batches get: expected <id>")?;
            let job = client.retrieve(&id).await?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        "cancel" => {
            let id = iter.next().ok_or("batches cancel: expected <id>")?;
            let job = client.cancel(&id).await?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        "wait" => {
            let id = iter.next().ok_or("batches wait: expected <id>")?;
            let mut poll_secs: u64 = 5;
            while let Some(flag) = iter.next() {
                match flag.as_str() {
                    "--poll-secs" => {
                        poll_secs = iter.next().ok_or("--poll-secs requires a value")?.parse()?;
                    }
                    other => return Err(format!("unknown flag: {other}").into()),
                }
            }
            let outcome = poll_to_terminal(adapter, &id, poll_secs).await?;
            dump_outcome(outcome).await?;
        }
        other => return Err(format!("unknown batches subcommand: {other}").into()),
    }
    Ok(())
}

// --- run (chain) ----------------------------------------------------------

async fn run_chain(adapter: &CerebrasAdapter, args: Vec<String>) -> Result<(), BoxErr> {
    let mut iter = args.into_iter();
    let path = iter.next().ok_or("run: expected <prompts.json>")?;
    let prompts = load_prompts(&path)?;
    let items = prompts
        .into_iter()
        .map(|entry| (entry.custom_id, entry.turn_request, entry.overrides));
    let client = adapter.batches();
    let job = client.submit_chat_batch(items, BTreeMap::new()).await?;
    println!("[submitted] id={} status={:?}", job.id, job.status);
    let outcome = poll_to_terminal(adapter, &job.id, 5).await?;
    dump_outcome(outcome).await?;
    Ok(())
}

// --- helpers --------------------------------------------------------------

struct PromptEntry {
    custom_id: String,
    turn_request: TurnRequest,
    overrides: Option<ChatOverrides>,
}

fn load_prompts(path: &str) -> Result<Vec<PromptEntry>, BoxErr> {
    let text = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)?;
    let arr = value
        .as_array()
        .ok_or("prompts.json must be a JSON array")?;
    let mut out = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let custom_id = entry
            .get("custom_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("item-{idx}"));
        let messages = entry
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("entry {custom_id} missing `messages` array"))?;
        let transcript = build_transcript(messages, &custom_id)?;
        let turn_request = TurnRequest {
            session_id: SessionId::new(format!("batch-session-{custom_id}")),
            turn_id: TurnId::new(format!("batch-turn-{custom_id}")),
            transcript,
            available_tools: Vec::new(),
            cache: None,
            metadata: MetadataMap::new(),
        };
        let overrides = entry.get("overrides").map(parse_overrides).transpose()?;
        out.push(PromptEntry {
            custom_id,
            turn_request,
            overrides,
        });
    }
    Ok(out)
}

fn build_transcript(messages: &[Value], custom_id: &str) -> Result<Vec<Item>, BoxErr> {
    let mut transcript = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("entry {custom_id}: message missing `role`"))?;
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("entry {custom_id}: only string `content` supported"))?;
        let kind = match role {
            "system" => ItemKind::System,
            "developer" => ItemKind::Developer,
            "user" => ItemKind::User,
            "assistant" => ItemKind::Assistant,
            other => return Err(format!("entry {custom_id}: unsupported role `{other}`").into()),
        };
        transcript.push(Item::text(kind, content));
    }
    Ok(transcript)
}

fn parse_overrides(raw: &Value) -> Result<ChatOverrides, BoxErr> {
    let mut out = ChatOverrides::default();
    if let Some(v) = raw.get("model").and_then(Value::as_str) {
        out.model = Some(v.into());
    }
    if let Some(v) = raw.get("max_completion_tokens").and_then(Value::as_u64) {
        out.max_completion_tokens = Some(v as u32);
    }
    if let Some(v) = raw.get("temperature").and_then(Value::as_f64) {
        out.temperature = Some(v as f32);
    }
    if let Some(reasoning) = raw.get("reasoning") {
        let mut r = ReasoningConfig::new();
        if let Some(effort) = reasoning.get("effort").and_then(Value::as_str) {
            r = r.with_effort(match effort {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "none" => ReasoningEffort::None,
                other => return Err(format!("bad reasoning.effort: {other}").into()),
            });
        }
        if let Some(format) = reasoning.get("format").and_then(Value::as_str) {
            r = r.with_format(match format {
                "parsed" => ReasoningFormat::Parsed,
                "raw" => ReasoningFormat::Raw,
                "hidden" => ReasoningFormat::Hidden,
                "none" => ReasoningFormat::None,
                other => return Err(format!("bad reasoning.format: {other}").into()),
            });
        }
        if let Some(b) = reasoning.get("clear_thinking").and_then(Value::as_bool) {
            r = r.with_clear_thinking(b);
        }
        if let Some(b) = reasoning.get("disable_reasoning").and_then(Value::as_bool) {
            r = r.with_disable_reasoning(b);
        }
        out.reasoning = Some(r);
    }
    if let Some(fmt) = raw.get("response_format") {
        out.response_format = Some(parse_output_format(fmt)?);
    }
    Ok(out)
}

fn parse_output_format(raw: &Value) -> Result<OutputFormat, BoxErr> {
    let t = raw
        .get("type")
        .and_then(Value::as_str)
        .ok_or("response_format missing `type`")?;
    Ok(match t {
        "text" => OutputFormat::Text,
        "json_object" => OutputFormat::JsonObject,
        "json_schema" => {
            let js = raw
                .get("json_schema")
                .ok_or("json_schema missing `json_schema` object")?;
            let schema = js
                .get("schema")
                .cloned()
                .ok_or("json_schema.schema missing")?;
            let strict = js.get("strict").and_then(Value::as_bool).unwrap_or(true);
            let name = js.get("name").and_then(Value::as_str).map(String::from);
            OutputFormat::JsonSchema {
                schema,
                strict,
                name,
            }
        }
        other => return Err(format!("unknown response_format.type: {other}").into()),
    })
}

fn render_jsonl_preview(adapter: &CerebrasAdapter, prompts: &[PromptEntry]) -> Result<(), BoxErr> {
    eprintln!("[jsonl preview]");
    for entry in prompts {
        let cfg = match &entry.overrides {
            Some(ov) => apply_overrides(adapter, ov),
            None => clone_config(adapter),
        };
        let built =
            agentkit_provider_cerebras::request::build_chat_body(&cfg, &entry.turn_request)?;
        let line = serde_json::json!({
            "custom_id": entry.custom_id,
            "method": "POST",
            "url": "/v1/chat/completions",
            "body": built.body,
        });
        eprintln!("{}", serde_json::to_string(&line)?);
    }
    Ok(())
}

fn clone_config(adapter: &CerebrasAdapter) -> CerebrasConfig {
    let mut cfg = adapter_config(adapter);
    cfg.streaming = false;
    cfg
}

fn apply_overrides(adapter: &CerebrasAdapter, ov: &ChatOverrides) -> CerebrasConfig {
    let mut cfg = clone_config(adapter);
    if let Some(m) = &ov.model {
        cfg.model = m.clone();
    }
    if let Some(v) = ov.max_completion_tokens {
        cfg.max_completion_tokens = Some(v);
    }
    if let Some(v) = ov.temperature {
        cfg.temperature = Some(v);
    }
    if let Some(r) = &ov.reasoning {
        cfg.reasoning = Some(r.clone());
    }
    if let Some(f) = &ov.response_format {
        cfg.output_format = Some(f.clone());
    }
    cfg
}

fn adapter_config(_adapter: &CerebrasAdapter) -> CerebrasConfig {
    // The adapter does not expose its internal `CerebrasConfig` directly
    // (held behind `Arc`), so for preview rendering we rebuild from env —
    // the same source the adapter was constructed from. Overrides from the
    // JSONL layer on top. Keeps the preview faithful to what `submit` sends.
    CerebrasConfig::from_env().expect("CEREBRAS_API_KEY/CEREBRAS_MODEL must be set")
}

async fn poll_to_terminal(
    adapter: &CerebrasAdapter,
    id: &str,
    poll_secs: u64,
) -> Result<BatchOutcome, BoxErr> {
    let controller = CancellationController::new();
    let signal_handle = controller.handle();
    let signal_interrupter = controller.clone();
    let signal_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_interrupter.interrupt();
    });

    let client = adapter.batches();
    let mut last_status: Option<BatchStatus> = None;
    loop {
        let job = client.retrieve(id).await?;
        if last_status != Some(job.status) {
            println!(
                "[status] id={} status={:?} counts={{total={}, completed={}, failed={}}}",
                job.id,
                job.status,
                job.request_counts.total,
                job.request_counts.completed,
                job.request_counts.failed
            );
            last_status = Some(job.status);
        }
        if matches!(
            job.status,
            BatchStatus::Completed
                | BatchStatus::Failed
                | BatchStatus::Expired
                | BatchStatus::Cancelled
        ) {
            signal_task.abort();
            return client
                .wait(
                    id,
                    Duration::from_millis(1),
                    Some(TurnCancellation::new(signal_handle)),
                )
                .await
                .map_err(Into::into);
        }
        let checkpoint = TurnCancellation::new(signal_handle.clone());
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(poll_secs)) => {}
            _ = checkpoint.cancelled() => {
                signal_task.abort();
                return Err("wait cancelled".into());
            }
        }
    }
}

async fn dump_outcome(outcome: BatchOutcome) -> Result<(), BoxErr> {
    println!("[terminal] status={:?}", outcome.job.status);
    println!("{}", serde_json::to_string_pretty(&outcome.job)?);
    if let Some(mut outputs) = outcome.outputs {
        println!("[outputs]");
        let mut stdout = std::io::stdout().lock();
        while let Some(chunk) = outputs.next().await {
            stdout.write_all(&chunk?)?;
        }
        stdout.flush()?;
    }
    if let Some(mut errors) = outcome.errors {
        eprintln!("[errors]");
        let mut stderr = std::io::stderr().lock();
        while let Some(chunk) = errors.next().await {
            stderr.write_all(&chunk?)?;
        }
        stderr.flush()?;
    }
    Ok(())
}
