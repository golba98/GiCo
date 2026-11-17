//! Intent-understanding layer: maps natural-language requests to coarse
//! intent classes. Deterministic phrase patterns handle common phrasings;
//! anything unrecognized falls back to the model (Intent::Ambiguous).
//! Patterns match natural variations, not just single keywords.

use crate::context::RawScan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Existing build intents.
    Build,
    Run,
    BuildAndRun,
    Test,
    Clean,
    /// Inspect the current directory.
    ListDir,
    /// Find/search files (optionally by type).
    SearchFiles,
    /// Size-aware filesystem query.
    LargestFiles,
    /// Git status / diff / log / branches.
    GitStatus,
    GitDiff,
    GitLog,
    /// Process / port / socket inspection.
    InspectPort,
    InspectProcess,
    /// Package installation / update.
    InstallPackage,
    /// Disk / memory diagnostics.
    DiskUsage,
    MemoryInfo,
    /// Explain a command (informational; model answers).
    Explain,
    /// Change the shell's working directory to a resolved path.
    NavigateDir,
    /// Repair a failing command (request embeds command/error).
    RepairCommand,
    /// Ambiguous — let the model interpret.
    Ambiguous,
}

impl Intent {
    /// Short human-readable label for the structured prompt contract.
    pub fn label(&self) -> &'static str {
        match self {
            Intent::Build => "build the project",
            Intent::Run => "run the project/program",
            Intent::BuildAndRun => "build and then run the project",
            Intent::Test => "run the project's tests",
            Intent::Clean => "clean build artifacts",
            Intent::ListDir => "list/inspect the current directory",
            Intent::SearchFiles => "search for files",
            Intent::LargestFiles => "find the largest files",
            Intent::GitStatus => "inspect git status",
            Intent::GitDiff => "show recent git changes (diff)",
            Intent::GitLog => "show recent git history",
            Intent::InspectPort => "inspect a network port",
            Intent::InspectProcess => "inspect processes",
            Intent::InstallPackage => "install a package",
            Intent::DiskUsage => "check disk usage",
            Intent::MemoryInfo => "check memory usage",
            Intent::Explain => "explain a command",
            Intent::NavigateDir => "change to another directory",
            Intent::RepairCommand => "repair a failing command",
            Intent::Ambiguous => "general shell task",
        }
    }
}

/// Scan-free classification (used where no RawScan is available, e.g. the
/// prompt template). Git intents still work when the request says "git".
pub fn classify_str(request: &str) -> Intent {
    classify(request, &RawScan::default_for_intent())
}

pub fn classify(request: &str, scan: &RawScan) -> Intent {
    let r = request.to_lowercase();
    let has = |phrases: &[&str]| phrases.iter().any(|p| r.contains(p));

    // Repair: mentions a command to fix plus error language.
    if has(&[
        "fix this command",
        "fix the command",
        "repair",
        "not working",
        "doesn't work",
        "does not work",
        "failed with",
        "error:",
    ]) && (r.contains('`') || r.contains("error") || r.contains("fail"))
    {
        return Intent::RepairCommand;
    }
    // Explain.
    if has(&[
        "what does",
        "explain",
        "how do i use",
        "what is the difference",
    ]) && !has(&["compile", "build", "run"])
    {
        return Intent::Explain;
    }
    // Build/run/test/clean (existing semantics, phrase-tolerant).
    let build = has(&[
        "compile",
        "build",
        "make ",
        "rebuild",
        "link",
        "fix the build",
    ]);
    let run = has(&["run", "execute", "launch", "start"]);
    let test = has(&["test", "testing"]);
    let clean = has(&["clean"]);
    if build && run {
        return Intent::BuildAndRun;
    }
    if test {
        return Intent::Test;
    }
    if clean {
        return Intent::Clean;
    }
    if build {
        return Intent::Build;
    }
    // Git.
    let in_repo = scan.git.branch.is_some();
    if in_repo || has(&["git "]) {
        if has(&["diff", "changes", "what changed", "uncommitted"]) {
            return Intent::GitDiff;
        }
        if has(&["log", "recent commits", "history", "last commits"]) {
            return Intent::GitLog;
        }
        if has(&["status", "branch", "branches", "repo"]) || r.contains("git") {
            return Intent::GitStatus;
        }
    }
    // Process/port.
    if has(&["port"]) || has(&["using port", "listening on"]) {
        return Intent::InspectPort;
    }
    if has(&["process", "running", "pid", "cpu usage", "top processes"]) {
        return Intent::InspectProcess;
    }
    // Package management.
    if has(&["install ", "install this", "how do i install", "package"])
        && !has(&["npm install", "pip install", "cargo add"])
    {
        return Intent::InstallPackage;
    }
    // Diagnostics.
    if has(&["disk", "space left", "storage"]) {
        return Intent::DiskUsage;
    }
    if has(&["memory", "ram", "mem "]) {
        return Intent::MemoryInfo;
    }
    // Directory navigation.
    if has(&[
        "change to",
        "cd to",
        "go to",
        "navigate to",
        "switch to",
        "take me to",
        "open the directory",
        "browse to",
    ]) {
        return Intent::NavigateDir;
    }
    // Filesystem search/inspection.
    if has(&["largest", "biggest", "most space", "size"]) && has(&["file", "files", "dir"]) {
        return Intent::LargestFiles;
    }
    if has(&[
        "find", "search", "locate", "where is", "grep for", "look for",
    ]) && has(&[
        "file", "files", ".cpp", ".py", ".rs", ".ts", ".js", ".go", ".md", ".txt", "name",
    ]) {
        return Intent::SearchFiles;
    }
    if has(&[
        "what is in",
        "what's in",
        "list",
        "show me",
        "contents of",
        "files here",
        "files in this",
    ]) && has(&["dir", "directory", "folder", "here", "this"])
        || r.trim() == "ls"
    {
        return Intent::ListDir;
    }
    if run {
        return Intent::Run;
    }
    Intent::Ambiguous
}

/// Extract a file-extension pattern from a search request ("cpp files" -> "*.cpp").
pub fn search_pattern(request: &str) -> Option<String> {
    let r = request.to_lowercase();
    let exts = [
        ("cpp", "*.cpp"),
        ("c++", "*.cpp"),
        ("c files", "*.c"),
        ("rust", "*.rs"),
        ("python", "*.py"),
        ("py files", "*.py"),
        ("typescript", "*.ts"),
        ("javascript", "*.js"),
        ("js files", "*.js"),
        ("go files", "*.go"),
        ("java", "*.java"),
        ("markdown", "*.md"),
        ("text", "*.txt"),
        ("json", "*.json"),
        ("yaml", "*.yaml"),
        ("toml", "*.toml"),
        ("png", "*.png"),
        ("jpg", "*.jpg"),
        ("jpeg", "*.jpeg"),
        ("gif", "*.gif"),
        ("pdf", "*.pdf"),
        ("log", "*.log"),
        ("sh files", "*.sh"),
        ("header", "*.h*"),
    ];
    for (k, pat) in exts {
        if r.contains(k) {
            return Some(pat.to_string());
        }
    }
    // explicit glob in request
    for tok in r.split_whitespace() {
        if tok.starts_with("*.") {
            return Some(
                tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '*' && c != '.')
                    .to_string(),
            );
        }
    }
    None
}

/// Extract a port number from a request ("port 3000" -> 3000).
pub fn port_number(request: &str) -> Option<u16> {
    let r = request.to_lowercase();
    let bytes = r.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if r[i..].starts_with("port") {
            let mut j = i + 4;
            while j < bytes.len() && (bytes[j].is_ascii_whitespace() || bytes[j] == b':') {
                j += 1;
            }
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(p) = r[start..j].parse::<u16>() {
                    return Some(p);
                }
            }
        }
        i += 1;
    }
    None
}

/// Extract a package name from an install request.
pub fn package_name(request: &str) -> Option<String> {
    let r = request.trim();
    let low = r.to_lowercase();
    for marker in ["install ", "install this ", "how do i install ", "package "] {
        if let Some(pos) = low.find(marker) {
            let rest = &r[pos + marker.len()..];
            let name: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(*c, '-' | '_' | '.' | '+' | ':'))
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// True when the request explicitly asks for a graphical/desktop application.
pub fn wants_gui(request: &str) -> bool {
    let r = request.to_lowercase();
    r.split(|c: char| !c.is_alphanumeric())
        .any(|word| matches!(word, "graphical" | "gui" | "windowed"))
        || r.contains("desktop app")
        || r.contains("the app window")
}

/// True when the request asks for a background service/daemon.
pub fn wants_service(request: &str) -> bool {
    let r = request.to_lowercase();
    r.split(|c: char| !c.is_alphanumeric())
        .any(|word| matches!(word, "service" | "daemon" | "server"))
}
