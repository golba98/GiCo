//! Context engine: collects verified, read-only environment context and
//! renders compact prompts for small local models.

pub mod cache;
pub mod cmake;
pub mod env;
pub mod fs;
pub mod git;
pub mod project;
pub mod prompt;
pub mod relevance;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use cache::Cache;
pub use env::Environment;
pub use fs::FileEntry;
pub use git::GitInfo;
pub use project::ProjectInfo;

/// A fully collected, request-scoped context ready to be rendered into a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub environment: Environment,
    /// Files selected by relevance within the token budget.
    pub files: Vec<FileEntry>,
    /// Total entries discovered before selection (may exceed `files.len()`).
    pub total_discovered: usize,
    /// Number of files omitted from the listing because of the budget.
    pub omitted: usize,
    pub project: Option<ProjectInfo>,
    pub git: GitInfo,
    pub request: String,
    pub excluded_sensitive: usize,
    pub excluded_injection: usize,
    pub budget_tokens: usize,
    pub estimated_tokens: usize,
    /// Classified intent label (structured model-input contract).
    pub intent: String,
    /// Verified constraints/prerequisites (stale build trees, missing tools).
    pub constraints: Vec<String>,
}

/// Raw, request-independent scan of a directory. This is what gets cached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawScan {
    pub environment: Environment,
    pub entries: Vec<FileEntry>,
    pub total_discovered: usize,
    pub project: Option<ProjectInfo>,
    pub git: GitInfo,
    pub excluded_sensitive: usize,
    pub excluded_injection: usize,
    /// Top-level directories that were skipped (heavy/excluded, e.g. build/).
    pub skipped_dirs: Vec<String>,
    /// Configured CMake build trees (contain CMakeCache.txt), e.g. ["build"].
    #[serde(default)]
    pub cmake_build_dirs: Vec<String>,
}

/// Configuration for the context engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Maximum directory entries scanned (hard cap, read-only).
    pub max_entries: usize,
    /// Maximum directory depth below the working directory.
    pub max_depth: usize,
    /// Token budget for the rendered context (approx. chars/4).
    pub budget_tokens: usize,
    /// Cache directory; None disables caching.
    pub cache_dir: Option<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_entries: 400,
            max_depth: 3,
            budget_tokens: 700,
            cache_dir: Some(default_cache_dir()),
        }
    }
}

pub fn default_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("gico");
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".cache").join("gico"),
        None => PathBuf::from(".gico-cache"),
    }
}

/// The reusable context engine.
pub struct ContextEngine {
    pub config: EngineConfig,
}

impl ContextEngine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Collect context for `request` in the current working directory.
    pub fn collect(&self, request: &str) -> std::io::Result<Context> {
        let cwd = std::env::current_dir()?;
        self.collect_at(&cwd, request)
    }

    /// Collect context for `request` rooted at `dir`.
    pub fn collect_at(&self, dir: &Path, request: &str) -> std::io::Result<Context> {
        let raw = match &self.config.cache_dir {
            Some(cache_dir) => {
                let cache = Cache::new(cache_dir.clone());
                cache.get_or_scan(dir, || self.scan(dir))
            }
            None => self.scan(dir),
        }?;

        Ok(self.context_from_scan(&raw, request))
    }

    pub fn context_from_scan(&self, raw: &RawScan, request: &str) -> Context {
        // Reserve room for the fixed template: the file listing gets whatever
        // budget remains after the environment/project scaffolding.
        let overhead_ctx = Context {
            environment: raw.environment.clone(),
            files: Vec::new(),
            total_discovered: raw.total_discovered,
            omitted: 0,
            project: raw.project.clone(),
            git: raw.git.clone(),
            request: request.to_string(),
            excluded_sensitive: raw.excluded_sensitive,
            excluded_injection: raw.excluded_injection,
            budget_tokens: self.config.budget_tokens,
            estimated_tokens: 0,
            intent: crate::intent::classify(request, raw).label().to_string(),
            constraints: Vec::new(),
        };
        let overhead = prompt::estimate_tokens(&prompt::render(&overhead_ctx));
        let file_budget = self.config.budget_tokens.saturating_sub(overhead);
        let files = relevance::select(raw, request, file_budget);
        let omitted = raw.entries.len().saturating_sub(files.len());
        let mut ctx = Context {
            environment: raw.environment.clone(),
            total_discovered: raw.total_discovered,
            files,
            omitted,
            project: raw.project.clone(),
            git: raw.git.clone(),
            request: request.to_string(),
            excluded_sensitive: raw.excluded_sensitive,
            excluded_injection: raw.excluded_injection,
            budget_tokens: self.config.budget_tokens,
            estimated_tokens: 0,
            intent: crate::intent::classify(request, raw).label().to_string(),
            constraints: build_constraints(&raw.environment.cwd, raw),
        };
        ctx.estimated_tokens = prompt::estimate_tokens(&prompt::render(&ctx));
        ctx
    }

    /// Read-only scan of `dir` (no request scoping, no caching).
    pub fn scan(&self, dir: &Path) -> std::io::Result<RawScan> {
        let environment = env::collect(dir);
        let scan = fs::scan(dir, self.config.max_entries, self.config.max_depth);
        let git = git::collect(dir);
        let mut raw = RawScan {
            environment,
            entries: scan.entries,
            total_discovered: scan.total_discovered,
            project: None,
            git,
            excluded_sensitive: scan.excluded_sensitive,
            excluded_injection: scan.excluded_injection,
            skipped_dirs: scan.skipped_dirs,
            cmake_build_dirs: scan.cmake_build_dirs,
        };
        raw.project = project::detect(&raw);
        Ok(raw)
    }

    /// Render the compact prompt for a collected context.
    pub fn render_prompt(&self, ctx: &Context) -> String {
        prompt::render(ctx)
    }
}

/// Verified constraints for the model-input contract: CMake build trees that
/// are demonstrably stale (never usable), valid configured trees, and declared
/// executable targets. Read-only, bounded to the discovered build dirs.
fn build_constraints(dir: &Path, raw: &RawScan) -> Vec<String> {
    let mut out = Vec::new();
    if !raw.cmake_build_dirs.is_empty() {
        let trees = cmake::discover_build_trees(dir, &raw.cmake_build_dirs);
        for t in &trees {
            if t.valid {
                out.push(format!("valid configured CMake build dir: {}", t.dir));
            } else if let Some(r) = &t.stale_reason {
                out.push(format!("stale CMake build dir {} is unusable: {r}", t.dir));
            }
        }
    }
    if let Some(p) = &raw.project {
        if !p.executable_targets.is_empty() {
            out.push(format!(
                "declared executable targets: {}",
                p.executable_targets.join(", ")
            ));
        }
    }
    out
}

impl RawScan {
    /// Empty scan for intent classification without filesystem context.
    pub fn default_for_intent() -> RawScan {
        RawScan {
            environment: env::collect(Path::new(".")),
            entries: Vec::new(),
            total_discovered: 0,
            project: None,
            git: git::GitInfo::default(),
            excluded_sensitive: 0,
            excluded_injection: 0,
            skipped_dirs: Vec::new(),
            cmake_build_dirs: Vec::new(),
        }
    }
}

/// Decide whether a request should short-circuit with an informational
/// (non-executable) response because there is nothing buildable here.
/// Returns (title, detail) when applicable.
pub fn informational_response(request: &str, scan: &RawScan) -> Option<(String, String)> {
    if scan.project.is_some() {
        return None;
    }
    let words = relevance::words(request);
    let intent = [
        "compile", "build", "run", "test", "start", "make", "link", "execute", "launch",
    ];
    if !words.iter().any(|w| intent.contains(&w.as_str())) {
        return None;
    }
    let source_count = scan
        .entries
        .iter()
        .filter(|e| matches!(e.kind, fs::FileKind::Source | fs::FileKind::Header))
        .count();
    if source_count > 0 {
        return None;
    }
    let lang = language_from_request(request, &words).unwrap_or("");
    let what = if lang.is_empty() {
        "project".to_string()
    } else {
        format!("{lang} project")
    };
    let title = format!("No buildable {what} found.");
    let detail = if scan.total_discovered == 0 {
        "This directory is empty — no files detected.".to_string()
    } else if !scan.skipped_dirs.is_empty() && scan.entries.is_empty() {
        format!(
            "Only excluded directories ({}) were found; no source files or build configuration detected.",
            scan.skipped_dirs.iter().map(|d| format!("{d}/")).collect::<Vec<_>>().join(", ")
        )
    } else {
        "No source files or build configuration detected.".to_string()
    };
    Some((title, detail))
}

fn language_from_request(request: &str, words: &[String]) -> Option<&'static str> {
    let lower = request.to_lowercase();
    // Substring checks first: "c++" does not survive word tokenization.
    if lower.contains("c++") || lower.contains("cpp") || lower.contains("c/c++") {
        return Some("C++");
    }
    for w in words {
        match w.as_str() {
            "rust" | "cargo" => return Some("Rust"),
            "python" | "py" => return Some("Python"),
            "javascript" | "typescript" | "node" | "js" | "ts" => {
                return Some("JavaScript/TypeScript")
            }
            "golang" => return Some("Go"),
            "java" => return Some("Java"),
            _ => {}
        }
    }
    if lower.contains("golang") {
        return Some("Go");
    }
    None
}
