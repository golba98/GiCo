//! Deterministic project detection. Detectors are pure functions over the
//! scanned file listing, so results are reproducible and testable.
//! New project types plug in by implementing [`ProjectDetector`] and pushing
//! into [`detectors`].

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::env;
use super::fs;
use super::fs::{FileEntry, FileKind};
use super::RawScan;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// Human-readable project type, e.g. "C++", "Rust".
    pub kind: String,
    pub languages: Vec<String>,
    /// Detected build system: "Make", "CMake", "Cargo", "npm", ...
    pub build_system: Option<String>,
    /// Preferred build command, derived only from detected files/tools.
    pub build_command: Option<String>,
    /// Preferred run command, when derivable without inventing names.
    pub run_command: Option<String>,
    /// Project configuration files that were detected.
    pub config_files: Vec<String>,
    /// Names of all detectors that matched (primary first).
    pub matched_detectors: Vec<String>,
    /// Declared executable targets (e.g. CMake add_executable names).
    #[serde(default)]
    pub executable_targets: Vec<String>,
}

pub trait ProjectDetector: Send + Sync {
    fn name(&self) -> &'static str;
    /// Lower priority runs first; first match becomes the primary project.
    fn priority(&self) -> u8 {
        100
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo>;
}

/// The default detector registry. Extensible: push more detectors here.
pub fn detectors() -> Vec<Box<dyn ProjectDetector>> {
    vec![
        Box::new(RustDetector),
        Box::new(GoDetector),
        Box::new(JsDetector),
        Box::new(PythonDetector),
        Box::new(CppDetector),
        Box::new(MakeDetector),
    ]
}

/// Evidence tier: 0 = a real project config file exists at the root
/// (CMakeLists.txt, Cargo.toml, go.mod, package.json, Makefile ...),
/// 1 = only source-file extensions matched. Tier 0 always wins, so a
/// CMake C++ project is never misreported as "Python" just because a
/// scripts/ directory contains .py files.
fn evidence_tier(scan: &RawScan, p: &ProjectInfo) -> u8 {
    let root_configs = [
        "cmakelists.txt",
        "cargo.toml",
        "go.mod",
        "package.json",
        "makefile",
        "gnumakefile",
        "pyproject.toml",
        "requirements.txt",
        "setup.py",
        "pipfile",
    ];
    let has_root_config = scan
        .entries
        .iter()
        .any(|e| e.depth == 0 && root_configs.contains(&e.name.to_lowercase().as_str()));
    if has_root_config
        && p.config_files
            .iter()
            .any(|c| root_configs.contains(&c.to_lowercase().as_str()))
    {
        0
    } else {
        1
    }
}

/// Run all detectors, return the primary project. Selection is by evidence
/// tier first (real config file beats loose source files), then priority.
pub fn detect(scan: &RawScan) -> Option<ProjectInfo> {
    let mut results: Vec<ProjectInfo> = detectors()
        .into_iter()
        .filter_map(|d| d.detect(scan))
        .collect();
    if results.is_empty() {
        return None;
    }
    results.sort_by_key(|p| {
        (
            evidence_tier(scan, p),
            detector_priority(&p.matched_detectors[0]),
        )
    });
    Some(results.remove(0))
}

/// Parse `add_executable(name ...)` declarations from a CMakeLists.txt.
/// Only names are extracted; file contents are never sent to the model.
pub fn cmake_executable_targets(cmake_lists_path: &Path) -> Vec<String> {
    let Ok(contents) = fs::read_text(cmake_lists_path, 500_000) else {
        return Vec::new();
    };
    if contents.len() > 500_000 {
        return Vec::new();
    }
    let mut targets = Vec::new();
    let mut rest = contents.as_str();
    while let Some(pos) = rest.to_lowercase().find("add_executable") {
        rest = &rest[pos + "add_executable".len()..];
        // skip whitespace and opening paren
        let trimmed = rest.trim_start();
        let Some(trimmed) = trimmed.strip_prefix('(') else {
            continue;
        };
        let name: String = trimmed
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.' || *c == '+')
            .collect();
        if !name.is_empty() {
            targets.push(name);
        }
        // advance past this call's closing paren (shallow scan)
        let mut depth = 1;
        let mut adv = trimmed;
        while depth > 0 && !adv.is_empty() {
            let c = adv.chars().next().unwrap();
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            }
            adv = &adv[c.len_utf8()..];
        }
        rest = adv;
    }
    targets
}

fn detector_priority(name: &str) -> u8 {
    match name {
        "rust" => 10,
        "go" => 20,
        "js" => 30,
        "python" => 40,
        "cpp" => 50,
        "make" => 60,
        _ => 100,
    }
}

// ---------- helpers ----------

fn names(scan: &RawScan) -> BTreeSet<String> {
    scan.entries.iter().map(|e| e.name.to_lowercase()).collect()
}

fn has_ext(scan: &RawScan, exts: &[&str]) -> bool {
    scan.entries.iter().any(|e| {
        e.kind != FileKind::Directory && exts.iter().any(|x| e.name.to_lowercase().ends_with(x))
    })
}

fn top_level(scan: &RawScan, name: &str) -> Option<FileEntry> {
    scan.entries
        .iter()
        .find(|e| e.depth == 0 && e.name.to_lowercase() == name.to_lowercase())
        .cloned()
}

fn source_files(scan: &RawScan, exts: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = scan
        .entries
        .iter()
        .filter(|e| {
            e.kind != FileKind::Directory && exts.iter().any(|x| e.name.to_lowercase().ends_with(x))
        })
        .map(|e| e.path.clone())
        .collect();
    v.sort();
    v
}

fn compilers() -> Vec<String> {
    ["g++", "clang++", "c++"]
        .iter()
        .filter(|c| env::on_path(c))
        .map(|s| s.to_string())
        .collect()
}

// ---------- detectors ----------

struct CppDetector;
impl ProjectDetector for CppDetector {
    fn name(&self) -> &'static str {
        "cpp"
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo> {
        let n = names(scan);
        let has_sources = has_ext(scan, &[".cpp", ".cc", ".cxx", ".c"]);
        let has_headers = has_ext(scan, &[".h", ".hpp", ".hh"]);
        let has_cmake = n.contains("cmakelists.txt");
        let has_make = n.contains("makefile") || n.contains("gnumakefile");
        if !has_sources && !has_cmake {
            return None;
        }
        let mut config_files: Vec<String> = Vec::new();
        for f in ["CMakeLists.txt", "Makefile", "GnuMakefile"] {
            if let Some(e) = top_level(scan, f) {
                config_files.push(e.name);
            }
        }
        // Declared executable targets from CMakeLists.txt (names only).
        let executable_targets = top_level(scan, "CMakeLists.txt")
            .map(|e| cmake_executable_targets(&scan.environment.cwd.join(&e.path)))
            .unwrap_or_default();
        // Prefer an already-configured CMake build tree when one exists.
        let configured = scan
            .cmake_build_dirs
            .iter()
            .find(|d| d.as_str() == "build")
            .or_else(|| scan.cmake_build_dirs.first())
            .cloned();
        let (build_system, build_command, run_command) = if has_make {
            (
                Some("Make".to_string()),
                Some("make".to_string()),
                Some("make run".to_string()),
            )
        } else if has_cmake {
            match &configured {
                Some(bd) => (
                    Some("CMake".to_string()),
                    Some(format!("cmake --build {}", super::fs::shell_quote(bd))),
                    executable_targets
                        .first()
                        .map(|t| format!("./{}/{}", bd, super::fs::shell_quote(t))),
                ),
                None => (
                    Some("CMake".to_string()),
                    Some("cmake -S . -B build && cmake --build build".to_string()),
                    executable_targets
                        .first()
                        .map(|t| format!("./build/{}", super::fs::shell_quote(t))),
                ),
            }
        } else {
            let cc = compilers().into_iter().next();
            match cc {
                Some(cc) => (
                    None,
                    Some(format!("{cc} *.cpp -o output")),
                    Some("./output".to_string()),
                ),
                None => (None, None, None),
            }
        };
        let _ = has_headers;
        Some(ProjectInfo {
            kind: "C/C++".to_string(),
            languages: vec!["C++".to_string()],
            build_system,
            build_command,
            run_command,
            config_files,
            matched_detectors: vec!["cpp".to_string()],
            executable_targets,
        })
    }
}

struct MakeDetector;
impl ProjectDetector for MakeDetector {
    fn name(&self) -> &'static str {
        "make"
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo> {
        let n = names(scan);
        if !(n.contains("makefile") || n.contains("gnumakefile")) {
            return None;
        }
        // Only primary when no language detector claimed the directory.
        if has_ext(
            scan,
            &[".cpp", ".cc", ".cxx", ".rs", ".py", ".js", ".ts", ".go"],
        ) {
            return None;
        }
        Some(ProjectInfo {
            kind: "Make".to_string(),
            languages: vec![],
            build_system: Some("Make".to_string()),
            build_command: Some("make".to_string()),
            run_command: None,
            config_files: vec!["Makefile".to_string()],
            matched_detectors: vec!["make".to_string()],
            executable_targets: Vec::new(),
        })
    }
}

struct RustDetector;
impl ProjectDetector for RustDetector {
    fn name(&self) -> &'static str {
        "rust"
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo> {
        let n = names(scan);
        if !n.contains("cargo.toml") {
            return None;
        }
        let has_rs = has_ext(scan, &[".rs"]);
        Some(ProjectInfo {
            kind: "Rust".to_string(),
            languages: vec!["Rust".to_string()],
            build_system: Some("Cargo".to_string()),
            build_command: Some("cargo build".to_string()),
            run_command: if has_rs {
                Some("cargo run".to_string())
            } else {
                None
            },
            config_files: vec!["Cargo.toml".to_string()],
            matched_detectors: vec!["rust".to_string()],
            executable_targets: Vec::new(),
        })
    }
}

struct GoDetector;
impl ProjectDetector for GoDetector {
    fn name(&self) -> &'static str {
        "go"
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo> {
        if !names(scan).contains("go.mod") {
            return None;
        }
        Some(ProjectInfo {
            kind: "Go".to_string(),
            languages: vec!["Go".to_string()],
            build_system: Some("Go modules".to_string()),
            build_command: Some("go build ./...".to_string()),
            run_command: Some("go run .".to_string()),
            config_files: vec!["go.mod".to_string()],
            matched_detectors: vec!["go".to_string()],
            executable_targets: Vec::new(),
        })
    }
}

struct PythonDetector;
impl ProjectDetector for PythonDetector {
    fn name(&self) -> &'static str {
        "python"
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo> {
        let n = names(scan);
        let has_meta = n.contains("pyproject.toml")
            || n.contains("requirements.txt")
            || n.contains("setup.py")
            || n.contains("setup.cfg")
            || n.contains("pipfile");
        let py_files = source_files(scan, &[".py"]);
        if !has_meta && py_files.is_empty() {
            return None;
        }
        let mut config_files: Vec<String> = Vec::new();
        for f in [
            "pyproject.toml",
            "requirements.txt",
            "setup.py",
            "setup.cfg",
            "Pipfile",
        ] {
            if let Some(e) = top_level(scan, f) {
                config_files.push(e.name);
            }
        }
        let venv = n.contains(".venv") || n.contains("venv") || n.contains("env");
        let run_command = if n.contains("main.py") {
            Some("python3 main.py".to_string())
        } else if py_files.len() == 1 {
            Some(format!("python3 {}", py_files[0]))
        } else {
            None
        };
        let _ = venv;
        Some(ProjectInfo {
            kind: "Python".to_string(),
            languages: vec!["Python".to_string()],
            build_system: if has_meta {
                Some("pip".to_string())
            } else {
                None
            },
            build_command: None,
            run_command,
            config_files,
            matched_detectors: vec!["python".to_string()],
            executable_targets: Vec::new(),
        })
    }
}

struct JsDetector;
impl ProjectDetector for JsDetector {
    fn name(&self) -> &'static str {
        "js"
    }
    fn detect(&self, scan: &RawScan) -> Option<ProjectInfo> {
        let n = names(scan);
        let has_pkg = top_level(scan, "package.json");
        let has_ts = has_ext(scan, &[".ts", ".tsx"]);
        let has_js = has_ext(scan, &[".js", ".jsx", ".mjs"]) || n.contains("package.json");
        if has_pkg.is_none() && !has_js && !has_ts {
            return None;
        }
        let mut config_files: Vec<String> = Vec::new();
        if let Some(e) = &has_pkg {
            config_files.push(e.name.clone());
        }
        for f in [
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "bun.lockb",
            "bun.lock",
            "tsconfig.json",
        ] {
            if let Some(e) = top_level(scan, f) {
                config_files.push(e.name);
            }
        }
        // Detect the package manager from lockfiles; never assume one.
        let (pm, build_system) = if n.contains("pnpm-lock.yaml") {
            ("pnpm", Some("pnpm"))
        } else if n.contains("yarn.lock") {
            ("yarn", Some("yarn"))
        } else if n.contains("bun.lockb") || n.contains("bun.lock") {
            ("bun", Some("bun"))
        } else if n.contains("package-lock.json") || n.contains("package.json") {
            ("npm", Some("npm"))
        } else {
            ("npm", None)
        };
        let scripts = package_json_scripts(scan, &has_pkg);
        let build_command = if scripts.contains("build") {
            Some(format!("{pm} run build"))
        } else if n.contains("package.json") {
            Some(format!("{pm} install"))
        } else {
            None
        };
        let run_command = if scripts.contains("start") {
            Some(format!("{pm} start"))
        } else if scripts.contains("dev") {
            Some(format!("{pm} run dev"))
        } else {
            None
        };
        let languages = {
            let mut l = Vec::new();
            if has_js {
                l.push("JavaScript");
            }
            if has_ts {
                l.push("TypeScript");
            }
            if l.is_empty() {
                l.push("JavaScript");
            }
            l.iter().map(|s| s.to_string()).collect()
        };
        Some(ProjectInfo {
            kind: if has_ts { "TypeScript" } else { "JavaScript" }.to_string(),
            languages,
            build_system: build_system.map(|s| s.to_string()),
            build_command,
            run_command,
            config_files,
            matched_detectors: vec!["js".to_string()],
            executable_targets: Vec::new(),
        })
    }
}

/// Parse package.json *only* to extract script names (never sent raw).
fn package_json_scripts(scan: &RawScan, pkg: &Option<FileEntry>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(entry) = pkg else { return out };
    if entry.depth != 0 {
        return out;
    }
    let path = Path::new(&scan.environment.cwd).join(&entry.path);
    let Ok(contents) = fs::read_text(&path, 200_000) else {
        return out;
    };
    if contents.len() > 200_000 {
        return out;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&contents) {
        if let Some(scripts) = v.get("scripts").and_then(|s| s.as_object()) {
            for (k, _) in scripts {
                if let Some(s) = fs::sanitize_name(k) {
                    out.insert(s);
                }
            }
        }
    }
    out
}
