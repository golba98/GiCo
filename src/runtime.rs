//! Runtime arguments inferred from bounded, target-specific project documentation.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::context::fs::{inside, is_sensitive, read_text};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageEvidence {
    pub usage: Option<String>,
    pub requires_positional: bool,
    pub sample_inputs: Vec<String>,
    pub output_flag: Option<String>,
}

/// The executable path is accepted for API compatibility. Inspection reads docs only.
pub fn gather_usage(root: &Path, target: &str, _built_exe: Option<&Path>) -> UsageEvidence {
    let mut docs = vec![root.join("README.md")];
    let doc_dir = root.join("docs");
    if inside(root, &doc_dir) {
        if let Ok(entries) = std::fs::read_dir(doc_dir) {
            let mut entries: Vec<_> = entries
                .take(100)
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
                .collect();
            entries.sort();
            docs.extend(entries.into_iter().take(10));
        }
    }

    let mut evidence = UsageEvidence::default();
    for doc in docs {
        let Ok(contents) = read_text(&doc, 300_000) else {
            continue;
        };
        let mut lines = contents.lines();
        while let Some(first) = lines.next() {
            let mut line = first.trim().to_owned();
            while let Some(prefix) = line.strip_suffix('\\') {
                let Some(next) = lines.next() else { break };
                if prefix.len() + next.len() > 300 {
                    line.clear();
                    break;
                }
                line = format!("{} {}", prefix.trim_end(), next.trim());
            }
            if line.len() > 300 || line.chars().any(char::is_control) {
                continue;
            }
            let Some(words) = shlex::split(line.trim_matches('`')) else {
                continue;
            };
            let Some(index) = words.iter().position(|word| {
                Path::new(word)
                    .file_name()
                    .is_some_and(|name| name == target)
            }) else {
                continue;
            };
            let args = &words[index + 1..];
            if args.iter().any(|arg| arg == "--output") {
                evidence.output_flag = Some("--output".to_owned());
            }
            if args.first().is_some_and(|arg| {
                (arg.starts_with('<') && arg.ends_with('>'))
                    || matches!(arg.as_str(), "input.png" | "input.jpg" | "input.jpeg")
            }) {
                evidence.requires_positional = true;
                evidence.usage.get_or_insert_with(|| line.clone());
            }
            if let Some(sample) = args.first().filter(|arg| {
                let path = Path::new(arg);
                !path.is_absolute()
                    && !arg.starts_with('-')
                    && arg.contains('/')
                    && path.components().all(|part| {
                        matches!(part, std::path::Component::CurDir)
                            || matches!(part, std::path::Component::Normal(name)
                                if name.to_str().is_some_and(|name| !is_sensitive(name, true)))
                    })
                    && !arg.trim_start_matches("./").starts_with("build")
                    && inside(root, &root.join(path))
                    && root.join(path).is_file()
            }) {
                evidence.requires_positional = true;
                if evidence.sample_inputs.len() < 5 && !evidence.sample_inputs.contains(sample) {
                    evidence.sample_inputs.push(sample.clone());
                }
            }
        }
    }
    evidence.sample_inputs.sort();
    evidence
}
