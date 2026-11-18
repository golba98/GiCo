use super::launch::{Launch, Request, Role};
use crate::context::{
    fs::{inside, read_text},
    RawScan,
};
use crate::intent::Intent;
use std::path::{Path, PathBuf};

fn json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&read_text(path, 64 * 1024).ok()?).ok()
}

fn recipe(
    label: &str,
    role: Role,
    argv: &[&str],
    build: Option<&[&str]>,
    evidence: Vec<PathBuf>,
) -> Launch {
    let owned = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect();
    Launch {
        label: label.into(),
        role,
        argv: owned(argv),
        build: build.map(owned),
        evidence,
        executable: None,
        shell: None,
        note: None,
    }
}

fn js(root: &Path, raw: &RawScan, pm: &str) -> Vec<Launch> {
    let mut options = Vec::new();
    let root_manifest = root.join("package.json");
    let Some(top) = json(&root_manifest) else {
        return options;
    };
    let workspaces = top["workspaces"]
        .as_array()
        .or_else(|| top["workspaces"]["packages"].as_array());
    let mut manifests = vec![root_manifest.clone()];
    if let Some(patterns) = workspaces {
        manifests.extend(
            raw.entries
                .iter()
                .filter(|e| e.name == "package.json" && e.depth > 0)
                .take(32)
                .filter(|e| {
                    patterns.iter().filter_map(|v| v.as_str()).any(|pattern| {
                        let member = Path::new(&e.path).parent().unwrap_or(Path::new(""));
                        match pattern.strip_suffix("/*") {
                            Some(prefix) => member.parent() == Some(Path::new(prefix)),
                            None => member == Path::new(pattern),
                        }
                    })
                })
                .map(|e| root.join(&e.path)),
        );
    }
    for path in manifests {
        let Some(doc) = json(&path) else { continue };
        let Some(scripts) = doc["scripts"].as_object() else {
            continue;
        };
        let name = doc["name"].as_str().unwrap_or("package");
        for (script, value) in scripts {
            let Some(text) = value.as_str() else { continue };
            let words = shlex::split(text).unwrap_or_default();
            let contains = |tool| words.iter().any(|w| w == tool);
            let entrypoint = words
                .windows(2)
                .find(|args| matches!(args[0].as_str(), "node" | "tsx"))
                .map(|args| path.parent().unwrap_or(root).join(&args[1]))
                .filter(|entry| inside(root, entry));
            let listens = entrypoint.as_ref().is_some_and(|entry| {
                read_text(entry, 65536).is_ok_and(|source| {
                    source.contains(".listen(") || source.contains("Deno.serve(")
                })
            });
            let role = if contains("electron") || contains("tauri") && contains("dev") {
                Role::Gui
            } else if (contains("vite") || contains("next") || contains("astro"))
                && !contains("build")
            {
                Role::Web
            } else if listens || contains("http-server") {
                Role::Service
            } else if matches!(script.as_str(), "start" | "serve" | "server" | "dev") {
                Role::Unspecified
            } else {
                continue;
            };
            let mut evidence = vec![root_manifest.clone(), path.clone()];
            evidence.extend(entrypoint);
            let mut base = vec![pm];
            if path != root_manifest {
                if pm != "npm" {
                    continue;
                }
                base.extend(["--workspace", name]);
            }
            let mut run = base.clone();
            if !matches!(script.as_str(), "start") {
                run.push("run");
            }
            run.push(script);
            base.extend(["run", "build"]);
            options.push(recipe(
                &format!("{name}/{script}"),
                role,
                &run,
                scripts.contains_key("build").then_some(&base),
                evidence,
            ));
        }
    }
    options
}

fn python(root: &Path, raw: &RawScan) -> Vec<Launch> {
    let mut result = Vec::new();
    let manifest = root.join("pyproject.toml");
    if let Some(config) = crate::context::cargo::manifest(&manifest) {
        for (table, role) in [("gui-scripts", Role::Gui), ("scripts", Role::Cli)] {
            if let Some(entries) = config
                .get("project")
                .and_then(|v| v.get(table))
                .and_then(toml::Value::as_table)
            {
                for (name, callable) in entries {
                    let relative = format!("./.venv/bin/{name}");
                    let path = root.join(&relative);
                    if callable.as_str().is_some_and(|s| s.contains(':'))
                        && path.is_file()
                        && !name.contains('/')
                        && inside(root, &path)
                    {
                        result.push(recipe(
                            name,
                            role,
                            &[&relative],
                            None,
                            vec![manifest.clone(), path],
                        ));
                    }
                }
            }
        }
    }
    if !result.is_empty() {
        return result;
    }
    let words = raw
        .project
        .as_ref()
        .and_then(|project| project.run_command.as_deref())
        .and_then(shlex::split);
    if let Some(words) = words.filter(|w| w.len() == 2 && w[0] == "python3") {
        let path = root.join(&words[1]);
        if let Some(content) = inside(root, &path)
            .then(|| read_text(&path, 64 * 1024).ok())
            .flatten()
        {
            let role = if super::launch::gui_source(&content) {
                Role::Gui
            } else {
                Role::Cli
            };
            let venv = root.join(".venv/bin/python");
            let python = if venv.is_file() {
                "./.venv/bin/python"
            } else {
                "python3"
            };
            result.push(recipe(
                &words[1],
                role,
                &[python, &words[1]],
                None,
                vec![path],
            ));
        }
    }
    result
}

fn go(root: &Path, raw: &RawScan) -> Vec<Launch> {
    let mut result: Vec<Launch> = Vec::new();
    for file in raw
        .entries
        .iter()
        .filter(|e| e.name.ends_with(".go") && !e.name.ends_with("_test.go"))
        .take(64)
    {
        let path = root.join(&file.path);
        let Ok(s) = read_text(&path, 64 * 1024) else {
            continue;
        };
        if !s.lines().any(|line| line.trim() == "package main") || !s.contains("func main(") {
            continue;
        }
        let Some(dir) = Path::new(&file.path).parent() else {
            continue;
        };
        let arg = if dir.as_os_str().is_empty() {
            ".".into()
        } else {
            format!("./{}", dir.display())
        };
        if result.iter().any(|v| v.label == arg) {
            continue;
        }
        let role = if s.contains("ListenAndServe(") {
            Role::Service
        } else {
            Role::Cli
        };
        result.push(recipe(
            &arg,
            role,
            &["go", "run", &arg],
            Some(&["go", "build", &arg]),
            vec![path, root.join("go.mod")],
        ));
    }
    result
}

fn dotnet(root: &Path, raw: &RawScan, request: &Request<'_>) -> Vec<Launch> {
    raw.entries
        .iter()
        .filter(|e| e.name.ends_with(".csproj"))
        .take(32)
        .filter_map(|file| {
            let path = root.join(&file.path);
            let value = read_text(&path, 65536).ok()?;
            let role = if value.contains("<UseWPF>true") || value.contains("<UseWindowsForms>true")
            {
                return None;
            } else if value.contains("Microsoft.NET.Sdk.Web") {
                Role::Service
            } else if value.contains("<OutputType>Exe</OutputType>") {
                Role::Cli
            } else {
                return None;
            };
            let settings = path.parent()?.join("Properties/launchSettings.json");
            let mut argv = vec!["dotnet", "run", "--project", &file.path];
            let mut evidence = vec![path];
            if let Some(profile) = &request.named_profile {
                let profiles = json(&settings)?;
                if profiles["profiles"][profile]["commandName"] != "Project" {
                    return None;
                }
                argv.extend(["--launch-profile", profile]);
                evidence.push(settings);
            } else if settings.is_file() {
                // Bind the configuration consumed automatically by the SDK to approval.
                evidence.push(settings);
            }
            Some(recipe(
                &file.name,
                role,
                &argv,
                Some(&["dotnet", "build", &file.path]),
                evidence,
            ))
        })
        .collect()
}

fn java(root: &Path, raw: &RawScan) -> Vec<Launch> {
    let mut wrappers = Vec::new();
    for (file, tool, application, goal) in [
        (
            "pom.xml",
            "./mvnw",
            "spring-boot-maven-plugin",
            "spring-boot:run",
        ),
        ("build.gradle", "./gradlew", "application", "run"),
        ("build.gradle.kts", "./gradlew", "application", "run"),
    ] {
        let path = root.join(file);
        let runner = root.join(tool);
        if runner.is_file() && read_text(&path, 65536).is_ok_and(|text| text.contains(application))
        {
            let role = if file == "pom.xml" {
                Role::Service
            } else {
                Role::Cli
            };
            wrappers.push(recipe(file, role, &[tool, goal], None, vec![path, runner]));
        }
    }
    if !wrappers.is_empty() {
        return wrappers;
    }
    raw.entries
        .iter()
        .filter(|e| e.name.ends_with(".java"))
        .take(32)
        .filter_map(|file| {
            let path = root.join(&file.path);
            let text = read_text(&path, 65536).ok()?;
            if !text.contains("static void main(") {
                return None;
            }
            let role = if super::launch::gui_source(&text) {
                Role::Gui
            } else {
                Role::Cli
            };
            Some(recipe(
                &file.name,
                role,
                &["java", &file.path],
                None,
                vec![path],
            ))
        })
        .collect()
}

fn task(root: &Path, file: &str, tool: &str) -> Vec<Launch> {
    let path = root.join(file);
    let Ok(text) = read_text(&path, 65536) else {
        return vec![];
    };
    let defines = |name: &str| {
        text.lines().any(|line| {
            line.strip_prefix(name)
                .is_some_and(|tail| tail.trim_start().starts_with(':'))
        })
    };
    if defines("run") {
        let build = [tool, "build"];
        let default_build = [tool];
        let build: Option<&[&str]> = if defines("build") {
            Some(&build)
        } else if tool == "make" && defines("all") {
            Some(&default_build)
        } else {
            None
        };
        vec![recipe(
            "run",
            Role::Unspecified,
            &[tool, "run"],
            build,
            vec![path],
        )]
    } else {
        vec![]
    }
}

pub(super) fn capabilities(
    raw: &RawScan,
    request: &Request<'_>,
    system: Option<&str>,
) -> Option<Vec<Launch>> {
    let root = &raw.environment.cwd;
    let mut found = match system {
        Some(pm @ ("npm" | "pnpm" | "yarn" | "bun")) => js(root, raw, pm),
        Some("Go modules") => go(root, raw),
        Some("pip") => python(root, raw),
        Some("Make") => {
            let tasks = task(root, "Makefile", "make");
            if tasks.is_empty() {
                return None;
            }
            tasks
        }
        _ if root.join("go.mod").is_file() || root.join("go.work").is_file() => go(root, raw),
        _ if raw.entries.iter().any(|e| e.name.ends_with(".csproj")) => dotnet(root, raw, request),
        _ if raw.entries.iter().any(|e| e.name.ends_with(".java")) => java(root, raw),
        _ if raw.project.as_ref().is_some_and(|p| p.kind == "Python") => python(root, raw),
        _ if root.join("Justfile").is_file() => task(root, "Justfile", "just"),
        _ if root.join("justfile").is_file() => task(root, "justfile", "just"),
        _ => return None,
    };
    // Facets come from project entrypoints or manifests. Filtering by a role
    // cannot substitute another role when discovery lacks evidence.
    if request.role != Role::Unspecified {
        found.retain(|f| f.role == request.role);
    }
    if found
        .iter()
        .any(|f| f.label.split('/').any(|part| request.names(part)))
    {
        found.retain(|f| f.label.split('/').any(|part| request.names(part)));
    }
    if request.action == Intent::BuildAndRun {
        found.retain(|f| f.build.is_some());
    }
    for item in &mut found {
        if request.action != Intent::BuildAndRun {
            item.build = None;
        }
        if !request.arguments.is_empty() {
            if matches!(item.argv[0].as_str(), "npm" | "pnpm" | "yarn" | "dotnet") {
                item.argv.push("--".into());
            }
            item.argv.extend(request.arguments.clone());
        }
    }
    Some(found)
}
