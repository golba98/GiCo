//! Optional context caching with mtime-based invalidation.
//! The cache key is the scanned directory; the entry stores the raw scan
//! plus a stamp of directory mtimes. Any change to the directory, its
//! top-level entries, or .git/HEAD invalidates the entry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::RawScan;

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    stamp: Vec<(String, u128)>,
    scan: RawScan,
}

pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn key_path(&self, root: &Path) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        root.canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .hash(&mut h);
        self.dir.join(format!("{:016x}.json", h.finish()))
    }

    fn stamp(root: &Path) -> Vec<(String, u128)> {
        let mut map = BTreeMap::new();
        let mut add = |p: PathBuf, name: String| {
            if let Ok(md) = std::fs::metadata(&p) {
                if let Ok(m) = md.modified() {
                    if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                        map.insert(name, d.as_nanos());
                    }
                }
            }
        };
        add(root.to_path_buf(), ".".to_string());
        if let Ok(rd) = std::fs::read_dir(root) {
            let mut children: Vec<PathBuf> = rd
                .take(200)
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            children.sort();
            for c in children {
                let name = c
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                add(c, name);
            }
        }
        if let Some(g) = super::git::find_git_dir(root) {
            add(g.join("HEAD"), ".git/HEAD".to_string());
        }
        map.into_iter().collect()
    }

    /// Return a cached scan if fresh, otherwise run `scan_fn`, cache and return.
    pub fn get_or_scan<F>(&self, root: &Path, scan_fn: F) -> std::io::Result<RawScan>
    where
        F: FnOnce() -> std::io::Result<RawScan>,
    {
        let path = self.key_path(root);
        let current = Self::stamp(root);
        if let Ok(data) = super::fs::read_text(&path, 2_000_000) {
            if let Ok(entry) = serde_json::from_str::<CacheEntry>(&data) {
                if entry.stamp == current {
                    return Ok(entry.scan);
                }
            }
        }
        let scan = scan_fn()?;
        let entry = CacheEntry {
            stamp: current,
            scan,
        };
        if std::fs::create_dir_all(&self.dir).is_ok() {
            if let (Ok(data), Ok(mut file)) = (
                serde_json::to_vec(&entry),
                tempfile::NamedTempFile::new_in(&self.dir),
            ) {
                use std::io::Write;
                if file.write_all(&data).is_ok() {
                    let _ = file.persist(&path);
                }
            }
        }
        Ok(entry.scan)
    }
}
