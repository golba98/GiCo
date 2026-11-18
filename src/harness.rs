//! Minimal agent-harness core for GiCo.
//!
//! Adapted (not copied) from proven upstream harness designs — provenance and
//! licenses in NOTICE.md:
//! - deepseek-ai/deepseek-harness `packages/core/agent-loop`: bounded rounds
//!   with a per-run cancellation token and an explicit op/turn log.
//! - deepseek-ai/deepseek-harness `packages/core/tools`: tools return one
//!   canonical, size-bounded structured `ToolResult` (ok / output / error /
//!   truncated) with deterministic argument validation — no free-form text.
//! - deepseek-ai/deepseek-harness `packages/interaction/user-approval`:
//!   approval is fail-closed; a missing answerer means "not approved".
//! - openai/codex `codex-rs/exec` + `shell-command`: process-group
//!   management for exec (setpgid, forward SIGINT to the child group, kill
//!   the group on timeout/cancel), streaming output with bounded tails.
//!
//! GiCo deliberately stays single-command: the harness orchestrates
//! plan → verify → (bounded repair) → approve → execute, and never runs an
//! autonomous tool loop. All tools here are read-only and bounded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub mod pipeline;
mod process;
mod readiness;
pub use process::{exec_shell, ExecOutcome, InterruptGuard};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Cancellation (adapted from dsh agent-loop's AbortController)
// ---------------------------------------------------------------------------

/// Cloneable cooperative cancellation token. Checked between harness rounds
/// and inside exec; never a hard guarantee, always fail-closed.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    interruptible: bool,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_interrupts(mut self) -> Self {
        self.interruptible = true;
        self
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst) || (self.interruptible && InterruptGuard::received())
    }
}

// ---------------------------------------------------------------------------
// Operation / outcome tracking (adapted from codex protocol events + dsh
// session events): every significant step is recorded with its outcome.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpKind {
    Inspect,
    Plan,
    Generate,
    Verify,
    Approve,
    Execute,
    Refine,
    Navigate,
}

impl OpKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OpKind::Inspect => "inspect",
            OpKind::Plan => "plan",
            OpKind::Generate => "generate",
            OpKind::Verify => "verify",
            OpKind::Approve => "approve",
            OpKind::Execute => "execute",
            OpKind::Refine => "refine",
            OpKind::Navigate => "navigate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpOutcome {
    Ok,
    Failed(String),
    Blocked(String),
    Exhausted,
    Cancelled,
    Interrupted(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpRecord {
    pub kind: OpKind,
    pub detail: String,
    pub outcome: OpOutcome,
    #[serde(rename = "ms")]
    pub elapsed_ms: u128,
    #[serde(skip)]
    pub elapsed_micros: u128,
}

// ---------------------------------------------------------------------------
// Structured tool results (adapted from dsh core/tools ToolOutputDefinition)
// ---------------------------------------------------------------------------

/// Canonical, size-bounded result of one read-only tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    /// Canonical JSON value; always present (null on failure).
    pub output: serde_json::Value,
    pub error: Option<String>,
    /// True when output was truncated to stay within the bound.
    pub truncated: bool,
}

impl ToolResult {
    fn ok(output: serde_json::Value) -> Self {
        ToolResult {
            ok: true,
            output,
            error: None,
            truncated: false,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        ToolResult {
            ok: false,
            output: serde_json::Value::Null,
            error: Some(msg.into()),
            truncated: false,
        }
    }
    fn truncate(mut self, max: usize) -> Self {
        let s = self.output.to_string();
        if s.len() > max {
            let end = s.floor_char_boundary(max);
            self.output = serde_json::Value::String(format!("{}…", &s[..end]));
            self.truncated = true;
        }
        self
    }
}

/// The small set of narrowly defined read-only tools. Small models cannot be
/// trusted with complex tool-calling schemas, so this set stays tiny and
/// every argument is validated deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// inspect_path {path}: existence + kind of one path.
    InspectPath,
    /// list_dir {path, limit?}: bounded directory listing.
    ListDir,
    /// command_on_path {name}: is an executable resolvable on PATH.
    CommandOnPath,
    /// read_head {path, lines?}: first N lines of a text file (untrusted).
    ReadHead,
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self {
            Tool::InspectPath => "inspect_path",
            Tool::ListDir => "list_dir",
            Tool::CommandOnPath => "command_on_path",
            Tool::ReadHead => "read_head",
        }
    }

    /// Deterministic argument validation + bounded read-only execution.
    pub fn call(&self, args: &serde_json::Value, cwd: &Path) -> ToolResult {
        match self {
            Tool::InspectPath => {
                let Some(p) = args.get("path").and_then(|v| v.as_str()) else {
                    return ToolResult::err("missing string arg 'path'");
                };
                let Some(abs) = resolve(cwd, p) else {
                    return ToolResult::err("path outside inspection scope");
                };
                let md = match std::fs::metadata(&abs) {
                    Ok(m) => m,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return ToolResult::ok(serde_json::json!({
                            "path": p, "exists": false
                        }))
                    }
                    Err(error) => return ToolResult::err(error.to_string()),
                };
                if !md.is_file() && !md.is_dir() {
                    return ToolResult::err("expected a regular file or directory");
                }
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    md.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                ToolResult::ok(serde_json::json!({
                    "path": p,
                    "exists": true,
                    "is_dir": md.is_dir(),
                    "is_file": md.is_file(),
                    "executable": exec,
                    "len": md.len(),
                }))
            }
            Tool::ListDir => {
                let Some(p) = args.get("path").and_then(|v| v.as_str()) else {
                    return ToolResult::err("missing string arg 'path'");
                };
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(100)
                    .min(500) as usize;
                let Some(abs) = resolve(cwd, p) else {
                    return ToolResult::err("path outside inspection scope");
                };
                let rd = match std::fs::read_dir(&abs) {
                    Ok(r) => r,
                    Err(e) => return ToolResult::err(format!("cannot read dir: {e}")),
                };
                let mut sampled = 0;
                let mut names: Vec<String> = rd
                    .take(limit + 1)
                    .inspect(|_| sampled += 1)
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().into_string().ok()?;
                        let kind = entry.file_type().ok()?;
                        (name.len() <= 120
                            && !name.chars().any(char::is_control)
                            && (kind.is_file() || kind.is_dir())
                            && !crate::context::fs::is_sensitive(&name, kind.is_dir()))
                        .then_some(name)
                    })
                    .collect();
                names.sort();
                let truncated = sampled > limit;
                names.truncate(limit);
                ToolResult {
                    ok: true,
                    output: serde_json::json!({ "path": p, "entries": names }),
                    error: None,
                    truncated,
                }
                .truncate(8_000)
            }
            Tool::CommandOnPath => {
                let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
                    return ToolResult::err("missing string arg 'name'");
                };
                if name.is_empty()
                    || name.len() > 120
                    || name.contains('/')
                    || name.chars().any(char::is_control)
                {
                    return ToolResult::err("invalid command name");
                }
                let found = crate::context::env::on_path(name);
                ToolResult::ok(serde_json::json!({ "name": name, "on_path": found }))
            }
            Tool::ReadHead => {
                let Some(p) = args.get("path").and_then(|v| v.as_str()) else {
                    return ToolResult::err("missing string arg 'path'");
                };
                let lines = args
                    .get("lines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20)
                    .min(200) as usize;
                let Some(abs) = resolve(cwd, p) else {
                    return ToolResult::err("path outside inspection scope");
                };
                let content = match crate::context::fs::read_text(&abs, 64_000) {
                    Ok(content) => content,
                    Err(e) => return ToolResult::err(format!("cannot read file: {e}")),
                };
                let head: Vec<&str> = content.lines().take(lines).collect();
                let truncated = content.lines().count() > lines;
                ToolResult {
                    ok: true,
                    output: serde_json::json!({
                        "path": p,
                        "lines": head,
                        "untrusted": true,
                    }),
                    error: None,
                    truncated,
                }
                .truncate(8_000)
            }
        }
    }
}

/// Resolve inspection paths within the working tree.
fn resolve(cwd: &Path, path: &str) -> Option<PathBuf> {
    use std::path::Component;

    if path.is_empty() || path.len() > 1024 || path.chars().any(char::is_control) {
        return None;
    }
    let root = cwd.canonicalize().ok()?;
    let joined = root.join(path);
    let relative = joined.strip_prefix(&root).ok()?;
    let mut resolved = root;
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name)
                if name
                    .to_str()
                    .is_some_and(|name| !crate::context::fs::is_sensitive(name, true)) =>
            {
                resolved.push(name);
                if std::fs::symlink_metadata(&resolved).is_ok_and(|file| file.is_symlink()) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(resolved)
}

// ---------------------------------------------------------------------------
// Bounded harness rounds (adapted from dsh agent-loop turn accounting)
// ---------------------------------------------------------------------------

/// Orchestrates bounded plan/generate/verify rounds with an op log.
pub struct Harness {
    pub max_rounds: usize,
    rounds_used: usize,
    inference_start: Option<std::time::Instant>,
    cancel: CancelToken,
    ops: Vec<OpRecord>,
}

fn remaining_call_budget(elapsed: std::time::Duration, rounds_used: usize) -> std::time::Duration {
    use std::time::Duration;
    Duration::from_secs(90)
        .saturating_sub(elapsed)
        .min(Duration::from_secs(if rounds_used == 0 { 60 } else { 30 }))
}

impl Harness {
    pub fn new(max_rounds: usize) -> Self {
        Harness {
            max_rounds,
            rounds_used: 0,
            inference_start: None,
            cancel: CancelToken::new(),
            ops: Vec::new(),
        }
    }

    pub fn inference_budget(&mut self) -> std::time::Duration {
        let elapsed = self
            .inference_start
            .get_or_insert_with(std::time::Instant::now)
            .elapsed();
        remaining_call_budget(elapsed, self.rounds_used)
    }

    pub fn begin_refinement(&mut self) {
        self.inference_start = None;
        self.rounds_used = 0;
    }

    pub fn enable_interrupts(&mut self) {
        self.cancel = self.cancel.clone().with_interrupts();
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// True while another round may start (bounded; fails closed).
    pub fn next_round(&mut self, kind: OpKind, detail: &str) -> bool {
        if self.cancel.is_cancelled() {
            self.record(kind, detail, OpOutcome::Cancelled);
            return false;
        }
        if self.rounds_used >= self.max_rounds {
            self.record(kind, detail, OpOutcome::Exhausted);
            return false;
        }
        self.rounds_used += 1;
        true
    }

    pub fn record(&mut self, kind: OpKind, detail: &str, outcome: OpOutcome) {
        self.record_ms(kind, detail, outcome, 0);
    }

    pub fn record_ms(&mut self, kind: OpKind, detail: &str, outcome: OpOutcome, ms: u128) {
        self.record_duration(
            kind,
            detail,
            outcome,
            std::time::Duration::from_millis(ms as u64),
        );
    }

    pub fn record_duration(
        &mut self,
        kind: OpKind,
        detail: &str,
        outcome: OpOutcome,
        elapsed: std::time::Duration,
    ) {
        self.ops.push(OpRecord {
            kind,
            detail: detail.to_string(),
            outcome,
            elapsed_ms: elapsed.as_millis(),
            elapsed_micros: elapsed.as_micros(),
        });
    }

    pub fn ops(&self) -> &[OpRecord] {
        &self.ops
    }

    pub fn rounds_used(&self) -> usize {
        self.rounds_used
    }
}

/// Fail-closed approval policy (adapted from dsh user-approval: a missing
/// answerer means NOT approved; grants apply only to the requested action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Ask the user for every execution (GiCo's only default mode).
    Ask,
    /// Never execute — display only.
    Never,
}

impl ApprovalPolicy {
    /// May `command` be executed? Fail closed on anything but an explicit,
    /// user-confirmed "run" for this exact command string.
    pub fn permits(&self, approved: Option<&str>, command: &str) -> bool {
        match self {
            ApprovalPolicy::Never => false,
            ApprovalPolicy::Ask => approved == Some(command),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cancel_token_is_fail_closed() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        t.cancel();
        assert!(t.is_cancelled());
        assert!(t.clone().is_cancelled());
    }

    #[test]
    fn approval_permits_only_exact_confirmed_command() {
        assert!(ApprovalPolicy::Ask.permits(Some("ls"), "ls"));
        assert!(!ApprovalPolicy::Ask.permits(Some("ls"), "ls -la"));
        assert!(!ApprovalPolicy::Ask.permits(None, "ls"));
        assert!(!ApprovalPolicy::Never.permits(Some("ls"), "ls"));
    }

    #[test]
    fn harness_rounds_are_bounded() {
        let mut h = Harness::new(2);
        assert!(h.next_round(OpKind::Generate, "r1"));
        assert!(h.next_round(OpKind::Generate, "r2"));
        assert!(!h.next_round(OpKind::Generate, "r3"));
        assert_eq!(h.rounds_used(), 2);
        // Only the refused round is recorded.
        assert_eq!(h.ops().len(), 1);
        assert_eq!(h.ops()[0].outcome, OpOutcome::Exhausted);
    }

    #[test]
    fn tools_are_bounded_and_structured() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello\nworld\n").unwrap();
        let r = Tool::InspectPath.call(&serde_json::json!({"path": "a.txt"}), tmp.path());
        assert!(r.ok);
        assert_eq!(r.output["is_file"], true);
        let r = Tool::InspectPath.call(&serde_json::json!({"path": "nope"}), tmp.path());
        assert!(r.ok);
        assert_eq!(r.output["exists"], false);
        let r = Tool::ListDir.call(&serde_json::json!({"path": ".", "limit": 5}), tmp.path());
        assert!(r.ok);
        assert_eq!(r.output["entries"][0], "a.txt");
        let r = Tool::ReadHead.call(
            &serde_json::json!({"path": "a.txt", "lines": 1}),
            tmp.path(),
        );
        assert!(r.ok);
        assert_eq!(r.output["lines"][0], "hello");
        assert!(r.output["untrusted"] == true);
        // missing arg fails deterministically
        assert!(!Tool::ListDir.call(&serde_json::json!({}), tmp.path()).ok);
    }

    #[test]
    fn exec_shell_captures_outcome_and_propagates_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let tok = CancelToken::new();
        let r = exec_shell(
            "echo out; echo err >&2; exit 3",
            tmp.path(),
            &tok,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(r.exit_code, Some(3));
        assert!(!r.success);
        assert!(r.stdout_tail.contains("out"));
        assert!(r.stderr_tail.contains("err"));
    }

    #[test]
    fn exec_shell_kills_on_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let tok = CancelToken::new();
        let r = exec_shell("sleep 30", tmp.path(), &tok, Duration::from_millis(300)).unwrap();
        assert!(r.timed_out && r.killed);
    }

    #[test]
    fn exec_shell_honours_cancel_token() {
        let tmp = tempfile::tempdir().unwrap();
        let tok = CancelToken::new();
        tok.cancel();
        let r = exec_shell("sleep 30", tmp.path(), &tok, Duration::from_secs(30)).unwrap();
        assert!(r.cancelled && !r.started);
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::remaining_call_budget;
    use std::time::Duration as D;
    #[test]
    fn slow_first_call_leaves_time_for_inspection_and_repair() {
        for (elapsed, rounds, budget) in [(0, 0, 60), (50, 1, 30), (78, 2, 12), (92, 2, 0)] {
            assert_eq!(
                remaining_call_budget(D::from_secs(elapsed), rounds),
                D::from_secs(budget)
            );
        }
    }
}
