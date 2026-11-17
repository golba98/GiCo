//! Relevance-based context selection within a token budget.
//! Deterministic: same scan + same request + same budget => same selection.

use super::fs::{FileEntry, FileKind};
use super::prompt::estimate_tokens;
use super::RawScan;

/// Map a lowercase file extension to request keywords that make it relevant.
fn ext_keywords(ext: &str) -> &'static [&'static str] {
    match ext {
        "cpp" | "cc" | "cxx" | "c" => &["c++", "cpp", "compile", "compiler", "build", "g++"],
        "h" | "hpp" => &["c++", "cpp", "header", "compile"],
        "rs" => &["rust", "cargo"],
        "py" => &["python", "script", "run"],
        "js" | "jsx" | "mjs" => &["javascript", "node", "npm", "js"],
        "ts" | "tsx" => &["typescript", "ts", "npm", "node"],
        "go" => &["go", "golang"],
        "java" => &["java", "maven", "gradle"],
        "toml" | "json" | "yaml" | "yml" | "lock" | "mod" => &["config", "project", "build"],
        "md" => &["readme", "doc", "documentation"],
        _ => &[],
    }
}

/// Split a string into lowercase alphanumeric word tokens (len >= 3).
pub fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            cur.push(c.to_ascii_lowercase());
        } else if !cur.is_empty() {
            if cur.len() >= 3 {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 3 {
        out.push(cur);
    }
    out
}

fn score(entry: &FileEntry, request_words: &[String]) -> i64 {
    let path_lower = entry.path.to_lowercase();
    let base = entry.name.to_lowercase();
    let ext = base.rsplit('.').next().unwrap_or("");
    let mut score = 0i64;

    // Keyword overlap between file name and request.
    let name_words = words(&entry.name);
    for w in &name_words {
        if request_words.contains(w) {
            score += 4;
        }
    }
    // Extension-language keywords mentioned in the request.
    for kw in ext_keywords(ext) {
        if request_words.iter().any(|w| w == kw) {
            score += 3;
        }
    }
    // Path segment match (e.g. request mentions "wallet").
    for w in request_words {
        if path_lower.contains(w.as_str()) {
            score += 2;
        }
    }
    // Structural boosts.
    match entry.kind {
        FileKind::Build | FileKind::Config => score += 3,
        FileKind::Directory => score -= 1,
        _ => {}
    }
    let base_lower = base;
    if base_lower == "main.rs"
        || base_lower == "main.py"
        || base_lower == "main.cpp"
        || base_lower == "lib.rs"
        || base_lower == "index.ts"
        || base_lower == "index.js"
        || base_lower == "app.py"
        || base_lower == "main.go"
    {
        score += 2;
    }
    if entry.name.ends_with(".lock") || entry.name.ends_with("-lock.yaml") {
        score += 1;
    }
    // Depth penalty keeps the listing flat and compact.
    score -= entry.depth as i64;
    score
}

/// Select the most relevant entries that fit within `budget_tokens`.
/// Project/build/config files are always kept when present.
pub fn select(scan: &RawScan, request: &str, budget_tokens: usize) -> Vec<FileEntry> {
    let request_words = words(request);
    let mut scored: Vec<(i64, &FileEntry)> = scan
        .entries
        .iter()
        .map(|e| (score(e, &request_words), e))
        .collect();
    // Stable, deterministic ordering: score desc, then path asc.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));

    let budget_chars = budget_tokens.saturating_mul(4);
    let mut selected: Vec<FileEntry> = Vec::new();
    let mut used = 0usize;

    // Pass 1: always-keep structural files (build/config at depth 0).
    for (_, e) in scored.iter() {
        if e.depth == 0 && matches!(e.kind, FileKind::Build | FileKind::Config) {
            let cost = e.path.len() + 2;
            if used + cost <= budget_chars || selected.is_empty() {
                used += cost;
                selected.push((*e).clone());
            }
        }
    }

    // Pass 2: best-scored remaining entries within budget.
    for (_, e) in &scored {
        if selected.iter().any(|s| s.path == e.path) {
            continue;
        }
        let cost = e.path.len() + 2;
        if used + cost > budget_chars && !selected.is_empty() {
            continue;
        }
        used += cost;
        selected.push((*e).clone());
    }

    selected.sort_by(|a, b| a.path.cmp(&b.path));
    let _ = estimate_tokens(""); // keep import used
    selected
}
