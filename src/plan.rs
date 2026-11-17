//! Evidence-based command planning.
//!
//! Deterministic planning handles recognized intents and project types;
//! the model only interprets genuinely ambiguous requests. All evidence is
//! gathered read-only and bounded; candidate commands are validated by the
//! verifier before presentation.

use std::path::Path;

use crate::context::cmake;
use crate::context::env;
use crate::context::fs::{shell_quote, FileKind};
use crate::context::RawScan;
use crate::intent::{self, Intent};
use crate::runtime;

#[derive(Debug, Clone)]
pub struct Plan {
    pub command: String,
    /// Informational note shown with the command (e.g. missing prerequisites).
    pub note: Option<String>,
}

/// Build a deterministic plan for the request in the scanned directory.
pub fn plan(raw: &RawScan, request: &str) -> Option<Plan> {
    let intent = intent::classify(request, raw);
    if matches!(intent, Intent::Run | Intent::BuildAndRun)
        && requested_target_kind(request).is_some()
        && raw.project.as_ref().and_then(|p| p.build_system.as_deref()) != Some("CMake")
    {
        return Some(Plan {
            command: String::new(),
            note: Some("No verified matching executable was found in this project.".into()),
        });
    }
    match intent {
        Intent::Build | Intent::Run | Intent::BuildAndRun | Intent::Test | Intent::Clean => {
            project_plan(raw, request, intent)
        }
        other => general_plan(raw, request, other),
    }
}

// ---------------------------------------------------------------------------
// General (non-project) intents
// ---------------------------------------------------------------------------

fn general_plan(raw: &RawScan, request: &str, intent: Intent) -> Option<Plan> {
    match intent {
        Intent::ListDir => Some(Plan {
            command: "ls -la".to_string(),
            note: None,
        }),
        Intent::SearchFiles => {
            let pat = intent::search_pattern(request).unwrap_or_else(|| "*".to_string());
            Some(Plan {
                command: format!(
                    "find . -type f -name '{}' -not -path '*/node_modules/*' -not -path '*/.git/*'",
                    pat.replace('\'', "")
                ),
                note: None,
            })
        }
        Intent::LargestFiles => {
            if env::on_path("du") {
                Some(Plan {
                    command: "du -h --max-depth=3 . 2>/dev/null | sort -rh | head -20".to_string(),
                    note: None,
                })
            } else {
                Some(Plan {
                    command: "find . -type f -not -path '*/node_modules/*' -not -path '*/.git/*' -printf '%s %p\\n' | sort -rn | head -20".to_string(),
                    note: None,
                })
            }
        }
        Intent::GitStatus if raw.git.branch.is_some() && env::on_path("git") => Some(Plan {
            command: "git status".to_string(),
            note: None,
        }),
        Intent::GitDiff if raw.git.branch.is_some() && env::on_path("git") => Some(Plan {
            command: "git diff".to_string(),
            note: None,
        }),
        Intent::GitLog if raw.git.branch.is_some() && env::on_path("git") => Some(Plan {
            command: "git log --oneline -10".to_string(),
            note: None,
        }),
        Intent::InspectPort => {
            let port = intent::port_number(request).unwrap_or(8080);
            if env::on_path("ss") {
                Some(Plan {
                    command: format!("ss -tlnp | grep ':{port}'"),
                    note: None,
                })
            } else if env::on_path("lsof") {
                Some(Plan {
                    command: format!("lsof -i :{port}"),
                    note: None,
                })
            } else if env::on_path("netstat") {
                Some(Plan {
                    command: format!("netstat -tlnp 2>/dev/null | grep ':{port}'"),
                    note: None,
                })
            } else {
                Some(Plan {
                    command: format!(
                        "grep -E ':{port}\\b' /proc/net/tcp /proc/net/tcp6 2>/dev/null || true"
                    ),
                    note: Some(
                        "no ss/lsof/netstat installed; showing raw /proc/net entries".to_string(),
                    ),
                })
            }
        }
        Intent::InspectProcess if env::on_path("ps") => Some(Plan {
            command: "ps aux --sort=-%cpu | head -15".to_string(),
            note: None,
        }),
        Intent::InstallPackage => {
            let pkg = intent::package_name(request)?;
            // Prefer the distribution's native package manager.
            for pm in ["dnf", "apt", "pacman", "zypper", "brew", "apk"] {
                if env::on_path(pm) {
                    let sudo = if pm == "brew" { "" } else { "sudo " };
                    return Some(Plan {
                        command: format!("{sudo}{pm} install {pkg}"),
                        note: None,
                    });
                }
            }
            Some(Plan {
                command: String::new(),
                note: Some(format!("no supported package manager found for '{pkg}'")),
            })
        }
        Intent::DiskUsage if env::on_path("df") => Some(Plan {
            command: "df -h".to_string(),
            note: None,
        }),
        Intent::MemoryInfo if env::on_path("free") => Some(Plan {
            command: "free -h".to_string(),
            note: None,
        }),
        Intent::NavigateDir => navigate_plan(raw, request),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directory navigation: resolve the destination from filesystem evidence.
// A child process cannot change the parent shell's cwd, so the plan is a
// plain `cd <verified path>`; with the opt-in zsh hook (`gico init-zsh`) the
// approved cd is applied in the user's own shell.
// ---------------------------------------------------------------------------

const NAV_STOPWORDS: &[&str] = &[
    "my",
    "the",
    "a",
    "an",
    "to",
    "me",
    "into",
    "in",
    "directory",
    "dir",
    "folder",
    "project",
    "projects",
    "development",
    "dev",
    "code",
    "work",
    "workspace",
    "repo",
    "repos",
    "please",
    "for",
    "and",
    "of",
    "change",
    "changed",
    "go",
    "goes",
    "navigate",
    "switch",
    "take",
    "open",
    "browse",
    "cd",
];

/// Directories that are never useful navigation targets (caches, toolchains,
/// package stores).
const NAV_SKIP_DIRS: &[&str] = &[
    ".cache",
    ".npm",
    ".cargo",
    ".rustup",
    ".local",
    ".config",
    ".git",
    "node_modules",
    "miniforge3",
    "anaconda3",
    "target",
    "__pycache__",
    ".venv",
    "venv",
];

/// Project-marker files that identify what a directory is used for.
const MARKER_FILES: &[(&str, &str)] = &[
    ("rust", "Cargo.toml"),
    ("python", "pyproject.toml"),
    ("python", "requirements.txt"),
    ("javascript", "package.json"),
    ("typescript", "package.json"),
    ("node", "package.json"),
    ("go", "go.mod"),
    ("c++", "CMakeLists.txt"),
    ("cpp", "CMakeLists.txt"),
    ("java", "pom.xml"),
    ("java", "build.gradle"),
];

fn resolve_directory(request: &str, cwd: &Path) -> Result<std::path::PathBuf, &'static str> {
    let home = std::env::var_os("HOME").ok_or("home directory is unavailable")?;
    let home = std::path::PathBuf::from(home);
    if let Some((_, destination)) = request.split_once(" to ") {
        let destination = destination.trim().trim_matches(['\'', '"']);
        let explicit = destination.starts_with('/')
            || destination.starts_with('.')
            || destination.starts_with("~/");
        if explicit {
            let path = if let Some(relative) = destination.strip_prefix("~/") {
                home.join(relative)
            } else {
                cwd.join(destination)
            };
            return path
                .canonicalize()
                .ok()
                .filter(|path| path.is_dir())
                .ok_or("could not find the requested directory");
        }
    }
    let lower = request.to_lowercase();
    let tokens: Vec<_> = lower
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '+'))
        .filter(|word| !word.is_empty() && !NAV_STOPWORDS.contains(word))
        .collect();
    if tokens.is_empty() {
        return Err("could not find a directory matching the request");
    }

    let mut queue = std::collections::VecDeque::from([(home, 0)]);
    let mut remaining = 20_000;
    let mut best = None;
    let mut tied = false;
    while let Some((dir, depth)) = queue.pop_front() {
        if depth == 3 || remaining == 0 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in entries.take(remaining) {
            remaining -= 1;
            let Ok(entry) = entry else {
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.')
                || NAV_SKIP_DIRS.contains(&name)
                || crate::context::fs::is_sensitive(name, true)
                || name.chars().any(char::is_control)
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let path = entry.path();
            let lower = name.to_lowercase();
            let name_words = lower.split(|c: char| !c.is_alphanumeric() && c != '+');
            let direct = tokens
                .iter()
                .filter(|token| name_words.clone().any(|word| word == **token))
                .count();
            if direct > 0 {
                let markers = MARKER_FILES
                    .iter()
                    .filter(|(token, marker)| {
                        tokens.contains(token)
                            && (path.join(marker).is_file() || dir_has_child_marker(&path, marker))
                    })
                    .count();
                let rank = (direct + 5 * markers, std::cmp::Reverse(depth));
                match best.as_ref() {
                    Some((current, _)) if rank == *current => tied = true,
                    Some((current, _)) if rank < *current => {}
                    _ => {
                        best = Some((rank, path.clone()));
                        tied = false;
                    }
                }
            }
            children.push(path);
        }
        children.sort();
        queue.extend(children.into_iter().map(|path| (path, depth + 1)));
    }
    if tied || remaining == 0 {
        return Err(
            "directory evidence is ambiguous or the search limit was reached; specify a path",
        );
    }
    best.map(|(_, path)| path)
        .ok_or("could not find a directory matching the request")
}

fn dir_has_child_marker(dir: &Path, marker: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.take(50).flatten().any(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir()) && entry.path().join(marker).is_file()
        })
    })
}

fn navigate_plan(raw: &RawScan, request: &str) -> Option<Plan> {
    let (command, note) = match resolve_directory(request, &raw.environment.cwd) {
        Ok(path) => match path
            .to_str()
            .filter(|path| !path.chars().any(char::is_control))
        {
            Some(path) => (format!("cd {}", shell_quote(path)), None),
            None => (
                String::new(),
                Some("the directory cannot be safely represented".to_owned()),
            ),
        },
        Err(message) => (String::new(), Some(message.to_owned())),
    };
    Some(Plan { command, note })
}

pub(crate) fn requested_target_kind(request: &str) -> Option<cmake::TargetKind> {
    if intent::wants_gui(request) {
        Some(cmake::TargetKind::Gui)
    } else if intent::wants_service(request) {
        Some(cmake::TargetKind::Service)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Project intents
// ---------------------------------------------------------------------------

fn project_plan(raw: &RawScan, request: &str, intent: Intent) -> Option<Plan> {
    let Some(project) = &raw.project else {
        return fallback_cpp(raw, intent);
    };
    match project.build_system.as_deref() {
        Some("CMake") => cmake_plan(raw, request, intent),
        Some("Make") => make_plan(raw, intent),
        Some("Cargo") => cargo_plan(raw, intent),
        Some(pm @ ("npm" | "pnpm" | "yarn" | "bun")) => npm_plan(intent, pm),
        Some("pip") | None if project.kind == "Python" => python_plan(project, intent),
        Some("Go modules") if env::on_path("go") => go_plan(intent),
        _ => fallback_cpp(raw, intent),
    }
}

fn join(steps: &[String]) -> String {
    steps.join(" && ")
}

fn cmake_plan(raw: &RawScan, request: &str, intent: Intent) -> Option<Plan> {
    let root = raw.environment.cwd.clone();
    let trees = cmake::discover_build_trees(&root, &raw.cmake_build_dirs);
    let (default_dir, configured) = cmake::select_build_dir(&root, &trees);
    let preset = if configured {
        None
    } else {
        cmake::presets(&root).into_iter().find_map(|preset| {
            let dir = cmake::preset_build_dir(&root, &preset)?;
            std::fs::symlink_metadata(root.join(&dir))
                .is_err()
                .then_some((preset, dir))
        })
    };
    let bd = preset.as_ref().map_or(default_dir, |(_, dir)| dir.clone());
    let bdq = shell_quote(&bd);
    let mut build_steps = Vec::new();
    if !configured {
        build_steps.push(match &preset {
            Some((preset, _)) => format!("cmake --preset {}", shell_quote(preset)),
            None => format!("cmake -S . -B {bdq}"),
        });
    }
    let build_preset = preset
        .as_ref()
        .and_then(|(name, _)| cmake::matching_build_preset(&root, name));
    build_steps.push(match build_preset {
        Some(name) => format!("cmake --build --preset {}", shell_quote(&name)),
        None => format!("cmake --build {bdq}"),
    });

    let requested = requested_target_kind(request);
    let kinds = if matches!(intent, Intent::Run | Intent::BuildAndRun) {
        cmake::target_kinds(&root)
    } else {
        Vec::new()
    };
    let eligible: Vec<_> = kinds
        .iter()
        .filter(|target| {
            requested.map_or(
                matches!(
                    target.kind,
                    cmake::TargetKind::Cli | cmake::TargetKind::Gui | cmake::TargetKind::Service
                ),
                |kind| target.kind == kind,
            )
        })
        .collect();
    let mut eligible = eligible;
    eligible.sort_by_key(|target| target.kind != cmake::TargetKind::Cli);
    let build_tree_names: Vec<&str> = if requested.is_some() && intent == Intent::Run {
        trees
            .iter()
            .filter(|tree| tree.valid)
            .map(|tree| tree.dir.as_str())
            .collect()
    } else if configured {
        vec![bd.as_str()]
    } else {
        Vec::new()
    };
    let selected = eligible
        .iter()
        .find_map(|target| {
            build_tree_names.iter().find_map(|dir| {
                cmake::find_built_executable(&root, dir, &target.artifact_name)
                    .map(|path| (*target, path, true))
            })
        })
        .or_else(|| {
            if requested.is_some() && intent == Intent::Run {
                return None;
            }
            eligible
                .iter()
                .find(|target| target.default_output)
                .map(|target| {
                    let path = Path::new(&bd)
                        .join(&target.declared_in)
                        .join(&target.artifact_name);
                    (*target, path.to_string_lossy().into_owned(), false)
                })
        });
    let stale_builds = trees
        .iter()
        .filter(|tree| !tree.valid)
        .map(|tree| format!("{}/", tree.dir.trim_end_matches('/')))
        .collect::<Vec<_>>()
        .join(", ");
    let stale_note =
        (!stale_builds.is_empty()).then(|| format!("Skipped stale builds: {stale_builds}"));

    match intent {
        Intent::Build => Some(Plan {
            command: join(&build_steps),
            note: stale_note,
        }),
        Intent::BuildAndRun | Intent::Run => {
            let Some((target, exe, built)) = selected else {
                if let Some(kind) = requested {
                    return Some(Plan {
                        command: String::new(),
                        note: Some(format!(
                            "No verified {} executable was found for this request.",
                            kind.as_str().to_uppercase()
                        )),
                    });
                }
                return Some(Plan {
                    command: join(&build_steps),
                    note: Some(
                        "no executable target with a verified output location; only building"
                            .into(),
                    ),
                });
            };
            let exeq = shell_quote(&format!("./{exe}"));
            let usage = runtime::gather_usage(&root, &target.artifact_name, None);
            let mut run = exeq.clone();
            let note = stale_note;
            if usage.requires_positional {
                match usage.sample_inputs.first() {
                    Some(sample) => {
                        run = format!("{exeq} {}", shell_quote(sample));
                        if let Some(flag) = &usage.output_flag {
                            let path = std::env::temp_dir().join("gico-out");
                            if let Some(path) = path
                                .to_str()
                                .filter(|path| !path.chars().any(char::is_control))
                            {
                                run = format!("{run} {flag} {}", shell_quote(path));
                            }
                        }
                    }
                    None => {
                        return Some(Plan {
                            command: join(&build_steps),
                            note: Some(format!(
                                "'{exe}' requires an input argument ({}); no verified sample input found, so only building",
                                usage.usage.as_deref().unwrap_or("usage unknown")
                            )),
                        });
                    }
                }
            }
            let cmd = if intent == Intent::BuildAndRun || !built {
                join(&[join(&build_steps), run])
            } else {
                run
            };
            let note = match (note, target.kind) {
                (n, cmake::TargetKind::Gui) => Some(match n {
                    Some(x) => format!("{x}\nGUI application · Display server required"),
                    None => "GUI application · Display server required".to_string(),
                }),
                (n, _) => n,
            };
            Some(Plan { command: cmd, note })
        }
        Intent::Test => {
            let mut steps = build_steps.clone();
            steps.push(format!("ctest --test-dir {bdq} --output-on-failure"));
            Some(Plan {
                command: join(&steps),
                note: None,
            })
        }
        Intent::Clean => Some(Plan {
            command: format!("cmake --build {bdq} --target clean"),
            note: None,
        }),
        _ => None,
    }
}

/// Parse Makefile target names (`target:` lines).
fn makefile_targets(path: &Path) -> Vec<String> {
    let Ok(contents) = crate::context::fs::read_text(path, 500_000) else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for line in contents.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('\t') || line.starts_with(' ') || line.is_empty() {
            continue;
        }
        if let Some((name, _)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
                && !name.contains('=')
            {
                targets.push(name.to_string());
            }
        }
    }
    targets
}

fn make_plan(raw: &RawScan, intent: Intent) -> Option<Plan> {
    let mk = raw
        .entries
        .iter()
        .find(|e| e.depth == 0 && (e.name == "Makefile" || e.name == "GnuMakefile"))
        .map(|e| raw.environment.cwd.join(&e.path))?;
    let targets = makefile_targets(&mk);
    match intent {
        Intent::Build => Some(Plan {
            command: "make".to_string(),
            note: None,
        }),
        Intent::BuildAndRun | Intent::Run => {
            if targets.iter().any(|t| t == "run") {
                let cmd = if intent == Intent::BuildAndRun {
                    "make && make run"
                } else {
                    "make run"
                };
                Some(Plan {
                    command: cmd.to_string(),
                    note: None,
                })
            } else {
                Some(Plan {
                    command: "make".to_string(),
                    note: Some("Makefile has no 'run' target; built only".to_string()),
                })
            }
        }
        Intent::Test => {
            if targets.iter().any(|t| t == "test" || t == "check") {
                Some(Plan {
                    command: "make test".to_string(),
                    note: None,
                })
            } else {
                Some(Plan {
                    command: "make".to_string(),
                    note: Some("no test target in Makefile".to_string()),
                })
            }
        }
        Intent::Clean => Some(Plan {
            command: "make clean".to_string(),
            note: None,
        }),
        _ => None,
    }
}

fn cargo_plan(raw: &RawScan, intent: Intent) -> Option<Plan> {
    let has_bin = raw
        .entries
        .iter()
        .any(|e| e.path == "src/main.rs" || e.path.starts_with("src/bin/"));
    match intent {
        Intent::Build => Some(Plan {
            command: "cargo build".to_string(),
            note: None,
        }),
        Intent::BuildAndRun | Intent::Run => {
            if has_bin {
                let cmd = if intent == Intent::BuildAndRun {
                    "cargo build && cargo run"
                } else {
                    "cargo run"
                };
                Some(Plan {
                    command: cmd.to_string(),
                    note: None,
                })
            } else {
                Some(Plan {
                    command: "cargo build".to_string(),
                    note: Some("library crate: no binary target, nothing to run".to_string()),
                })
            }
        }
        Intent::Test => Some(Plan {
            command: "cargo test".to_string(),
            note: None,
        }),
        Intent::Clean => Some(Plan {
            command: "cargo clean".to_string(),
            note: None,
        }),
        _ => None,
    }
}

fn npm_plan(intent: Intent, pm: &str) -> Option<Plan> {
    match intent {
        Intent::Build => Some(Plan {
            command: format!("{pm} run build"),
            note: None,
        }),
        Intent::BuildAndRun => Some(Plan {
            command: format!("{pm} run build && {pm} start"),
            note: None,
        }),
        Intent::Run => Some(Plan {
            command: format!("{pm} start"),
            note: None,
        }),
        Intent::Test => Some(Plan {
            command: format!("{pm} test"),
            note: None,
        }),
        Intent::Clean => Some(Plan {
            command: format!("{pm} run clean"),
            note: None,
        }),
        _ => None,
    }
}

fn python_plan(project: &crate::context::ProjectInfo, intent: Intent) -> Option<Plan> {
    match intent {
        Intent::Build => Some(Plan {
            command: "python3 -m compileall -q .".to_string(),
            note: None,
        }),
        Intent::Run | Intent::BuildAndRun => project.run_command.clone().map(|c| Plan {
            command: c,
            note: None,
        }),
        Intent::Test => Some(Plan {
            command: "python3 -m pytest".to_string(),
            note: None,
        }),
        Intent::Clean => Some(Plan {
            command: "find . -name '__pycache__' -type d -prune -exec rm -rf {} +".to_string(),
            note: None,
        }),
        _ => None,
    }
}

fn go_plan(intent: Intent) -> Option<Plan> {
    match intent {
        Intent::Build => Some(Plan {
            command: "go build ./...".to_string(),
            note: None,
        }),
        Intent::BuildAndRun => Some(Plan {
            command: "go build ./... && go run .".to_string(),
            note: None,
        }),
        Intent::Run => Some(Plan {
            command: "go run .".to_string(),
            note: None,
        }),
        Intent::Test => Some(Plan {
            command: "go test ./...".to_string(),
            note: None,
        }),
        Intent::Clean => Some(Plan {
            command: "go clean".to_string(),
            note: None,
        }),
        _ => None,
    }
}

/// No build system, but plain C++ sources exist: compile them directly.
fn fallback_cpp(raw: &RawScan, intent: Intent) -> Option<Plan> {
    let top_cpp = raw
        .entries
        .iter()
        .filter(|e| {
            e.depth == 0 && e.kind == FileKind::Source && e.name.to_lowercase().ends_with(".cpp")
        })
        .count();
    let any_cpp = raw
        .entries
        .iter()
        .any(|e| e.kind == FileKind::Source && e.name.to_lowercase().ends_with(".cpp"));
    if top_cpp == 0 && !any_cpp {
        return None;
    }
    let compiler = ["g++", "clang++", "c++"]
        .iter()
        .find(|c| env::on_path(c))
        .map(|s| s.to_string())?;
    let build = if top_cpp == 0 {
        format!("{compiler} $(find . -name '*.cpp' -not -path './build*/*' -not -path './.*') -o output")
    } else {
        format!("{compiler} *.cpp -o output")
    };
    match intent {
        Intent::Build | Intent::Test | Intent::Clean => Some(Plan {
            command: build,
            note: None,
        }),
        Intent::BuildAndRun | Intent::Run => Some(Plan {
            command: format!("{build} && ./output"),
            note: None,
        }),
        _ => None,
    }
}
