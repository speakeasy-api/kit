//! Drives one (scenario, arm, repetition) tuple through a live agent loop and
//! collects metrics.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentkit_core::{Item, ItemKind};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, PromptCacheRequest, PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tool_compose::{ComposeConfig, ComposeTool, ResultEncoding, RunletBackend};

use crate::metrics::{MetricsObserver, MetricsState, RunRecord};
use crate::scenario::{Arm, BenchError, SYSTEM_PROMPT, Scenario};

pub struct BenchConfig {
    pub max_model_requests: u64,
    pub timeout: Duration,
    pub out_dir: std::path::PathBuf,
    /// Nested tool-call budget inside a compose script. The crate default (64)
    /// is smaller than a naive full-fan-out script for some scenarios; the
    /// benchmark raises it so arms compare composition, not self-repair skill.
    pub compose_max_nested_calls: usize,
    /// Encoding of the final compose result in the transcript, applied to
    /// both composition arms so they stay comparable.
    pub result_encoding: ResultEncoding,
}

pub async fn run_once(
    scenario: &dyn Scenario,
    arm: Arm,
    rep: u32,
    config: &BenchConfig,
) -> Result<RunRecord, BenchError> {
    let instance = scenario.setup(arm)?;
    let provider_config = OpenRouterConfig::from_env()?;
    let model = provider_config.model.clone();
    let adapter = OpenRouterAdapter::new(provider_config)?;

    let metrics = Arc::new(Mutex::new(MetricsState::default()));
    let mut builder = Agent::builder()
        .model(adapter)
        .observer(MetricsObserver(metrics.clone()))
        .transcript(vec![Item::text(ItemKind::System, SYSTEM_PROMPT)])
        .input(vec![Item::text(ItemKind::User, instance.user_prompt)]);

    let compose_config = ComposeConfig::new()
        .with_max_nested_tool_calls(config.compose_max_nested_calls)
        .with_result_encoding(config.result_encoding);
    builder = match arm {
        Arm::Compose => {
            builder.add_tool_source(ComposeTool::wrap(instance.tools).with_config(compose_config))
        }
        Arm::RunletCompose => builder.add_tool_source(
            ComposeTool::wrap(instance.tools)
                .with_config(compose_config)
                .with_backend(RunletBackend),
        ),
        Arm::Granular | Arm::Bash => builder.add_tool_source(instance.tools),
    };
    if let Some(permissions) = instance.permissions {
        builder = builder.permissions(permissions);
    }
    let agent = builder.build()?;

    let session_id = format!("compose-bench-{}-{}-{rep}", scenario.name(), arm.as_str());
    let mut driver = agent
        .start(SessionConfig::new(session_id).with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ))
        .await?;

    let started = Instant::now();
    let mut completed = false;
    let mut failure: Option<String> = None;

    loop {
        let elapsed = started.elapsed();
        if elapsed >= config.timeout {
            failure = Some(format!("timed out after {:?}", config.timeout));
            break;
        }
        let requests_so_far = metrics.lock().expect("metrics lock").model_requests;
        if requests_so_far >= config.max_model_requests {
            failure = Some(format!(
                "hit model request cap ({})",
                config.max_model_requests
            ));
            break;
        }
        let step = tokio::time::timeout(config.timeout - elapsed, driver.next()).await;
        match step {
            Err(_) => {
                failure = Some(format!("timed out after {:?}", config.timeout));
                break;
            }
            Ok(Err(error)) => {
                failure = Some(format!("loop error: {error}"));
                break;
            }
            Ok(Ok(LoopStep::Finished(_))) => {
                completed = true;
                break;
            }
            Ok(Ok(LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)))) => {}
            Ok(Ok(LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)))) => {
                failure = Some("model stopped and asked for more input".into());
                break;
            }
            Ok(Ok(LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)))) => {
                // The benchmark sandbox is disposable; approve everything so
                // arms are not penalised by interactive gating.
                pending.approve(&mut driver)?;
            }
        }
    }
    let wall_ms = started.elapsed().as_millis();

    write_transcript(
        &config.out_dir,
        scenario.name(),
        arm,
        rep,
        &driver.snapshot().transcript,
    )?;

    let submitted = instance
        .submission
        .lock()
        .expect("submission lock")
        .is_some();
    let score = (instance.scorer)();
    let metrics = metrics.lock().expect("metrics lock").clone();

    Ok(RunRecord {
        scenario: scenario.name().to_string(),
        arm: arm.as_str().to_string(),
        rep,
        model,
        wall_ms,
        metrics,
        accuracy: score.accuracy,
        submitted,
        completed,
        failure,
        notes: score.notes,
    })
}

fn write_transcript(
    out_dir: &Path,
    scenario: &str,
    arm: Arm,
    rep: u32,
    transcript: &[Item],
) -> Result<(), BenchError> {
    let dir = out_dir.join("transcripts");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{scenario}-{}-{rep}.json", arm.as_str()));
    std::fs::write(&path, serde_json::to_string_pretty(transcript)?)?;
    Ok(())
}
