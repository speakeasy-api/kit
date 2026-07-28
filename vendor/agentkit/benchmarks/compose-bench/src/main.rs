//! compose-bench: does the `compose` Lua tool give agents the same efficiency
//! win over granular tool calls that Bash pipelines give over file tools?
//!
//! Runs life-like scenarios (mock SaaS backends + one file-backed task)
//! against a live model via OpenRouter, in up to three arms per scenario:
//!
//! - `granular` — scenario tools only, one model round-trip per call
//! - `compose`  — the same tools wrapped by `ComposeTool` (compose AND the
//!   granular tools are both visible, so the model's *preference* is measured,
//!   not forced)
//! - `bash`     — `shell_exec` only (file-backed scenario), the reference point
//!
//! Per run it records wall time, model round-trips, tool calls (and how many
//! went through compose), token usage, peak context size, provider-reported
//! cost, and rubric-scored accuracy.
//!
//! Usage:
//!
//! ```bash
//! OPENROUTER_API_KEY=.. OPENROUTER_MODEL=anthropic/claude-sonnet-4.6 \
//!   cargo run -p compose-bench --release -- --reps 3
//! ```
//!
//! Flags: `--scenarios a,b`, `--arms granular,compose,bash`, `--reps N`,
//! `--max-requests N`, `--timeout-secs N`, `--tool-latency-ms N`,
//! `--compose-max-nested N`, `--result-encoding json|toon`,
//! `--concurrency N`, `--out DIR`.

mod harness;
mod metrics;
mod scenario;
mod scenarios;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentkit_tool_compose::ResultEncoding;
use harness::BenchConfig;
use metrics::RunRecord;
use scenario::{Arm, Scenario};

struct Args {
    scenarios: Option<Vec<String>>,
    arms: Option<Vec<Arm>>,
    reps: u32,
    max_requests: u64,
    timeout_secs: u64,
    tool_latency_ms: u64,
    compose_max_nested: usize,
    result_encoding: ResultEncoding,
    concurrency: usize,
    out: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        scenarios: None,
        arms: None,
        reps: 1,
        max_requests: 60,
        timeout_secs: 600,
        tool_latency_ms: 0,
        compose_max_nested: 256,
        result_encoding: ResultEncoding::Json,
        concurrency: 1,
        out: PathBuf::from("target/compose-bench-results"),
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .ok_or_else(|| format!("flag {flag} requires a value"))
        };
        match flag.as_str() {
            "--scenarios" => {
                args.scenarios = Some(value()?.split(',').map(str::to_string).collect());
            }
            "--arms" => {
                let arms = value()?
                    .split(',')
                    .map(|raw| Arm::parse(raw).ok_or_else(|| format!("unknown arm `{raw}`")))
                    .collect::<Result<Vec<_>, _>>()?;
                args.arms = Some(arms);
            }
            "--reps" => args.reps = value()?.parse().map_err(|e| format!("--reps: {e}"))?,
            "--max-requests" => {
                args.max_requests = value()?
                    .parse()
                    .map_err(|e| format!("--max-requests: {e}"))?;
            }
            "--timeout-secs" => {
                args.timeout_secs = value()?
                    .parse()
                    .map_err(|e| format!("--timeout-secs: {e}"))?;
            }
            "--tool-latency-ms" => {
                args.tool_latency_ms = value()?
                    .parse()
                    .map_err(|e| format!("--tool-latency-ms: {e}"))?;
            }
            "--compose-max-nested" => {
                args.compose_max_nested = value()?
                    .parse()
                    .map_err(|e| format!("--compose-max-nested: {e}"))?;
            }
            "--result-encoding" => {
                args.result_encoding = match value()?.as_str() {
                    "json" => ResultEncoding::Json,
                    "toon" => ResultEncoding::Toon,
                    other => return Err(format!("unknown result encoding `{other}`")),
                };
            }
            "--concurrency" => {
                args.concurrency = value()?
                    .parse::<usize>()
                    .map_err(|e| format!("--concurrency: {e}"))?
                    .max(1);
            }
            "--out" => args.out = PathBuf::from(value()?),
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    let args = parse_args().map_err(|message| {
        eprintln!("{message}");
        message
    })?;

    scenario::set_tool_latency(Duration::from_millis(args.tool_latency_ms));
    std::fs::create_dir_all(&args.out)?;
    let config = Arc::new(BenchConfig {
        max_model_requests: args.max_requests,
        timeout: Duration::from_secs(args.timeout_secs),
        out_dir: args.out.clone(),
        compose_max_nested_calls: args.compose_max_nested,
        result_encoding: args.result_encoding,
    });

    let scenarios: Vec<Arc<dyn Scenario>> = scenarios::all().into_iter().map(Arc::from).collect();
    let selected: Vec<_> = scenarios
        .iter()
        .filter(|s| {
            args.scenarios
                .as_ref()
                .is_none_or(|names| names.iter().any(|n| n == s.name()))
        })
        .collect();
    if selected.is_empty() {
        return Err(format!(
            "no scenarios selected; available: {}",
            scenarios
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    let runs_path = args.out.join("runs.jsonl");
    let mut runs_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&runs_path)?;
    let mut records: Vec<RunRecord> = Vec::new();

    // Runs are independent (each gets a fresh world from `Scenario::setup`),
    // so they fan out across a semaphore-bounded task set. `--concurrency 1`
    // (the default) preserves the old one-at-a-time behaviour.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(args.concurrency));
    let mut pending = tokio::task::JoinSet::new();
    for scenario in &selected {
        let arms: Vec<Arm> = scenario
            .arms()
            .into_iter()
            .filter(|arm| args.arms.as_ref().is_none_or(|wanted| wanted.contains(arm)))
            .collect();
        for arm in arms {
            for rep in 1..=args.reps {
                let scenario = Arc::clone(scenario);
                let config = Arc::clone(&config);
                let semaphore = Arc::clone(&semaphore);
                let reps = args.reps;
                pending.spawn(async move {
                    let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                    println!(
                        "=== {} / {} / rep {rep}/{reps} started ===",
                        scenario.name(),
                        arm.as_str(),
                    );
                    let outcome = harness::run_once(scenario.as_ref(), arm, rep, &config).await;
                    (scenario.name(), arm, rep, outcome)
                });
            }
        }
    }
    while let Some(joined) = pending.join_next().await {
        let (name, arm, rep, outcome) = joined?;
        match outcome {
            Ok(record) => {
                println!(
                    "{name} / {} / rep {rep}: wall={:.1}s requests={} tool_calls={} (compose={}, failed={}) tokens(in/out)={}/{} peak_ctx={} cost={} accuracy={:.2}{}",
                    arm.as_str(),
                    record.wall_ms as f64 / 1000.0,
                    record.metrics.model_requests,
                    record.metrics.tool_calls,
                    record.metrics.compose_calls,
                    record.metrics.compose_failures,
                    record.metrics.input_tokens + record.metrics.cached_input_tokens,
                    record.metrics.output_tokens,
                    record.metrics.peak_context_tokens,
                    if record.metrics.cost_reported {
                        format!("${:.4}", record.metrics.cost_usd)
                    } else {
                        "n/a".into()
                    },
                    record.accuracy,
                    record
                        .failure
                        .as_deref()
                        .map(|f| format!(" [{f}]"))
                        .unwrap_or_default(),
                );
                writeln!(runs_file, "{}", serde_json::to_string(&record)?)?;
                records.push(record);
            }
            Err(error) => {
                eprintln!(
                    "{name} / {} / rep {rep}: run failed before metrics could be collected: {error}",
                    arm.as_str(),
                );
            }
        }
    }

    let report = render_report(&records);
    let report_path = args.out.join("report.md");
    std::fs::write(&report_path, &report)?;
    println!("\n{report}");
    println!("raw runs: {}", runs_path.display());
    println!("report:   {}", report_path.display());
    Ok(())
}

struct Aggregate {
    runs: usize,
    wall_s: Stats,
    requests: Stats,
    tool_calls: Stats,
    compose_share: f64,
    compose_used_runs: usize,
    compose_failures: Stats,
    total_tokens: Stats,
    peak_context: Stats,
    cost: Option<Stats>,
    accuracy: Stats,
}

struct Stats {
    mean: f64,
    sd: f64,
}

fn stats(values: &[f64]) -> Stats {
    let n = values.len().max(1) as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    Stats {
        mean,
        sd: variance.sqrt(),
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.sd > 0.0 {
            write!(f, "{:.1}±{:.1}", self.mean, self.sd)
        } else {
            write!(f, "{:.1}", self.mean)
        }
    }
}

fn aggregate(records: &[&RunRecord]) -> Aggregate {
    let wall: Vec<f64> = records.iter().map(|r| r.wall_ms as f64 / 1000.0).collect();
    let requests: Vec<f64> = records
        .iter()
        .map(|r| r.metrics.model_requests as f64)
        .collect();
    let tool_calls: Vec<f64> = records
        .iter()
        .map(|r| r.metrics.tool_calls as f64)
        .collect();
    let tokens: Vec<f64> = records
        .iter()
        .map(|r| {
            (r.metrics.input_tokens + r.metrics.cached_input_tokens + r.metrics.output_tokens)
                as f64
        })
        .collect();
    let peak: Vec<f64> = records
        .iter()
        .map(|r| r.metrics.peak_context_tokens as f64)
        .collect();
    let accuracy: Vec<f64> = records.iter().map(|r| r.accuracy).collect();
    let costs: Vec<f64> = records
        .iter()
        .filter(|r| r.metrics.cost_reported)
        .map(|r| r.metrics.cost_usd)
        .collect();
    let total_calls: u64 = records.iter().map(|r| r.metrics.tool_calls).sum();
    let compose_calls: u64 = records.iter().map(|r| r.metrics.compose_calls).sum();
    Aggregate {
        runs: records.len(),
        wall_s: stats(&wall),
        requests: stats(&requests),
        tool_calls: stats(&tool_calls),
        compose_share: if total_calls == 0 {
            0.0
        } else {
            compose_calls as f64 / total_calls as f64
        },
        compose_used_runs: records
            .iter()
            .filter(|r| r.metrics.compose_calls > 0)
            .count(),
        compose_failures: stats(
            &records
                .iter()
                .map(|r| r.metrics.compose_failures as f64)
                .collect::<Vec<_>>(),
        ),
        total_tokens: stats(&tokens),
        peak_context: stats(&peak),
        cost: if costs.len() == records.len() && !costs.is_empty() {
            Some(stats(&costs))
        } else {
            None
        },
        accuracy: stats(&accuracy),
    }
}

fn render_report(records: &[RunRecord]) -> String {
    let mut grouped: BTreeMap<(String, String), Vec<&RunRecord>> = BTreeMap::new();
    for record in records {
        grouped
            .entry((record.scenario.clone(), record.arm.clone()))
            .or_default()
            .push(record);
    }

    let model = records
        .first()
        .map(|r| r.model.as_str())
        .unwrap_or("unknown");
    let mut out = String::new();
    let _ = writeln!(out, "# compose-bench report\n");
    let _ = writeln!(
        out,
        "Model: `{model}`. Token columns sum all requests in a run; peak ctx is the largest single request (input + cached + output). Cost is provider-reported (blank when OpenRouter omitted it).\n"
    );
    let _ = writeln!(
        out,
        "| scenario | arm | runs | wall s | model reqs | tool calls | compose share | compose fails | total tokens | peak ctx | cost $ | accuracy |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|---|");
    for ((scenario, arm), group) in &grouped {
        let agg = aggregate(group);
        let _ = writeln!(
            out,
            "| {scenario} | {arm} | {} | {} | {} | {} | {:.0}% ({}/{} runs) | {} | {} | {} | {} | {:.2}±{:.2} |",
            agg.runs,
            agg.wall_s,
            agg.requests,
            agg.tool_calls,
            agg.compose_share * 100.0,
            agg.compose_used_runs,
            agg.runs,
            agg.compose_failures,
            agg.total_tokens,
            agg.peak_context,
            agg.cost
                .map(|c| format!("{:.4}±{:.4}", c.mean, c.sd))
                .unwrap_or_else(|| "—".into()),
            agg.accuracy.mean,
            agg.accuracy.sd,
        );
    }

    // Per-scenario deltas: each composition arm vs granular.
    let _ = writeln!(out, "\n## composition arms vs granular (per scenario)\n");
    let _ = writeln!(
        out,
        "| scenario | arm | Δ wall | Δ model reqs | Δ total tokens | Δ cost | Δ accuracy |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|");
    let scenario_names: Vec<String> = {
        let mut names: Vec<String> = grouped.keys().map(|(s, _)| s.clone()).collect();
        names.dedup();
        names
    };
    for scenario in scenario_names {
        let Some(granular) = grouped.get(&(scenario.clone(), "granular".into())) else {
            continue;
        };
        let g = aggregate(granular);
        for arm in ["compose", "runlet"] {
            let Some(composed) = grouped.get(&(scenario.clone(), arm.into())) else {
                continue;
            };
            let c = aggregate(composed);
            let pct = |new: f64, old: f64| -> String {
                if old == 0.0 {
                    "—".into()
                } else {
                    format!("{:+.0}%", (new - old) / old * 100.0)
                }
            };
            let cost_delta = match (&c.cost, &g.cost) {
                (Some(c), Some(g)) => pct(c.mean, g.mean),
                _ => "—".into(),
            };
            let _ = writeln!(
                out,
                "| {scenario} | {arm} | {} | {} | {} | {cost_delta} | {:+.2} |",
                pct(c.wall_s.mean, g.wall_s.mean),
                pct(c.requests.mean, g.requests.mean),
                pct(c.total_tokens.mean, g.total_tokens.mean),
                c.accuracy.mean - g.accuracy.mean,
            );
        }
    }
    out
}
