//! Read Cargo manifest declarations and conventional binary paths without running Cargo.
use super::{fs, RawScan};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize)]
pub struct Binary {
    pub package: String,
    pub name: String,
    pub path: PathBuf,
    pub manifest: PathBuf,
}

fn member(pattern: &str, dir: &Path) -> bool {
    let want: Vec<_> = Path::new(pattern).components().collect();
    let have: Vec<_> = dir.components().collect();
    want.len() == have.len()
        && want
            .iter()
            .zip(have)
            .all(|(a, b)| a.as_os_str() == "*" || a == &b)
}

pub fn binaries(raw: &RawScan) -> Vec<Binary> {
    let root = &raw.environment.cwd;
    let parse = |p: &Path| toml::from_str::<toml::Value>(&fs::read_text(p, 131072).ok()?).ok();
    let Some(workspace) = parse(&root.join("Cargo.toml")) else {
        return vec![];
    };
    let members = workspace
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|v| v.as_array());
    let mut bins = vec![];
    for e in raw
        .entries
        .iter()
        .filter(|e| e.name == "Cargo.toml")
        .take(24)
    {
        let manifest = Path::new(&e.path);
        let parent = manifest.parent().unwrap_or(Path::new(""));
        if !parent.as_os_str().is_empty()
            && !members.is_some_and(|m| {
                m.iter()
                    .filter_map(|v| v.as_str())
                    .any(|p| member(p, parent))
            })
        {
            continue;
        }
        let Some(doc) = parse(&root.join(manifest)) else {
            continue;
        };
        let Some(package) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let mut add = |name: &str, path: PathBuf| {
            if !name.starts_with('-')
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                && raw
                    .entries
                    .iter()
                    .any(|e| Path::new(&e.path) == path && e.kind == fs::FileKind::Source)
                && !bins
                    .iter()
                    .any(|b: &Binary| b.package == package && b.name == name)
            {
                bins.push(Binary {
                    package: package.into(),
                    name: name.into(),
                    path,
                    manifest: manifest.into(),
                });
            }
        };
        if let Some(items) = doc.get("bin").and_then(|v| v.as_array()) {
            for item in items {
                if let (Some(name), Some(path)) = (
                    item.get("name").and_then(|v| v.as_str()),
                    item.get("path").and_then(|v| v.as_str()),
                ) {
                    add(name, parent.join(path));
                }
            }
        }
        if doc["package"].get("autobins").and_then(|v| v.as_bool()) != Some(false) {
            add(package, parent.join("src/main.rs"));
            let bin = parent.join("src/bin");
            for e in &raw.entries {
                let path = Path::new(&e.path);
                if path.parent() == Some(&bin) && path.extension().is_some_and(|e| e == "rs") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        add(name, path.into());
                    }
                }
            }
        }
    }
    bins
}
