//! Bounded, read-only directory scanning with sensitive-file exclusion and
//! prompt-injection sanitisation. Only file *names* are ever collected;
//! file contents are never read here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Source,
    Header,
    Build,
    Config,
    Doc,
    Data,
    Script,
    Directory,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Exact on-disk name (capitalisation preserved), sanitised.
    pub name: String,
    /// Path relative to the scanned root.
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub entries: Vec<FileEntry>,
    pub total_discovered: usize,
    pub excluded_sensitive: usize,
    pub excluded_injection: usize,
    /// Top-level directories that were skipped (heavy/excluded, e.g. build/).
    pub skipped_dirs: Vec<String>,
    /// Directories that look like configured CMake build trees
    /// (contain CMakeCache.txt), recorded without descending into them.
    pub cmake_build_dirs: Vec<String>,
}

/// Directories never descended into (heavy or private).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".svn",
    ".hg",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".cache",
    ".idea",
    ".vscode",
    ".cargo",
    "vendor",
    "coverage",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".private",
    ".github",
];

/// A directory is treated as a CMake build tree when it contains this file.
const CMAKE_CACHE: &str = "CMakeCache.txt";

/// True if a directory name looks like a CMake build tree
/// ("build", "build-debug", "build_release", ...).
pub fn is_build_dir_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "build" || lower.starts_with("build-") || lower.starts_with("build_")
}

/// Basenames (lowercased) that are never listed or read: credentials,
/// secrets, keys, shell history, VCS internals.
const SENSITIVE_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".netrc",
    ".pgpass",
    ".bash_history",
    ".zsh_history",
    ".sh_history",
    ".mysql_history",
    ".psql_history",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "credentials",
    "credentials.json",
    "secrets",
    "secrets.json",
    "secrets.yaml",
    "config.json", // only sensitive inside .docker; handled by dir rule
];

const SENSITIVE_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".config/google-chrome",
    ".pki",
    ".cert",
];

const SENSITIVE_SUFFIXES: &[&str] = &[
    ".pem", ".key", ".p12", ".pfx", ".crt", ".der", ".asc", ".gpg", ".env", ".secret",
];

const SENSITIVE_SUBSTRINGS: &[&str] = &[
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "credential",
    "secret",
    "token",
];

/// Filename fragments that look like prompt-injection attempts. Matching
/// names are excluded from context entirely.
const INJECTION_MARKERS: &[&str] = &[
    "ignore previous",
    "ignore all",
    "disregard",
    "new instructions",
    "system:",
    "you are now",
    "act as",
    "jailbreak",
    "do anything now",
    "```",
    "<|im_start|>",
    "<|im_end|>",
];

/// True if a basename refers to a sensitive file/directory we must never expose.
pub fn is_sensitive(name: &str, is_dir: bool) -> bool {
    let lower = name.to_lowercase();
    if is_dir
        && (SENSITIVE_DIRS.contains(&lower.as_str())
            || matches!(
                lower.as_str(),
                ".private" | ".git" | ".ollama" | ".codex" | ".config" | ".local"
            ))
    {
        return true;
    }
    if SENSITIVE_NAMES.contains(&lower.as_str()) {
        return true;
    }
    if lower.starts_with(".env") {
        return true;
    }
    if SENSITIVE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return true;
    }
    // id_rsa, id_rsa.pub, prod.credentials.txt, aws_secret.key, auth-token ...
    if SENSITIVE_SUBSTRINGS.iter().any(|s| lower.contains(s)) {
        return true;
    }
    false
}

/// True if a filename contains prompt-injection markers.
pub fn is_injection_attempt(name: &str) -> bool {
    let lower = name.to_lowercase();
    INJECTION_MARKERS.iter().any(|m| lower.contains(m))
}

/// Strip control characters and cap length so a hostile filename cannot
/// break the prompt format. Returns None if nothing safe remains.
pub fn sanitize_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() || cleaned.len() > 120 {
        return None;
    }
    Some(cleaned)
}

pub fn classify(name: &str, is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Directory;
    }
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "cpp" | "cc" | "cxx" | "c" | "rs" | "py" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "go"
        | "java" | "kt" | "swift" | "rb" | "php" | "lua" | "sh" | "bash" | "zsh" | "pl" | "r"
        | "cs" | "scala" | "hs" => FileKind::Source,
        "h" | "hpp" | "hh" | "inc" => FileKind::Header,
        "mk" => FileKind::Build,
        "toml" | "json" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "lock" | "mod" | "sum"
        | "xml" | "editorconfig" | "gitignore" | "dockerignore" => FileKind::Config,
        "md" | "txt" | "rst" | "pdf" | "adoc" => FileKind::Doc,
        "csv" | "tsv" | "db" | "sqlite" | "jsonl" => FileKind::Data,
        _ => {
            if lower == "makefile" || lower == "gnumakefile" || lower == "cmakelists.txt" {
                FileKind::Build
            } else if lower == "dockerfile" || lower == "justfile" {
                FileKind::Script
            } else {
                FileKind::Other
            }
        }
    }
}

/// Read a regular text file without following a symlink at its final component.
pub(crate) fn read_text(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "file exceeds inspection limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| Error::new(ErrorKind::InvalidData, error))
}

pub(crate) fn inside(root: &Path, path: &Path) -> bool {
    root.canonicalize()
        .and_then(|root| path.canonicalize().map(|path| path.starts_with(root)))
        .unwrap_or(false)
}

/// Bounded read-only scan of `root`. Never follows symlinks out of the tree,
/// never reads file contents, never touches sensitive names.
pub fn scan(root: &Path, max_entries: usize, max_depth: usize) -> ScanResult {
    let mut entries = Vec::new();
    let mut total = 0usize;
    let mut excl_sens = 0usize;
    let mut excl_inj = 0usize;
    let mut skipped = Vec::new();
    let mut cmake_build_dirs = Vec::new();
    walk(
        root,
        root,
        0,
        max_depth,
        max_entries,
        &mut entries,
        &mut total,
        &mut excl_sens,
        &mut excl_inj,
        &mut skipped,
        &mut cmake_build_dirs,
    );
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort();
    cmake_build_dirs.sort();
    ScanResult {
        entries,
        total_discovered: total,
        excluded_sensitive: excl_sens,
        excluded_injection: excl_inj,
        skipped_dirs: skipped,
        cmake_build_dirs,
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    entries: &mut Vec<FileEntry>,
    total: &mut usize,
    excl_sens: &mut usize,
    excl_inj: &mut usize,
    skipped: &mut Vec<String>,
    cmake_build_dirs: &mut Vec<String>,
) {
    let visit_limit = max_entries.saturating_mul(10);
    if entries.len() >= max_entries || depth > max_depth || *total >= visit_limit {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = rd
        .take(visit_limit - *total)
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    children.sort();
    let mut directories = Vec::new();
    for path in children {
        if *total >= visit_limit {
            break;
        }
        *total += 1;
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.is_dir() && !md.is_file() {
            continue;
        }
        let is_dir = md.is_dir();
        let raw_name = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        if entries.len() >= max_entries {
            // Entry cap reached: keep counting (so callers can report how
            // much was omitted) but do not store or descend further.
            continue;
        }
        if is_sensitive(&raw_name, is_dir) {
            *excl_sens += 1;
            continue;
        }
        if is_injection_attempt(&raw_name) || raw_name.chars().any(char::is_control) {
            *excl_inj += 1;
            continue;
        }
        // Any directory (any name, shallow depth) containing a CMake cache is
        // a configured build tree: record it and never descend into it.
        if is_dir && depth <= 2 && path.join(CMAKE_CACHE).is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if !cmake_build_dirs.contains(&rel) {
                cmake_build_dirs.push(rel);
            }
            if depth == 0 {
                skipped.push(raw_name.clone());
            }
            continue;
        }
        if is_dir
            && (SKIP_DIRS.contains(&raw_name.to_lowercase().as_str())
                || is_build_dir_name(&raw_name))
        {
            if depth == 0 {
                skipped.push(raw_name.clone());
            }
            continue;
        }
        let Some(name) = sanitize_name(&raw_name) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        entries.push(FileEntry {
            name,
            path: rel,
            kind: classify(&raw_name, is_dir),
            size: if is_dir { 0 } else { md.len() },
            depth,
        });
        if is_dir && depth < max_depth {
            directories.push(path);
        }
    }
    for path in directories {
        walk(
            root,
            &path,
            depth + 1,
            max_depth,
            max_entries,
            entries,
            total,
            excl_sens,
            excl_inj,
            skipped,
            cmake_build_dirs,
        );
    }
}

/// Quote a path/argument for safe use in a shell command. Paths without
/// special characters are returned unchanged; others are single-quoted
/// with the '\'' idiom for embedded apostrophes.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || "-_./:@%+=,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}
