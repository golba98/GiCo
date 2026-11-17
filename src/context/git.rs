//! Read-only git metadata: repository root and current branch.
//! Never reads .git/config (remote URLs may embed tokens) or any other file
//! that could contain credentials.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub dirty: Option<bool>,
}

/// Find the .git directory/file walking up from `dir`.
pub fn find_git_dir(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir.to_path_buf());
    while let Some(d) = cur {
        let dot = d.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            // worktree: "gitdir: /path/to/real/git"
            if let Ok(content) = super::fs::read_text(&dot, 4096) {
                if let Some(target) = content.trim().strip_prefix("gitdir:") {
                    return Some(d.join(target.trim()));
                }
            }
        }
        cur = d.parent().map(|p| p.to_path_buf());
    }
    None
}

fn branch_from_head(git_dir: &Path) -> Option<String> {
    let head = super::fs::read_text(&git_dir.join("HEAD"), 4096).ok()?;
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref: refs/heads/") {
        (r.len() <= 200
            && !r.is_empty()
            && r.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '.')))
        .then(|| r.to_owned())
    } else if matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        // detached HEAD: report short sha
        Some(format!("detached:{}", &head[..12]))
    } else {
        None
    }
}

/// Collect git metadata for `dir` (the working directory itself, not git dir).
pub fn collect(dir: &Path) -> GitInfo {
    let Some(git_dir) = find_git_dir(dir) else {
        return GitInfo::default();
    };
    let branch = branch_from_head(&git_dir);
    // repo root: parent of .git unless worktree pointer
    let root = git_dir.parent().map(|p| p.to_path_buf());
    GitInfo {
        root,
        branch,
        dirty: None,
    }
}
