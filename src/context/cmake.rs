//! CMake build-tree intelligence: discover configured build directories
//! (any name), validate their caches against the *current* project location
//! (handles renames, moves and symlinks), detect presets, and enumerate
//! executable targets declared anywhere in the source tree.
//!
//! All inspections are read-only and bounded. A demonstrably stale
//! configuration is never selected automatically.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use super::fs::{inside, is_sensitive, read_text};

use serde::{Deserialize, Serialize};

/// One configured CMake build tree and whether its cache is usable now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildTree {
    /// Path relative to the project root, e.g. "build" or "build-gico-test".
    pub dir: String,
    pub cache_home: Option<String>,
    pub cache_file_dir: Option<String>,
    pub generator: Option<String>,
    pub build_type: Option<String>,
    pub valid: bool,
    pub stale_reason: Option<String>,
}

fn cache_value(cache: &str, key: &str) -> Option<String> {
    for line in cache.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some((_, v)) = rest.split_once('=') {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Canonicalize a path for comparison, resolving symlinks; fall back to the
/// lexical path when it does not exist (e.g. a renamed-away old location).
fn canon(p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    pb.canonicalize().unwrap_or(pb)
}

/// Inspect one build directory. `project_root` is the current source root.
pub fn inspect_build_tree(project_root: &Path, dir_rel: &str) -> BuildTree {
    let dir = project_root.join(dir_rel);
    let cache_path = dir.join("CMakeCache.txt");
    let Some(cache) = inside(project_root, &dir)
        .then(|| read_text(&cache_path, 2_000_000).ok())
        .flatten()
    else {
        return BuildTree {
            dir: dir_rel.to_string(),
            cache_home: None,
            cache_file_dir: None,
            generator: None,
            build_type: None,
            valid: false,
            stale_reason: Some("unreadable CMakeCache.txt".to_string()),
        };
    };
    let home = cache_value(&cache, "CMAKE_HOME_DIRECTORY:INTERNAL");
    let cfd = cache_value(&cache, "CMAKE_CACHEFILE_DIR:INTERNAL");
    let generator = cache_value(&cache, "CMAKE_GENERATOR:INTERNAL");
    let build_type = cache_value(&cache, "CMAKE_BUILD_TYPE:STRING");

    let root_canon = canon(&project_root.to_string_lossy());
    // A cache is stale only when it *demonstrably* points elsewhere (renamed /
    // moved project or build tree). Missing keys cannot prove staleness, so the
    // tree stays selectable: `cmake -S . -B <dir>` reconfigures in place safely.
    let mut valid = true;
    let mut stale_reason = None;
    if let Some(h) = &home {
        if canon(h) != root_canon {
            valid = false;
            stale_reason = Some(format!(
                "cache was configured for '{}' but the project now lives at '{}'",
                h,
                project_root.display()
            ));
        }
    }
    if valid {
        if let Some(c) = &cfd {
            if canon(c) != canon(&dir.to_string_lossy()) {
                valid = false;
                stale_reason = Some(format!("cache build dir '{c}' != current '{dir_rel}'"));
            }
        }
    }
    BuildTree {
        dir: dir_rel.to_string(),
        cache_home: home,
        cache_file_dir: cfd,
        generator,
        build_type,
        valid,
        stale_reason,
    }
}

/// All configured build trees under the project, inspected and sorted
/// deterministically: valid trees first (prefer "build", then name order),
/// then stale ones.
pub fn discover_build_trees(project_root: &Path, dirs: &[String]) -> Vec<BuildTree> {
    let mut trees: Vec<BuildTree> = dirs
        .iter()
        .map(|d| inspect_build_tree(project_root, d))
        .collect();
    trees.sort_by(|a, b| {
        let ka = (!a.valid, a.dir != "build", a.dir.clone());
        let kb = (!b.valid, b.dir != "build", b.dir.clone());
        ka.cmp(&kb)
    });
    trees
}

fn preset_manifest(root: &Path) -> Option<serde_json::Value> {
    let text = read_text(&root.join("CMakePresets.json"), 500_000).ok()?;
    serde_json::from_str(&text).ok()
}

fn visible_presets<'a>(
    manifest: &'a serde_json::Value,
    kind: &str,
) -> impl Iterator<Item = &'a serde_json::Value> {
    manifest
        .get(kind)
        .and_then(|values| values.as_array())
        .into_iter()
        .flatten()
        .filter(|preset| preset.get("hidden").and_then(|value| value.as_bool()) != Some(true))
}

/// CMake configure preset names from CMakePresets.json.
pub fn presets(root: &Path) -> Vec<String> {
    preset_manifest(root).map_or_else(Vec::new, |manifest| {
        visible_presets(&manifest, "configurePresets")
            .filter_map(|preset| {
                preset
                    .get("name")?
                    .as_str()
                    .filter(|name| literal_target(name))
                    .map(str::to_owned)
            })
            .collect()
    })
}

pub(crate) fn preset_build_dir(root: &Path, name: &str) -> Option<String> {
    let manifest = preset_manifest(root)?;
    let preset = visible_presets(&manifest, "configurePresets")
        .find(|preset| preset.get("name").and_then(|value| value.as_str()) == Some(name))?;
    let dir = preset
        .get("binaryDir")?
        .as_str()?
        .trim_start_matches("${sourceDir}/")
        .replace("${presetName}", name);
    (!dir.is_empty()
        && !dir.contains('$')
        && !dir.chars().any(char::is_control)
        && Path::new(&dir)
            .components()
            .all(|part| matches!(part, Component::Normal(_))))
    .then_some(dir)
}

pub(crate) fn matching_build_preset(root: &Path, configure: &str) -> Option<String> {
    let manifest = preset_manifest(root)?;
    let result = visible_presets(&manifest, "buildPresets").find_map(|preset| {
        let name = preset.get("name")?.as_str()?;
        (literal_target(name) && preset.get("configurePreset")?.as_str()? == configure)
            .then(|| name.to_owned())
    });
    result
}

/// Executable targets declared in CMakeLists.txt files, following
/// add_subdirectory() references. Bounded: at most 25 files, 500 KB each.
pub fn executable_targets(project_root: &Path) -> Vec<String> {
    target_kinds(project_root)
        .into_iter()
        .filter(|target| target.kind != TargetKind::Library)
        .map(|target| target.name)
        .collect()
}

/// Find a built executable for `target` inside a build tree (bounded walk).
/// Returns the path relative to the project root, e.g. "build/it-pixel-cli".
pub fn find_built_executable(project_root: &Path, build_dir: &str, target: &str) -> Option<String> {
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;

    let build = project_root.join(build_dir);
    if !inside(project_root, &build) {
        return None;
    }
    let mut queue = VecDeque::from([(build, 0)]);
    let mut remaining = 4_096;
    while let Some((dir, depth)) = queue.pop_front() {
        let candidate = dir.join(target);
        if std::fs::symlink_metadata(&candidate)
            .is_ok_and(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
        {
            return candidate
                .strip_prefix(project_root)
                .ok()?
                .to_str()
                .map(str::to_owned);
        }
        if depth == 3 || remaining == 0 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in entries.take(remaining) {
            remaining -= 1;
            let Ok(entry) = entry else { continue };
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name != "CMakeFiles" && !is_sensitive(name, true))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                children.push(entry.path());
            }
        }
        children.sort();
        queue.extend(children.into_iter().map(|path| (path, depth + 1)));
    }
    None
}

/// Choose the build directory for planning: a valid existing tree when one
/// exists, otherwise a fresh directory name that does not collide with
/// anything on disk. Never reuses a stale cache and never deletes anything.
pub fn select_build_dir(project_root: &Path, trees: &[BuildTree]) -> (String, bool) {
    for t in trees {
        if t.valid {
            return (t.dir.clone(), true);
        }
    }
    // Fresh configure into a non-existing directory.
    for cand in [
        "build",
        "build-gico",
        "build-gico-2",
        "build-gico-3",
        "build-local",
    ] {
        if !project_root.join(cand).exists() {
            return (cand.to_string(), false);
        }
    }
    ("build-gico-fresh".to_string(), false)
}

// ---------------------------------------------------------------------------
// Target classification: CLI vs GUI vs test vs service vs library.
// Evidence-based: CMake properties (WIN32_EXECUTABLE/MACOSX_BUNDLE), linked
// GUI frameworks, directory names (gui/, tests/) and target-name tokens.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Cli,
    Gui,
    Test,
    Service,
    Library,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetKind::Cli => "cli",
            TargetKind::Gui => "gui",
            TargetKind::Test => "test",
            TargetKind::Service => "service",
            TargetKind::Library => "library",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetInfo {
    pub name: String,
    pub kind: TargetKind,
    /// CMakeLists.txt directory (relative) declaring the target.
    pub declared_in: String,
    /// Literal artifact name when CMake sets OUTPUT_NAME.
    pub artifact_name: String,
    /// Whether the default CMake output location is supported by the source evidence.
    pub default_output: bool,
}

const GUI_FRAMEWORKS: &[&str] = &[
    "qt6::widgets",
    "qt5::widgets",
    "qt::widgets",
    "wxwidgets",
    "wx::core",
    "gtkmm",
    "gtk::gtk",
    "gtk4",
    "gtk3",
    "fltk",
    "sdl2::sdl2",
    "glfw",
];

#[derive(PartialEq)]
enum Token {
    Word(String),
    Open,
    Close,
}

fn bracket_argument(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let mut probe = chars.clone();
    let equals = probe.by_ref().take_while(|c| *c == '=').count();
    let mut opening = chars.clone();
    for _ in 0..equals {
        opening.next();
    }
    if opening.next() != Some('[') {
        return false;
    }
    chars.nth(equals);
    while let Some(c) = chars.next() {
        if c == ']' {
            let mut closing = chars.clone();
            if (0..equals).all(|_| closing.next() == Some('=')) && closing.next() == Some(']') {
                chars.nth(equals);
                break;
            }
        }
    }
    true
}

fn cmake_calls(text: &str) -> Vec<(String, Vec<String>)> {
    let mut chars = text.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '#' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    if bracket_argument(&mut chars) {
                        continue;
                    }
                }
                while chars.next_if(|c| *c != '\n').is_some() {}
            }
            '[' if bracket_argument(&mut chars) => tokens.push(Token::Word(String::new())),
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            '"' => {
                let mut word = String::new();
                while let Some(c) = chars.next() {
                    if c == '"' {
                        break;
                    }
                    if c == '\\' {
                        if let Some(c) = chars.next() {
                            word.push(c);
                        }
                    } else {
                        word.push(c);
                    }
                }
                tokens.push(Token::Word(word));
            }
            c if c.is_whitespace() => {}
            c => {
                let mut word = String::from(c);
                while let Some(c) =
                    chars.next_if(|c| !c.is_whitespace() && !matches!(c, '(' | ')' | '"' | '#'))
                {
                    word.push(c);
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    let mut calls = Vec::new();
    let mut tokens = tokens.into_iter().peekable();
    while let Some(Token::Word(name)) = tokens.find(|token| matches!(token, Token::Word(_))) {
        if tokens.next_if_eq(&Token::Open).is_none() {
            continue;
        }
        let mut args = Vec::new();
        let mut depth = 1;
        for token in tokens.by_ref() {
            match token {
                Token::Open => depth += 1,
                Token::Close => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Token::Word(word) if depth == 1 => args.push(word),
                _ => {}
            }
        }
        if depth == 0 {
            calls.push((name.to_ascii_lowercase(), args));
        }
    }
    calls
}

#[derive(Default)]
struct TargetEvidence {
    declarations: BTreeMap<String, TargetInfo>,
    links: BTreeMap<String, Vec<String>>,
    properties: BTreeMap<String, Vec<(String, String)>>,
    tests: BTreeSet<String>,
    visited: BTreeSet<PathBuf>,
    custom_output: bool,
}

fn literal_target(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
}

fn name_parts(name: &str) -> impl Iterator<Item = &str> {
    name.split(|c: char| !c.is_ascii_alphanumeric())
}

fn collect_evidence(root: &Path, dir: &Path, depth: usize, evidence: &mut TargetEvidence) {
    if depth > 4 || evidence.visited.len() >= 25 || !inside(root, dir) {
        return;
    }
    let file = dir.join("CMakeLists.txt");
    let Ok(real_dir) = dir.canonicalize() else {
        return;
    };
    if !evidence.visited.insert(real_dir) {
        return;
    }
    let Ok(contents) = read_text(&file, 500_000) else {
        return;
    };
    let rel = dir.strip_prefix(root).unwrap_or(dir).to_string_lossy();
    for (call, args) in cmake_calls(&contents) {
        let Some(name) = args.first() else {
            continue;
        };
        match call.as_str() {
            "add_executable" | "add_library" if literal_target(name) => {
                if args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "IMPORTED" | "ALIAS"))
                {
                    continue;
                }
                let kind = if call == "add_library" {
                    TargetKind::Library
                } else if args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "WIN32" | "MACOSX_BUNDLE"))
                {
                    TargetKind::Gui
                } else {
                    TargetKind::Cli
                };
                evidence
                    .declarations
                    .entry(name.clone())
                    .or_insert(TargetInfo {
                        name: name.clone(),
                        declared_in: rel.to_string(),
                        kind,
                        artifact_name: name.clone(),
                        default_output: true,
                    });
            }
            "target_link_libraries" => {
                evidence
                    .links
                    .entry(name.clone())
                    .or_default()
                    .extend(args[1..].iter().cloned());
            }
            "set_target_properties" => {
                if let Some(index) = args.iter().position(|arg| arg == "PROPERTIES") {
                    for target in &args[..index] {
                        let properties = evidence.properties.entry(target.clone()).or_default();
                        properties.extend(
                            args[index + 1..]
                                .chunks_exact(2)
                                .map(|pair| (pair[0].clone(), pair[1].clone())),
                        );
                    }
                }
            }
            "add_test" => {
                let test = if name == "NAME" {
                    args.iter()
                        .position(|arg| arg == "COMMAND")
                        .and_then(|index| args.get(index + 1))
                } else {
                    args.get(1)
                };
                if let Some(test) = test {
                    evidence.tests.insert(
                        test.strip_prefix("$<TARGET_FILE:")
                            .and_then(|value| value.strip_suffix('>'))
                            .unwrap_or(test)
                            .to_owned(),
                    );
                }
            }
            "set" if name.starts_with("CMAKE_RUNTIME_OUTPUT_DIRECTORY") => {
                evidence.custom_output = true;
            }
            "add_subdirectory"
                if Path::new(name).components().all(|component| {
                    matches!(component, Component::Normal(name)
                    if name.to_str().is_some_and(|name| !is_sensitive(name, true)
                        && !name.chars().any(char::is_control)))
                }) && !name.is_empty() =>
            {
                if args
                    .get(1)
                    .is_some_and(|arg| !matches!(arg.as_str(), "EXCLUDE_FROM_ALL" | "SYSTEM"))
                {
                    evidence.custom_output = true;
                }
                collect_evidence(root, &dir.join(name), depth + 1, evidence);
            }
            _ => {}
        }
    }
}

fn links_gui(
    target: &str,
    links: &BTreeMap<String, Vec<String>>,
    visited: &mut BTreeSet<String>,
) -> bool {
    if visited.len() >= 64 || !visited.insert(target.to_owned()) {
        return false;
    }
    links.get(target).is_some_and(|dependencies| {
        dependencies.iter().any(|link| {
            let lower = link.to_ascii_lowercase();
            GUI_FRAMEWORKS.contains(&lower.as_str())
                || lower.starts_with("gtkmm")
                || links_gui(link, links, visited)
        })
    })
}

/// Discover literal executable and library declarations and classify each target.
pub fn target_kinds(project_root: &Path) -> Vec<TargetInfo> {
    let mut evidence = TargetEvidence::default();
    collect_evidence(project_root, project_root, 0, &mut evidence);
    evidence
        .declarations
        .into_values()
        .map(|mut target| {
            if target.kind == TargetKind::Library {
                return target;
            }
            let name = target.name.to_ascii_lowercase();
            let dir = target.declared_in.to_ascii_lowercase();
            let mut gui = target.kind == TargetKind::Gui;
            target.default_output = !evidence.custom_output;
            for (key, value) in evidence.properties.get(&target.name).into_iter().flatten() {
                if matches!(key.as_str(), "WIN32_EXECUTABLE" | "MACOSX_BUNDLE")
                    && matches!(
                        value.to_ascii_uppercase().as_str(),
                        "ON" | "TRUE" | "1" | "YES"
                    )
                {
                    gui = true;
                }
                if key == "OUTPUT_NAME" && literal_target(value) {
                    target.artifact_name.clone_from(value);
                } else if key.contains("OUTPUT_NAME") || key.starts_with("RUNTIME_OUTPUT_DIRECTORY")
                {
                    target.default_output = false;
                }
            }
            target.kind = if evidence.tests.contains(&target.name)
                || name_parts(&name)
                    .chain(name_parts(&dir))
                    .any(|part| matches!(part, "test" | "tests"))
            {
                TargetKind::Test
            } else if gui || links_gui(&target.name, &evidence.links, &mut BTreeSet::new()) {
                TargetKind::Gui
            } else if name_parts(&name)
                .chain(name_parts(&dir))
                .any(|part| matches!(part, "server" | "daemon" | "service"))
                || name.ends_with("server")
                || name.ends_with("daemon")
            {
                TargetKind::Service
            } else {
                TargetKind::Cli
            };
            target
        })
        .collect()
}
