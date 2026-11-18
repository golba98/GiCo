//! Private project maps. File identity distinguishes worktrees and survives a rename on the same filesystem.
use super::{cmake, fs, RawScan};
use crate::config;
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
    (record["version"] == 1
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
        &json!({"version":1,"observed_at":now(),"root":root,"signature":signature,"facts":facts(raw)}),
    )
}

fn facts(raw: &RawScan) -> Value {
    let root = &raw.environment.cwd;
    let sources:Vec<_>=raw.entries.iter().filter(|e|e.depth<3 && (MANIFESTS.contains(&e.name.as_str()) ||
        matches!(e.name.as_str(),"main.rs"|"main.py"|"__main__.py"|"main.cpp"|"index.ts"|"index.js"|"README.md")))
        .take(32).map(|e|json!({"path":e.path,"role":if MANIFESTS.contains(&e.name.as_str()) {"manifest"} else if e.name=="README.md" {"documentation"} else {"entrypoint candidate"},
            "mtime":root.join(&e.path).metadata().ok().map(|m|m.mtime()),"bytes":e.size})).collect();
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
        ] {
            if let Some(plan) = crate::plan::plan(raw, request).filter(|p| !p.command.is_empty()) {
                recipes.insert(action,json!({"command":plan.command,"cwd":".","evidence":p.config_files,"note":plan.note}));
            }
        }
    }
    json!({"purpose":purpose.map(|s|s.chars().take(200).collect::<String>()),"evidence":sources,"project":raw.project,
        "entrypoints":entries,"targets":targets,"recipes":recipes,"working_directory":".",
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
    json!({"observations":facts,"preferences":prefs.ok()})
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
    let name = term.strip_prefix("/dev/")?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '/') {
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

/// Search only known workspace locations. Never traverse hidden directories or symlinks.
fn workspace_projects() -> Vec<PathBuf> {
    let Ok(home) = config::home() else {
        return vec![];
    };
    let start = home.join("Development");
    if !start.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return vec![];
    }
    let mut todo = vec![(start, 0)];
    let mut result = Vec::new();
    let mut remaining = 2048;
    while let Some((p, depth)) = todo.pop() {
        if remaining == 0 {
            break;
        }
        if depth > 0 && manifest(&p) {
            result.push(p);
            continue;
        }
        if depth >= 3 {
            continue;
        }
        if let Ok(entries) = p.read_dir() {
            for e in entries.take(remaining).flatten() {
                remaining -= 1;
                let name = e.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.')
                    && !fs::is_sensitive(&name, true)
                    && !matches!(name.as_ref(), "node_modules" | "target" | "build")
                    && e.file_type().is_ok_and(|f| f.is_dir())
                {
                    todo.push((e.path(), depth + 1));
                }
            }
        }
    }
    result
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
    if nav.is_none() && !project_task && !requested.contains("that one") {
        return Ok(None);
    }
    if nav.is_some_and(|destination| {
        let path = destination.trim().trim_matches(['\'', '"']);
        crate::intent::is_home_destination(path)
            || path.starts_with(['/', '.'])
            || path.starts_with("~/")
    }) {
        return Ok(None);
    }
    if requested.contains("that one") {
        let recent = session_path()
            .and_then(|p| read::<Vec<(u64, String, PathBuf)>>(&p).ok())
            .unwrap_or_default();
        return recent
            .first()
            .filter(|(t, id, p)| {
                now().saturating_sub(*t) < 86400 && identity(p).ok().as_ref() == Some(id)
            })
            .map(|(_, _, p)| Some(p.clone()))
            .ok_or("Which project do you mean?".into());
    }
    let dir = data().map_err(|e| e.to_string())?;
    let mut paths: Vec<_> = read::<Locations>(&dir.join("locations.json"))
        .unwrap_or_default()
        .into_iter()
        .filter(|(id, p)| identity(p).ok().as_ref() == Some(id))
        .map(|(_, p)| p)
        .collect();
    paths.extend(workspace_projects());
    paths.sort();
    paths.dedup();
    let mut matched = Vec::new();
    for p in paths {
        let Some(id) = identity(&p).ok() else {
            continue;
        };
        let prefs = read::<Preferences>(&dir.join("projects").join(id).join("preferences.json"))
            .unwrap_or_default();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let name = name.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
        if prefs
            .aliases
            .iter()
            .chain(std::iter::once(&name.to_owned()))
            .any(|a| contains_name(nav.unwrap_or(request), a))
        {
            matched.push(p);
        }
    }
    match matched.as_slice() {
        [p] => Ok(Some(p.clone())),
        [] if nav.is_none() => {
            let root = cwd.ancestors().find(|p| manifest(p));
            if let Some(nearest) = root.filter(|p| p.join("CMakeLists.txt").is_file()) {
                // Subdirectory manifests often lack a project declaration. Walk only the
                // containing project, stopping at a repository or another build system.
                for p in nearest.ancestors() {
                    if let Ok(source) = fs::read_text(&p.join("CMakeLists.txt"), 65536) {
                        if source.lines().any(|line| {
                            line.trim_start()
                                .to_ascii_lowercase()
                                .starts_with("project(")
                        }) {
                            return Ok(Some(p.to_owned()));
                        }
                    }
                    if p.join(".git").exists()
                        || (p != nearest && manifest(p) && !p.join("CMakeLists.txt").is_file())
                    {
                        break;
                    }
                }
            }
            Ok(root.map(Path::to_owned))
        }
        [] => Ok(None),
        _ => Err(format!(
            "Which project do you mean? {}",
            matched
                .iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        )),
    }
}
