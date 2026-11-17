//! Keyboard mapping tests: exact controls from the spec.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use gico::interact::{self, Action};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn enter_maps_to_run() {
    assert_eq!(
        interact::map_key(key(KeyCode::Enter, KeyModifiers::NONE)),
        Some(Action::Run)
    );
}

#[test]
fn r_refine_e_edit_c_cancel() {
    assert_eq!(
        interact::map_key(key(KeyCode::Char('r'), KeyModifiers::NONE)),
        Some(Action::Refine)
    );
    assert_eq!(
        interact::map_key(key(KeyCode::Char('e'), KeyModifiers::NONE)),
        Some(Action::Edit)
    );
    assert_eq!(
        interact::map_key(key(KeyCode::Char('c'), KeyModifiers::NONE)),
        Some(Action::Cancel)
    );
}

#[test]
fn escape_and_ctrl_c_cancel() {
    assert_eq!(
        interact::map_key(key(KeyCode::Esc, KeyModifiers::NONE)),
        Some(Action::Cancel)
    );
    assert_eq!(
        interact::map_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Action::Cancel)
    );
}

#[test]
fn key_release_events_are_ignored() {
    let mut k = key(KeyCode::Enter, KeyModifiers::NONE);
    k.kind = KeyEventKind::Release;
    assert_eq!(interact::map_key(k), None);
}

#[test]
fn unrelated_keys_do_nothing() {
    assert_eq!(
        interact::map_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        interact::map_key(key(KeyCode::F(1), KeyModifiers::NONE)),
        None
    );
}
