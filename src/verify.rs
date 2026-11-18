//! Post-generation command verification.
//!
//! Uses a conservative shell lexer/parser (quotes, escapes, operators,
//! redirects, subshells, command substitutions) instead of naive
//! whitespace splitting. Every command gets a verdict:
//!
//! - [`Verdict::Valid`]   — no issues; Enter may execute it.
//! - [`Verdict::Warning`] — issues to show, but execution allowed with approval.
//! - [`Verdict::Blocked`] — blocking errors; Enter must never execute it.
//!
//! Verification never executes anything.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::env;
use crate::context::fs::{FileEntry, FileKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Valid,
    Warning,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub command: String,
    pub issues: Vec<Issue>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::Error)
    }
    pub fn errors(&self) -> Vec<&Issue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .collect()
    }
    pub fn warnings(&self) -> Vec<&Issue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
            .collect()
    }
    pub fn verdict(&self) -> Verdict {
        if self.has_errors() {
            Verdict::Blocked
        } else if !self.warnings().is_empty() {
            Verdict::Warning
        } else {
            Verdict::Valid
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A plain word with all quoting/escapes resolved.
    Word(String),
    /// An operator: && || | |& ; & ( ) < > >> 2> 2>> &>
    Op(&'static str),
    /// $(...) or `...` — inner text kept for recursive verification.
    Subst(String),
}

const TWO_CHAR_OPS: &[&str] = &["&&", "||", ">>", "<<", "|&", "&>", "2>", "2>>"];
const ONE_CHAR_OPS: &[char] = &[';', '|', '&', '(', ')', '<', '>'];

/// Lex a shell command line. Returns Err with a human-readable reason for
/// unsupported or malformed syntax (unterminated quotes, heredocs, ...).
#[allow(unused_assignments)]
fn lex(input: &str) -> Result<Vec<Tok>, String> {
    if input.len() > 16_384 || input.chars().any(|c| c.is_control() && c != '\t') {
        return Err("command is too long or contains unsupported control characters".into());
    }
    let chars: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    let mut word = String::new();
    let mut in_word = false;

    macro_rules! flush {
        () => {
            if in_word {
                toks.push(Tok::Word(std::mem::take(&mut word)));
                in_word = false;
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            flush!();
            i += 1;
            continue;
        }
        // Comments: '#' at a word boundary starts a comment.
        if c == '#' && !in_word {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Two-character operators (including fd redirects like 2>).
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if TWO_CHAR_OPS.contains(&two.as_str()) {
            if two == "<<" {
                return Err("heredocs (<<) are not supported by the verifier".to_string());
            }
            flush!();
            toks.push(Tok::Op(match two.as_str() {
                "&&" => "&&",
                "||" => "||",
                ">>" => ">>",
                "|&" => "|&",
                "&>" => "&>",
                "2>" => "2>",
                "2>>" => "2>>",
                _ => unreachable!(),
            }));
            i += 2;
            continue;
        }
        if ONE_CHAR_OPS.contains(&c) {
            flush!();
            toks.push(Tok::Op(match c {
                ';' => ";",
                '|' => "|",
                '&' => "&",
                '(' => "(",
                ')' => ")",
                '<' => "<",
                _ => ">",
            }));
            i += 1;
            continue;
        }
        // Word accumulation. Quote state is tracked only to find the word
        // boundary; the actual unquoting/unescaping is delegated to `shlex`
        // — the same crate Codex's shell-command parser uses for POSIX
        // word splitting (see NOTICE.md).
        in_word = true;
        let mut raw_start = i;
        let mut in_single = false;
        let mut in_double = false;
        macro_rules! flush_raw {
            () => {
                if in_word {
                    let raw: String = chars[raw_start..i].iter().collect();
                    if !raw.is_empty() {
                        match shlex::split(&raw) {
                            Some(parts) if !parts.is_empty() => word = parts.join(""),
                            _ => return Err("unterminated quote".to_string()),
                        }
                        toks.push(Tok::Word(std::mem::take(&mut word)));
                    }
                    in_word = false;
                }
            };
        }
        while i < chars.len() {
            let c = chars[i];
            if in_single {
                if c == '\'' {
                    in_single = false;
                }
                i += 1;
                continue;
            }
            if in_double {
                if c == '\x60' || (c == '$' && chars.get(i + 1) == Some(&'(')) {
                    return Err("command substitution inside double quotes is unsupported".into());
                }
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    in_double = false;
                }
                i += 1;
                continue;
            }
            match c {
                '\'' => {
                    in_single = true;
                    i += 1;
                }
                '"' => {
                    in_double = true;
                    i += 1;
                }
                '\\' => {
                    if i + 1 >= chars.len() {
                        return Err("trailing backslash".to_string());
                    }
                    i += 2;
                }
                '$' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                    flush_raw!();
                    let (inner, next) = read_balanced(&chars, i + 2, '(', ')')?;
                    toks.push(Tok::Subst(inner));
                    i = next;
                    raw_start = i;
                    in_word = true;
                }
                '`' => {
                    flush_raw!();
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != '`' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err("unterminated backtick substitution".to_string());
                    }
                    toks.push(Tok::Subst(chars[start..i].iter().collect()));
                    i += 1;
                    raw_start = i;
                    in_word = true;
                }
                c if c.is_whitespace() => break,
                c => {
                    let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
                    if TWO_CHAR_OPS.contains(&two.as_str()) || ONE_CHAR_OPS.contains(&c) {
                        break;
                    }
                    i += 1;
                }
            }
        }
        flush_raw!();
    }
    Ok(toks)
}

/// Read a balanced (...) region starting just after the opening paren.
/// Returns (inner_text, index_after_closing_paren).
fn read_balanced(
    chars: &[char],
    mut i: usize,
    open: char,
    close: char,
) -> Result<(String, usize), String> {
    let start = i;
    let mut depth = 1usize;
    while i < chars.len() {
        match chars[i] {
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Ok((chars[start..i].iter().collect(), i + 1));
                }
            }
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated quote inside substitution".to_string());
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated quote inside substitution".to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err("unterminated $(...) substitution".to_string())
}

/// Split a single literal command without shell expansion or control operators.
pub(crate) fn literal_words(command: &str) -> Option<Vec<String>> {
    let mut quote = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c.is_control() {
            return None;
        }
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => {
                if chars.next()?.is_control() {
                    return None;
                }
            }
            (Some('"'), '$' | '\x60') => return None,
            (None, '\'' | '"') => quote = Some(c),
            (
                None,
                '$' | '\x60' | '&' | '|' | ';' | '<' | '>' | '(' | ')' | '~' | '*' | '?' | '['
                | ']' | '{' | '}' | '!' | '#',
            ) => return None,
            _ => {}
        }
    }
    quote.is_none().then(|| shlex::split(command)).flatten()
}

pub fn navigation_target(command: &str, cwd: &Path) -> Option<PathBuf> {
    let words = literal_words(command)?;
    let [program, destination] = words.as_slice() else {
        return None;
    };
    if program != "cd" || destination.is_empty() || destination.starts_with('-') {
        return None;
    }
    cwd.join(destination).canonicalize().ok().filter(|path| {
        path.is_dir()
            && path
                .to_str()
                .is_some_and(|path| !path.chars().any(char::is_control))
    })
}

/// Backwards-compatible quote-aware tokenizer (words only, operators kept).
pub fn tokenize(command: &str) -> Vec<String> {
    match lex(command) {
        Ok(toks) => toks
            .into_iter()
            .map(|t| match t {
                Tok::Word(w) => w,
                Tok::Op(o) => o.to_string(),
                Tok::Subst(s) => format!("$({s})"),
            })
            .collect(),
        Err(_) => command.split_whitespace().map(|s| s.to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// Parser: tokens -> commands
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ParsedCommand {
    /// Program + arguments (env assignments stripped).
    words: Vec<String>,
    /// Redirect targets: (op, target).
    redirects: Vec<(&'static str, String)>,
    /// True when this came from a ( ... ) subshell — no program to check.
    subshell: bool,
}

/// Maximum nesting depth for subshells / command substitutions. Mirrors
/// Codex's MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH so hostile nesting fails
/// closed instead of recursing without bound (see NOTICE.md).
const MAX_WRAPPER_DEPTH: usize = 8;

fn parse(toks: &[Tok]) -> Result<Vec<ParsedCommand>, String> {
    parse_depth(toks, 0)
}

fn parse_depth(toks: &[Tok], depth: usize) -> Result<Vec<ParsedCommand>, String> {
    if depth > MAX_WRAPPER_DEPTH {
        return Err("command nesting too deep (possible wrapper bomb)".to_string());
    }
    let mut cmds = Vec::new();
    let mut cur_words: Vec<String> = Vec::new();
    let mut cur_redirs: Vec<(&'static str, String)> = Vec::new();
    let mut subshell = false;
    let mut i = 0usize;

    fn finish(
        cmds: &mut Vec<ParsedCommand>,
        words: &mut Vec<String>,
        redirs: &mut Vec<(&'static str, String)>,
        subshell: bool,
    ) {
        if !words.is_empty() || !redirs.is_empty() || subshell {
            cmds.push(ParsedCommand {
                words: std::mem::take(words),
                redirects: std::mem::take(redirs),
                subshell,
            });
        }
    }

    while i < toks.len() {
        match &toks[i] {
            Tok::Op(";")
            | Tok::Op("&&")
            | Tok::Op("||")
            | Tok::Op("|")
            | Tok::Op("|&")
            | Tok::Op("&") => {
                finish(&mut cmds, &mut cur_words, &mut cur_redirs, false);
                subshell = false;
                i += 1;
            }
            Tok::Op("(") => {
                // Collect the subshell body and verify it recursively later.
                let mut depth = 1usize;
                let mut j = i + 1;
                while j < toks.len() && depth > 0 {
                    match &toks[j] {
                        Tok::Op("(") => depth += 1,
                        Tok::Op(")") => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                if depth != 0 {
                    return Err("unbalanced parentheses".to_string());
                }
                let inner = &toks[i + 1..j - 1];
                let inner_cmds = parse_depth(inner, depth + 1)?;
                cmds.extend(inner_cmds);
                finish(&mut cmds, &mut cur_words, &mut cur_redirs, false);
                subshell = false;
                i = j;
            }
            Tok::Op(")") => return Err("unexpected ')'".to_string()),
            Tok::Op(op @ ("<" | ">" | ">>" | "2>" | "2>>" | "&>")) => {
                // Redirect: next token must be a word target.
                match toks.get(i + 1) {
                    Some(Tok::Word(target)) => {
                        cur_redirs.push((op, target.clone()));
                        i += 2;
                    }
                    Some(Tok::Subst(t)) => {
                        cmds.extend(parse_depth(&lex(t)?, depth + 1)?);
                        cur_redirs.push((op, format!("$({t})")));
                        i += 2;
                    }
                    _ => return Err(format!("redirect '{op}' missing a target")),
                }
            }
            Tok::Word(w) => {
                cur_words.push(w.clone());
                i += 1;
            }
            Tok::Subst(s) => {
                // Command substitution as an argument: verify inner recursively
                // by appending as its own command; keep a placeholder word.
                cmds.extend(parse_depth(&lex(s)?, depth + 1)?);
                cur_words.push(format!("$({s})"));
                i += 1;
            }
            Tok::Op(other) => return Err(format!("unexpected operator '{other}'")),
        }
    }
    finish(&mut cmds, &mut cur_words, &mut cur_redirs, subshell);
    Ok(cmds)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Levenshtein distance (small strings only).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j] + 1).min(cur[j] + 1).min(prev[j + 1] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn expand_tilde(p: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if p == "~" {
            return PathBuf::from(home).display().to_string();
        }
        if let Some(rest) = p.strip_prefix("~/") {
            return format!("{}/{rest}", PathBuf::from(home).display());
        }
    }
    p.to_string()
}

fn looks_like_flag(t: &str) -> bool {
    t.starts_with('-') && t.len() > 1 && !t.starts_with("--/")
}

/// Flags whose following word is a value, not a file reference.
const FLAGS_WITH_VALUE: &[&str] = &[
    "-o",
    "-type",
    "-name",
    "-iname",
    "-path",
    "-ipath",
    "-size",
    "-mtime",
    "-mmin",
    "-maxdepth",
    "-mindepth",
    "-perm",
    "-user",
    "-group",
    "-newer",
    "-newermt",
    "--output",
    "--file",
    "--include",
    "--exclude",
];

fn is_env_assignment(w: &str) -> bool {
    let Some((name, _)) = w.split_once('=') else {
        return false;
    };
    let mut it = name.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

const DANGEROUS_TARGETS: &[&str] = &[
    "/", "/*", "/etc", "/usr", "/var", "/bin", "/sbin", "/dev", "/home", "/root", "/boot", "~",
    "$HOME",
];

fn destructive_check(cmds: &[ParsedCommand]) -> Vec<Issue> {
    let mut issues = Vec::new();
    for c in cmds {
        if c.subshell {
            continue;
        }
        let Some(prog) = c.words.first() else {
            continue;
        };
        let base = prog.rsplit('/').next().unwrap_or(prog);
        let bl = base.to_lowercase();
        let args: Vec<&String> = c.words[1..].iter().collect();
        match bl.as_str() {
            "rm" => {
                let recursive = args
                    .iter()
                    .any(|a| a.starts_with('-') && (a.contains('r') || a.contains("recursive")));
                if recursive {
                    for t in args.iter().filter(|a| !a.starts_with('-')) {
                        let ex = expand_tilde(t);
                        if DANGEROUS_TARGETS.contains(&ex.as_str())
                            || DANGEROUS_TARGETS.contains(&t.as_str())
                        {
                            issues.push(Issue {
                                severity: Severity::Error,
                                message: format!(
                                    "destructive: 'rm -r' targeting system path '{t}'"
                                ),
                                suggestion: Some(
                                    "narrow the target to a specific project directory".into(),
                                ),
                            });
                        } else if matches!(t.as_str(), "." | "./" | "*" | "./*" | "~" | "~/*")
                            || ex == std::env::var("HOME").unwrap_or_default()
                        {
                            issues.push(Issue {
                                severity: Severity::Error,
                                message: format!(
                                    "destructive: 'rm -r' targeting '{t}' would delete the current/home directory contents"
                                ),
                                suggestion: Some(
                                    "narrow the target to a specific build or temp directory".into(),
                                ),
                            });
                        } else {
                            issues.push(Issue {
                                severity: Severity::Warning,
                                message: format!("destructive: recursive deletion of '{t}'"),
                                suggestion: Some("verify the target before running".into()),
                            });
                        }
                    }
                }
            }
            b if b.starts_with("mkfs") || matches!(b, "wipefs" | "blkdiscard" | "shred") => {
                if args.iter().any(|a| a.starts_with("/dev/")) {
                    issues.push(Issue {
                        severity: Severity::Error,
                        message: format!("'{bl}' destroys data on a block device"),
                        suggestion: None,
                    });
                }
            }
            "dd" => {
                for a in &args {
                    let al = a.to_lowercase();
                    if al.starts_with("of=/dev/") {
                        issues.push(Issue {
                            severity: Severity::Error,
                            message: format!("dd writing to block device ({a})"),
                            suggestion: None,
                        });
                    }
                }
            }
            "shutdown" | "reboot" | "poweroff" | "halt" => {
                issues.push(Issue {
                    severity: Severity::Warning,
                    message: format!("'{bl}' is disruptive to the whole system"),
                    suggestion: None,
                });
            }
            "chmod" | "chown" => {
                let mut recursive = false;
                for a in &args {
                    if a.as_str() == "-R" || a.as_str() == "--recursive" {
                        recursive = true;
                    }
                    if recursive && (a.as_str() == "/" || a.as_str() == "/*") {
                        issues.push(Issue {
                            severity: Severity::Error,
                            message: format!("'{bl} -R' on filesystem root"),
                            suggestion: None,
                        });
                    }
                }
            }
            "cd" => {
                if let Some(t) = args.iter().find(|a| !a.starts_with('-')) {
                    let ex = expand_tilde(t);
                    if let Ok(cwd) = std::env::current_dir() {
                        let target = if Path::new(&ex).is_absolute() {
                            PathBuf::from(&ex)
                        } else {
                            cwd.join(&ex)
                        };
                        if target.canonicalize().ok() == cwd.canonicalize().ok() {
                            issues.push(Issue {
                                severity: Severity::Warning,
                                message: format!("'cd {t}' has no effect: already in that directory (and a subprocess cannot change your shell's directory)"),
                                suggestion: None,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        // Redirect onto a block device.
        for (op, target) in &c.redirects {
            let tl = target.to_lowercase();
            if matches!(*op, ">" | ">>" | "2>" | "2>>" | "&>")
                && (tl.starts_with("/dev/sd")
                    || tl.starts_with("/dev/nvme")
                    || tl.starts_with("/dev/vd"))
            {
                issues.push(Issue {
                    severity: Severity::Error,
                    message: format!("redirecting output onto block device '{target}'"),
                    suggestion: None,
                });
            }
        }
    }
    issues
}

fn pipe_to_shell_check(toks: &[Tok]) -> Vec<Issue> {
    let mut issues = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t == &Tok::Op("|") || t == &Tok::Op("|&") {
            let has_remote = toks[..i]
                .iter()
                .any(|x| matches!(x, Tok::Word(w) if w == "curl" || w == "wget"));
            let has_shell = toks[i + 1..]
                .iter()
                .any(|x| matches!(x, Tok::Word(w) if w == "sh" || w == "bash" || w == "zsh"));
            if has_remote && has_shell {
                issues.push(Issue {
                    severity: Severity::Error,
                    message: "destructive/untrusted: piping a remote download directly into a shell executes unvetted remote code".to_string(),
                    suggestion: Some("download first, inspect the script, then run it explicitly".into()),
                });
            }
        }
    }
    // fork bomb textual pattern
    issues
}

/// Paths that existed when a candidate was displayed. Recheck these at approval
/// even if the current scan no longer contains any paths with the same suffix.
pub(crate) fn existing_paths(command: &str, cwd: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(commands) = lex(command).and_then(|tokens| parse(&tokens)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for command in commands {
        let words = command
            .words
            .iter()
            .skip_while(|word| is_env_assignment(word));
        if let Some(program) = words.clone().next().filter(|w| !w.contains('/')) {
            if let Some(p) = std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|parent| cwd.join(parent).join(program))
                    .find(|p| {
                        use std::os::unix::fs::PermissionsExt;
                        p.metadata()
                            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    })
            }) {
                if let Ok(target) = p.canonicalize() {
                    found.push((p, target));
                }
            }
        }
        for word in words.chain(command.redirects.iter().map(|(_, word)| word)) {
            if word == "-" || looks_like_flag(word) || word.chars().any(char::is_control) {
                continue;
            }
            let path = cwd.join(expand_tilde(word));
            if let Ok(resolved) = path.canonicalize() {
                if !found.iter().any(|(seen, _)| seen == &path) {
                    found.push((path, resolved));
                }
            }
        }
    }
    found
}

/// Verify `command` against the real filesystem rooted at `cwd`.
pub fn verify(command: &str, cwd: &Path, known: &[FileEntry]) -> Report {
    let mut issues = Vec::new();

    if command.trim().is_empty() {
        return Report {
            command: command.to_owned(),
            issues: vec![Issue {
                severity: Severity::Error,
                message: "no command to verify".into(),
                suggestion: None,
            }],
        };
    }

    let toks = match lex(command) {
        Ok(t) => t,
        Err(e) => {
            issues.push(Issue {
                severity: Severity::Error,
                message: format!("cannot verify: {e}"),
                suggestion: Some("edit the command or cancel".into()),
            });
            return Report {
                command: command.to_string(),
                issues,
            };
        }
    };

    if command.contains(":(){ :|:& };:") || command.contains(":() { :|:& };:") {
        issues.push(Issue {
            severity: Severity::Error,
            message: "fork bomb pattern detected".into(),
            suggestion: None,
        });
    }

    if cwd.to_string_lossy().contains(char::is_whitespace)
        && toks
            .iter()
            .any(|token| matches!(token, Tok::Subst(inner) if inner.trim() == "pwd"))
    {
        issues.push(Issue {
            severity: Severity::Error,
            message: "unquoted working directory substitution splits a path containing spaces"
                .into(),
            suggestion: Some(
                "pass the working directory as a literal argument or use double-quoted $PWD".into(),
            ),
        });
    }
    issues.extend(pipe_to_shell_check(&toks));

    let cmds = match parse(&toks) {
        Ok(c) => c,
        Err(e) => {
            issues.push(Issue {
                severity: Severity::Error,
                message: format!("cannot verify: {e}"),
                suggestion: Some("edit the command or cancel".into()),
            });
            return Report {
                command: command.to_string(),
                issues,
            };
        }
    };

    issues.extend(destructive_check(&cmds));

    // Names produced earlier in the command line (-o out, > file, tee f).
    let mut produced: Vec<String> = Vec::new();
    for c in &cmds {
        for (i, w) in c.words.iter().enumerate() {
            if w == "-o" {
                if let Some(n) = c.words.get(i + 1) {
                    produced.push(n.clone());
                }
            }
        }
        for (op, target) in &c.redirects {
            if matches!(*op, ">" | ">>" | "2>" | "2>>" | "&>") {
                produced.push(target.clone());
            }
        }
    }

    // Executable checks per command (skip env assignments).
    for c in &cmds {
        if c.subshell {
            continue;
        }
        let prog_idx = c.words.iter().position(|w| !is_env_assignment(w));
        let Some(idx) = prog_idx else { continue };
        let prog = &c.words[idx];
        if c.words.len() == idx + 1
            && matches!(
                Path::new(prog).file_name().and_then(|n| n.to_str()),
                Some("basename" | "dirname" | "cp" | "mv" | "mkdir" | "touch" | "chmod" | "chown")
            )
        {
            issues.push(Issue {
                severity: Severity::Error,
                message: format!("missing required operand for '{prog}'"),
                suggestion: Some("supply the requested filename or directory".into()),
            });
        }
        if !prog.contains('/') && !env::command_available(prog) {
            issues.push(Issue {
                severity: Severity::Error,
                message: format!("executable not found on PATH: '{prog}'"),
                suggestion: None,
            });
        }
        if prog.contains('/') {
            let p = if Path::new(prog).is_absolute() {
                PathBuf::from(expand_tilde(prog))
            } else {
                cwd.join(prog)
            };
            let prog_base = Path::new(prog)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let produced_here = produced
                .iter()
                .any(|pr| pr == prog || prog_base == *pr || prog.ends_with(&format!("/{pr}")));
            // A "./path" program built earlier in the same command line
            // (cmake --build, make, cargo build, compiler -o ...) is fine.
            let built_inline = prog.starts_with("./")
                && [
                    "cmake --build",
                    "cargo build",
                    "make",
                    "g++",
                    "clang++",
                    "-o ",
                ]
                .iter()
                .any(|b| command.contains(b));
            if p.exists() && !built_inline && !produced_here {
                use std::os::unix::fs::PermissionsExt;
                if !p
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                {
                    issues.push(Issue {
                        severity: Severity::Error,
                        message: format!("program is not an executable file: '{prog}'"),
                        suggestion: None,
                    });
                }
            }
            if !p.exists() && !produced_here && !built_inline {
                issues.push(Issue {
                    severity: Severity::Error,
                    message: format!("program path does not exist: '{prog}'"),
                    suggestion: None,
                });
            }
        }
    }

    // Extensions present in the directory (for "must exist" logic).
    let known_exts: Vec<String> = known
        .iter()
        .filter(|e| e.kind != FileKind::Directory)
        .filter_map(|e| {
            let n = e.name.to_lowercase();
            n.contains('.')
                .then(|| n.rsplit('.').next().unwrap_or("").to_string())
        })
        .collect();
    let known_names: Vec<&str> = known.iter().map(|e| e.name.as_str()).collect();

    // Argument / redirect-target path checks.
    for c in &cmds {
        let program = c
            .words
            .iter()
            .find(|word| !is_env_assignment(word))
            .and_then(|word| Path::new(word).file_name())
            .and_then(|name| name.to_str());
        for (wi, w) in c.words.iter().enumerate().skip(1) {
            if w == "-" || is_env_assignment(w) || looks_like_flag(w) {
                continue;
            }
            if w.contains("://") {
                continue; // URL, not a filesystem path
            }
            if let Some(prev) = c.words.get(wi.wrapping_sub(1)) {
                if FLAGS_WITH_VALUE.contains(&prev.as_str()) {
                    continue; // value of a flag (e.g. -type f), not a file
                }
            }
            if w.starts_with("$(") || w.starts_with('`') || w.contains('$') {
                continue; // substitution / variable: cannot resolve statically
            }
            if w.chars().any(|c| matches!(c, '*' | '?' | '[')) {
                continue; // glob
            }
            if w.ends_with("/...") || w == "./..." {
                continue; // Go package pattern, not a filesystem path
            }
            let produced_here = produced.iter().any(|p| {
                let pb = Path::new(p)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                *p == *w || pb == *w || w.ends_with(&format!("/{pb}"))
            });
            if produced_here {
                continue;
            }
            if program == Some("kill") {
                if let Ok(pid) = w.parse::<u32>() {
                    if pid == 0 || !Path::new("/proc").join(w).is_dir() {
                        issues.push(Issue {
                            severity: Severity::Error,
                            message: format!(
                                "process does not exist or PID is not a single process: '{w}'"
                            ),
                            suggestion: None,
                        });
                    }
                }
                continue;
            }
            let is_explicit_path = w.contains('/')
                || w.starts_with("./")
                || w.starts_with("../")
                || w == "~"
                || w.starts_with("~/");
            let abs = if is_explicit_path {
                let ex = expand_tilde(w);
                let p = Path::new(&ex);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                }
            } else {
                cwd.join(w)
            };
            if abs.exists() {
                continue;
            }
            let base = Path::new(w)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| w.clone());
            if (program == Some("cp") && wi + 1 == c.words.len() || program == Some("touch"))
                && abs.parent().is_some_and(Path::is_dir)
            {
                continue;
            }
            // Invented filename / wrong capitalization.
            let mut best: Option<(&str, usize, bool)> = None;
            for k in &known_names {
                let d = levenshtein(&base.to_lowercase(), &k.to_lowercase());
                let cie = base.eq_ignore_ascii_case(k);
                if cie || d <= 2 {
                    match best {
                        Some((_, bd, _)) if d >= bd && !cie => {}
                        _ => best = Some((k, d, cie)),
                    }
                }
            }
            if let Some((k, _, cie)) = best {
                issues.push(Issue {
                    severity: Severity::Error,
                    message: if cie {
                        format!("file exists but with different capitalization: '{base}' -> actual file is '{k}'")
                    } else {
                        format!("referenced name '{base}' does not exist; closest real file is '{k}' (possible invented filename)")
                    },
                    suggestion: Some(format!("use '{k}'")),
                });
                continue;
            }
            if is_explicit_path || program == Some("cat") {
                issues.push(Issue {
                    severity: Severity::Error,
                    message: format!("path does not exist: '{w}'"),
                    suggestion: None,
                });
                continue;
            }
            let ext = base.to_lowercase();
            let ext = ext.rsplit('.').next().unwrap_or("").to_string();
            if base.contains('.') && known_exts.contains(&ext) {
                issues.push(Issue {
                    severity: Severity::Error,
                    message: format!("file not found: '{base}'"),
                    suggestion: None,
                });
            }
        }
        // Redirect targets.
        for (op, target) in &c.redirects {
            if target.contains('$') {
                continue;
            }
            let ex = expand_tilde(target);
            let p = if Path::new(&ex).is_absolute() {
                PathBuf::from(&ex)
            } else {
                cwd.join(&ex)
            };
            match *op {
                "<" => {
                    if !p.exists() {
                        issues.push(Issue {
                            severity: Severity::Error,
                            message: format!("input redirect target does not exist: '{target}'"),
                            suggestion: None,
                        });
                    }
                }
                _ => {
                    if let Some(parent) = p.parent() {
                        if !parent.exists() {
                            issues.push(Issue {
                                severity: Severity::Error,
                                message: format!(
                                    "redirect target directory does not exist: '{}'",
                                    parent.display()
                                ),
                                suggestion: None,
                            });
                        }
                    }
                }
            }
        }
    }

    Report {
        command: command.to_string(),
        issues,
    }
}

/// Intent-aware validation: detect clear mismatches between what the user
/// asked for and what the command actually does. These are Errors, so a
/// mismatched command is Blocked and can never be executed with Enter.
pub fn intent_issues(request: &str, command: &str) -> Vec<Issue> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    intent_issues_at(request, command, &cwd)
}

pub(crate) fn intent_issues_at(request: &str, command: &str, cwd: &Path) -> Vec<Issue> {
    use crate::intent::{classify_str, Intent};
    let intent = classify_str(request);
    if intent == Intent::NavigateDir {
        return if navigation_target(command, cwd).is_some() {
            Vec::new()
        } else {
            vec![Issue {
                severity: Severity::Error,
                message: "navigation requires a single literal cd to an existing directory".into(),
                suggestion: Some("resolve the directory from the filesystem first".into()),
            }]
        };
    }
    let lower = request.to_ascii_lowercase();
    if !matches!(
        intent,
        Intent::Build
            | Intent::Run
            | Intent::BuildAndRun
            | Intent::Test
            | Intent::Clean
            | Intent::Explain
            | Intent::RepairCommand
    ) && !lower.contains("extension")
        && !crate::constraints::preserves_files(request, command)
        && !crate::constraints::requested_files(request).is_empty()
    {
        return vec![Issue {
            severity: Severity::Error,
            message: "command does not address the requested literal file".into(),
            suggestion: Some("retain the requested path as one operand".into()),
        }];
    }
    let read_only = matches!(
        intent,
        Intent::Explain
            | Intent::ListDir
            | Intent::InspectProcess
            | Intent::InspectPort
            | Intent::GitLog
            | Intent::GitDiff
            | Intent::GitStatus
            | Intent::MemoryInfo
            | Intent::DiskUsage
    ) && ![
        "save ", "write ", "create ", "replace ", "commit ", "install ", "remove ", "delete ",
        "move ", "append ", "copy ",
    ]
    .iter()
    .any(|verb| lower.contains(verb));
    if read_only {
        let commands = lex(command).and_then(|t| parse(&t)).unwrap_or_default();
        let writes = commands.iter().any(|c| {
            c.redirects
                .iter()
                .any(|(op, dest)| op.contains('>') && dest != "/dev/null")
                || c.words.first().is_some_and(|p| {
                    matches!(
                        Path::new(p).file_name().and_then(|n| n.to_str()),
                        Some(
                            "rm" | "mv"
                                | "touch"
                                | "tee"
                                | "chmod"
                                | "chown"
                                | "kill"
                                | "dd"
                                | "mkdir"
                                | "install"
                                | "sudo"
                        )
                    )
                })
        });
        if writes {
            return vec![Issue{severity:Severity::Error,message:"this inspection request does not authorize the proposed write or process change".into(),suggestion:None}];
        }
    }
    if !matches!(
        intent,
        Intent::Build | Intent::BuildAndRun | Intent::Run | Intent::Test | Intent::Clean
    ) {
        return Vec::new();
    }
    let toks = tokenize(command);
    let first = toks.first().map(|s| s.as_str()).unwrap_or("");
    if first == "cd" {
        return vec![Issue {
            severity: Severity::Error,
            message: "irrelevant to the request: 'cd' does not build or run anything (and a subprocess cannot change your shell's directory)".into(),
            suggestion: Some("use the detected build system instead".into()),
        }];
    }
    if matches!(first, "sh" | "bash" | "zsh" | "fish" | "python3" | "python") && toks.len() <= 1 {
        return vec![Issue {
            severity: Severity::Error,
            message: format!(
                "irrelevant to the request: '{first}' alone does not build or run anything"
            ),
            suggestion: None,
        }];
    }
    // A build/run/test/clean request must reference a build tool, a source
    // pattern, or a built executable path.
    let evidence = [
        "cmake", "make", "ninja", "cargo", "npm", "pnpm", "yarn", "bun", "g++", "gcc", "clang",
        "clang++", "cc", "c++", "python3", "python", "go ", "javac", "ctest", "./", "*.cpp",
        "*.py", "output",
    ];
    let joined = command.to_lowercase();
    if !evidence.iter().any(|e| joined.contains(e)) {
        return vec![Issue {
            severity: Severity::Error,
            message: "command does not build or run anything — it does not address the request"
                .into(),
            suggestion: Some("use the detected build system / preferred build command".into()),
        }];
    }
    // "build AND run" requests must contain a run step, not only build steps.
    if intent == Intent::BuildAndRun {
        let stages = lex(command).and_then(|t| parse(&t)).unwrap_or_default();
        let has_run_step = stages.iter().any(|stage| {
            let Some((program, args)) = stage.words.split_first() else {
                return false;
            };
            let name = Path::new(program)
                .file_name()
                .and_then(|p| p.to_str())
                .unwrap_or("");
            let sub = args.first().map(String::as_str);
            match name {
                "cargo" | "go" => sub == Some("run"),
                "npm" | "pnpm" | "yarn" | "bun" => {
                    let script = if matches!(sub, Some("run" | "run-script")) {
                        args.get(1).map(String::as_str)
                    } else {
                        sub
                    };
                    script.is_some_and(|s| {
                        matches!(s, "start" | "dev" | "serve")
                            && crate::context::fs::read_text(&cwd.join("package.json"), 65536)
                                .ok()
                                .and_then(|text| {
                                    serde_json::from_str::<serde_json::Value>(&text).ok()
                                })
                                .is_some_and(|pkg| pkg["scripts"][s].is_string())
                    })
                }
                "python" | "python3" | "node" => {
                    args.first().is_some_and(|s| cwd.join(s).is_file())
                }
                _ => {
                    program.starts_with("./")
                        || (Path::new(program).is_absolute()
                            && Path::new(program).starts_with(cwd)
                            && Path::new(program).is_file())
                }
            }
        });
        if !has_run_step {
            return vec![Issue {
                severity: Severity::Error,
                message: "request asks to build AND run, but this command only builds — it does not address the request".into(),
                suggestion: Some("append the run step, e.g. '&& ./<executable>' with a real target".into()),
            }];
        }
    }
    Vec::new()
}
