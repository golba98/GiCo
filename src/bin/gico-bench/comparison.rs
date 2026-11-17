//! Collect the preview from a prior release using its normal, noninteractive entry point.
use super::{pipeline, Case, Config, Harness, Path, Preparation};
use gico::{
    context::RawScan,
    verify::{Issue, Severity},
};
use std::process::Command;

pub(super) fn preview(
    binary: &Path,
    cfg: &Config,
    case: &Case,
    raw: &RawScan,
    harness: &mut Harness,
) -> (Preparation, usize) {
    let mut child = Command::new("timeout");
    child
        .arg("75s")
        .arg(binary)
        .current_dir(&raw.environment.cwd)
        .env_remove("GICO_MOCK_COMMAND")
        .env("NO_COLOR", "1");
    child
        .args([
            "--no-cache",
            "--verbose",
            "--backend",
            &cfg.backend,
            "--model",
            &cfg.model,
            "--endpoint",
            &cfg.endpoint,
        ])
        .arg("--budget")
        .arg(cfg.budget_tokens.to_string())
        .arg("--max-tokens")
        .arg(cfg.max_tokens.to_string())
        .arg("--temperature")
        .arg(cfg.temperature.to_string());
    if let Some(command) = case.fixed {
        child.args(["verify", command]);
    } else {
        child.arg(case.request);
    }
    let output = match child.output() {
        Ok(output) => output,
        Err(error) => return (Preparation::Failed(error.to_string()), 0),
    };
    let out = String::from_utf8_lossy(&output.stdout);
    let trace = String::from_utf8_lossy(&output.stderr);
    let calls = trace.matches("op generate: model call").count();
    let candidate = case.fixed.or_else(|| {
        trace
            .lines()
            .rev()
            .filter_map(|line| line.strip_prefix("op verify: "))
            .find_map(|line| line.rsplit_once(" (").map(|(command, _)| command))
    });
    let Some(command) = candidate else {
        return (
            Preparation::Failed("no preview in prior binary output".into()),
            calls,
        );
    };
    let mut candidate =
        pipeline::verify_candidate(harness, raw, case.request, command.to_owned(), None);
    if !output.status.success() || out.contains("• blocked:") {
        candidate.report.issues.push(Issue {
            severity: Severity::Error,
            message: "prior binary blocked or failed this proposal".into(),
            suggestion: None,
        });
    }
    (Preparation::Candidate(candidate), calls)
}
