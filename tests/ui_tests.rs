//! UI rendering tests: minimal two-line interface, alignment, colors,
//! NO_COLOR, narrow widths, informational responses.

use gico::ui::{self, Style};
use gico::verify::{Issue, Report, Severity, Verdict};

fn report(cmd: &str, issues: Vec<Issue>) -> Report {
    Report {
        command: cmd.to_string(),
        issues,
    }
}

fn command_indent() -> usize {
    ui::display_width("  gico › ")
}

fn ok_report(cmd: &str) -> Report {
    report(cmd, vec![])
}

#[test]
fn suggestion_is_two_lines_for_valid_command() {
    let s = ui::render_suggestion(
        "find . -type f -name '*.cpp'",
        &ok_report("x"),
        80,
        &Style::plain(),
        None,
    );
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 lines, got: {s:?}");
    assert!(lines[0].starts_with("  gico › "), "line0: {:?}", lines[0]);
    assert!(lines[0].contains("find . -type f -name '*.cpp'"));
    assert!(lines[1].contains("↵ run"), "line1: {:?}", lines[1]);
    assert!(lines[1].contains("r refine"));
    assert!(lines[1].contains("e edit"));
    assert!(lines[1].contains("c cancel"));
    // controls are aligned under the command (same indent as marker text)
    assert!(
        lines[1].starts_with(&" ".repeat(command_indent())),
        "indent: {:?}",
        lines[1]
    );
}

#[test]
fn blocked_command_has_no_run_control_and_shows_error() {
    let r = report(
        "cat missing.txt",
        vec![Issue {
            severity: Severity::Error,
            message: "file not found: 'missing.txt'".into(),
            suggestion: None,
        }],
    );
    let s = ui::render_suggestion("cat missing.txt", &r, 80, &Style::plain(), None);
    assert!(!s.contains("↵ run"), "blocked must not offer run: {s}");
    assert!(s.contains("blocked:"));
    assert!(s.contains("missing.txt"));
    assert!(s.contains("r refine") && s.contains("e edit") && s.contains("c cancel"));
    assert_eq!(r.verdict(), Verdict::Blocked);
}

#[test]
fn warning_keeps_run_control_and_shows_warning() {
    let r = report(
        "reboot",
        vec![Issue {
            severity: Severity::Warning,
            message: "disruptive".into(),
            suggestion: None,
        }],
    );
    let s = ui::render_suggestion("reboot", &r, 80, &Style::plain(), None);
    assert!(s.contains("↵ run"));
    assert!(s.contains("warning:"));
    assert_eq!(r.verdict(), Verdict::Warning);
}

#[test]
fn informational_response_has_no_controls() {
    let s = ui::render_info(
        "No buildable C++ project found.",
        "No source files or build configuration detected.",
        &Style::plain(),
    );
    assert!(s.contains("gico › No buildable C++ project found."));
    assert!(s.contains("No source files"));
    assert!(!s.contains("run"));
    assert!(!s.contains("cancel"));
    assert_eq!(s.lines().count(), 2);
}

#[test]
fn ansi_colors_present_only_when_enabled() {
    let colored = ui::render_suggestion("ls", &ok_report("ls"), 80, &Style::colored(), None);
    assert!(
        colored.contains("\x1b["),
        "expected ANSI codes: {colored:?}"
    );
    let plain = ui::render_suggestion("ls", &ok_report("ls"), 80, &Style::plain(), None);
    assert!(!plain.contains("\x1b["));
    assert_eq!(ui::strip_ansi(&colored), plain);
}

#[test]
fn narrow_terminal_wraps_instead_of_overflowing() {
    let long = "g++ main.cpp Wallet.cpp OrderBook.cpp OrderBookEntry.cpp MerkelMain.cpp CSVReader.cpp -o output";
    let s = ui::render_suggestion(long, &ok_report(long), 40, &Style::plain(), None);
    for line in s.lines() {
        // allow the controls line to be short; command lines must fit width
        if line.contains("g++") || line.contains("Wallet.cpp") || line.contains("CSVReader.cpp") {
            assert!(
                line.chars().count() <= 40,
                "line too wide ({}): {line:?}",
                line.chars().count()
            );
        }
    }
    // nothing lost
    assert!(ui::strip_ansi(&s)
        .replace('\n', " ")
        .contains("CSVReader.cpp"));
}

#[test]
fn marker_is_gico_not_shell_prompt() {
    let s = ui::render_suggestion("ls", &ok_report("ls"), 80, &Style::plain(), None);
    assert!(s.contains("gico ›"));
    assert!(!s.contains("OK 4-IT Pixel Generator"));
}

#[test]
fn note_line_is_aligned_with_controls_and_separated_by_one_blank_row() {
    let s = ui::render_suggestion(
        "cat missing.txt",
        &report(
            "cat missing.txt",
            vec![Issue {
                severity: Severity::Error,
                message: "file not found: 'missing.txt'".into(),
                suggestion: None,
            }],
        ),
        80,
        &Style::plain(),
        Some("Enter cannot run a blocked command"),
    );
    let note_line = s.lines().find(|l| l.contains("Enter cannot run")).unwrap();
    assert!(
        note_line.starts_with(&" ".repeat(command_indent())),
        "note must be aligned: {note_line:?}"
    );
    let controls = s.lines().find(|l| l.contains("cancel")).unwrap();
    assert!(
        controls.starts_with(&" ".repeat(command_indent())),
        "controls must be aligned: {controls:?}"
    );
    assert_eq!(s.lines().filter(|line| line.is_empty()).count(), 1);
    assert!(s.contains(&format!("\n\n{controls}")));
}

#[test]
fn gui_notes_at_wide_width_match_the_inline_layout() {
    let command = "./build-gico-test/src/gui/it-pixel-gui";
    let note =
        "Skipped stale builds: build/, build-debug/\nGUI application · Display server required";
    let plain = ui::render_suggestion(
        command,
        &ok_report(command),
        160,
        &Style::plain(),
        Some(note),
    );
    assert_eq!(
        plain,
        "  gico › ./build-gico-test/src/gui/it-pixel-gui\n         • Skipped stale builds: build/, build-debug/\n         • GUI application · Display server required\n\n         ↵ run · r refine · e edit · c cancel"
    );
    let color = ui::render_suggestion(
        command,
        &ok_report(command),
        160,
        &Style::colored(),
        Some(note),
    );
    assert_eq!(ui::strip_ansi(&color), plain);
    assert!(color.contains("\x1b[36mgico ›\x1b[0m ./build"));
    assert!(color.contains("\x1b[33m• Skipped stale builds"));
    assert!(color.contains("\x1b[2m↵ run · r refine"));
}

#[test]
fn narrow_layout_wraps_quoted_paths_notes_and_controls_by_display_columns() {
    let command = "cat 'archive folder/文件資料🧑‍💻e\u{301}abcdeabcdeabcde.txt'";
    let warning = Issue {
        severity: Severity::Warning,
        message: "This long warning wraps beneath its first word.".into(),
        suggestion: None,
    };
    let r = report(command, vec![warning]);
    for width in [12, 18, 27, 80] {
        let colored = ui::render_suggestion(
            command,
            &r,
            width,
            &Style::colored(),
            Some("First note\nSecond note with several words"),
        );
        let plain = ui::strip_ansi(&colored);
        for line in plain.lines() {
            assert!(ui::display_width(line) <= width, "at {width}: {line:?}");
        }
        assert!(plain.contains("🧑‍💻"));
        assert!(plain.contains("e\u{301}"));
        assert!(plain.contains("r refine") && plain.contains("c cancel"));
        let action = plain
            .lines()
            .find(|line| line.contains("r refine"))
            .unwrap();
        let indent = action.len() - action.trim_start().len();
        let label = if width >= 20 { "warning:" } else { "warn" };
        assert!(plain.contains(&format!("{}• {label}", " ".repeat(indent))));
    }
}

#[test]
fn display_width_ignores_ansi_and_keeps_combining_and_emoji_sequences() {
    assert_eq!(ui::display_width("\x1b[2K\x1b[36m文e\u{301}🧑‍💻\x1b[0m"), 5);
    assert_eq!(ui::display_width("\x1b]0;hidden title\x07gico"), 4);
    assert_eq!(
        ui::wrap("printf 'a  b'", 80, command_indent()),
        ["printf 'a  b'"]
    );
}

#[test]
fn redirected_preview_contains_diagnostics_without_terminal_actions() {
    let preview = ui::render_preview("ls", &ok_report("ls"), Some("A relevant note\n "));
    assert_eq!(preview, "  gico › ls\n         • A relevant note");
    assert!(!preview.contains('\x1b'));
    assert!(!preview.contains('\r'));
}
