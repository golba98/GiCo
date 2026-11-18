//! Private project maps. File identity distinguishes worktrees and survives a rename on the same filesystem.
use super::{cmake, fs, RawScan};
use crate::{config, intent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "CMakeLists.txt",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "go.work",
    "Justfile",
    "justfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Makefile",
    ".git",
];
fn data() -> io::Result<PathBuf> {
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(config::home()?.join(".local/share"))
        .join("gico"))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn identity(p: &Path) -> io::Result<String> {
    let m = p.metadata()?;
    Ok(format!("{:x}-{:x}", m.dev(), m.ino()))
}
fn manifest(p: &Path) -> bool {
    MANIFESTS.iter().any(|name| {
        p.join(name)
            .symlink_metadata()
            .is_ok_and(|m| m.is_file() || *name == ".git" && m.is_dir())
    })
}
fn read<T: serde::de::DeserializeOwned>(p: &Path) -> io::Result<T> {
    serde_json::from_str(&fs::read_text(p, 512 * 1024)?).map_err(io::Error::other)
}
fn write<T: Serialize>(p: &Path, v: &T) -> io::Result<()> {
    config::private_write(p, &serde_json::to_vec_pretty(v)?)
}
fn lock(dir: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(".lock"))?;
    f.lock()?;
    Ok(f)
}

#[derive(Default, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default)]
    pub aliases: Vec<String>,
    pub purpose: Option<String>,
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
    #[serde(flatten)]
    additional: BTreeMap<String, Value>,
}

type Locations = BTreeMap<String, PathBuf>;

fn signature(raw: &RawScan) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hash = DefaultHasher::new();
    let root = &raw.environment.cwd;
    root.hash(&mut hash);
    raw.environment.compilers.hash(&mut hash);
    raw.environment.package_managers.hash(&mut hash);
    raw.environment.utilities.hash(&mut hash);
    let manifests = raw.entries.iter().map(|e| root.join(&e.path));
    let caches = raw
        .cmake_build_dirs
        .iter()
        .map(|d| root.join(d).join("CMakeCache.txt"));
    let mut artifacts = Vec::new();
    if !raw.cmake_build_dirs.is_empty() {
        let targets = cmake::target_kinds(root);
        for tree in cmake::discover_build_trees(root, &raw.cmake_build_dirs)
            .into_iter()
            .filter(|tree| tree.valid)
            .take(4)
        {
            for target in targets
                .iter()
                .filter(|t| t.kind != cmake::TargetKind::Library)
                .take(8)
            {
                if let Some(rel) =
                    cmake::find_built_executable(root, &tree.dir, &target.artifact_name)
                {
                    artifacts.push(root.join(rel));
                }
            }
        }
    }
    for path in manifests.chain(caches).chain(artifacts) {
        path.hash(&mut hash);
        if let Ok(m) = path.symlink_metadata() {
            (m.ino(), m.len(), m.ctime(), m.ctime_nsec()).hash(&mut hash);
        }
    }
    format!("{:x}", hash.finish())
}

fn checked_record(path: &Path, signature: &str) -> Option<Value> {
    let record: Value = read(path).ok()?;
    (record["version"] == 2
        && record["signature"].as_str() == Some(signature)
        && record["facts"].is_object())
    .then_some(record)
}

pub fn observe(raw: &RawScan) -> io::Result<()> {
    let root = &raw.environment.cwd;
    if !manifest(root) {
        return Ok(());
    }
    let dir = data()?;
    let _lock = lock(&dir)?;
    let id = identity(root)?;
    let index = dir.join("locations.json");
    let mut roots: Locations = read(&index).unwrap_or_default();
    if roots.get(&id) != Some(root) {
        roots.insert(id.clone(), root.clone());
        write(&index, &roots)?;
    }
    let record = dir.join("projects").join(&id).join("context.json");
    let signature = signature(raw);
    if checked_record(&record, &signature).is_some() {
        return Ok(());
    }
    write(
        &record,
        &json!({"version":2,"observed_at":now(),"root":root,"signature":signature,"facts":facts(raw)}),
    )
}

fn facts(raw: &RawScan) -> Value {
    let root = &raw.environment.cwd;
    let sources:Vec<_>=raw.entries.iter().filter(|e|e.depth<3 && (MANIFESTS.contains(&e.name.as_str()) ||
        matches!(e.name.as_str(),"main.rs"|"main.py"|"__main__.py"|"main.cpp"|"index.ts"|"index.js"|"README.md")))
        .take(32).map(|e|json!({"path":e.path,"role":if MANIFESTS.contains(&e.name.as_str()) {"manifest"} else if e.name=="README.md" {"documentation"} else {"entrypoint candidate"},
            "mtime":root.join(&e.path).metadata().ok().map(|m|m.mtime()),"bytes":e.size})).collect();
    let purpose = project_purpose(root);
    let mut entries = Vec::new();
    if let Ok(text) = fs::read_text(&root.join("package.json"), 65536) {
        if let Ok(pkg) = serde_json::from_str::<Value>(&text) {
            for key in ["main", "module", "bin"] {
                let paths: Vec<_> = match &pkg[key] {
                    Value::String(p) => vec![p.as_str()],
                    Value::Object(bins) => bins.values().filter_map(Value::as_str).collect(),
                    _ => vec![],
                };
                for p in paths {
                    if raw
                        .entries
                        .iter()
                        .any(|e| e.path == p.trim_start_matches("./"))
                    {
                        entries.push(json!({"path":p,"source":format!("package.json#{key}")}));
                    }
                }
            }
        }
    }
    if raw.project.as_ref().and_then(|p| p.build_system.as_deref()) == Some("Cargo") {
        entries.extend(
            super::cargo::binaries(raw)
                .into_iter()
                .map(|b| json!({"path":b.path,"name":b.name,"source":b.manifest})),
        );
    }
    if entries.is_empty() {
        entries.extend(
            sources
                .iter()
                .filter(|s| s["role"] == "entrypoint candidate")
                .map(|s| json!({"path":s["path"],"source":"source layout"})),
        );
    }
    let targets = if root.join("CMakeLists.txt").is_file() {
        cmake::target_kinds(root)
    } else {
        vec![]
    };
    let mut recipes = BTreeMap::new();
    if let Some(p) = &raw.project {
        for (action, request) in [
            ("build", "build this project"),
            ("test", "run the tests"),
            ("run", "run this project"),
            ("gui", "launch the GUI"),
            ("service", "start the service"),
            ("web", "open the web preview"),
        ] {
            if let Some(plan) = crate::plan::plan(raw, request).filter(|p| !p.command.is_empty()) {
                recipes.insert(action,json!({"command":plan.command,"cwd":".","evidence":p.config_files,"note":plan.note}));
            }
        }
    }
    json!({"purpose":purpose.map(|s|s.chars().take(200).collect::<String>()),"evidence":sources,"project":raw.project,
        "entrypoints":entries,"targets":targets,"recipes":recipes,"working_directory":".","scan_status":{"entries":raw.entries.len(),"discovered":raw.total_discovered},
        "prerequisites":{"build_system":raw.project.as_ref().and_then(|p|p.build_system.as_deref()),"gui_requires_display":targets.iter().any(|t|t.kind==cmake::TargetKind::Gui)},
        "build_trees":cmake::discover_build_trees(root,&raw.cmake_build_dirs).iter().map(|t|json!({"dir":t.dir,"valid":t.valid})).collect::<Vec<_>>()})
}

pub fn evidence(raw: &RawScan) -> Value {
    let root = &raw.environment.cwd;
    if !manifest(root) {
        return Value::Null;
    }
    let prefs = (|| -> io::Result<Preferences> {
        read(
            &data()?
                .join("projects")
                .join(identity(root)?)
                .join("preferences.json"),
        )
    })();
    let stored = data().ok().zip(identity(root).ok()).and_then(|(d, id)| {
        checked_record(
            &d.join("projects").join(id).join("context.json"),
            &signature(raw),
        )
    });
    let facts = stored
        .map(|mut record| record["facts"].take())
        .unwrap_or_else(|| facts(raw));
    let preferences = prefs
        .ok()
        .map(|p| json!({"aliases":p.aliases,"roles":p.roles,"purpose":p.purpose}));
    json!({"observations":facts,"preferences":preferences})
}

pub fn alias(root: &Path, alias: &str) -> io::Result<()> {
    if alias.trim().is_empty()
        || alias.len() > 80
        || alias.chars().any(char::is_control)
        || !manifest(root)
    {
        return Err(io::Error::other("supply an alias in a project root"));
    }
    let dir = data()?;
    let _lock = lock(&dir)?;
    let path = dir
        .join("projects")
        .join(identity(root)?)
        .join("preferences.json");
    let mut prefs: Preferences = match read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Preferences::default(),
        Err(e) => return Err(e),
    };
    if !prefs.aliases.iter().any(|s| s.eq_ignore_ascii_case(alias)) {
        prefs.aliases.push(alias.to_owned());
    }
    write(&path, &prefs)
}

fn session_path() -> Option<PathBuf> {
    let term = std::env::var("GICO_TERMINAL").ok()?;
    let session = std::env::var("GICO_SESSION").ok();
    let name = session.as_deref().or_else(|| term.strip_prefix("/dev/"))?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-'))
    {
        return None;
    }
    Some(
        data()
            .ok()?
            .join("sessions")
            .join(format!("{}.json", name.replace('/', "-"))),
    )
}
pub fn remember(root: &Path) {
    let Some(path) = session_path() else { return };
    let Ok(id) = identity(root) else { return };
    let Ok(_lock) = lock(path.parent().unwrap()) else {
        return;
    };
    let mut recent: Vec<(u64, String, PathBuf)> = read(&path).unwrap_or_default();
    recent.retain(|(time, old, _)| now().saturating_sub(*time) < 86400 && old != &id);
    recent.insert(0, (now(), id, root.to_owned()));
    recent.truncate(8);
    let _ = write(&path, &recent);
}

fn contains_name(request: &str, name: &str) -> bool {
    if name.trim().len() < 2 {
        return false;
    }
    let request = format!(
        " {} ",
        request
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let name = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    !name.is_empty() && request.contains(&format!(" {name} "))
}

/// A bounded shared inventory for both project tasks and directory navigation.
fn workspace_places(cwd: &Path) -> (Vec<PathBuf>, bool) {
    let mut roots = vec![cwd.to_owned()];
    if let Ok(home) = config::home() {
        roots.push(home.join("Development"));
        // Only immediate everyday folders from HOME belong to the inventory.
        roots.extend(
            home.read_dir()
                .into_iter()
                .flatten()
                .take(128)
                .flatten()
                .filter(|e| {
                    e.file_type().is_ok_and(|m| m.is_dir())
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .map(|e| e.path())
                .filter(|p| !p.starts_with(home.join("Development")))
                .take(32),
        );
    }
    if let Ok(path) = config::path() {
        if let Ok(config) = config::Config::load(&path) {
            roots.extend(
                config
                    .workspace_roots
                    .into_iter()
                    .filter(|p| p.is_absolute())
                    .take(12),
            );
        }
    }
    let mut pending = std::collections::VecDeque::new();
    for root in roots {
        if let Ok(root) = root.canonicalize() {
            let depth = if root == cwd
                || root.ends_with("Development")
                || !config::home().is_ok_and(|home| root.parent() == Some(&home))
            {
                0
            } else {
                3
            };
            pending.push_back((root, depth));
        }
    }
    let mut visited = std::collections::BTreeSet::new();
    let mut places = Vec::new();
    let mut budget = 4000;
    while let Some((dir, depth)) = pending.pop_front() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        places.push(dir.clone());
        if depth >= 3 || config::home().is_ok_and(|home| dir == home) {
            continue;
        }
        let Ok(entries) = dir.read_dir() else {
            continue;
        };
        let mut child_dirs = Vec::new();
        for entry in entries.take(budget).flatten() {
            budget -= 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.')
                && !fs::is_sensitive(&name, true)
                && !NAV_SKIP_DIRS.contains(&name.as_ref())
                && entry.file_type().is_ok_and(|t| t.is_dir())
            {
                if fs::is_build_dir_name(&name) {
                    places.push(entry.path());
                } else {
                    child_dirs.push(entry.path());
                }
            }
        }
        child_dirs.sort();
        pending.extend(child_dirs.into_iter().map(|p| (p, depth + 1)));
        if budget == 0 {
            break;
        }
    }
    (places, budget == 0)
}

#[derive(Default)]
struct LocationFacts {
    aliases: Vec<String>,
    purpose: String,
}

fn project_purpose(root: &Path) -> Option<String> {
    let mut purpose = None;
    for (file, section) in [
        ("Cargo.toml", "package"),
        ("pyproject.toml", "project"),
        ("package.json", ""),
    ] {
        if let Ok(text) = fs::read_text(&root.join(file), 65536) {
            purpose = if section.is_empty() {
                serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["description"].as_str().map(str::to_owned))
            } else {
                toml::from_str::<toml::Value>(&text).ok().and_then(|v| {
                    v.get(section)?
                        .get("description")?
                        .as_str()
                        .map(str::to_owned)
                })
            };
            if purpose.is_some() {
                break;
            }
        }
    }
    purpose
}

// The introduction is used locally for ranking. It is not part of model context.
fn readme_purpose(root: &Path) -> Option<String> {
    let text = fs::read_text(&root.join("README.md"), 65536).ok()?;
    let introduction = text
        .lines()
        .take(18)
        .filter(|line| !line.starts_with(['[', '!']) && !line.starts_with("```"))
        .take_while(|line| !line.starts_with("## "))
        .collect::<Vec<_>>()
        .join(" ");
    (!introduction.trim().is_empty()).then(|| introduction.chars().take(460).collect())
}

fn location_facts(path: &Path) -> LocationFacts {
    let Some(base) = data()
        .ok()
        .zip(identity(path).ok())
        .map(|(dir, id)| dir.join("projects").join(id))
    else {
        return LocationFacts::default();
    };
    let prefs = read::<Preferences>(&base.join("preferences.json")).unwrap_or_default();
    let purpose = prefs
        .purpose
        .or_else(|| project_purpose(path))
        .or_else(|| readme_purpose(path))
        .unwrap_or_default()
        .to_lowercase();
    LocationFacts {
        aliases: prefs.aliases,
        purpose,
    }
}

fn choices(paths: &[PathBuf]) -> String {
    format!(
        "Which directory do you mean?{}",
        paths
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, path)| format!("\n{}. {}", i + 1, path.display()))
            .collect::<String>()
    )
}

pub fn choose_project(prompt: &str, answer: &str) -> Option<PathBuf> {
    let mut choices = prompt
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (n, path) = line.trim().split_once(". ")?;
            Some((n.parse::<usize>().ok()?, PathBuf::from(path)))
        })
        .filter(|(n, p)| {
            answer.parse::<usize>().ok() == Some(*n)
                || p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.eq_ignore_ascii_case(answer))
                || p == Path::new(answer)
        });
    let (_, path) = choices.next()?;
    (choices.next().is_none() && path.is_dir()).then_some(path)
}

pub fn project_for(cwd: &Path, request: &str) -> Result<Option<PathBuf>, String> {
    let requested = request.to_lowercase();
    let intent = crate::intent::classify_str(request);
    let project_task = matches!(
        intent,
        crate::intent::Intent::Build
            | crate::intent::Intent::Run
            | crate::intent::Intent::BuildAndRun
            | crate::intent::Intent::Test
            | crate::intent::Intent::Clean
    ) || requested.contains("entrypoint")
        || requested.contains("where does")
        || requested.contains(" gui")
        || requested.contains("project");
    let nav = crate::intent::navigation_destination(request);
    if nav.is_none()
        && !project_task
        && !previous_directory(request)
        && !["that one", "that project", "previous project"]
            .iter()
            .any(|q| requested.contains(q))
    {
        return Ok(None);
    }

    if ["that one", "that project", "previous project"]
        .iter()
        .any(|q| requested.contains(q))
    {
        let recent = session_path()
            .and_then(|p| read::<Vec<(u64, String, PathBuf)>>(&p).ok())
            .unwrap_or_default();
        let current = ancestor_project(cwd).unwrap_or_else(|| cwd.to_owned());
        return recent
            .iter()
            .find(|(t, id, p)| {
                now().saturating_sub(*t) < 86400
                    && identity(p).ok().as_ref() == Some(id)
                    && (!requested.contains("previous project") || p != &current)
            })
            .map(|(_, _, p)| Some(p.clone()))
            .ok_or("Which project do you mean?".into());
    }
    if nav.is_some() || previous_directory(request) {
        return resolve_directory(request, cwd).map(Some);
    }
    let words = directory_words(&requested).collect::<Vec<_>>();
    if words
        .iter()
        .any(|w| matches!(*w, "this" | "here" | "current"))
        && !requested.contains(" in /")
    {
        return Ok(ancestor_project(cwd));
    }
    if let Some((_, trailing)) = request.split_once(" in ") {
        let selected = trailing.trim().trim_matches(['\'', '"']);
        if selected.starts_with('/') || selected.starts_with("./") || selected.starts_with("../") {
            return cwd
                .join(selected)
                .canonicalize()
                .ok()
                .filter(|p| p.is_dir())
                .map(Some)
                .ok_or_else(|| "The requested project directory is unavailable.".into());
        }
    }
    let phrase: Vec<_> = words
        .into_iter()
        .filter(|w| {
            !NAV_STOPWORDS.contains(w)
                && !matches!(
                    *w,
                    "please"
                        | "run"
                        | "launch"
                        | "start"
                        | "open"
                        | "execute"
                        | "build"
                        | "compile"
                        | "then"
                        | "app"
                        | "application"
                        | "gui"
                        | "cli"
                        | "server"
                        | "service"
                        | "web"
                        | "preview"
                        | "show"
                        | "where"
                        | "does"
                        | "entrypoint"
                )
        })
        .collect();
    if !phrase.is_empty() {
        let needle = phrase.join(" ");
        match rank_locations(cwd, &needle, None, &phrase, true) {
            Ok(path) => return Ok(Some(path)),
            Err(message) if message.starts_with("Which ") => return Err(message),
            Err(_) => {}
        }
    }
    Ok(ancestor_project(cwd))
}

fn ancestor_project(cwd: &Path) -> Option<PathBuf> {
    let nearest = cwd.ancestors().find(|p| manifest(p))?;
    if nearest.join("CMakeLists.txt").is_file() {
        for dir in nearest.ancestors() {
            if fs::read_text(&dir.join("CMakeLists.txt"), 65536).is_ok_and(|source| {
                source.lines().any(|line| {
                    line.trim_start()
                        .to_ascii_lowercase()
                        .starts_with("project(")
                })
            }) {
                return Some(dir.to_owned());
            }
            if dir.join(".git").exists()
                || (dir != nearest && manifest(dir) && !dir.join("CMakeLists.txt").is_file())
            {
                break;
            }
        }
    }
    if nearest.join("Cargo.toml").is_file() {
        for dir in nearest.ancestors().skip(1).take(6) {
            if dir.join(".git").exists() || dir.join("Cargo.toml").is_file() {
                if super::cargo::manifest(&dir.join("Cargo.toml"))
                    .is_some_and(|m| m.get("workspace").is_some())
                {
                    return Some(dir.to_owned());
                }
                break;
            }
        }
    }
    Some(nearest.to_owned())
}

const NAV_STOPWORDS: &[&str] = &[
    "my",
    "the",
    "that",
    "a",
    "an",
    "to",
    "me",
    "i",
    "meant",
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

pub fn previous_directory(request: &str) -> bool {
    let request = request
        .trim()
        .trim_end_matches(['?', '.', '!'])
        .to_lowercase();
    matches!(
        request.as_str(),
        "go back"
            | "go back where i was"
            | "go back to the previous directory"
            | "go to the previous directory"
            | "cd -"
    )
}

pub fn resolve_directory(request: &str, cwd: &Path) -> Result<PathBuf, String> {
    let home = config::home().map_err(|_| "home directory is unavailable")?;
    let actual = |path: PathBuf| {
        path.canonicalize()
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| "could not find the requested directory".to_owned())
    };
    if previous_directory(request) {
        return std::env::var_os("GICO_PREVIOUS_DIR")
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "The previous directory is unavailable in this shell.".into())
            .and_then(|p| actual(PathBuf::from(p)));
    }
    let destination = intent::navigation_destination(request)
        .or_else(|| request.split_once(" to ").map(|(_, target)| target));
    if let Some(destination) = destination {
        let path = destination.trim().trim_matches(['\'', '"']);
        if intent::is_home_destination(destination) {
            return actual(home);
        }
        let label = path.to_ascii_lowercase();
        let label = label
            .trim_start_matches("my ")
            .trim_end_matches(" directory")
            .trim_end_matches(" folder");
        if matches!(
            label,
            "downloads" | "documents" | "desktop" | "pictures" | "music" | "videos"
        ) {
            let key = if label == "downloads" {
                "DOWNLOAD".into()
            } else {
                label.to_uppercase()
            };
            let cfg = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            if let Ok(contents) = fs::read_text(&cfg.join("user-dirs.dirs"), 16384) {
                if let Some(value) = contents
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("XDG_{key}_DIR=")))
                {
                    if let Some(value) =
                        shlex::split(value).and_then(|mut v| (v.len() == 1).then(|| v.remove(0)))
                    {
                        let configured = value
                            .strip_prefix("$HOME/")
                            .map(|p| home.join(p))
                            .unwrap_or_else(|| PathBuf::from(value));
                        if configured.is_absolute() {
                            return actual(configured);
                        }
                    }
                }
            }
            let mut fallback = label.to_owned();
            fallback[..1].make_ascii_uppercase();
            return actual(home.join(fallback));
        }
        if let Some(relative) = path.strip_prefix("~/") {
            return actual(home.join(relative));
        }
        if path.starts_with(['/', '.']) || cwd.join(path).is_dir() {
            return actual(cwd.join(path));
        }
    }
    let lower = destination.unwrap_or(request).to_lowercase();
    // “to where <name> is” identifies a named directory. A partial match
    // (for example, a sibling project sharing two words) is insufficient.
    let named = lower
        .trim_end_matches(['.', '?', '!'])
        .trim_end()
        .strip_prefix("where ")
        .and_then(|end| end.strip_suffix(" is"))
        .map(|name| name.trim().trim_matches(['\'', '"']));
    let tokens: Vec<_> = directory_words(&lower)
        .filter(|word| !word.is_empty() && !NAV_STOPWORDS.contains(word))
        .collect();
    if tokens.is_empty() {
        return Err("could not find a directory matching the request".into());
    }

    rank_locations(cwd, &lower, named, &tokens, false)
}

fn rank_locations(
    cwd: &Path,
    lower: &str,
    named: Option<&str>,
    tokens: &[&str],
    projects_only: bool,
) -> Result<PathBuf, String> {
    let (mut places, incomplete) = workspace_places(cwd);
    if let Ok(dir) = data() {
        places.extend(
            read::<Locations>(&dir.join("locations.json"))
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(id, p)| (identity(&p).ok().as_ref() == Some(&id)).then_some(p)),
        );
    }
    places.sort();
    places.dedup();
    let mut ranked = Vec::new();
    for path in places {
        if projects_only && !manifest(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|p| p.to_str()) else {
            continue;
        };
        let lower_name = name.to_lowercase();
        let facts = if manifest(&path) {
            location_facts(&path)
        } else {
            LocationFacts::default()
        };
        let parent_name = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let js = path.join("package.json").is_file() && !path.join("tsconfig.json").is_file();
        if tokens.iter().any(|w| match *w {
            "js" | "javascript" => {
                !js && !directory_words(&lower_name)
                    .chain(directory_words(&parent_name))
                    .any(|n| matches!(n, "js" | "javascript"))
            }
            "ts" | "typescript" => {
                !path.join("tsconfig.json").is_file()
                    && !directory_words(&lower_name)
                        .chain(directory_words(&parent_name))
                        .any(|n| matches!(n, "ts" | "typescript"))
            }
            "old" | "legacy" => !directory_words(&lower_name)
                .any(|n| matches!(n, "old" | "legacy" | "archive" | "archived")),
            "production" | "development" => !directory_words(&lower_name).any(|n| {
                n == *w
                    || (*w == "development" && n == "dev")
                    || (*w == "production" && n == "prod")
            }),
            _ => false,
        }) {
            continue;
        }
        let rank = if let Some(expected) = named {
            (lower_name == expected).then_some(100)
        } else if facts
            .aliases
            .iter()
            .any(|a| a.eq_ignore_ascii_case(lower.trim()) || contains_name(lower, a))
        {
            Some(120)
        } else if lower_name == lower.trim()
            || (projects_only
                && contains_name(
                    lower,
                    lower_name
                        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '-')
                        .trim(),
                ))
        {
            Some(110)
        } else {
            let meaningful: Vec<_> = tokens
                .iter()
                .copied()
                .filter(|w| {
                    !matches!(
                        *w,
                        "old" | "legacy" | "js" | "ts" | "production" | "development"
                    )
                })
                .collect();
            directory_rank(&path, &lower_name, &meaningful)
                .map(|r| 30 + r)
                .or_else(|| {
                    let name = directory_words(&lower_name);
                    let purpose = directory_words(&facts.purpose);
                    (!facts.purpose.is_empty()
                        && !meaningful.is_empty()
                        && meaningful
                            .iter()
                            .all(|token| name.clone().chain(purpose.clone()).any(|w| w == *token)))
                    .then(|| {
                        20 + 3 * meaningful
                            .iter()
                            .filter(|token| name.clone().any(|w| w == **token))
                            .count()
                    })
                })
        };
        if let Some(rank) = rank {
            ranked.push((rank, path));
        }
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let Some((rank, path)) = ranked.first() else {
        return Err(if incomplete {
            "Directory search reached its limit before finding a match."
        } else {
            "could not find a directory matching the request"
        }
        .into());
    };
    let close = ranked
        .iter()
        .take_while(|(r, _)| rank.saturating_sub(*r) <= 1)
        .map(|(_, p)| p.clone())
        .collect::<Vec<_>>();
    if close.len() > 1 {
        return Err(choices(&close));
    }
    if incomplete && *rank < 100 {
        return Err(
            "Directory search reached its limit. Supply a project alias or a narrower location."
                .into(),
        );
    }
    Ok(cmake::relocated_source(path)
        .map_err(str::to_owned)?
        .unwrap_or_else(|| path.clone()))
}

fn directory_words(name: &str) -> impl Iterator<Item = &str> + Clone {
    name.split(|c: char| !c.is_alphanumeric() && c != '+')
        .filter(|word| !word.is_empty())
}

fn canonical_language(word: &str) -> &str {
    match word {
        "cpp" => "c++",
        _ => word,
    }
}

fn directory_rank(path: &Path, name: &str, tokens: &[&str]) -> Option<usize> {
    let words = directory_words(name).map(canonical_language);
    let mut score = 0;
    for &token in tokens {
        let token = canonical_language(token);
        if words.clone().any(|word| word == token) {
            score += 3;
            continue;
        }
        let language = MARKER_FILES.iter().any(|&(lang, _)| lang == token);
        if language {
            // A language can qualify a project name through its containing
            // folder or a build file. A language-only request needs a named folder.
            if tokens.len() == 1 {
                return None;
            }
            let parent = path.parent()?.file_name()?.to_str()?.to_lowercase();
            if directory_words(&parent)
                .map(canonical_language)
                .any(|word| word == token)
                || MARKER_FILES
                    .iter()
                    .any(|&(lang, file)| lang == token && path.join(file).is_file())
            {
                continue;
            }
            return None;
        }
        let quality = words
            .clone()
            .filter_map(|word| {
                if token.len() >= 3 && word.starts_with(token) {
                    return Some(2);
                }
                (token.len() >= 4
                    && word.len() >= 4
                    && word.len().abs_diff(token.len()) <= 1
                    && strsim::osa_distance(token, word) <= 1)
                    .then_some(1)
            })
            .max()?;
        score += quality;
    }
    if let [token] = tokens {
        let marker = MARKER_FILES.iter().any(|(lang, file)| {
            lang == token && (path.join(file).is_file() || dir_has_child_marker(path, file))
        });
        score += usize::from(marker);
    }
    Some(score)
}

fn dir_has_child_marker(dir: &Path, marker: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.take(50).flatten().any(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir()) && entry.path().join(marker).is_file()
        })
    })
}
