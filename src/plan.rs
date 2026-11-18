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
    if let Some(query) = crate::query::Query::parse(request) {
        return Some(match query.render(&raw.environment.cwd) {
            Ok(command) => Plan {
                command,
                note: None,
            },
            Err(reason) => Plan {
                command: String::new(),
                note: Some(reason),
            },
        });
    }
    let intent = intent::classify(request, raw);
    use crate::launch::{Request, Resolution};
    match crate::launch::resolve(raw, &Request::parse(request, raw)) {
        Resolution::Resolved(launch)
            if crate::constraints::shortcut_fits(raw, intent, request, &launch.command()) =>
        {
            return Some(Plan {
                command: launch.command(),
                note: launch.note,
            });
        }
        Resolution::NeedUserChoice(choices) => {
            return Some(Plan {
                command: String::new(),
                note: Some(crate::launch::choice_prompt(&choices)),
            })
        }
        Resolution::Unavailable(note) => {
            return Some(Plan {
                command: String::new(),
                note: Some(note),
            })
        }
        Resolution::Resolved(_) | Resolution::NeedEvidence => {}
    }
    if matches!(intent, Intent::Run | Intent::BuildAndRun)
        && requested_target_kind(request).is_some()
        && raw.project.as_ref().and_then(|p| p.build_system.as_deref()) != Some("CMake")
    {
        return None;
    }
    let plan = match intent {
        Intent::Build | Intent::Run | Intent::BuildAndRun | Intent::Test | Intent::Clean => {
            project_plan(raw, request, intent)
        }
        other => general_plan(raw, request, other),
    }?;
    if !plan.command.is_empty()
        && intent != Intent::NavigateDir
        && !crate::constraints::shortcut_fits(raw, intent, request, &plan.command)
    {
        return None;
    }
    Some(plan)
}

// ---------------------------------------------------------------------------
// General (non-project) intents
// ---------------------------------------------------------------------------

fn general_plan(raw: &RawScan, request: &str, intent: Intent) -> Option<Plan> {
    let lower = request.to_lowercase();
    let words: Vec<_> = lower
        .trim_end_matches(['?', '!', '.'])
        .split_whitespace()
        .collect();
    let count = words
        .iter()
        .any(|w| matches!(*w, "count" | "many" | "number"));
    let qualified = words.iter().any(|w| {
        matches!(
            *w,
            "only"
                | "except"
                | "excluding"
                | "under"
                | "within"
                | "matching"
                | "older"
                | "newer"
                | "named"
                | "from"
                | "since"
                | "by"
                | "containing"
                | "ending"
                | "starting"
                | "without"
        ) || w.contains('/')
    });
    if qualified && !matches!(intent, Intent::NavigateDir | Intent::GitLog) {
        return None;
    }
    if count && intent != Intent::InspectProcess {
        return None;
    }
    match intent {
        Intent::ListDir
            if words.iter().all(|w| {
                matches!(
                    *w,
                    "list"
                        | "ls"
                        | "all"
                        | "the"
                        | "files"
                        | "file"
                        | "in"
                        | "this"
                        | "directory"
                        | "folder"
                        | "here"
                        | "what"
                        | "is"
                        | "what's"
                        | "show"
                        | "me"
                        | "contents"
                        | "of"
                        | "current"
                        | "detailed"
                        | "listing"
                        | "a"
                )
            }) =>
        {
            Some(Plan {
                command: "ls -la".to_string(),
                note: None,
            })
        }
        Intent::SearchFiles => {
            let pat = intent::search_pattern(request)?;
            Some(Plan {
                command: format!(
                    "find . -type f -name '{}' -not -path '*/node_modules/*' -not -path '*/.git/*'",
                    pat.replace('\'', "")
                ),
                note: None,
            })
        }
        Intent::LargestFiles => {
            if !words.iter().all(|w| {
                matches!(
                    *w,
                    "which"
                        | "are"
                        | "the"
                        | "files"
                        | "take"
                        | "up"
                        | "what"
                        | "most"
                        | "space"
                        | "size"
                        | "by"
                        | "show"
                        | "me"
                        | "largest"
                        | "biggest"
                        | "list"
                        | "in"
                        | "this"
                        | "directory"
                        | "log"
                ) || w.parse::<u32>().is_ok()
            }) {
                return None;
            }
            let limit = words
                .iter()
                .find_map(|w| w.parse::<u32>().ok())
                .unwrap_or(20);
            let filter = if words.contains(&"log") {
                " -name '*.log'"
            } else {
                ""
            };
            Some(Plan {
                command: format!(
                    "find . -type f{filter} -printf '%s %p\\n' | sort -rn | head -n {limit}"
                ),
                note: None,
            })
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
            command: history_command(&words, &raw.environment.cwd)?,
            note: None,
        }),
        Intent::InspectPort => {
            let port = intent::port_number(request)?;
            let command = if env::on_path("ss") {
                format!("ss -lntup 'sport = :{port}'")
            } else if env::on_path("lsof") {
                format!("lsof -i :{port}")
            } else {
                return None;
            };
            Some(Plan {
                command,
                note: None,
            })
        }
        Intent::InspectProcess if env::on_path("ps") => Some(Plan {
            command: if count {
                "ps -e -o pid= | wc -l".into()
            } else {
                "ps aux --sort=-%cpu | head -15".into()
            },
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
fn history_command(words: &[&str], cwd: &Path) -> Option<String> {
    let mut qty = None;
    let mut file = None;
    let mut subjects = false;
    let mut singleton = false;
    let mut iter = words.iter().copied();
    while let Some(word) = iter.next() {
        let number = word.parse::<usize>().ok().or_else(|| {
            [
                "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            ]
            .iter()
            .position(|n| *n == word)
            .map(|n| n + 1)
        });
        if let Some(n) = number {
            if n == 0 || qty.replace(n).is_some() {
                return None;
            }
            continue;
        }
        match word {
            "commit" => singleton = true,
            "subjects" | "subject" => subjects = true,
            "newest" | "most" | "only" | "that" | "give" | "display" | "first" | "me," => (),
            "git" | "commits" | "log" | "history" | "recent" | "last" | "latest" | "the"
            | "briefly" | "show" | "me" | "list" | "please" | "oneline" => (),
            "touching" | "affecting" | "changed" | "for" => {
                let path = iter.next()?.trim_end_matches(',');
                if !cwd.join(path).is_file() || file.is_some() {
                    return None;
                }
                file = Some(path);
            }
            _ => return None,
        }
    }
    let format = if subjects { "--format=%s" } else { "--oneline" };
    let mut command = format!(
        "git log {format} -{}",
        qty.unwrap_or(if singleton { 1 } else { 10 })
    );
    if let Some(f) = file {
        command.push_str(" -- ");
        command.push_str(&shell_quote(f));
    }
    Some(command)
}

// Directory navigation: resolve the destination from filesystem evidence.
// A child process cannot change the parent shell's cwd, so the plan is a
// plain `cd <verified path>`; with the opt-in zsh hook (`gico init-zsh`) the
// approved cd is applied in the user's own shell.
// ---------------------------------------------------------------------------

fn navigate_plan(raw: &RawScan, request: &str) -> Option<Plan> {
    let (command, note) =
        match crate::context::registry::resolve_directory(request, &raw.environment.cwd) {
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
        Some("Cargo") => cargo_plan(raw, intent, request),
        Some(pm @ ("npm" | "pnpm" | "yarn" | "bun")) => npm_plan(raw, intent, pm),
        Some("pip") | None if project.kind == "Python" => python_plan(project, intent),
        Some("Go modules") if env::on_path("go") => go_plan(intent),
        _ => fallback_cpp(raw, intent),
    }
}

fn join(steps: &[String]) -> String {
    steps.join(" && ")
}

pub(crate) fn cmake_plan(raw: &RawScan, request: &str, intent: Intent) -> Option<Plan> {
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
    let names = request
        .split(|c: char| !c.is_alphanumeric() && !matches!(c, '-' | '_'))
        .collect::<Vec<_>>();
    let explicit: Vec<_> = eligible
        .iter()
        .copied()
        .filter(|t| names.iter().any(|n| t.name.eq_ignore_ascii_case(n)))
        .collect();
    let eligible = if explicit.is_empty() {
        eligible
    } else {
        explicit
    };
    if requested.is_some() && eligible.len() > 1 {
        return Some(Plan {
            command: String::new(),
            note: Some(format!(
                "Which {} target? {}",
                requested?.as_str(),
                eligible
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        });
    }
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

fn cargo_plan(_: &RawScan, intent: Intent, _: &str) -> Option<Plan> {
    match intent {
        Intent::Build => Some(Plan {
            command: "cargo build".into(),
            note: None,
        }),
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

fn npm_plan(raw: &RawScan, intent: Intent, pm: &str) -> Option<Plan> {
    let pkg: serde_json::Value = serde_json::from_str(
        &crate::context::fs::read_text(&raw.environment.cwd.join("package.json"), 65536).ok()?,
    )
    .ok()?;
    let scripts = pkg.get("scripts").and_then(|s| s.as_object());
    let has = |k: &str| scripts.is_some_and(|s| s.get(k).is_some_and(|v| v.is_string()));
    let launcher = if has("start") {
        Some(format!("{pm} start"))
    } else if has("dev") {
        Some(format!("{pm} run dev"))
    } else {
        None
    };
    let command = match intent {
        Intent::Build if has("build") => format!("{pm} run build"),
        Intent::Test if has("test") => format!("{pm} test"),
        Intent::Clean if has("clean") => format!("{pm} run clean"),
        Intent::Run => launcher?,
        Intent::BuildAndRun if has("build") => format!("{pm} run build && {}", launcher?),
        _ => return None,
    };
    Some(Plan {
        command,
        note: None,
    })
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
