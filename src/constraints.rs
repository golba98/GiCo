//! Check explicit operands that would be lost by a generic planner shortcut.
use crate::{context::RawScan, intent::Intent};

/// Only literal filenames (including a quoted name with spaces) supplied in the request.
pub fn requested_files(request: &str) -> Vec<String> {
    shlex::split(request)
        .unwrap_or_else(|| request.split_whitespace().map(str::to_owned).collect())
        .into_iter()
        .map(|w| w.trim_end_matches(['?', ',', ';']).to_owned())
        .filter(|w| {
            !w.starts_with('-')
                && !w.contains("://")
                && std::path::Path::new(w)
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| {
                        !ext.is_empty() && ext.len() <= 10 && ext.chars().all(char::is_alphanumeric)
                    })
        })
        .collect()
}

pub fn preserves_files(request: &str, command: &str) -> bool {
    let words = crate::verify::tokenize(command);
    requested_files(request).iter().all(|file| {
        words
            .iter()
            .any(|w| w == file || std::path::Path::new(w).ends_with(file))
    })
}

pub fn shortcut_fits(raw: &RawScan, intent: Intent, request: &str, command: &str) -> bool {
    if !preserves_files(request, command) {
        return false;
    }
    let lower = request.to_ascii_lowercase();
    let words = lower.split_whitespace().collect::<Vec<_>>();
    match intent {
        Intent::GitStatus => !words.iter().any(|w| {
            matches!(
                *w,
                "commit" | "pull" | "push" | "remotes" | "branches" | "checkout" | "switch"
            )
        }),
        Intent::Build | Intent::BuildAndRun | Intent::Test | Intent::Run | Intent::Clean => {
            if words.contains(&"release") && !command.to_ascii_lowercase().contains("release") {
                return false;
            }
            if words.iter().any(|w| matches!(*w, "warning" | "warnings"))
                && !command.contains("-Wall")
            {
                return false;
            }
            if words
                .iter()
                .any(|w| matches!(*w, "only" | "except" | "without" | "called" | "named"))
            {
                return false;
            }
            if let Some(project) = &raw.project {
                for target in &project.executable_targets {
                    if words.contains(&target.to_ascii_lowercase().as_str())
                        && !command.contains(target)
                    {
                        return false;
                    }
                }
            }
            true
        }
        _ => true,
    }
}
