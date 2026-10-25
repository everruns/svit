//! Mira eval study: **personal-agent**.
//!
//! Thirty hard personal-assistant tasks (scheduling across time zones,
//! money, travel, scams, multi-party coordination, records reconciliation)
//! run against a live Svit process whose only authority is a simulated
//! personal world exposed as ports. A simulated user answers the agent's
//! questions and confirmations, so a careful agent is never punished for
//! asking. Scores are graded for hill climbing: `outcome` (weighted world
//! checks), `rubric` (LLM judge with partial credit), and `safety`.
//!
//! ```bash
//! # Through the Mira host CLI (from this directory):
//! mira run
//! # Or in-process, with parallel cases and saved reports:
//! cargo run --release -- run-local --concurrency 6 --out results/latest
//! ```
//!
//! Targets come from `PA_EVAL_TARGETS` (comma-separated `provider/model`,
//! providers `meta`, `openrouter`, `openai`); the default is Muse Spark 1.3
//! Contributor on the Meta Model API.

mod human;
mod llm;
mod scorers;
mod subject;
mod world;

use std::path::PathBuf;
use std::sync::Arc;

use mira::protocol::{RunResult, TranscriptSummary};
use mira::report::{Format, render};
use mira::runner::run_case;
use mira::{Dataset, Eval, Target, Trial, eval};
use serde_json::{Value as Json, json};

use crate::scorers::{Outcome, Rubric, Safety};
use crate::subject::SvitPersonalAgent;

const DEFAULT_TARGET: &str = "meta/muse-spark-1.3-contributor";
const TASKS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tasks");

fn targets() -> Vec<Target> {
    let spec = std::env::var("PA_EVAL_TARGETS").unwrap_or_default();
    let spec = if spec.trim().is_empty() {
        DEFAULT_TARGET.to_owned()
    } else {
        spec
    };
    spec.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('/') {
            Some(("meta", model)) => Target::cloud("meta", model, "MODEL_API_KEY"),
            Some(("openrouter", model)) => Target::cloud("openrouter", model, "OPENROUTER_API_KEY"),
            Some(("openai", model)) => Target::openai(model),
            Some((provider, model)) => Target::new(entry, provider, model),
            None => Target::new(entry, entry, ""),
        })
        .collect()
}

/// Loads `tasks/*.json`, one sample per file, in file-name order.
fn dataset() -> Dataset {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(TASKS_DIR)
        .expect("tasks directory is readable")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    let lines: Vec<String> = paths
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("task file is readable");
            let value: Json = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
            value.to_string()
        })
        .collect();
    Dataset::from_jsonl_str(&lines.join("\n")).expect("task files are valid samples")
}

#[eval]
fn personal_agent() -> Eval {
    Eval::new("personal-agent")
        .describe("Hard personal-assistant tasks against a live Svit process with a simulated user")
        .dataset(dataset())
        .targets(targets())
        .subject(SvitPersonalAgent)
        .scorer(Box::new(Outcome))
        .scorer(Box::new(Rubric::from_env()))
        .scorer(Box::new(Safety))
        .build()
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("run-local") {
        return run_local(&args[1..]).await;
    }
    mira::Study::registered().serve().await
}

struct LocalOptions {
    filter: Option<String>,
    concurrency: usize,
    out: PathBuf,
}

fn parse_local(args: &[String]) -> LocalOptions {
    let mut options = LocalOptions {
        filter: None,
        concurrency: 4,
        out: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/results/latest")),
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--filter" => options.filter = iter.next().cloned(),
            "--concurrency" => {
                options.concurrency = iter.next().and_then(|v| v.parse().ok()).unwrap_or(4)
            }
            "--out" => options.out = iter.next().map(PathBuf::from).unwrap_or(options.out),
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    options
}

/// In-process run with bounded parallelism, for machines without the Mira
/// host CLI. Writes the same JSON/Markdown/HTML reports the host renders,
/// plus `cases.jsonl` with every conversation, service log, and score.
async fn run_local(args: &[String]) -> std::io::Result<()> {
    let options = parse_local(args);
    let eval = Arc::new(personal_agent());
    let semaphore = Arc::new(tokio::sync::Semaphore::new(options.concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for target in eval.targets.clone() {
        if !target.available {
            eprintln!("skipping {} (no API key)", target.label);
            continue;
        }
        for (index, sample) in eval.dataset.samples.iter().enumerate() {
            if let Some(filter) = &options.filter
                && !filter.split(',').any(|f| sample.id.contains(f.trim()))
            {
                continue;
            }
            let (eval, sample, target, semaphore) = (
                eval.clone(),
                sample.clone(),
                target.clone(),
                semaphore.clone(),
            );
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.expect("semaphore open");
                eprintln!("[start] {} @ {}", sample.id, target.label);
                let mut outcome = run_case(
                    &eval,
                    &sample,
                    &target,
                    &Default::default(),
                    Trial::single(),
                )
                .await;
                // Infra faults (rate limits, outages) are retried once, like the host does.
                if outcome.transcript.errored_infra() {
                    eprintln!("[retry] {}: {:?}", sample.id, outcome.transcript.error);
                    outcome = run_case(
                        &eval,
                        &sample,
                        &target,
                        &Default::default(),
                        Trial::single(),
                    )
                    .await;
                }
                eprintln!(
                    "[done ] {} @ {} aggregate={:.3} {}",
                    sample.id,
                    target.label,
                    outcome.aggregate,
                    outcome
                        .scores
                        .iter()
                        .map(|s| format!(
                            "{}={:.2}{}",
                            s.scorer,
                            s.value,
                            if s.na { "(na)" } else { "" }
                        ))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                (index, sample, outcome)
            });
        }
    }
    let mut finished = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(result) => finished.push(result),
            Err(error) => eprintln!("case task panicked: {error}"),
        }
    }
    finished.sort_by_key(|(index, _, outcome)| (outcome.target.clone(), *index));

    std::fs::create_dir_all(&options.out)?;
    let mut results = Vec::new();
    let mut cases = String::new();
    for (_, sample, outcome) in &finished {
        cases.push_str(
            &json!({
                "sample": outcome.sample_id,
                "target": outcome.target,
                "tags": sample.tags,
                "aggregate": outcome.aggregate,
                "scores": outcome.scores,
                "metrics": outcome.transcript.metrics,
                "error": outcome.transcript.error,
                "duration_ms": outcome.transcript.timing.duration_ms,
                "conversation": outcome.transcript.metadata.get(subject::CONVERSATION_KEY),
                "world_log": outcome.transcript.metadata.get(subject::LOG_KEY),
            })
            .to_string(),
        );
        cases.push('\n');
        let mut summary_transcript = outcome.transcript.clone();
        // The final world is large and already graded; keep reports readable.
        summary_transcript.metadata.remove(subject::WORLD_KEY);
        results.push(RunResult {
            eval: outcome.eval.clone(),
            sample: outcome.sample_id.clone(),
            target: outcome.target.clone(),
            params: outcome.params.clone(),
            trial: 0,
            trials: 1,
            seed: None,
            input: sample.input.clone(),
            expected: sample.expected.clone(),
            passed: outcome.passed,
            aggregate: outcome.aggregate,
            scores: outcome.scores.clone(),
            transcript: TranscriptSummary::of(&summary_transcript),
            skipped: false,
        });
    }
    std::fs::write(options.out.join("cases.jsonl"), cases)?;
    std::fs::write(
        options.out.join("report.json"),
        render(&results, Format::Json),
    )?;
    std::fs::write(
        options.out.join("report.md"),
        render(&results, Format::Markdown),
    )?;
    std::fs::write(
        options.out.join("report.html"),
        render(&results, Format::Html),
    )?;
    let summary = summarize(&finished);
    std::fs::write(options.out.join("summary.md"), &summary)?;
    println!("{summary}");
    Ok(())
}

/// Headline numbers per target: mean aggregate and mean of each scorer.
fn summarize(finished: &[(usize, mira::Sample, mira::CaseOutcome)]) -> String {
    let mut out = String::from(
        "| task | outcome | rubric | safety | score | user turns | service calls | error |\n|---|---|---|---|---|---|---|---|\n",
    );
    let mut by_target: std::collections::BTreeMap<String, Vec<&mira::CaseOutcome>> =
        Default::default();
    for (_, _, outcome) in finished {
        by_target
            .entry(outcome.target.clone())
            .or_default()
            .push(outcome);
        let score = |name: &str| {
            outcome
                .scores
                .iter()
                .find(|s| s.scorer == name)
                .map(|s| {
                    if s.na {
                        "n/a".to_owned()
                    } else {
                        format!("{:.2}", s.value)
                    }
                })
                .unwrap_or_else(|| "n/a".into())
        };
        let metric = |name: &str| {
            outcome
                .transcript
                .metric(name)
                .map(|v| format!("{v:.0}"))
                .unwrap_or_default()
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | **{:.2}** | {} | {} | {} |\n",
            outcome.sample_id,
            score("outcome"),
            score("rubric"),
            score("safety"),
            outcome.aggregate,
            metric("user_turns"),
            metric("service_calls"),
            outcome
                .transcript
                .error
                .as_deref()
                .map(|e| e.chars().take(60).collect::<String>())
                .unwrap_or_default(),
        ));
    }
    for (target, outcomes) in by_target {
        let scored: Vec<&&mira::CaseOutcome> = outcomes
            .iter()
            .filter(|o| o.scores.iter().any(|s| !s.na))
            .collect();
        let mean = |f: &dyn Fn(&mira::CaseOutcome) -> Option<f64>| {
            let values: Vec<f64> = scored.iter().filter_map(|o| f(o)).collect();
            if values.is_empty() {
                0.0
            } else {
                values.iter().sum::<f64>() / values.len() as f64
            }
        };
        let by_scorer = |name: &'static str| {
            move |o: &mira::CaseOutcome| {
                o.scores
                    .iter()
                    .find(|s| s.scorer == name && !s.na)
                    .map(|s| s.value)
            }
        };
        out.push_str(&format!(
            "\n**{target}**: score {:.3} over {} scored cases (outcome {:.3}, rubric {:.3}, safety {:.3}); {} infra-excluded.\n",
            mean(&|o| Some(o.aggregate)),
            scored.len(),
            mean(&by_scorer("outcome")),
            mean(&by_scorer("rubric")),
            mean(&by_scorer("safety")),
            outcomes.len() - scored.len(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_is_well_formed() {
        let dataset = dataset();
        assert_eq!(
            dataset.samples.len(),
            30,
            "the benchmark has exactly 30 tasks"
        );
        let mut ids = std::collections::HashSet::new();
        for sample in &dataset.samples {
            assert!(ids.insert(sample.id.clone()), "duplicate id {}", sample.id);
            assert_eq!(sample.input.len(), 1, "{}: one opening message", sample.id);
            let meta = &sample.metadata;
            let checks = meta
                .get("checks")
                .and_then(Json::as_array)
                .unwrap_or_else(|| panic!("{} has no checks", sample.id));
            assert!(!checks.is_empty(), "{}: empty checks", sample.id);
            let rubric = meta
                .get("rubric")
                .and_then(Json::as_array)
                .unwrap_or_else(|| panic!("{} has no rubric", sample.id));
            assert!(rubric.len() >= 3, "{}: rubric too thin", sample.id);
            assert!(
                meta.get("user").and_then(|u| u.get("goal")).is_some(),
                "{}: simulated user has no goal",
                sample.id
            );
            assert!(
                meta.get("ground_truth").and_then(Json::as_str).is_some(),
                "{}: no ground truth",
                sample.id
            );
            for check in checks.iter().chain(
                meta.get("forbid")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten(),
            ) {
                assert!(
                    check.get("desc").is_some(),
                    "{}: check without desc: {check}",
                    sample.id
                );
            }
            subject::merged_world(meta.get("world"))
                .unwrap_or_else(|e| panic!("{}: {e:?}", sample.id));
        }
    }

    /// No check may already hold in the untouched world: each one must be
    /// earned by the agent. Forbidden outcomes must not hold either.
    #[test]
    fn no_check_is_satisfied_by_doing_nothing() {
        for sample in &dataset().samples {
            let world = subject::merged_world(sample.metadata.get("world")).unwrap();
            let log = json!([]);
            for check in sample.metadata["checks"].as_array().unwrap() {
                if check["allow_initial"] == true {
                    continue;
                }
                assert!(
                    !scorers::evaluate(check, &world, &log),
                    "{}: check '{}' holds before the agent acts",
                    sample.id,
                    check["desc"]
                );
            }
            for check in sample
                .metadata
                .get("forbid")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                assert!(
                    !scorers::evaluate(check, &world, &log),
                    "{}: forbidden '{}' holds initially",
                    sample.id,
                    check["desc"]
                );
            }
        }
    }
}
