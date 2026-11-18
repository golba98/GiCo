//! Read workspace manifests and their declared entrypoints without executing project code.
use super::{fs, RawScan};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Serialize)]
pub struct Binary {
    pub package: String,
    pub name: String,
    pub path: PathBuf,
    pub manifest: PathBuf,
    pub example: bool,
    pub required_features: Vec<String>,
    pub default_run: bool,
}

pub(crate) fn manifest(path: &Path) -> Option<toml::Value> {
    toml::from_str(&fs::read_text(path, 131_072).ok()?).ok()
}

fn strings(value: &toml::Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn members(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    for part in Path::new(pattern).components() {
        let Component::Normal(part) = part else {
            return Vec::new();
        };
        let part = part.to_string_lossy();
        if let Some((prefix, suffix)) = part.split_once('*') {
            paths = paths
                .iter()
                .flat_map(|p| p.read_dir().into_iter().flatten().take(128).flatten())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| {
                    e.file_name().to_str().is_some_and(|n| {
                        !fs::is_sensitive(n, true)
                            && !n.starts_with('.')
                            && n.starts_with(prefix)
                            && n.ends_with(suffix)
                    })
                })
                .map(|e| e.path())
                .take(64)
                .collect();
        } else {
            for p in &mut paths {
                p.push(part.as_ref());
            }
        }
    }
    paths.retain(|p| fs::inside(root, p));
    paths
}

pub fn binaries(raw: &RawScan) -> Vec<Binary> {
    let root = &raw.environment.cwd;
    let Some(top) = manifest(&root.join("Cargo.toml")) else {
        return vec![];
    };
    let mut dirs = vec![root.clone()];
    if let Some(ws) = top.get("workspace") {
        let exclusions: Vec<_> = ws
            .get("exclude")
            .map(strings)
            .unwrap_or_default()
            .iter()
            .flat_map(|p| members(root, p))
            .collect();
        dirs.extend(
            ws.get("members")
                .map(strings)
                .unwrap_or_default()
                .iter()
                .flat_map(|p| members(root, p))
                .filter(|p| !exclusions.contains(p))
                .take(64),
        );
    }
    dirs.sort();
    dirs.dedup();
    let mut bins = vec![];
    for dir in dirs {
        let path = dir.join("Cargo.toml");
        let Some(doc) = manifest(&path) else { continue };
        let Some(pkg) = doc.get("package") else {
            continue;
        };
        let Some(package) = pkg.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        let default = pkg.get("default-run").and_then(toml::Value::as_str);
        let mut add =
            |name: &str, source: PathBuf, example: bool, required_features: Vec<String>| {
                let Ok(rel) = source.strip_prefix(root) else {
                    return;
                };
                if name.starts_with('-')
                    || name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                    || !source.is_file()
                    || !fs::inside(root, &source)
                    || bins.iter().any(|b: &Binary| {
                        b.package == package && b.name == name && b.example == example
                    })
                {
                    return;
                }
                bins.push(Binary {
                    package: package.into(),
                    name: name.into(),
                    path: rel.into(),
                    manifest: path.strip_prefix(root).expect("member under root").into(),
                    example,
                    required_features,
                    default_run: !example && default == Some(name),
                });
            };
        for (key, example, folder, auto) in [
            ("bin", false, "src/bin", "autobins"),
            ("example", true, "examples", "autoexamples"),
        ] {
            for entry in doc
                .get(key)
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = entry.get("name").and_then(toml::Value::as_str) else {
                    continue;
                };
                let source = entry
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map(|p| dir.join(p))
                    .unwrap_or_else(|| dir.join(folder).join(format!("{name}.rs")));
                add(
                    name,
                    source,
                    example,
                    entry
                        .get("required-features")
                        .map(strings)
                        .unwrap_or_default(),
                );
            }
            if pkg.get(auto).and_then(toml::Value::as_bool) == Some(false) {
                continue;
            }
            if !example {
                add(package, dir.join("src/main.rs"), false, Vec::new());
            }
            for item in dir
                .join(folder)
                .read_dir()
                .into_iter()
                .flatten()
                .take(64)
                .flatten()
            {
                let path = item.path();
                if path.extension().is_some_and(|e| e == "rs") {
                    if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                        add(name, path.clone(), example, Vec::new());
                    }
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    add(name, path.join("main.rs"), example, Vec::new());
                }
            }
        }
    }
    bins.sort_by(|a, b| (&a.package, &a.name, a.example).cmp(&(&b.package, &b.name, b.example)));
    bins
}
