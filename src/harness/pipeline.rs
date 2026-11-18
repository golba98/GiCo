//! Candidate preparation and one-use approval, adapted from DeepSeek Harness's
//! tools pipeline and user approval service. See NOTICE.md.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::context::{ContextEngine, RawScan};
use crate::intent::{self, Intent};
use crate::verify::{self, Issue, Report, Severity, Verdict};
use crate::{discovery::Reply, plan};

use super::{exec_shell, ApprovalPolicy, CancelToken, ExecOutcome, Harness, OpKind, OpOutcome};

#[derive(PartialEq)]
struct FileIdentity(u64, u64, Option<(u64, i64, i64)>);
fn fingerprint(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let md = path.metadata().ok()?;
    Some(FileIdentity(
        md.dev(),
        md.ino(),
        md.is_file()
            .then(|| (md.len(), md.ctime(), md.ctime_nsec())),
    ))
}

pub struct Candidate {
    pub command: String,
    pub report: Report,
    pub note: Option<String>,
    cwd: PathBuf,
    directory_identity: Option<FileIdentity>,
    argv: Option<Vec<String>>,
    paths: Vec<(PathBuf, PathBuf, FileIdentity)>,
}

impl Candidate {
    pub fn argv(&self) -> Option<&[String]> {
        self.argv.as_deref()
    }
}

pub enum Preparation {
    Candidate(Candidate),
    Information { title: String, detail: String },
    Clarify(String),
    Failed(String),
    Cancelled,
}

pub enum Dispatch {
    Shell(ExecOutcome),
    Navigation,
}

enum Operation {
    Argv(Vec<String>),
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
            Operation::Argv(argv) => {
                let mut builder = std::process::Command::new(&argv[0]);
                builder.args(&argv[1..]);
                super::process::exec_process(builder, &self.cwd, cancel, timeout)
                    .map(Dispatch::Shell)
            }
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
    let query = crate::query::Query::parse(request);
    if let Some(expected) = query
        .as_ref()
        .map(|q| q.render(cwd))
        .or_else(|| crate::query::CountEvidence::from_request(request, raw).map(|e| e.command(cwd)))
    {
        if !expected.as_ref().is_ok_and(|planned| {
            planned == &command
                || verify::literal_words(planned)
                    .zip(verify::literal_words(&command))
                    .is_some_and(|(a, b)| a == b)
        }) {
            report.issues.push(Issue {
                severity: Severity::Error,
                message: expected.err().unwrap_or_else(|| {
                    "command does not preserve the verified query, inputs and destination".into()
                }),
                suggestion: None,
            });
        }
    }
    if matches!(
        intent::classify(request, raw),
        Intent::Run | Intent::BuildAndRun
    ) && query.is_none()
        && plan::requested_target_kind(request).is_some()
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
        let mut paths = verify::existing_paths(&command, cwd);
        if matches!(
            intent::classify(request, raw),
            Intent::Build | Intent::BuildAndRun | Intent::Run | Intent::Test | Intent::Clean
        ) {
            for rel in raw
                .entries
                .iter()
                .filter(|e| {
                    matches!(
                        e.name.as_str(),
                        "CMakeLists.txt"
                            | "Cargo.toml"
                            | "package.json"
                            | "pyproject.toml"
                            | "Makefile"
                            | "CMakePresets.json"
                    )
                })
                .map(|e| PathBuf::from(&e.path))
                .chain(
                    raw.cmake_build_dirs
                        .iter()
                        .map(|d| Path::new(d).join("CMakeCache.txt")),
                )
            {
                let path = cwd.join(rel);
                if let Ok(resolved) = path.canonicalize() {
                    paths.push((path, resolved));
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter_map(|(p, r)| {
                let id = fingerprint(&r)?;
                Some((p, r, id))
            })
            .collect()
    };
    harness.record_duration(OpKind::Verify, &command, outcome, start.elapsed());
    Candidate {
        paths,
        argv: None,
        command,
        report,
        note,
        directory_identity: fingerprint(cwd),
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
    if candidate.report.verdict() == Verdict::Blocked
        || candidate.cwd != *cwd
        || fingerprint(cwd) != candidate.directory_identity
        || !ApprovalPolicy::Ask.permits(answer, command)
    {
        harness.record(
            OpKind::Approve,
            command,
            OpOutcome::Blocked("approval unavailable".into()),
        );
        return Err("approval unavailable".into());
    }
    for (path, resolved, identity) in &candidate.paths {
        if path.canonicalize().as_ref().ok() != Some(resolved)
            || fingerprint(path).as_ref() != Some(identity)
        {
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
    if verified.paths != candidate.paths && verified.report.verdict() != Verdict::Blocked {
        let reason = "resolved inputs changed since preview";
        harness.record(OpKind::Approve, command, OpOutcome::Blocked(reason.into()));
        return Err(reason.into());
    }
    if verified.report.verdict() == Verdict::Blocked {
        let outcome = blocked(&verified.report);
        let message = verified.report.errors()[0].message.clone();
        harness.record(OpKind::Approve, command, outcome);
        return Err(message);
    }
    let operation = match verify::navigation_target(command, cwd) {
        Some(target) => Operation::Navigate(target),
        None => candidate
            .argv
            .clone()
            .map(Operation::Argv)
            .unwrap_or_else(|| Operation::Shell(command.clone())),
    };
    harness.record(OpKind::Approve, command, OpOutcome::Ok);
    Ok(ApprovedOperation {
        operation,
        cwd: cwd.clone(),
    })
}

fn missing_choice(raw: &RawScan, request: &str) -> Option<String> {
    let lower = request.to_lowercase();
    let words: Vec<_> = lower.split_whitespace().collect();
    let has = |word: &str| words.contains(&word);
    if intent::classify(request, raw) == Intent::GitLog && raw.git.branch.is_none() {
        return Some("Which repository should I use? There is no Git history here.".into());
    }
    if (has("attach") || has("attached")) && raw.entries.iter().all(|e| !request.contains(&e.path))
    {
        return Some(
            "Which file should I read? Provide the missing attachment or its path.".into(),
        );
    }
    if (has("send") || has("deploy") || has("upload"))
        && (has("usual") || has("my") && has("server"))
        && !words
            .iter()
            .any(|w| w.contains('@') || w.contains("://") || w.contains(':'))
    {
        return Some("Which destination should receive the files?".into());
    }
    if has("previous")
        && (has("compare") || has("difference"))
        && raw
            .entries
            .iter()
            .filter(|e| request.contains(&e.path))
            .count()
            < 2
    {
        return Some("Which previous result or file should I compare with this one?".into());
    }
    if words.len() <= 3 && has("it") && (has("fix") || has("repair")) {
        return Some("What command or error should I fix?".into());
    }
    if has("meant") || lower.contains("equally named") {
        let candidates: Vec<_> = raw
            .entries
            .iter()
            .filter(|e| e.kind != crate::context::fs::FileKind::Directory)
            .filter(|e| {
                words
                    .iter()
                    .any(|w| w.len() > 2 && e.name.to_lowercase().contains(*w))
                    || lower.contains("equally named")
            })
            .map(|e| e.path.as_str())
            .take(4)
            .collect();
        if candidates.len() > 1 {
            return Some(format!(
                "Which file do you mean: {}?",
                candidates.join(" or ")
            ));
        }
    }
    None
}

pub fn prepare(
    harness: &mut Harness,
    raw: &RawScan,
    request: &str,
    mut generate: impl FnMut(Option<(&str, &str)>, &mut Harness) -> Result<Reply, String>,
) -> Preparation {
    if harness.cancel_token().is_cancelled() {
        harness.record(OpKind::Plan, "initial preparation", OpOutcome::Cancelled);
        return Preparation::Cancelled;
    }
    if !request.contains("\nClarification:") {
        if let Some(question) = missing_choice(raw, request) {
            harness.record(OpKind::Plan, "missing evidence", OpOutcome::Ok);
            return Preparation::Clarify(question);
        }
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
            if let Some(note) = plan.note.as_ref().filter(|n| n.starts_with("Which ")) {
                return Preparation::Clarify(note.clone());
            }
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
                || issue.message.contains("file not found")
                || issue.message.contains("path does not exist")
                || issue.message.contains("requested literal file")
                || issue.message.contains("missing required operand")
                || issue.message.contains("unquoted working directory")
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
    generate: impl FnOnce(&mut Harness) -> Result<Reply, String>,
) -> Preparation {
    let kind = if refinement.is_some() {
        OpKind::Refine
    } else {
        OpKind::Generate
    };
    let start = Instant::now();
    let cancel = harness.cancel_token();
    let generated = generate(harness);
    let (outcome, result) = if cancel.is_cancelled() {
        (OpOutcome::Cancelled, Preparation::Cancelled)
    } else {
        match generated {
            Ok(Reply::Argv(argv)) => {
                let command = argv
                    .iter()
                    .map(|a| crate::context::fs::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut candidate = verify_candidate(harness, raw, request, command, None);
                candidate.argv = Some(argv);
                (OpOutcome::Ok, Preparation::Candidate(candidate))
            }
            Ok(Reply::Answer(title)) => (
                OpOutcome::Ok,
                Preparation::Information {
                    title,
                    detail: String::new(),
                },
            ),
            Ok(Reply::Clarify(question)) => (OpOutcome::Ok, Preparation::Clarify(question)),
            Ok(Reply::Command(command)) => {
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
                        "candidate generation",
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
    harness.record_ms(
        kind,
        "candidate generation",
        outcome,
        start.elapsed().as_millis(),
    );
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
