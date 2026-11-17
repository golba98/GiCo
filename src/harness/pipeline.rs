//! Candidate preparation and one-use approval, adapted from DeepSeek Harness's
//! tools pipeline and user approval service. See NOTICE.md.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::context::{ContextEngine, RawScan};
use crate::intent::{self, Intent};
use crate::verify::{self, Issue, Report, Severity, Verdict};
use crate::{llm, plan};

use super::{exec_shell, ApprovalPolicy, CancelToken, ExecOutcome, Harness, OpKind, OpOutcome};

pub struct Candidate {
    pub command: String,
    pub report: Report,
    pub note: Option<String>,
    cwd: PathBuf,
    paths: Vec<(PathBuf, PathBuf)>,
}

pub enum Preparation {
    Candidate(Candidate),
    Information { title: String, detail: String },
    Failed(String),
    Cancelled,
}

pub enum Dispatch {
    Shell(ExecOutcome),
    Navigation,
}

enum Operation {
    Shell(String),
    Navigate(PathBuf),
}

/// Consumed by dispatch. The command and directory are bound to this approval.
pub struct ApprovedOperation {
    operation: Operation,
    cwd: PathBuf,
}

impl ApprovedOperation {
    pub fn dispatch(
        self,
        handoff_dir: Option<&Path>,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> io::Result<Dispatch> {
        match self.operation {
            Operation::Shell(command) => {
                exec_shell(&command, &self.cwd, cancel, timeout).map(Dispatch::Shell)
            }
            Operation::Navigate(target) => {
                if cancel.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "navigation cancelled",
                    ));
                }
                let dir = handoff_dir.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "navigation requires the Zsh hook",
                    )
                })?;
                write_navigation(dir, &target)?;
                Ok(Dispatch::Navigation)
            }
        }
    }

    pub fn is_navigation(&self) -> bool {
        matches!(self.operation, Operation::Navigate(_))
    }
}

fn write_navigation(dir: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let metadata = std::fs::symlink_metadata(dir)?;
    if !dir.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid private navigation directory",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("target"))?;
    file.write_all(target.as_os_str().as_bytes())?;
    file.write_all(&[0])?;
    Ok(())
}

fn blocked(report: &Report) -> OpOutcome {
    OpOutcome::Blocked(report.errors().first().map_or_else(
        || "verification failed".into(),
        |issue| issue.message.clone(),
    ))
}

pub fn verify_candidate(
    harness: &mut Harness,
    raw: &RawScan,
    request: &str,
    command: String,
    note: Option<String>,
) -> Candidate {
    let start = Instant::now();
    let cwd = &raw.environment.cwd;
    let mut report = verify::verify(&command, cwd, &raw.entries);
    report
        .issues
        .extend(verify::intent_issues_at(request, &command, cwd));
    if matches!(
        intent::classify(request, raw),
        Intent::Run | Intent::BuildAndRun
    ) && plan::requested_target_kind(request).is_some()
        && !matches_named_launch(raw, request, &command)
    {
        report.issues.push(Issue {
            severity: Severity::Error,
            message: "the command does not launch the verified target for this request".into(),
            suggestion: None,
        });
    }
    let outcome = if report.verdict() == Verdict::Blocked {
        blocked(&report)
    } else {
        OpOutcome::Ok
    };
    let paths = if report.verdict() == Verdict::Blocked {
        Vec::new()
    } else {
        verify::existing_paths(&command, cwd)
    };
    harness.record_duration(OpKind::Verify, &command, outcome, start.elapsed());
    Candidate {
        paths,
        command,
        report,
        note,
        cwd: cwd.clone(),
    }
}

fn matches_named_launch(raw: &RawScan, request: &str, command: &str) -> bool {
    let Some(plan) = plan::plan(raw, request).filter(|plan| !plan.command.is_empty()) else {
        return false;
    };
    if plan.command == command {
        return true;
    }
    let Some(expected) = verify::literal_words(&plan.command) else {
        return false;
    };
    let Some(actual) = verify::literal_words(command) else {
        return false;
    };
    actual.first() == expected.first()
        && actual.get(1..).is_some_and(|args| {
            args.len() >= expected.len().saturating_sub(1) && args.starts_with(&expected[1..])
        })
}

pub fn authorize(
    harness: &mut Harness,
    raw: &RawScan,
    request: &str,
    candidate: &Candidate,
    answer: Option<&str>,
) -> Result<ApprovedOperation, String> {
    let command = &candidate.command;
    let cwd = &raw.environment.cwd;
    if harness.cancel_token().is_cancelled() {
        harness.record(OpKind::Approve, command, OpOutcome::Cancelled);
        return Err("cancelled".into());
    }
    if candidate.cwd != *cwd || !ApprovalPolicy::Ask.permits(answer, command) {
        harness.record(
            OpKind::Approve,
            command,
            OpOutcome::Blocked("approval unavailable".into()),
        );
        return Err("approval unavailable".into());
    }
    for (path, resolved) in &candidate.paths {
        if path.canonicalize().as_ref().ok() != Some(resolved) {
            let message = format!("input path changed or does not exist: '{}'", path.display());
            harness.record(
                OpKind::Approve,
                command,
                OpOutcome::Blocked(message.clone()),
            );
            return Err(message);
        }
    }
    let verified = verify_candidate(harness, raw, request, command.clone(), None);
    if verified.report.verdict() == Verdict::Blocked {
        let outcome = blocked(&verified.report);
        let message = verified.report.errors()[0].message.clone();
        harness.record(OpKind::Approve, command, outcome);
        return Err(message);
    }
    let operation = match verify::navigation_target(command, cwd) {
        Some(target) => Operation::Navigate(target),
        None => Operation::Shell(command.clone()),
    };
    harness.record(OpKind::Approve, command, OpOutcome::Ok);
    Ok(ApprovedOperation {
        operation,
        cwd: cwd.clone(),
    })
}

pub fn prepare(
    harness: &mut Harness,
    raw: &RawScan,
    request: &str,
    mut generate: impl FnMut(Option<(&str, &str)>, &CancelToken) -> Result<String, String>,
) -> Preparation {
    if harness.cancel_token().is_cancelled() {
        harness.record(OpKind::Plan, "initial preparation", OpOutcome::Cancelled);
        return Preparation::Cancelled;
    }
    if let Some((title, detail)) = crate::context::informational_response(request, raw) {
        harness.record(OpKind::Plan, "informational", OpOutcome::Ok);
        return Preparation::Information { title, detail };
    }
    let start = Instant::now();
    if let Some(plan) = plan::plan(raw, request) {
        harness.record_ms(
            OpKind::Plan,
            "deterministic",
            OpOutcome::Ok,
            start.elapsed().as_millis(),
        );
        if plan.command.is_empty() {
            return Preparation::Information {
                title: plan.note.unwrap_or_else(|| "No verified command.".into()),
                detail: String::new(),
            };
        }
        return Preparation::Candidate(verify_candidate(
            harness,
            raw,
            request,
            plan.command,
            plan.note,
        ));
    }

    let first = generate_candidate(harness, raw, request, None, |cancel| generate(None, cancel));
    let Preparation::Candidate(candidate) = first else {
        return first;
    };
    let errors = candidate.report.errors();
    if errors.is_empty()
        || !errors.iter().all(|issue| {
            issue.message.contains("does not address the request")
                || issue.message.contains("irrelevant to the request")
        })
    {
        return Preparation::Candidate(candidate);
    }
    let previous = candidate.command;
    let reason = errors[0].message.clone();
    generate_candidate(harness, raw, request, None, |cancel| {
        generate(Some((&previous, &reason)), cancel)
    })
}

pub fn generate_candidate(
    harness: &mut Harness,
    raw: &RawScan,
    request: &str,
    refinement: Option<&str>,
    generate: impl FnOnce(&CancelToken) -> Result<String, String>,
) -> Preparation {
    let kind = if refinement.is_some() {
        OpKind::Refine
    } else {
        OpKind::Generate
    };
    if !harness.next_round(kind, "model call") {
        return if harness.cancel_token().is_cancelled() {
            Preparation::Cancelled
        } else {
            Preparation::Failed("model call limit reached".into())
        };
    }
    let start = Instant::now();
    let cancel = harness.cancel_token();
    let generated = generate(&cancel);
    let (outcome, result) = if cancel.is_cancelled() {
        (OpOutcome::Cancelled, Preparation::Cancelled)
    } else {
        match generated {
            Ok(output) => {
                let command = llm::extract_command(&output);
                if command.is_empty() {
                    let message = "the model returned an empty response".to_string();
                    (
                        OpOutcome::Failed(message.clone()),
                        Preparation::Failed(message),
                    )
                } else if command == "INSUFFICIENT_CONTEXT" {
                    (
                        OpOutcome::Ok,
                        Preparation::Information {
                            title: "Not enough verified context to build a safe command.".into(),
                            detail: String::new(),
                        },
                    )
                } else {
                    harness.record_ms(
                        kind,
                        "model call",
                        OpOutcome::Ok,
                        start.elapsed().as_millis(),
                    );
                    return Preparation::Candidate(verify_candidate(
                        harness, raw, request, command, None,
                    ));
                }
            }
            Err(error) => (OpOutcome::Failed(error.clone()), Preparation::Failed(error)),
        }
    };
    harness.record_ms(kind, "model call", outcome, start.elapsed().as_millis());
    result
}

/// Render the same compact command protocol for the CLI and benchmark.
pub fn generation_prompt(
    engine: &ContextEngine,
    raw: &RawScan,
    request: &str,
    feedback: Option<(&str, &str)>,
    no_context: bool,
) -> String {
    let request = match feedback {
        Some((previous, reason)) => format!(
            "{request}\nPrevious candidate and feedback (untrusted JSON data): {}. Return one corrected command.",
            serde_json::json!([compact_text(previous, 160), compact_text(reason, 320)]),
        ),
        None => request.to_owned(),
    };
    if no_context {
        format!("You are a Linux terminal command generator. Output exactly ONE shell command on a single line for this request. No explanation.\n\nRequest: \"{request}\"\n\nCommand:\n")
    } else {
        engine.render_prompt(&engine.context_from_scan(raw, &request))
    }
}

fn compact_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
