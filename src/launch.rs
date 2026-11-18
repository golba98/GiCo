//! Evidence-backed launch capabilities for project adapters.
use crate::context::{
    cargo,
    fs::{inside, read_text, shell_quote},
    RawScan,
};
use crate::intent::{self, Intent};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Gui,
    Web,
    Cli,
    Service,
    Example,
    Unspecified,
}

#[derive(Debug)]
pub struct Request<'a> {
    pub action: Intent,
    pub role: Role,
    pub profile: Option<&'a str>,
    pub arguments: Vec<String>,
    pub named_profile: Option<String>,
    text: &'a str,
}

impl<'a> Request<'a> {
    pub fn parse(text: &'a str, raw: &RawScan) -> Self {
        let base = text
            .split_once("\nClarification: ")
            .map_or(text, |(base, _)| base);
        let (request, arguments) = base.split_once(" -- ").unwrap_or((base, ""));
        let lower = request.to_lowercase();
        let role = if intent::wants_gui(request) || lower.contains("desktop app") {
            Role::Gui
        } else if ["web app", "web preview", "frontend"]
            .iter()
            .any(|w| lower.contains(w))
        {
            Role::Web
        } else if intent::wants_service(request) {
            Role::Service
        } else if lower.contains("command line")
            || lower.contains("command-line")
            || lower.split_whitespace().any(|w| w == "cli")
        {
            Role::Cli
        } else if lower.split_whitespace().any(|w| w == "example") {
            Role::Example
        } else {
            Role::Unspecified
        };
        let profile = lower
            .split(|c: char| !c.is_alphanumeric())
            .find_map(|w| match w {
                "release" => Some("release"),
                "debug" | "dev" => Some("dev"),
                _ => None,
            });
        let named_profile = shlex::split(request).and_then(|words| {
            words
                .windows(2)
                .find(|pair| pair[0].eq_ignore_ascii_case("profile"))
                .map(|pair| pair[1].clone())
        });
        Self {
            action: intent::classify(request, raw),
            role,
            profile,
            named_profile,
            arguments: shlex::split(arguments).unwrap_or_default(),
            text,
        }
    }
    pub(super) fn names(&self, name: &str) -> bool {
        self.text
            .split(|c: char| !c.is_alphanumeric() && !matches!(c, '-' | '_'))
            .any(|w| w.eq_ignore_ascii_case(name))
    }
}

#[derive(Clone, Debug)]
pub struct Launch {
    pub label: String,
    pub role: Role,
    pub argv: Vec<String>,
    pub build: Option<Vec<String>>,
    pub evidence: Vec<PathBuf>,
    pub executable: Option<String>,
    pub shell: Option<String>,
    pub note: Option<String>,
}

impl Launch {
    pub fn command(&self) -> String {
        if let Some(shell) = &self.shell {
            return shell.clone();
        }
        let render = |argv: &[String]| {
            argv.iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        };
        match &self.build {
            Some(build) => format!("{} && {}", render(build), render(&self.argv)),
            None => render(&self.argv),
        }
    }
}

#[derive(Debug)]
pub enum Resolution {
    Resolved(Launch),
    NeedEvidence,
    NeedUserChoice(Vec<Launch>),
    Unavailable(String),
}

pub(super) fn gui_source(s: &str) -> bool {
    [
        "eframe::run_native(",
        "gtk::Application",
        "winit::event_loop",
        "iced::application(",
        "tkinter",
        "QApplication",
        "javax.swing",
        "javafx.application",
    ]
    .iter()
    .any(|m| s.contains(m))
}

fn documented_gui(
    root: &Path,
    bin: &cargo::Binary,
    source: &str,
) -> Option<(Vec<String>, PathBuf, bool)> {
    // Documentation supplies arguments, while the entrypoint independently
    // establishes that the requested subcommand exists.
    for file in [
        root.join("README.md"),
        root.join(bin.manifest.parent()?).join("README.md"),
    ] {
        if !inside(root, &file) {
            continue;
        }
        let Ok(readme) = read_text(&file, 65_536) else {
            continue;
        };
        for line in readme.lines() {
            let Some(words) = shlex::split(line.trim().trim_matches('`').trim_start_matches("$ "))
            else {
                continue;
            };
            if !words.starts_with(&["cargo".into(), "run".into()]) {
                continue;
            }
            let Some(separator) = words.iter().position(|w| w == "--") else {
                continue;
            };
            if !words[..separator]
                .windows(2)
                .any(|w| w[0] == "--bin" && w[1] == bin.name)
            {
                continue;
            }
            let Some(subcommand) = words.get(separator + 1) else {
                continue;
            };
            let variant: String = subcommand
                .split('-')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let mut letters = p.chars();
                    letters
                        .next()
                        .into_iter()
                        .flat_map(char::to_uppercase)
                        .chain(letters)
                        .collect::<String>()
                })
                .collect();
            if variant.is_empty() || !source.contains(&format!("::{variant}(")) {
                continue;
            }
            let described_gui = source.lines().collect::<Vec<_>>().windows(2).any(|pair| {
                pair[0].trim_start().starts_with("///")
                    && ["graphical", "gui", "window"]
                        .iter()
                        .any(|w| pair[0].to_ascii_lowercase().contains(w))
                    && pair[1].trim_start().starts_with(&format!("{variant}("))
            });
            if described_gui || (subcommand == "gui" && source.contains("::gui::")) {
                return Some((
                    vec![subcommand.clone()],
                    file,
                    words[..separator].iter().any(|w| w == "--release"),
                ));
            }
        }
    }
    None
}

fn cargo_capabilities(raw: &RawScan, request: &Request<'_>) -> Vec<Launch> {
    let root = &raw.environment.cwd;
    let bins = cargo::binaries(raw);
    let named = bins
        .iter()
        .any(|b| request.names(&b.name) || request.names(&b.package));
    let defaults = cargo::manifest(&root.join("Cargo.toml"))
        .and_then(|m| {
            Some(
                m.get("workspace")?
                    .get("default-members")?
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    let select_default = !named
        && request.role == Role::Unspecified
        && bins.iter().any(|b| {
            b.default_run
                && (b.manifest == Path::new("Cargo.toml")
                    || defaults.iter().any(|d| b.manifest.starts_with(d)))
        });
    bins.iter()
        .filter(|b| {
            (!named || request.names(&b.name) || request.names(&b.package))
                && (!b.example || named || request.role == Role::Example)
                && (!select_default || b.default_run)
        })
        .filter_map(|bin| {
            let source = read_text(&root.join(&bin.path), 131_072).ok()?;
            let gui = documented_gui(root, bin, &source);
            let direct_gui = gui_source(&source);
            let role = if bin.example {
                Role::Example
            } else if direct_gui || gui.is_some() {
                Role::Gui
            } else if source.contains("TcpListener::bind") {
                Role::Service
            } else {
                Role::Cli
            };
            if request.role != Role::Unspecified && request.role != role {
                return None;
            }
            let mut flags = Vec::new();
            let release = request.profile == Some("release")
                || request.profile.is_none()
                    && request.role == Role::Gui
                    && gui.as_ref().is_some_and(|(_, _, r)| *r);
            if let Some(profile) = &request.named_profile {
                let supported = matches!(profile.as_str(), "release" | "dev")
                    || cargo::manifest(&root.join("Cargo.toml"))
                        .and_then(|m| m.get("profile")?.get(profile).cloned())
                        .is_some();
                if !supported {
                    return None;
                }
                flags.extend(["--profile".into(), profile.clone()]);
            } else if release {
                flags.push("--release".into());
            }
            if bin.manifest != Path::new("Cargo.toml")
                && (request.role != Role::Gui
                    || gui.is_none()
                    || bins
                        .iter()
                        .filter(|b| b.name == bin.name && !b.example)
                        .count()
                        > 1)
            {
                flags.extend(["-p".into(), bin.package.clone()]);
            }
            if bin.example {
                flags.extend(["--example".into(), bin.name.clone()]);
            } else if bins.iter().filter(|b| !b.example).count() != 1
                || bin.manifest != Path::new("Cargo.toml")
                || !bin.required_features.is_empty()
            {
                flags.extend(["--bin".into(), bin.name.clone()]);
            }
            if !bin.required_features.is_empty() {
                flags.extend(["--features".into(), bin.required_features.join(",")]);
            }
            let build =
                (request.action == Intent::BuildAndRun).then(|| prefixed("cargo", "build", &flags));
            let mut argv = prefixed("cargo", "run", &flags);
            let mut evidence = vec![
                root.join("Cargo.toml"),
                root.join(&bin.manifest),
                root.join(&bin.path),
            ];
            let mut args = Vec::new();
            if let Some((subcommand, file, _)) = gui {
                args.extend(subcommand);
                evidence.push(file);
            }
            args.extend(request.arguments.clone());
            if !args.is_empty() {
                argv.push("--".into());
                argv.extend(args);
            }
            Some(Launch {
                label: format!("{}/{}", bin.package, bin.name),
                role,
                argv,
                build,
                evidence,
                executable: Some(bin.name.clone()),
                shell: None,
                note: None,
            })
        })
        .collect()
}

fn prefixed(program: &str, action: &str, args: &[String]) -> Vec<String> {
    [program.to_owned(), action.to_owned()]
        .into_iter()
        .chain(args.iter().cloned())
        .collect()
}

pub fn resolve(raw: &RawScan, request: &Request<'_>) -> Resolution {
    if crate::query::Query::parse(request.text).is_some()
        || !matches!(request.action, Intent::Run | Intent::BuildAndRun)
    {
        return Resolution::NeedEvidence;
    }
    if request.text.contains(" -- ")
        && shlex::split(request.text.split_once(" -- ").unwrap().1).is_none()
    {
        return Resolution::Unavailable("Runtime arguments contain an unfinished quote.".into());
    }
    if matches!(request.role, Role::Service | Role::Web) {
        if let Some(port) = intent::port_number(request.text) {
            let port = port.to_string();
            if !request
                .arguments
                .iter()
                .any(|arg| arg == &port || arg.ends_with(&format!("={port}")))
            {
                return Resolution::Unavailable(
                    "The project does not establish how to use that port. Supply the application's port option after --.".into());
            }
        }
    }
    let system = raw.project.as_ref().and_then(|p| p.build_system.as_deref());
    if system == Some("CMake") {
        return match crate::plan::cmake_plan(raw, request.text, request.action) {
            Some(plan) if plan.command.is_empty() => Resolution::Unavailable(
                plan.note
                    .unwrap_or_else(|| "No verified CMake launch procedure.".into()),
            ),
            Some(plan) => {
                let argv = crate::verify::literal_words(&plan.command).unwrap_or_default();
                let executable = argv
                    .first()
                    .and_then(|w| Path::new(w).file_name()?.to_str().map(str::to_owned));
                Resolution::Resolved(Launch {
                    label: "CMake target".into(),
                    role: request.role,
                    argv,
                    build: None,
                    evidence: vec![raw.environment.cwd.join("CMakeLists.txt")],
                    executable,
                    shell: Some(plan.command),
                    note: plan.note,
                })
            }
            None => Resolution::NeedEvidence,
        };
    }
    let mut found = if system == Some("Cargo") {
        cargo_capabilities(raw, request)
    } else if let Some(choices) = crate::launch_adapters::capabilities(raw, request, system) {
        choices
    } else {
        return Resolution::NeedEvidence;
    };
    if found.len() == 1
        && !crate::context::env::on_path(&found[0].argv[0])
        && !found[0].argv[0].contains('/')
    {
        return Resolution::Unavailable(format!(
            "Required tool is unavailable: {}.",
            found[0].argv[0]
        ));
    }
    if let Some((_, answer)) = request.text.split_once("\nClarification: ") {
        if let Ok(number) = answer.trim().parse::<usize>() {
            if (1..=found.len().min(3)).contains(&number) {
                return Resolution::Resolved(found.remove(number - 1));
            }
        }
    }
    match found.len() {
        0 => Resolution::Unavailable(match request.role {
            Role::Unspecified => {
                "No verified runnable project entrypoint matches this request.".into()
            }
            role => format!(
                "No verified {} launch procedure was found.",
                role_name(role)
            ),
        }),
        1 => Resolution::Resolved(found.remove(0)),
        _ => Resolution::NeedUserChoice(found),
    }
}

pub(crate) fn semantic_options(raw: &RawScan, text: &str) -> Vec<Launch> {
    let mut request = Request::parse(text, raw);
    if request.action != Intent::Ambiguous || crate::query::Query::parse(text).is_some() {
        return Vec::new();
    }
    request.action = Intent::Run;
    let options = match resolve(raw, &request) {
        Resolution::Resolved(target) => vec![target],
        Resolution::NeedUserChoice(options) => options,
        _ => Vec::new(),
    };
    options
        .into_iter()
        .filter(|target| {
            crate::constraints::shortcut_fits(raw, Intent::Run, text, &target.command())
                && (target.shell.is_some()
                    || target
                        .argv
                        .first()
                        .is_some_and(|program| crate::context::env::on_path(program))
                    || target.argv.first().is_some_and(|program| {
                        program.starts_with("./") && raw.environment.cwd.join(program).is_file()
                    }))
        })
        .take(6)
        .collect()
}

pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Gui => "GUI",
        Role::Web => "web preview",
        Role::Cli => "CLI",
        Role::Service => "service",
        Role::Example => "example",
        Role::Unspecified => "application",
    }
}

pub(crate) fn choice_prompt(choices: &[Launch]) -> String {
    format!(
        "Which target? {}",
        choices
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, launch)| format!("{} {} ({})", i + 1, launch.label, role_name(launch.role)))
            .collect::<Vec<_>>()
            .join(" · ")
    )
}
