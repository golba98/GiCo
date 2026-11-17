//! Read-only environment collection: OS, shell, working directory and the
//! command-line utilities / compilers / package managers available on PATH.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Verified environment facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub os: String,
    pub arch: String,
    pub shell: String,
    pub cwd: PathBuf,
    /// Command-line utilities present on PATH (subset of `tools` that matched).
    pub utilities: Vec<String>,
    /// Compilers present on PATH.
    pub compilers: Vec<String>,
    /// Language runtimes present on PATH.
    pub runtimes: Vec<String>,
    /// Package managers present on PATH.
    pub package_managers: Vec<String>,
}

const TOOLS: &[&str] = &[
    "git", "make", "cmake", "ninja", "g++", "gcc", "clang", "clang++", "cc", "c++", "cargo",
    "rustc", "go", "javac", "java", "python3", "python", "pip3", "node", "npm", "pnpm", "yarn",
    "bun", "dotnet", "ruby", "perl", "php", "lua", "docker", "podman", "curl", "wget", "jq", "rg",
    "fd", "fzf", "tmux", "vim", "nvim", "emacs",
];

const COMPILERS: &[&str] = &[
    "g++", "gcc", "clang++", "clang", "cc", "c++", "rustc", "go", "javac",
];
const RUNTIMES: &[&str] = &[
    "python3", "python", "node", "bun", "ruby", "perl", "php", "lua", "java",
];
const PKG_MANAGERS: &[&str] = &[
    "cargo", "pip3", "npm", "pnpm", "yarn", "bun", "dotnet", "apt", "dnf", "yum", "pacman", "go",
];

/// True if `name` resolves to an executable file somewhere on PATH.
pub fn on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(name)))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn os_name() -> String {
    if let Ok(data) = std::fs::read_to_string("/etc/os-release") {
        for line in data.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                return v.trim_matches('"').to_string();
            }
        }
    }
    format!("{} Linux", std::env::consts::OS)
}

fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            Path::new(&s)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Collect environment facts for `cwd`. Purely read-only.
pub fn collect(cwd: &Path) -> Environment {
    let present: BTreeSet<&str> = TOOLS.iter().copied().filter(|t| on_path(t)).collect();
    let pick = |list: &[&str]| -> Vec<String> {
        list.iter()
            .filter(|t| present.contains(*t))
            .map(|s| s.to_string())
            .collect()
    };
    Environment {
        os: os_name(),
        arch: std::env::consts::ARCH.to_string(),
        shell: shell_name(),
        cwd: cwd.to_path_buf(),
        utilities: pick(TOOLS),
        compilers: pick(COMPILERS),
        runtimes: pick(RUNTIMES),
        package_managers: pick(PKG_MANAGERS),
    }
}

/// Basename helper used across the crate.
pub fn basename(p: &Path) -> String {
    p.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// True if `name` is a shell builtin / keyword (not a PATH executable).
pub fn is_shell_builtin(name: &str) -> bool {
    matches!(
        name,
        "cd" | "echo"
            | "printf"
            | "export"
            | "unset"
            | "alias"
            | "set"
            | "read"
            | "source"
            | "."
            | "exit"
            | "return"
            | "exec"
            | "eval"
            | "umask"
            | "jobs"
            | "fg"
            | "bg"
            | "wait"
            | "true"
            | "false"
            | "test"
            | "["
            | "pwd"
            | "let"
            | "local"
            | "type"
            | "history"
            | "pushd"
            | "popd"
            | "dirs"
            | "shopt"
            | "bind"
            | "complete"
            | "compgen"
            | "declare"
            | "typeset"
            | "hash"
            | "ulimit"
            | "time"
            | "command"
            | "builtin"
            | "getopts"
            | "shift"
            | "trap"
            | "kill"
            | "disown"
            | "suspend"
            | "logout"
            | "readonly"
            | "mapfile"
            | "readarray"
            | "caller"
            | "enable"
            | "help"
            | "fc"
    )
}

/// Superset check used by the verifier: builtin or resolvable on PATH.
pub fn command_available(name: &str) -> bool {
    is_shell_builtin(name) || on_path(name)
}

#[allow(dead_code)]
pub fn os_str(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}
