//! Raw-mode keyboard interaction: single-key controls and inline line
//! editors for refine/edit. No full-screen TUI — just crossterm events.

use std::io::{self, Write};

use crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, queue, terminal};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::{self, display_width};

/// What the user asked for with a single key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Run,
    Refine,
    Edit,
    Cancel,
}

/// Pure mapping from a key event to an action. Enter runs (the caller guards
/// blocked commands); r refines; e edits; c, Escape and Ctrl+C cancel.
pub fn map_key(k: KeyEvent) -> Option<Action> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    match (k.code, k.modifiers) {
        (KeyCode::Enter, _) => Some(Action::Run),
        (KeyCode::Char('r'), KeyModifiers::NONE) | (KeyCode::Char('R'), KeyModifiers::SHIFT) => {
            Some(Action::Refine)
        }
        (KeyCode::Char('e'), KeyModifiers::NONE) | (KeyCode::Char('E'), KeyModifiers::SHIFT) => {
            Some(Action::Edit)
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) | (KeyCode::Char('C'), KeyModifiers::SHIFT) => {
            Some(Action::Cancel)
        }
        (KeyCode::Esc, _) => Some(Action::Cancel),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Cancel),
        _ => None,
    }
}

/// RAII raw-mode guard: restores the terminal on drop, even on panic paths
/// that unwind through it.
pub struct RawMode;

pub fn enable_raw_mode() -> io::Result<RawMode> {
    terminal::enable_raw_mode()?;
    Ok(RawMode)
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Read one key press. Returns None for unmapped keys.
pub fn read_action() -> io::Result<Option<Action>> {
    loop {
        let ev = event::read()?;
        if let event::Event::Key(k) = ev {
            if let Some(a) = map_key(k) {
                return Ok(Some(a));
            }
        }
    }
}

/// Write a complete block while the terminal has output processing disabled.
pub fn write_raw_block(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\r{}\r\n", text.replace('\n', "\r\n"))?;
    out.flush()
}

fn editor_view(
    prompt: &str,
    text: &str,
    cursor: usize,
    start: &mut usize,
    width: usize,
) -> (String, usize) {
    let max_columns = width.saturating_sub(1);
    let prompt_width = display_width(prompt);
    // Tiny windows still get an input cell and never wrap the prompt.
    let shown_prompt = if prompt_width < max_columns {
        prompt.to_owned()
    } else {
        let mut clipped = String::new();
        let mut room = max_columns.saturating_sub(1);
        for part in ui::strip_ansi(prompt).graphemes(true) {
            let columns = display_width(part);
            if columns > room {
                break;
            }
            clipped.push_str(part);
            room -= columns;
        }
        clipped
    };
    let prompt_width = display_width(&shown_prompt);
    let room = max_columns - prompt_width;
    let parts: Vec<_> = text
        .grapheme_indices(true)
        .map(|(offset, part)| {
            let visible = ui::safe_inline(part);
            let columns = display_width(&visible);
            (offset, visible, columns)
        })
        .collect();
    let caret = parts.partition_point(|part| part.0 < cursor);
    let mut first = parts.partition_point(|part| part.0 < *start).min(caret);
    let mut cursor_columns: usize = parts[first..caret].iter().map(|part| part.2).sum();
    let marker = "…";
    let marker_width = display_width(marker);
    let left_marker = |first| usize::from(first > 0 && room > marker_width) * marker_width;
    let right_reserve = usize::from(caret < parts.len() && room > marker_width) * marker_width;
    while first < caret && left_marker(first) + cursor_columns + right_reserve > room {
        cursor_columns -= parts[first].2;
        first += 1;
    }
    *start = parts.get(first).map_or(text.len(), |part| part.0);

    let mut frame = shown_prompt;
    let left = left_marker(first);
    if left > 0 {
        frame.push_str(marker);
    }
    let mut remaining = room - left;
    let remaining_text: usize = parts[first..].iter().map(|part| part.2).sum();
    let right = remaining_text > remaining && remaining >= marker_width;
    if right {
        remaining -= marker_width;
    }
    for (_, part, columns) in &parts[first..] {
        if *columns > remaining {
            break;
        }
        frame.push_str(part);
        remaining -= columns;
    }
    if right {
        frame.push_str(marker);
    }
    (frame, prompt_width + left + cursor_columns)
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(offset, _)| offset)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |part| cursor + part.len())
}

fn normalize_boundary(text: &str, cursor: usize) -> usize {
    text.grapheme_indices(true)
        .find(|(offset, _)| *offset >= cursor)
        .map_or(text.len(), |(offset, _)| offset)
}

/// Read a line with a scrolling inline editor. Cursor movements use grapheme
/// boundaries, and only the visible viewport is drawn on the current row.
pub fn read_line(prompt: &str, initial: &str) -> io::Result<Option<String>> {
    let mut text = initial.to_owned();
    let mut caret = text.len();
    let mut start = 0;
    let mut out = io::stdout().lock();
    loop {
        let (frame, column) = editor_view(prompt, &text, caret, &mut start, term_width());
        queue!(
            out,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::CurrentLine)
        )?;
        write!(out, "{frame}")?;
        queue!(out, cursor::MoveToColumn(column as u16))?;
        out.flush()?;
        let key = match event::read()? {
            event::Event::Key(k) if k.kind != KeyEventKind::Release => k,
            _ => continue,
        };
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _)
            | (KeyCode::Esc, _)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                write!(out, "\r\n")?;
                out.flush()?;
                return Ok((key.code == KeyCode::Enter).then_some(text));
            }
            (KeyCode::Backspace, _) => {
                let previous = previous_boundary(&text, caret);
                text.drain(previous..caret);
                caret = normalize_boundary(&text, previous);
            }
            (KeyCode::Delete, _) => {
                let next = next_boundary(&text, caret);
                text.drain(caret..next);
                caret = normalize_boundary(&text, caret);
            }
            (KeyCode::Left, _) => caret = previous_boundary(&text, caret),
            (KeyCode::Right, _) => caret = next_boundary(&text, caret),
            (KeyCode::Home, _) => caret = 0,
            (KeyCode::End, _) => caret = text.len(),
            (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
                text.insert(caret, c);
                caret = normalize_boundary(&text, caret + c.len_utf8());
            }
            _ => {}
        }
    }
}

/// Current terminal width (columns), with a fallback for unset PTY sizes.
pub fn term_width() -> usize {
    terminal::size()
        .ok()
        .map(|(width, _)| usize::from(width))
        .filter(|width| *width > 0)
        .unwrap_or(ui::DEFAULT_WIDTH)
}
