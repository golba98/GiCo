//! Minimal inline terminal rendering. Layout uses visible terminal columns.

use std::io::IsTerminal;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::verify::{Issue, Report, Verdict};

/// Render untrusted text without terminal control sequences.
pub fn safe_multiline(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\n' {
            safe.extend(c.escape_default());
        } else {
            safe.push(c);
        }
    }
    safe
}

pub(crate) fn safe_inline(text: &str) -> String {
    safe_multiline(text).replace('\n', "\\n")
}

pub const MARKER: &str = "gico ›";
/// Reference indent for a standard-width terminal. Layout measures the prefix.
pub const INDENT: usize = 9;
pub const DEFAULT_WIDTH: usize = 80;
const MARGIN: &str = "  ";
const BULLET: &str = "• ";
const ACTIONS: [&str; 4] = ["↵ run", "r refine", "e edit", "c cancel"];
const ACTION_SEPARATOR: &str = " · ";

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn detect() -> Self {
        Style {
            color: std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal(),
        }
    }

    pub fn plain() -> Self {
        Style { color: false }
    }

    pub fn colored() -> Self {
        Style { color: true }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        let text = safe_inline(text);
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    }

    pub fn marker(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn command(&self, text: &str) -> String {
        safe_inline(text)
    }

    pub fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn error(&self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn ok(&self, text: &str) -> String {
        self.paint("32", text)
    }
}

/// Strip terminal escape sequences before measuring styled text.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                chars.by_ref().find(|c| ('\x40'..='\x7e').contains(c));
            }
            Some(']' | 'P' | 'X' | '^' | '_') => {
                let mut esc = false;
                for c in chars.by_ref() {
                    if c == '\x07' || (esc && c == '\\') {
                        break;
                    }
                    esc = c == '\x1b';
                }
            }
            Some('\x20'..='\x2f') => {
                chars.by_ref().find(|c| !('\x20'..='\x2f').contains(c));
            }
            _ => {}
        }
    }
    out
}

pub fn display_width(text: &str) -> usize {
    if text.contains('\x1b') {
        strip_ansi(text).width()
    } else {
        text.width()
    }
}

/// Wrap unstyled text to the space available after indentation.
pub fn wrap(text: &str, width: usize, indent: usize) -> Vec<String> {
    let text = safe_inline(text);
    let available = width.saturating_sub(indent).max(1);
    let mut remaining = text.as_str();
    let mut lines = Vec::new();
    while !remaining.is_empty() {
        let mut columns = 0;
        let mut end = 0;
        let mut space = None;
        for (offset, grapheme) in remaining.grapheme_indices(true) {
            if offset > 0 && grapheme == " " {
                space = Some(offset);
            }
            columns += grapheme.width();
            if columns > available && offset > 0 {
                break;
            }
            end = offset + grapheme.len();
        }
        if end < remaining.len() {
            if let Some(space) = space {
                lines.push(remaining[..space].to_owned());
                remaining = &remaining[space + 1..];
                continue;
            }
        }
        lines.push(remaining[..end].to_owned());
        remaining = &remaining[end..];
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

struct Layout {
    width: usize,
    indent: usize,
    hanging: bool,
    lines: Vec<String>,
}

impl Layout {
    fn new(width: usize) -> Self {
        let width = width.max(1);
        let prefix = display_width(&format!("{MARGIN}{MARKER} "));
        let action_width = ACTIONS
            .iter()
            .map(|action| display_width(action))
            .max()
            .unwrap_or(0);
        let hanging = width >= prefix + action_width;
        let indent = if hanging {
            prefix
        } else if width >= MARGIN.len() + action_width {
            MARGIN.len()
        } else {
            0
        };
        Self {
            width,
            indent,
            hanging,
            lines: Vec::new(),
        }
    }

    fn title(&mut self, title: &str, style: &Style) {
        let pad = " ".repeat(self.indent);
        let mut lines = wrap(title, self.width, self.indent).into_iter();
        if self.hanging {
            let first = lines.next().unwrap_or_default();
            let marker = style.marker(MARKER);
            let line = if first.is_empty() {
                format!("{MARGIN}{marker}")
            } else {
                format!("{MARGIN}{marker} {first}")
            };
            self.lines.push(line);
        } else {
            let margin = if self.width >= MARGIN.len() + display_width(MARKER) {
                MARGIN
            } else {
                ""
            };
            for line in wrap(MARKER, self.width, margin.len()) {
                self.lines.push(format!("{margin}{}", style.marker(&line)));
            }
        }
        self.lines.extend(
            lines
                .filter(|line| !line.is_empty())
                .map(|line| format!("{pad}{line}")),
        );
    }

    fn note(&mut self, text: &str, style: impl Fn(&str) -> String) {
        let pad = " ".repeat(self.indent);
        let bullet_width = display_width(BULLET);
        if self.width <= self.indent + bullet_width {
            self.lines.extend(
                wrap(text, self.width, self.indent)
                    .into_iter()
                    .map(|line| format!("{pad}{}", style(&line))),
            );
            return;
        }
        for (index, line) in wrap(text, self.width, self.indent + bullet_width)
            .into_iter()
            .enumerate()
        {
            let prefix = if index == 0 {
                BULLET.to_owned()
            } else {
                " ".repeat(bullet_width)
            };
            self.lines
                .push(format!("{pad}{}", style(&format!("{prefix}{line}"))));
        }
    }

    fn controls(&mut self, verdict: Verdict, style: &Style) {
        let actions = if verdict == Verdict::Blocked {
            &ACTIONS[1..]
        } else {
            &ACTIONS
        };
        let pad = " ".repeat(self.indent);
        let capacity = self.width - self.indent;
        let mut row = String::new();
        for action in actions {
            if display_width(action) > capacity {
                if !row.is_empty() {
                    self.lines.push(format!("{pad}{}", style.dim(&row)));
                    row.clear();
                }
                self.lines.extend(
                    wrap(action, self.width, self.indent)
                        .into_iter()
                        .map(|line| format!("{pad}{}", style.dim(&line))),
                );
                continue;
            }
            if !row.is_empty() {
                if display_width(&row) + display_width(ACTION_SEPARATOR) + display_width(action)
                    > capacity
                {
                    self.lines.push(format!("{pad}{}", style.dim(&row)));
                    row.clear();
                } else {
                    row.push_str(ACTION_SEPARATOR);
                }
            }
            row.push_str(action);
        }
        if !row.is_empty() {
            self.lines.push(format!("{pad}{}", style.dim(&row)));
        }
    }
}

fn suggestion(
    command: &str,
    report: &Report,
    width: usize,
    style: &Style,
    note: Option<&str>,
    controls: bool,
) -> String {
    let mut layout = Layout::new(width);
    layout.title(command, style);
    let title_lines = layout.lines.len();
    let verdict = report.verdict();
    match verdict {
        Verdict::Blocked => {
            for issue in report.errors() {
                layout.note(&format!("blocked: {}", issue.message), |text| {
                    style.error(text)
                });
                if let Some(suggestion) = &issue.suggestion {
                    layout.note(&format!("suggestion: {suggestion}"), |text| style.dim(text));
                }
            }
        }
        Verdict::Warning => {
            for issue in report.warnings() {
                layout.note(&format!("warning: {}", issue.message), |text| {
                    style.warn(text)
                });
            }
        }
        Verdict::Valid => {}
    }
    for note in note
        .into_iter()
        .flat_map(str::lines)
        .map(str::trim)
        .filter(|note| !note.is_empty())
    {
        layout.note(note, |text| {
            if note.starts_with("Skipped stale builds: ") {
                style.warn(text)
            } else {
                style.dim(text)
            }
        });
    }
    if controls {
        if layout.lines.len() > title_lines {
            layout.lines.push(String::new());
        }
        layout.controls(verdict, style);
    }
    layout.lines.join("\n")
}

/// Render an interactive suggestion with its available actions.
pub fn render_suggestion(
    command: &str,
    report: &Report,
    width: usize,
    style: &Style,
    note: Option<&str>,
) -> String {
    suggestion(command, report, width, style, note, true)
}

/// Render a suggestion for piped or redirected output.
pub fn render_preview(command: &str, report: &Report, note: Option<&str>) -> String {
    suggestion(command, report, DEFAULT_WIDTH, &Style::plain(), note, false)
}

/// Render an informational response at the default width.
pub fn render_info(title: &str, detail: &str, style: &Style) -> String {
    render_info_at_width(title, detail, DEFAULT_WIDTH, style)
}

pub fn render_info_at_width(title: &str, detail: &str, width: usize, style: &Style) -> String {
    let mut layout = Layout::new(width);
    layout.title(title, style);
    for line in detail.lines().filter(|line| !line.trim().is_empty()) {
        layout.note(line, |text| style.dim(text));
    }
    layout.lines.join("\n")
}

/// One-line prompt used for refine/edit input.
pub fn render_prompt_line(prompt: &str, text: &str, style: &Style) -> String {
    format!("{} {}", style.marker(prompt), safe_inline(text))
}

pub fn issues_summary(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|i| i.message.as_str()).collect()
}
